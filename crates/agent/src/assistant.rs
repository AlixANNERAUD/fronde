//! Fronde's conversation: one agent session, its turns recorded in the conversation log.

use crate::acp::{AgentClient, AgentEvent, AgentSpec, Failure, PermissionAsk, TurnEnd};
use crate::confine::Confine;
use crate::legacy;
use crate::log::{ConversationLog, Event, ToolState, TurnOutcome};
use crate::permission;
use anyhow::{anyhow, bail};
use async_trait::async_trait;
use fronde_db::DatabaseConnection;
use fronde_protocol::{Card, Outcome, RequestId};
use fronde_requests::{Ask, Caller, Journal, Requests, User};
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

const BUSY: &str = "Fronde répond encore ; attends la fin ou arrête-la.";
const NEW_CONVERSATION: &str =
    "Nouvelle conversation avec l'agent : l'ancienne n'a pas pu être reprise.";
const NO_START: &str = "L'agent ne répond pas au démarrage.";
const NOT_INSTALLED: &str = "Impossible de démarrer l'agent : lance `mise run agent` ou indique son chemin dans [agent] command.";
const SIGNED_OUT: &str =
    "L'agent n'est pas connecté à Claude : voir le README (claude setup-token).";
const FAILED: &str = "L'agent a rencontré une erreur.";
const INTERRUPTED: &str = "La réponse précédente a été interrompue.";
/// The conversation's stream in the database.
pub const TOPIC: &str = "conversation";
/// The agent's session id in the database's key-value table: scope, key.
pub const SESSION: (&str, &str) = ("agent", "session");
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

/// Starts the agent process with `AgentClient::spawn`, unconfined: for tests.
#[cfg(feature = "testing")]
pub fn process_connector() -> Connector {
    Arc::new(|spec: &AgentSpec, resume, events| {
        let spec = spec.clone();
        Box::pin(async move { AgentClient::spawn(&spec, resume, events).await })
    })
}

/// Starts the agent process through `fronde __confine` with `AgentClient::spawn_confined`.
pub fn confined_connector(confine: Confine) -> Connector {
    Arc::new(move |spec: &AgentSpec, resume, events| {
        let (spec, confine) = (spec.clone(), confine.clone());
        Box::pin(async move { AgentClient::spawn_confined(&spec, &confine, resume, events).await })
    })
}

pub struct Assistant {
    spec: AgentSpec,
    connector: Connector,
    log: ConversationLog,
    db: DatabaseConnection,
    session: Mutex<Option<Session>>,
    /// The running turn; `None` when idle.
    turn: tokio::sync::Mutex<Option<Turn>>,
    /// The requests a turn asks through, the agent's permissions among them, cancelled when it
    /// ends or is stopped, and the user they are for. Weak: they hold the assistant as their
    /// journal.
    requests: OnceLock<(Weak<Requests>, User)>,
    /// How long a permission card waits for an answer.
    permission_timeout: Mutex<Duration>,
}

struct Turn {
    cancel: CancellationToken,
    /// The seq of its user message, if it was logged.
    seq: Option<u64>,
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

impl Assistant {
    /// Opens the conversation stored in `db`, first importing the files that held it in
    /// `data_dir` before the database; the agent is started on the first message.
    pub async fn open(
        spec: AgentSpec,
        data_dir: &Path,
        db: DatabaseConnection,
        connector: Connector,
    ) -> anyhow::Result<Arc<Self>> {
        legacy::import(&db, data_dir, TOPIC, SESSION).await?;
        let log = ConversationLog::open(&db, TOPIC).await?;
        let assistant = Arc::new(Self {
            spec,
            connector,
            log,
            db,
            session: Mutex::new(None),
            turn: tokio::sync::Mutex::new(None),
            requests: OnceLock::new(),
            permission_timeout: Mutex::new(permission::PERMISSION_TIMEOUT),
        });
        // A turn the server stopped in the middle of would otherwise run forever on the pages.
        let last_turn =
            assistant
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
            assistant
                .append(Event::Notice {
                    text: INTERRUPTED.into(),
                })
                .await;
            assistant
                .append(Event::TurnEnded {
                    outcome: TurnOutcome::Error,
                })
                .await;
        }
        Ok(assistant)
    }

    pub fn log(&self) -> &ConversationLog {
        &self.log
    }

