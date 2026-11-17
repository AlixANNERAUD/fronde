//! One thread's conversation: its agent session, its turns recorded in the thread's stream.

use crate::acp::{
    AgentClient, AgentEvent, AgentSpec, ElicitationAsk, Failure, PermissionAsk, Reply, TurnEnd,
    Unanswered,
};
use crate::confine::Confine;
use crate::elicitation;
use crate::log::{ConversationLog, Event, ToolState, TurnOutcome};
use crate::permission;
use crate::threads::{self, Changes, Pool, Slot, ThreadId};
use crate::tokens::{TokenLease, Tokens};
use agent_client_protocol::v1::schema::CreateElicitationResponse;
use anyhow::{Context, anyhow};
use chrono::{DateTime, Utc};
use fronde_db::DatabaseConnection;
use fronde_protocol::{Card, Outcome, RequestId, ThreadStatus};
use fronde_requests::{Ask, Caller, Requests, Resolution, User};
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
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
const ARCHIVED: &str = "Cette conversation est archivée : elle ne reçoit plus de message.";
const WAITING: &str = "En attente : d'autres conversations utilisent l'agent.";
/// The main thread's stream in the database.
pub const TOPIC: &str = "conversation";
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

/// What every thread's assistant gets from the registry.
pub(crate) struct Shared {
    pub(crate) db: DatabaseConnection,
    pub(crate) connector: Connector,
    /// Where each start of an agent gets its own hub token.
    pub(crate) tokens: Tokens,
    /// The agent processes that may run at once.
    pub(crate) pool: Arc<Pool>,
    /// Told when a thread's status changes, for the thread list.
    pub(crate) changes: Changes,
}

pub struct Assistant {
    thread: ThreadId,
    /// The thread's stream.
    topic: String,
    /// How this thread's agent starts: its directories and its scope's name are the thread's.
    spec: AgentSpec,
    connector: Connector,
    /// Where each start of the agent gets its own hub token.
    tokens: Tokens,
    /// Where its agent gets a place among those running at once.
    pool: Arc<Pool>,
    /// The owner started this thread: its agent may take the pool's last slot.
    owners: bool,
    log: ConversationLog,
    db: DatabaseConnection,
    /// The thread is archived: it refuses messages.
    archived: AtomicBool,
    session: Mutex<Option<Session>>,
    /// When a turn last started or ended: the pool stops the agent idle for the longest time
    /// first.
    last_active: Mutex<tokio::time::Instant>,
    /// Stops the agent once it has been idle for the pool's `idle_stop`; cancelled by a turn.
    idle_timer: Mutex<Option<CancellationToken>>,
    /// The running turn; `None` when idle.
    turn: tokio::sync::Mutex<Option<Turn>>,
    /// The requests a turn asks through, the agent's permissions among them, cancelled when it
    /// ends or is stopped, and the user they are for. Weak: they hold the threads, this one's
    /// among them, as their journal.
    requests: OnceLock<(Weak<Requests>, User)>,
    /// How long a permission card waits for an answer.
    permission_timeout: Mutex<Duration>,
    /// How long an elicitation's card waits for an answer.
    elicitation_timeout: Mutex<Duration>,
    /// Where its turns stand, for the thread list.
    activity: Mutex<Activity>,
    /// Told when [`Assistant::status`] changes.
    changes: Changes,
}

/// Where a thread's turns stand, as the thread list shows it.
#[derive(Clone, Copy, Debug, Default)]
struct Activity {
    /// A turn runs: from its message to its end.
    running: bool,
    /// The running turn waits for a place among the agents running at once.
    queued: bool,
    /// How the last turn ended, if one did.
    last_outcome: Option<TurnOutcome>,
}

struct Turn {
    cancel: CancellationToken,
    /// The seq of its user message, if it was logged.
    seq: Option<u64>,
    /// Its requests are being cancelled: it asks nothing more.
    ending: bool,
}

