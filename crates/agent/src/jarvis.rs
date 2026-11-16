//! Jarvis's conversation: one agent session, its turns recorded in the conversation log.

use crate::acp::{AgentClient, AgentEvent, AgentSpec, TurnEnd};
use crate::log::{ConversationLog, Event, ToolState, TurnOutcome};
use anyhow::Context;
use futures::future::BoxFuture;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

const BUSY: &str = "Jarvis répond encore ; attends la fin ou arrête-le.";
const NOT_ACTIVE: &str = "Cette demande n'est plus active.";
const NEW_CONVERSATION: &str =
    "Nouvelle conversation avec l'agent : l'ancienne n'a pas pu être reprise.";

/// Opens an agent session: `AgentClient::spawn` in production, a fake agent in tests.
pub type Connector = Arc<
    dyn Fn(
            &AgentSpec,
            Option<String>,
            UnboundedSender<AgentEvent>,
        ) -> BoxFuture<'static, anyhow::Result<AgentClient>>
        + Send
        + Sync,
>;

/// Starts the agent process with `AgentClient::spawn`.
pub fn process_connector() -> Connector {
    Arc::new(|spec: &AgentSpec, resume, events| {
        let spec = spec.clone();
        Box::pin(async move { AgentClient::spawn(&spec, resume, events).await })
    })
}

pub struct Jarvis {
    spec: AgentSpec,
    connector: Connector,
    log: ConversationLog,
    session_file: PathBuf,
    session: Mutex<Option<Session>>,
    /// Permission requests shown in the log and not answered yet.
    pending: Mutex<HashSet<u64>>,
    busy: AtomicBool,
    /// Set by `cancel` during a turn, checked before the prompt is sent.
    cancel_requested: AtomicBool,
}

/// A connected agent and the task recording its events.
#[derive(Clone)]
struct Session {
    client: Arc<AgentClient>,
    flushes: UnboundedSender<oneshot::Sender<()>>,
}

impl Session {
    /// Returns once every event the agent sent so far is in the log.
    async fn flush(&self) {
        let (done, flushed) = oneshot::channel();
        if self.flushes.send(done).is_ok() {
            let _ = flushed.await;
        }
    }
}

impl Jarvis {
    /// Opens the conversation stored in `data_dir`; the agent is started on the first message.
    pub fn open(
        spec: AgentSpec,
        data_dir: &Path,
        connector: Connector,
    ) -> anyhow::Result<Arc<Self>> {
        let log = ConversationLog::open(&data_dir.join("jarvis.jsonl"))?;
        Ok(Arc::new(Self {
            spec,
            connector,
            log,
            session_file: data_dir.join("jarvis.session"),
            session: Mutex::new(None),
            pending: Mutex::default(),
            busy: AtomicBool::new(false),
            cancel_requested: AtomicBool::new(false),
        }))
    }

    pub fn log(&self) -> &ConversationLog {
        &self.log
    }

    /// Logs the message and runs the turn in the background; refused while a turn runs.
    pub async fn send(self: &Arc<Self>, text: String) {
        if self.busy.swap(true, Ordering::SeqCst) {
            self.append(Event::Notice { text: BUSY.into() });
            return;
        }
        self.cancel_requested.store(false, Ordering::SeqCst);
        self.append(Event::User { text: text.clone() });
        let jarvis = self.clone();
        tokio::spawn(async move {
            let outcome = match jarvis.run_turn(&text).await {
                Ok(outcome) => outcome,
                Err(error) => {
                    jarvis.append(Event::Notice {
                        text: format!("{error:#}"),
                    });
                    TurnOutcome::Error
                }
            };
            jarvis.append(Event::TurnEnded { outcome });
            jarvis.busy.store(false, Ordering::SeqCst);
        });
    }

    /// Stops the running turn, if any.
    pub fn cancel(&self) {
        if !self.busy.load(Ordering::SeqCst) {
            return;
        }
        self.cancel_requested.store(true, Ordering::SeqCst);
        let session = self.session_slot().clone();
        if let Some(session) = session
            && let Err(error) = session.client.cancel()
        {
            tracing::warn!("cannot cancel the turn: {error:#}");
        }
    }

    /// Answers the permission request `id` shown in the log.
    pub fn answer(&self, id: u64, allow: bool) {
        let session = self.session_slot().clone();
        let active = lock(&self.pending).remove(&id);
        let Some(session) = session.filter(|_| active) else {
            self.append(Event::Notice {
                text: NOT_ACTIVE.into(),
            });
            return;
        };
        // Logged first so the answer precedes whatever the agent does next.
        self.append(Event::PermissionAnswered {
            id: id.to_string(),
            allowed: allow,
        });
        if let Err(error) = session.client.answer_permission(id, allow) {
            tracing::warn!("the answer came too late: {error:#}");
        }
    }