    /// The requests the assistant's turns ask through, for `owner`: the agent's permissions
    /// become cards there, and a turn that ends or is stopped cancels those still pending. Set
    /// once; until then, every permission is refused.
    pub fn set_requests(&self, requests: &Arc<Requests>, owner: User) {
        if self
            .requests
            .set((Arc::downgrade(requests), owner))
            .is_err()
        {
            tracing::warn!("the assistant's requests were already set");
        }
    }

    /// How long a permission card waits for an answer before the agent is refused:
    /// [`permission::PERMISSION_TIMEOUT`] unless changed here.
    pub fn set_permission_timeout(&self, timeout: Duration) {
        *lock(&self.permission_timeout) = timeout;
    }

    /// Logs the message and runs the turn in the background; refused while a turn runs.
    pub async fn send(self: &Arc<Self>, text: String) {
        // The turn is locked while its start or refusal is logged, as it is while its end is
        // logged: a message sent on seeing `TurnEnded` finds the turn over. Locks are always
        // taken turn first, then log.
        let (cancel, seq) = {
            let mut turn = self.turn.lock().await;
            if turn.is_some() {
                self.append(Event::Notice { text: BUSY.into() }).await;
                return;
            }
            let seq = match self.log.append(Event::User { text: text.clone() }).await {
                Ok(entry) => Some(entry.seq),
                Err(error) => {
                    tracing::error!("cannot write the conversation log: {error:#}");
                    None
                }
            };
            let cancel = CancellationToken::new();
            *turn = Some(Turn {
                cancel: cancel.clone(),
                seq,
            });
            (cancel, seq)
        };
        let assistant = self.clone();
        tokio::spawn(async move {
            let outcome = match assistant.run_turn(&text, &cancel).await {
                Ok(outcome) => outcome,
                Err(error) => {
                    tracing::warn!("the turn failed: {error:#}");
                    assistant
                        .append(Event::Notice {
                            text: notice_for(&error).into(),
                        })
                        .await;
                    TurnOutcome::Error
                }
            };
            // Nobody waits for the answers any more, and their cards close before the turn.
            assistant.cancel_requests(seq).await;
            let mut turn = assistant.turn.lock().await;
            assistant.append(Event::TurnEnded { outcome }).await;
            *turn = None;
        });
    }

    /// Stops the running turn, if any, and cancels the requests it asked: the agent may wait
    /// on one, and its cancel may never reach the tool call.
    pub async fn cancel(&self) {
        let seq = match self.turn.lock().await.as_ref() {
            Some(turn) => {
                turn.cancel.cancel();
                turn.seq
            }
            None => return,
        };
        self.cancel_requests(seq).await;
    }

    /// Cancels the pending requests of the turn whose user message is `seq`.
    async fn cancel_requests(&self, seq: Option<u64>) {
        let requests = self.requests();
        if let (Some(seq), Some(requests)) = (seq, requests)
            && let Err(error) = requests.cancel_turn(TOPIC, seq).await
        {
            tracing::error!("cannot cancel the requests of the turn: {error:#}");
        }
    }

    /// The seq of the running turn's user message, `None` when idle: what a request asked
    /// during the turn is tied to.
    pub async fn current_turn(&self) -> Option<u64> {
        self.turn.lock().await.as_ref().and_then(|turn| turn.seq)
    }

    fn requests(&self) -> Option<Arc<Requests>> {
        self.requests
            .get()
            .and_then(|(requests, _)| requests.upgrade())
    }