/// A connected agent and the task recording its events.
#[derive(Clone)]
struct Session {
    client: Arc<AgentClient>,
    flushes: UnboundedSender<oneshot::Sender<()>>,
    /// The agent process's hub token: revoked once the last clone of the session is dropped,
    /// with the client that stops the process, or at once by [`Assistant::retire`].
    lease: Arc<TokenLease>,
    /// Its place among the agents running at once, freed with it.
    _slot: Arc<Slot>,
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
    /// Opens the conversation of `thread`, stored in `db` under `topic`; the agent is started on
    /// the first message. [`Threads`](crate::threads::Threads) opens each thread once.
    /// `owners`: the owner started the thread.
    pub(crate) async fn open(
        thread: ThreadId,
        topic: String,
        spec: AgentSpec,
        shared: Shared,
        archived: bool,
        owners: bool,
    ) -> anyhow::Result<Arc<Self>> {
        let Shared {
            db,
            connector,
            tokens,
            pool,
            changes,
        } = shared;
        let log = ConversationLog::open(&db, &topic).await?;
        let assistant = Arc::new(Self {
            thread,
            topic,
            spec,
            connector,
            tokens,
            pool,
            owners,
            log,
            db,
            archived: AtomicBool::new(archived),
            session: Mutex::new(None),
            last_active: Mutex::new(tokio::time::Instant::now()),
            idle_timer: Mutex::new(None),
            turn: tokio::sync::Mutex::new(None),
            requests: OnceLock::new(),
            permission_timeout: Mutex::new(permission::PERMISSION_TIMEOUT),
            elicitation_timeout: Mutex::new(fronde_requests::elicitation::DEFAULT_TIMEOUT),
            activity: Mutex::new(Activity::default()),
            changes,
        });
        // A turn the server stopped in the middle of would otherwise run forever on the pages.
        let last_turn = assistant
            .log
            .since(0)
            .iter()
            .rev()
            .find_map(|entry| turn_open_at(&entry.event));
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
        lock(&assistant.activity).last_outcome = last_outcome(&assistant.log.since(0));
        Ok(assistant)
    }

    pub fn log(&self) -> &ConversationLog {
        &self.log
    }

    pub fn thread(&self) -> ThreadId {
        self.thread
    }

    /// The thread's stream.
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// Where the thread's turns stand: queued or running, else how its last turn ended.
    pub fn status(&self) -> ThreadStatus {
        let activity = *lock(&self.activity);
        if activity.queued {
            return ThreadStatus::Queued;
        }
        if activity.running {
            return ThreadStatus::Running;
        }
        status_after(activity.last_outcome)
    }

    /// When the thread's newest entry was written, if it has one.
    pub fn last_activity(&self) -> Option<DateTime<Utc>> {
        self.log.last_at()
    }

    /// Records a change of the thread's activity, and tells the thread list when its status
    /// changed.
    fn set_activity(&self, change: impl FnOnce(&mut Activity)) {
        let before = self.status();
        change(&mut lock(&self.activity));
        if self.status() != before {
            self.changes.notify();
        }
    }