    async fn run_turn(self: &Arc<Self>, text: &str) -> anyhow::Result<TurnOutcome> {
        let session = self.connect().await?;
        if self.cancel_requested.load(Ordering::SeqCst) {
            return Ok(TurnOutcome::Cancelled);
        }
        let end = session.client.prompt(text).await;
        session.flush().await;
        if end.is_err() {
            // A failed turn may leave the connection broken: the next message reconnects.
            *self.session_slot() = None;
        }
        Ok(match end? {
            TurnEnd::Done => TurnOutcome::Done,
            TurnEnd::Cancelled => TurnOutcome::Cancelled,
            TurnEnd::Refused => TurnOutcome::Refused,
            TurnEnd::Limit => TurnOutcome::Limit,
        })
    }

    /// The current session, connecting (and resuming the saved one) when there is none.
    async fn connect(self: &Arc<Self>) -> anyhow::Result<Session> {
        if let Some(session) = self.session_slot().clone() {
            return Ok(session);
        }
        let saved = self.saved_session_id();
        let (events, agent_events) = mpsc::unbounded_channel();
        let client = (self.connector)(&self.spec, saved.clone(), events).await?;
        let id = client.session_id();
        if saved.as_deref() != Some(id) {
            if saved.is_some() {
                self.append(Event::Notice {
                    text: NEW_CONVERSATION.into(),
                });
            }
            if let Err(error) = self.save_session_id(id) {
                tracing::warn!("cannot save the agent session id: {error:#}");
            }
        }
        let (flushes, flush_requests) = mpsc::unbounded_channel();
        tokio::spawn(record_events(
            Arc::downgrade(self),
            agent_events,
            flush_requests,
        ));
        let session = Session {
            client: Arc::new(client),
            flushes,
        };
        *self.session_slot() = Some(session.clone());
        Ok(session)
    }

    fn saved_session_id(&self) -> Option<String> {
        match std::fs::read_to_string(&self.session_file) {
            Ok(id) => Some(id.trim().to_owned()).filter(|id| !id.is_empty()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                tracing::warn!(
                    "cannot read {}, starting a new session: {error}",
                    self.session_file.display()
                );
                None
            }
        }
    }

    /// Written aside then renamed, so a crash never leaves half an id behind.
    fn save_session_id(&self, id: &str) -> anyhow::Result<()> {
        let temporary = self.session_file.with_extension("session.tmp");
        std::fs::write(&temporary, id)
            .with_context(|| format!("writing {}", temporary.display()))?;
        std::fs::rename(&temporary, &self.session_file)
            .with_context(|| format!("replacing {}", self.session_file.display()))
    }

    fn record(&self, event: AgentEvent, tool_titles: &mut HashMap<String, String>) {
        let event = match event {
            AgentEvent::Text(text) => Event::Assistant { text },
            AgentEvent::ToolStarted { id, title } => {
                tool_titles.insert(id.clone(), title.clone());
                Event::Tool {
                    id,
                    title,
                    state: ToolState::Running,
                }
            }
            AgentEvent::ToolFinished { id, ok } => Event::Tool {
                title: tool_titles.remove(&id).unwrap_or_default(),
                id,
                state: if ok {
                    ToolState::Done
                } else {
                    ToolState::Failed
                },
            },
            AgentEvent::PermissionAsked { id, title } => {
                lock(&self.pending).insert(id);
                Event::Permission {
                    id: id.to_string(),
                    title,
                }
            }
            AgentEvent::PermissionClosed { id } => {
                // Already logged as answered when the answer raced the end of the turn.
                if !lock(&self.pending).remove(&id) {
                    return;
                }
                Event::PermissionAnswered {
                    id: id.to_string(),
                    allowed: false,
                }
            }
        };
        self.append(event);
    }

    fn append(&self, event: Event) {
        if let Err(error) = self.log.append(event) {
            tracing::error!("cannot write the conversation log: {error:#}");
        }
    }

    fn session_slot(&self) -> MutexGuard<'_, Option<Session>> {
        lock(&self.session)
    }
}

/// Logs the agent's events for the lifetime of its client and answers flush requests.
async fn record_events(
    jarvis: Weak<Jarvis>,
    mut events: UnboundedReceiver<AgentEvent>,
    mut flush_requests: UnboundedReceiver<oneshot::Sender<()>>,
) {
    let mut tool_titles = HashMap::new();
    loop {
        tokio::select! {
            // Events first: a flush is acknowledged only once those sent before it are logged.
            biased;
            event = events.recv() => {
                let (Some(event), Some(owner)) = (event, jarvis.upgrade()) else {
                    break;
                };
                owner.record(event, &mut tool_titles);
            }
            Some(done) = flush_requests.recv() => {
                let _ = done.send(());
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