    /// Shows the agent's permission request as a card of the running turn, and answers the
    /// agent once the card is answered, expires (a refusal), or is cancelled with its turn.
    /// Refused at once when it cannot be shown: nobody could answer it.
    async fn ask_permission(&self, ask: PermissionAsk) {
        let PermissionAsk {
            tool_call,
            options,
            reply,
        } = ask;
        let deny = || {
            reply.send(permission::denial(&options));
        };
        let (Some(requests), Some((_, owner))) = (self.requests(), self.requests.get()) else {
            tracing::warn!("a permission request came with no requests to show it: refused");
            return deny();
        };
        // Only a turn's request is cancelled with its turn; there is none to tie it to.
        let Some(turn) = self.current_turn().await else {
            tracing::warn!("a permission request came outside a turn: refused");
            return deny();
        };
        let Some(card) = permission::card(&tool_call, &options, owner.admin) else {
            tracing::warn!("a permission request offered nothing the card can show: refused");
            return deny();
        };
        let timeout = *lock(&self.permission_timeout);
        let asked = requests
            .ask(Ask {
                topic: TOPIC.into(),
                turn: Some(turn),
                caller: Caller::Assistant,
                client_request_id: None,
                card,
                for_user: owner.name.clone(),
                admin_only: false,
                timeout,
            })
            .await;
        let waiter = match asked {
            Ok(waiter) => waiter,
            Err(error) => {
                tracing::error!("cannot show the permission request, refusing it: {error:#}");
                return deny();
            }
        };
        // Waiting here would hold up the agent's next events.
        tokio::spawn(async move {
            let resolution = waiter.wait().await;
            reply.send(permission::outcome(&resolution, &options));
        });
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
        let saved = self.saved_session_id().await;
        let (events, agent_events) = mpsc::unbounded_channel();
        let client = (self.connector)(&self.spec, saved.clone(), events).await?;
        let id = client.session_id();
        if saved.as_deref() != Some(id) {
            if saved.is_some() {
                self.append(Event::Notice {
                    text: NEW_CONVERSATION.into(),
                })
                .await;
            }
            if let Err(error) = self.save_session_id(id).await {
                tracing::warn!("cannot save the agent session id: {error:#}");
            }
        }
        let client = Arc::new(client);
        let (flushes, flush_requests) = mpsc::unbounded_channel();
        tokio::spawn(record_events(
            Arc::downgrade(self),
            Recorder::default(),
            agent_events,
            flush_requests,
        ));
        let session = Session { client, flushes };
        *self.session_slot() = Some(session.clone());
        Ok(session)
    }

    async fn saved_session_id(&self) -> Option<String> {
        let (scope, key) = SESSION;
        match fronde_db::kv::get(&self.db, scope, key).await {
            Ok(id) => id.and_then(|id| String::from_utf8(id).ok()),
            Err(error) => {
                tracing::warn!("cannot read the agent session id, starting a new session: {error}");
                None
            }
        }
    }

    async fn save_session_id(&self, id: &str) -> anyhow::Result<()> {
        let (scope, key) = SESSION;
        fronde_db::kv::set(&self.db, scope, key, id.into(), None).await?;
        Ok(())
    }

    async fn record(&self, event: AgentEvent, recorder: &mut Recorder) {
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
            AgentEvent::Permission(ask) => {
                self.ask_permission(*ask).await;
                return;
            }
        };
        self.append(event).await;
    }

    async fn append(&self, event: Event) {
        if let Err(error) = self.log.append(event).await {
            tracing::error!("cannot write the conversation log: {error:#}");
        }
    }

    fn session_slot(&self) -> MutexGuard<'_, Option<Session>> {
        lock(&self.session)
    }
}

/// What recording one connection's events remembers.
#[derive(Default)]
struct Recorder {
    tool_titles: HashMap<String, String>,
}

/// Logs the agent's events for the lifetime of its client and answers flush requests.
async fn record_events(
    assistant: Weak<Assistant>,
    mut recorder: Recorder,
    mut events: UnboundedReceiver<AgentEvent>,
    mut flush_requests: UnboundedReceiver<oneshot::Sender<()>>,
) {
    loop {
        tokio::select! {
            // Events first: a flush is acknowledged only once those sent before it are logged.
            biased;
            event = events.recv() => {
                let (Some(event), Some(owner)) = (event, assistant.upgrade()) else {
                    break;
                };
                owner.record(event, &mut recorder).await;
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

/// Requests are shown in the conversation, the only stream so far.
#[async_trait]
impl Journal for Assistant {
    async fn announce(
        &self,
        topic: &str,
        id: RequestId,
        card: &Card,
        from: Option<&str>,
    ) -> anyhow::Result<u64> {
        if topic != TOPIC {
            bail!("no stream {topic:?} to show request {id} in");
        }
        let entry = self
            .log
            .append(Event::Request {
                id,
                card: card.clone(),
                from: from.map(str::to_owned),
            })
            .await?;
        Ok(entry.seq)
    }

    async fn close(
        &self,
        topic: &str,
        id: RequestId,
        outcome: &Outcome,
        by: Option<&str>,
    ) -> anyhow::Result<()> {
        if topic != TOPIC {
            bail!("no stream {topic:?} to close request {id} in");
        }
        self.log
            .append(Event::RequestClosed {
                id,
                outcome: outcome.clone(),
                by: by.map(str::to_owned),
            })
            .await?;
        Ok(())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