    pub(crate) fn set_archived(&self, archived: bool) {
        self.archived.store(archived, Ordering::SeqCst);
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

    /// How long an elicitation's card waits for an answer before the agent gets a cancel:
    /// [`fronde_requests::elicitation::DEFAULT_TIMEOUT`] unless changed here, at most
    /// [`fronde_requests::elicitation::MAX_TIMEOUT`].
    pub fn set_elicitation_timeout(&self, timeout: Duration) {
        *lock(&self.elicitation_timeout) = timeout.min(fronde_requests::elicitation::MAX_TIMEOUT);
    }

    /// Logs the message and runs the turn in the background; refused while a turn runs, and
    /// when the thread is archived.
    pub async fn send(self: &Arc<Self>, text: String) {
        if self.archived.load(Ordering::SeqCst) {
            self.append(Event::Notice {
                text: ARCHIVED.into(),
            })
            .await;
            return;
        }
        // The turn is locked while its start or refusal is logged, as it is while its end is
        // logged: a message sent on seeing `TurnEnded` finds the turn over. Locks are always
        // taken turn first, then log.
        let (cancel, seq) = {
            let mut turn = self.turn.lock().await;
            if turn.is_some() {
                self.append(Event::Notice { text: BUSY.into() }).await;
                return;
            }
            let message = Event::User {
                text: text.clone(),
                by: None,
            };
            let seq = match self.log.append(message).await {
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
                ending: false,
            });
            // A turn keeps its agent: the idle stop waits for its end.
            if let Some(timer) = lock(&self.idle_timer).take() {
                timer.cancel();
            }
            self.touch();
            self.set_activity(|activity| activity.running = true);
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
            // Nobody waits for the answers any more, and their cards close before the turn. A
            // request coming in meanwhile (a background task of the agent's) finds the turn
            // over rather than one whose requests were already cancelled.
            if let Some(turn) = assistant.turn.lock().await.as_mut() {
                turn.ending = true;
            }
            assistant.cancel_requests(seq).await;
            let mut turn = assistant.turn.lock().await;
            assistant.append(Event::TurnEnded { outcome }).await;
            *turn = None;
            assistant.set_activity(|activity| {
                *activity = Activity {
                    running: false,
                    queued: false,
                    last_outcome: Some(outcome),
                }
            });
            drop(turn);
            assistant.touch();
            assistant.arm_idle_stop();
            assistant.pool.turn_ended();
        });
    }

    /// The agent was used now.
    fn touch(&self) {
        *lock(&self.last_active) = tokio::time::Instant::now();
    }

    /// When a turn last started or ended.
    pub(crate) fn last_active(&self) -> tokio::time::Instant {
        *lock(&self.last_active)
    }

    /// Whether the owner started this thread: its agent may take the pool's last slot.
    pub(crate) fn owners(&self) -> bool {
        self.owners
    }

    /// Stops the agent once it has been idle for the pool's `idle_stop`, unless a turn starts
    /// first; a running agent only.
    fn arm_idle_stop(self: &Arc<Self>) {
        if !self.agent_running() {
            return;
        }
        let timer = CancellationToken::new();
        if let Some(earlier) = lock(&self.idle_timer).replace(timer.clone()) {
            earlier.cancel();
        }
        let after = self.pool.idle_stop();
        let assistant = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::select! {
                () = timer.cancelled() => return,
                () = tokio::time::sleep(after) => {}
            }
            if let Some(assistant) = assistant.upgrade()
                && assistant.stop_agent().await
            {
                tracing::info!(
                    "the agent of thread {} was idle for {after:?}: stopped, its session resumes \
                     on the next message",
                    assistant.thread
                );
            }
        });
    }

    /// Says, once per turn, that the turn waits for another thread's agent to stop.
    pub(crate) async fn waiting_for_a_slot(&self) {
        self.set_activity(|activity| activity.queued = true);
        self.append(Event::Notice {
            text: WAITING.into(),
        })
        .await;
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
            && let Err(error) = requests.cancel_turn(&self.topic, seq).await
        {
            tracing::error!("cannot cancel the requests of the turn: {error:#}");
        }
    }

    /// The seq of the running turn's user message, `None` when idle or ending: what a request
    /// asked during the turn is tied to.
    pub async fn current_turn(&self) -> Option<u64> {
        self.turn
            .lock()
            .await
            .as_ref()
            .filter(|turn| !turn.ending)
            .and_then(|turn| turn.seq)
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
        // Answered on the wire meanwhile (a cancel): there is nothing left to ask.
        if reply.is_answered() {
            return;
        }
        let Some((_, owner)) = self.requests.get() else {
            tracing::warn!("a permission request came with no requests to show it: refused");
            reply.send(permission::denial(&options));
            return;
        };
        let rules = &self.spec.provider.permissions;
        let Some(card) = permission::card(rules, &tool_call, &options, owner.admin) else {
            tracing::warn!("a permission request offered nothing the card can show: refused");
            reply.send(permission::denial(&options));
            return;
        };
        let offered = permission::offered(&card);
        let timeout = *lock(&self.permission_timeout);
        let denial = permission::denial(&options);
        self.ask_in_turn(
            "permission",
            card,
            timeout,
            reply,
            denial,
            move |resolution| permission::outcome(resolution, &options, &offered),
        )
        .await;
    }

    /// Shows the agent's form as a card of the running turn, and answers the agent once the
    /// card is answered, declined, expires or is cancelled with its turn. A form the card
    /// cannot show is declined at once, the reason in the answer's `_meta`.
    async fn ask_elicitation(&self, ask: ElicitationAsk) {
        let ElicitationAsk { request, reply } = ask;
        if reply.is_answered() {
            return;
        }
        let elicited = match elicitation::card(&request) {
            Ok(elicited) => elicited,
            Err(refused) => {
                tracing::warn!("refused a form from the agent: {refused}");
                reply.send(elicitation::refusal(&refused));
                return;
            }
        };
        let card = elicited.card.clone();
        let timeout = *lock(&self.elicitation_timeout);
        let cancel = CreateElicitationResponse::unanswered();
        self.ask_in_turn(
            "elicitation",
            card,
            timeout,
            reply,
            cancel,
            move |resolution| elicitation::response(resolution, &elicited),
        )
        .await;
    }

    /// Shows `card` as a request of the running turn and answers `reply` with what `answer`
    /// makes of the request's end, or with `refusal` at once when it cannot be shown: nobody
    /// could answer it. A request outside a turn, or whose turn ends while it is shown, is never
    /// left open: only a turn's request is cancelled with its turn.
    async fn ask_in_turn<T: Unanswered + Send + 'static>(
        &self,
        what: &'static str,
        card: Card,
        timeout: Duration,
        reply: Arc<Reply<T>>,
        refusal: T,
        answer: impl FnOnce(&Resolution) -> T + Send + 'static,
    ) {
        let (Some(requests), Some((_, owner))) = (self.requests(), self.requests.get()) else {
            tracing::warn!("a {what} request came with no requests to show it: refused");
            reply.send(refusal);
            return;
        };
        let Some(turn) = self.current_turn().await else {
            tracing::warn!("a {what} request came outside a turn: refused");
            reply.send(refusal);
            return;
        };
        let asked = requests
            .ask(Ask {
                topic: self.topic.clone(),
                turn: Some(turn),
                caller: Caller::Thread(self.thread),
                client_request_id: None,
                card,
                for_user: owner.name.clone(),
                admin_only: false,
                timeout,
                // The agent's call alone waits on it.
                tied: true,
            })
            .await;
        let waiter = match asked {
            Ok(waiter) => waiter,
            Err(error) => {
                tracing::error!("cannot show the {what} request, refusing it: {error:#}");
                reply.send(refusal);
                return;
            }
        };
        let id = waiter.id();
        // The turn may have ended while the card was being shown, after its requests were
        // cancelled: this one must not outlive it.
        if self.current_turn().await != Some(turn) {
            if let Err(error) = requests.cancel(id).await {
                tracing::error!("cannot cancel {what} request {id}: {error:#}");
            }
            reply.send(T::unanswered());
            return;
        }
        // Waiting here would hold up the agent's next events.
        tokio::spawn(async move {
            tokio::select! {
                resolution = waiter.wait() => {
                    let outcome = answer(&resolution);
                    if !reply.send(outcome) && matches!(resolution, Resolution::Answered(_)) {
                        tracing::warn!(
                            "{what} request {id} was answered after the agent got its answer: the agent did not get this one"
                        );
                    }
                }
                // The agent got its answer elsewhere (a cancel on the wire, a withdrawn
                // request): the card closes, so that no later answer is recorded as given.
                () = reply.answered() => match requests.cancel(id).await {
                    Ok(true) => {}
                    Ok(false) => tracing::warn!(
                        "{what} request {id} was closed after the agent got its answer: the agent did not get this one"
                    ),
                    Err(error) => tracing::error!("cannot cancel {what} request {id}: {error:#}"),
                },
            }
        });
    }

    async fn run_turn(
        self: &Arc<Self>,
        text: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<TurnOutcome> {
        let running = self.session_slot().clone();
        let session = match running {
            Some(session) => session,
            None => {
                // A place among the agents running at once first, as long as it takes: the
                // start's timeout begins once the agent may start. Stop gives up the wait.
                let slot = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Ok(TurnOutcome::Cancelled),
                    slot = self.pool.acquire(self) => slot?,
                };
                self.set_activity(|activity| activity.queued = false);
                // Dropping the connect future stops an agent that is still starting.
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Ok(TurnOutcome::Cancelled),
                    connected = tokio::time::timeout(START_TIMEOUT, self.connect(slot)) => {
                        connected.map_err(|_| anyhow!(StartTimeout))??
                    }
                }
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

    /// Starts the agent in `slot` and resumes the saved session, or opens a new one. Each start
    /// of the agent gets a new hub token, which the session holds with the slot.
    async fn connect(self: &Arc<Self>, slot: Slot) -> anyhow::Result<Session> {
        // The main thread's directories are the data directory's `agent` and `home`, which the
        // server makes; another thread's are made when its agent first starts.
        if self.thread != ThreadId::MAIN {
            for dir in [&self.spec.cwd, &self.spec.home, &self.spec.home.join("tmp")] {
                fronde_fs::create_dir_all(dir).with_context(|| {
                    format!("cannot create the agent's directory {}", dir.display())
                })?;
            }
        }
        let saved = self.saved_session_id().await;
        let mut lease = self.tokens.issue(self.thread)?;
        let mut spec = self.spec.clone();
        // Only the spec, dropped once the agent started, holds the token itself.
        spec.hub_token = lease
            .take_token()
            .context("a new token lease holds its token")?;
        let (events, agent_events) = mpsc::unbounded_channel();
        let client = (self.connector)(&spec, saved.clone(), events).await?;
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
        // After each open or resume: a resumed session may come back on the agent's default.
        self.choose_model(&client).await?;
        let client = Arc::new(client);
        let (flushes, flush_requests) = mpsc::unbounded_channel();
        let recorder = record_events(
            Arc::downgrade(self),
            Arc::downgrade(&client),
            Recorder::default(),
            agent_events,
            flush_requests,
        );
        let session = Session {
            client,
            flushes,
            lease: Arc::new(lease),
            _slot: Arc::new(slot),
        };
        // In its slot before its events are read: a connection that ended at once finds it.
        *self.session_slot() = Some(session.clone());
        tokio::spawn(recorder);
        Ok(session)
    }

    /// Stops the thread's agent process when no turn runs, revoking its hub token and freeing
    /// its place in the pool; the next message starts it again, resuming its session. Whether it
    /// stopped one.
    pub async fn stop_agent(&self) -> bool {
        let turn = self.turn.lock().await;
        if turn.is_some() {
            return false;
        }
        if let Some(timer) = lock(&self.idle_timer).take() {
            timer.cancel();
        }
        self.session_slot().take().is_some()
    }

    /// Lets the thread's agent go for good, as archiving does: cancels the running turn, if
    /// any, and revokes the agent's hub token at once; its process stops when the turn has
    /// ended, or now when no turn runs.
    pub(crate) async fn retire(&self) {
        self.cancel().await;
        if let Some(timer) = lock(&self.idle_timer).take() {
            timer.cancel();
        }
        let session = self.session_slot().take();
        if let Some(session) = session {
            session.lease.revoke();
            tracing::info!("the agent of thread {} was let go", self.thread);
        }
    }

    /// Whether the thread's agent process is running, as far as Fronde knows.
    pub fn agent_running(&self) -> bool {
        self.session_slot().is_some()
    }

    /// The agent connection of `client` ended (its process exited): its session goes, and with
    /// it the token the process held, unless a newer session already replaced it.
    fn connection_ended(&self, client: &Weak<AgentClient>) {
        let mut slot = self.session_slot();
        if slot
            .as_ref()
            .is_some_and(|session| std::ptr::eq(Arc::as_ptr(&session.client), client.as_ptr()))
        {
            tracing::info!("the agent of thread {} stopped", self.thread);
            *slot = None;
        }
    }

    /// Applies the thread's model to the session just opened or resumed, and remembers the
    /// models the agent offered. A model the agent does not offer fails the start, saying which
    /// it does.
    async fn choose_model(&self, client: &AgentClient) -> anyhow::Result<()> {
        let provider = self.spec.provider.id;
        let models = client.models();
        if !models.is_empty()
            && let Err(error) = threads::cache_models(&self.db, provider, &models).await
        {
            tracing::warn!("cannot remember the models of {provider}: {error:#}");
        }
        let model = threads::model(&self.db, self.thread)
            .await
            .context("cannot read the thread's model")?;
        if let Some(model) = model {
            client
                .set_model(&model)
                .await
                .with_context(|| format!("cannot run thread {} on {model}", self.thread))?;
        }
        Ok(())
    }

    async fn saved_session_id(&self) -> Option<String> {
        match threads::session_id(&self.db, self.thread).await {
            Ok(id) => id,
            Err(error) => {
                tracing::warn!(
                    "cannot read the agent session id, starting a new session: {error:#}"
                );
                None
            }
        }
    }

    async fn save_session_id(&self, id: &str) -> anyhow::Result<()> {
        threads::set_session_id(&self.db, self.thread, id).await
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
            AgentEvent::Elicitation(ask) => {
                self.ask_elicitation(*ask).await;
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

/// Logs the agent's events for the lifetime of its connection and answers flush requests. When
/// the connection ends, the session of `client` goes.
async fn record_events(
    assistant: Weak<Assistant>,
    client: Weak<AgentClient>,
    mut recorder: Recorder,
    mut events: UnboundedReceiver<AgentEvent>,
    mut flush_requests: UnboundedReceiver<oneshot::Sender<()>>,
) {
    loop {
        tokio::select! {
            // Events first: a flush is acknowledged only once those sent before it are logged.
            biased;
            event = events.recv() => {
                let Some(owner) = assistant.upgrade() else {
                    break;
                };
                let Some(event) = event else {
                    // Every sender is gone: the connection ended.
                    owner.connection_ended(&client);
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

/// How the last turn of `entries` ended, if one did and none started since.
fn last_outcome(entries: &[crate::log::Entry]) -> Option<TurnOutcome> {
    entries.iter().rev().find_map(|entry| match entry.event {
        Event::TurnEnded { outcome } => Some(Some(outcome)),
        Event::User { .. } => Some(None),
        _ => None,
    })?
}

/// A thread's status when no turn runs, from how its last turn ended.
pub(crate) fn status_after(outcome: Option<TurnOutcome>) -> ThreadStatus {
    match outcome {
        None => ThreadStatus::Idle,
        Some(TurnOutcome::Done) => ThreadStatus::Done,
        Some(TurnOutcome::Cancelled) => ThreadStatus::Cancelled,
        Some(TurnOutcome::Refused | TurnOutcome::Limit | TurnOutcome::Error) => {
            ThreadStatus::Failed
        }
    }
}

/// Whether a turn is open after `event`: `Some(true)` once a message starts one,
/// `Some(false)` once it ended, `None` for an event within a turn.
pub(crate) fn turn_open_at(event: &Event) -> Option<bool> {
    match event {
        Event::User { .. } => Some(true),
        Event::TurnEnded { .. } => Some(false),
        _ => None,
    }
}

/// What [`Threads`](crate::threads::Threads) shows of a request asked in this thread.
impl Assistant {
    pub(crate) async fn announce(
        &self,
        id: RequestId,
        card: &Card,
        caller: &Caller,
    ) -> anyhow::Result<u64> {
        let entry = self
            .log
            .append(Event::Request {
                id,
                card: card.clone(),
                from: caller.from().map(str::to_owned),
                server: caller.is_source(),
            })
            .await?;
        Ok(entry.seq)
    }

    pub(crate) async fn close(
        &self,
        id: RequestId,
        outcome: &Outcome,
        by: Option<&str>,
    ) -> anyhow::Result<()> {
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
