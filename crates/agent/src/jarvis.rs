//! Jarvis's conversation: one agent session, its turns recorded in the conversation log.

use crate::acp::{AgentClient, AgentEvent, AgentSpec, Failure, TurnEnd};
use crate::log::{ConversationLog, Event, ToolState, TurnOutcome};
use anyhow::{Context, anyhow};
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

const BUSY: &str = "Jarvis répond encore ; attends la fin ou arrête-le.";
const NOT_ACTIVE: &str = "Cette demande n'est plus active.";
const NEW_CONVERSATION: &str =
    "Nouvelle conversation avec l'agent : l'ancienne n'a pas pu être reprise.";
const NO_START: &str = "L'agent ne répond pas au démarrage.";
const NOT_INSTALLED: &str =
    "Impossible de démarrer l'agent : installe @agentclientprotocol/claude-agent-acp.";
const SIGNED_OUT: &str =
    "L'agent n'est pas connecté à Claude : voir le README (claude setup-token).";
const FAILED: &str = "L'agent a rencontré une erreur.";
const INTERRUPTED: &str = "La réponse précédente a été interrompue.";
/// How long the agent gets to start and open its session.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the agent gets to end a cancelled turn before it is stopped.
const CANCEL_GRACE: Duration = Duration::from_secs(10);

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
    /// Permission requests shown in the log and not answered yet, by their id in the log: the
    /// client that asked and its own id for the request.
    pending: Mutex<HashMap<u64, (Weak<AgentClient>, u64)>>,
    /// Cancels the running turn; `None` when idle.
    turn: Mutex<Option<CancellationToken>>,
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
        let jarvis = Arc::new(Self {
            spec,
            connector,
            log,
            session_file: data_dir.join("jarvis.session"),
            session: Mutex::new(None),
            pending: Mutex::default(),
            turn: Mutex::new(None),
        });
        // A turn the server stopped in the middle of would otherwise run forever on the pages.
        let last_turn = jarvis
            .log
            .since(0)
            .into_iter()
            .rev()
            .find_map(|entry| match entry.event {
                Event::User { .. } => Some(true),
                Event::TurnEnded { .. } => Some(false),
                _ => None,
            });
        if last_turn == Some(true) {
            jarvis.append(Event::Notice {
                text: INTERRUPTED.into(),
            });
            jarvis.append(Event::TurnEnded {
                outcome: TurnOutcome::Error,
            });
        }
        Ok(jarvis)
    }

    pub fn log(&self) -> &ConversationLog {
        &self.log
    }

    /// Logs the message and runs the turn in the background; refused while a turn runs.
    pub async fn send(self: &Arc<Self>, text: String) {
        // The turn is locked while its start or refusal is logged, as it is while its end is
        // logged: a message sent on seeing `TurnEnded` finds the turn over. Locks are always
        // taken turn first, then log.
        let cancel = {
            let mut turn = lock(&self.turn);
            if turn.is_some() {
                self.append(Event::Notice { text: BUSY.into() });
                return;
            }
            self.append(Event::User { text: text.clone() });
            turn.insert(CancellationToken::new()).clone()
        };
        let jarvis = self.clone();
        tokio::spawn(async move {
            let outcome = match jarvis.run_turn(&text, &cancel).await {
                Ok(outcome) => outcome,
                Err(error) => {
                    tracing::warn!("the turn failed: {error:#}");
                    jarvis.append(Event::Notice {
                        text: notice_for(&error).into(),
                    });
                    TurnOutcome::Error
                }
            };
            let mut turn = lock(&jarvis.turn);
            jarvis.append(Event::TurnEnded { outcome });
            *turn = None;
        });
    }

    /// Stops the running turn, if any.
    pub fn cancel(&self) {
        if let Some(cancel) = lock(&self.turn).as_ref() {
            cancel.cancel();
        }
    }

    /// Answers the permission request `id` shown in the log.
    pub fn answer(&self, id: u64, allow: bool) {
        let asked = lock(&self.pending).remove(&id);
        let Some((client, request)) =
            asked.and_then(|(client, request)| Some((client.upgrade()?, request)))
        else {
            self.append(Event::Notice {
                text: NOT_ACTIVE.into(),
            });
            return;
        };
        // Logged first so the answer precedes whatever the agent does next.
        self.append(Event::PermissionAnswered { id, allowed: allow });
        if let Err(error) = client.answer_permission(request, allow) {
            tracing::warn!("the answer came too late: {error:#}");
        }
    }

    async fn run_turn(
        self: &Arc<Self>,
        text: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<TurnOutcome> {
        // Dropping the connect future stops an agent that is still starting.
        let session = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(TurnOutcome::Cancelled),
            connected = tokio::time::timeout(START_TIMEOUT, self.connect()) => {
                connected.map_err(|_| anyhow!(StartTimeout))??
            }
        };
        let prompt = session.client.prompt(text);
        tokio::pin!(prompt);
        let mut wedged = false;
        let end = tokio::select! {
            // The prompt is polled first so the cancel always reaches the agent after it.
            biased;
            end = &mut prompt => end,
            () = cancel.cancelled() => {
                if let Err(error) = session.client.cancel() {
                    tracing::warn!("cannot cancel the turn: {error:#}");
                }
                match tokio::time::timeout(CANCEL_GRACE, &mut prompt).await {
                    Ok(end) => end,
                    Err(_) => {
                        tracing::warn!(
                            "the agent did not end the cancelled turn within {CANCEL_GRACE:?}; stopping it"
                        );
                        wedged = true;
                        Ok(TurnEnd::Cancelled)
                    }
                }
            }
        };
        session.flush().await;
        if end.is_err() || wedged {
            // A failed turn may leave the connection broken, and a wedged agent is stopped by
            // dropping its client: the next message reconnects.
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
        let client = Arc::new(client);
        let (flushes, flush_requests) = mpsc::unbounded_channel();
        tokio::spawn(record_events(
            Arc::downgrade(self),
            Recorder::new(Arc::downgrade(&client)),
            agent_events,
            flush_requests,
        ));
        let session = Session { client, flushes };
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

    fn record(&self, event: AgentEvent, recorder: &mut Recorder) {
        let event = match event {
            AgentEvent::Text(text) => Event::Assistant { text },
            AgentEvent::ToolStarted { id, title } => {
                recorder.tool_titles.insert(id.clone(), title.clone());
                Event::Tool {
                    id,
                    title,
                    state: ToolState::Running,
                }
            }
            AgentEvent::ToolFinished { id, ok } => Event::Tool {
                title: recorder.tool_titles.remove(&id).unwrap_or_default(),
                id,
                state: if ok {
                    ToolState::Done
                } else {
                    ToolState::Failed
                },
            },
            AgentEvent::PermissionAsked { id: request, title } => {
                // The agent numbers its requests from 1 on each connection: the log's id is the
                // entry's seq. It is pending before the entry is broadcast, so an answer to it
                // finds it. Lock order: log, then pending.
                let client = recorder.client.clone();
                let asked = self.log.append_with(|id| {
                    lock(&self.pending).insert(id, (client, request));
                    Event::Permission { id, title }
                });
                match asked {
                    Ok(entry) => {
                        recorder.permissions.insert(request, entry.seq);
                    }
                    Err(error) => {
                        tracing::error!("cannot write the conversation log: {error:#}");
                    }
                }
                return;
            }
            AgentEvent::PermissionClosed { id: request } => {
                let Some(id) = recorder.permissions.remove(&request) else {
                    return;
                };
                // Already logged as answered when the answer raced the end of the turn.
                if lock(&self.pending).remove(&id).is_none() {
                    return;
                }
                Event::PermissionAnswered { id, allowed: false }
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

/// What recording one connection's events remembers.
struct Recorder {
    client: Weak<AgentClient>,
    tool_titles: HashMap<String, String>,
    /// The log's id of each permission request, by the agent's id.
    permissions: HashMap<u64, u64>,
}

impl Recorder {
    fn new(client: Weak<AgentClient>) -> Self {
        Self {
            client,
            tool_titles: HashMap::new(),
            permissions: HashMap::new(),
        }
    }
}

/// Logs the agent's events for the lifetime of its client and answers flush requests.
async fn record_events(
    jarvis: Weak<Jarvis>,
    mut recorder: Recorder,
    mut events: UnboundedReceiver<AgentEvent>,
    mut flush_requests: UnboundedReceiver<oneshot::Sender<()>>,
) {
    loop {
        tokio::select! {
            // Events first: a flush is acknowledged only once those sent before it are logged.
            biased;
            event = events.recv() => {
                let (Some(event), Some(owner)) = (event, jarvis.upgrade()) else {
                    break;
                };
                owner.record(event, &mut recorder);
            }
            Some(done) = flush_requests.recv() => {
                let _ = done.send(());
            }
        }
    }
}

/// The agent did not start within [`START_TIMEOUT`].
#[derive(Debug)]
struct StartTimeout;

impl std::fmt::Display for StartTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the agent did not start within {START_TIMEOUT:?}")
    }
}

impl std::error::Error for StartTimeout {}

/// What the page says about a failed turn; the details go to the server's log.
fn notice_for(error: &anyhow::Error) -> &'static str {
    if error.is::<StartTimeout>() {
        return NO_START;
    }
    match Failure::of(error) {
        Failure::NotInstalled => NOT_INSTALLED,
        Failure::SignedOut => SIGNED_OUT,
        Failure::Other => FAILED,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
