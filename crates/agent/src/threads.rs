//! Threads: Fronde's conversations, each a row of the `threads` table over a stream of its own,
//! with its own agent session. Thread 1 is the main thread, the chat from before threads, over
//! the stream `conversation`; the others use `thread:<id>`. Thread 1 is the owner's home
//! conversation; anyone else's home is a root thread of theirs ([`Threads::home`]).
//!
//! [`Threads`] is the registry: it makes sure thread 1 exists, opens each thread's [`Assistant`]
//! when it is first needed, and shows every request in the stream of the thread it was asked in.
//!
//! Each thread's agent has its own directories ([`ThreadDirs`]), which its sandbox lets it write,
//! and no other thread's; and each start of an agent process its own hub token ([`Tokens`]),
//! by which the hub knows the thread, revoked when Fronde lets the process go.
//!
//! At most [`Budget::max_running`] agent processes run at once ([`Pool`]), one of them kept for
//! the threads the owner started; an agent idle for [`Budget::idle_stop`] stops, and resumes its
//! session on the next message.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::Duration;

use anyhow::{Context, bail};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use fronde_db::DatabaseConnection;
use fronde_entity::{stream_entries, streams, threads};
use fronde_protocol::{Card, Outcome, RequestId, ThreadSummary};
use fronde_requests::{Ask, Caller, Journal, LateAnswer, Requests, User, Users, Waiter};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect,
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, watch};

pub use fronde_protocol::ThreadId;

use crate::acp::{AgentSpec, Model, OauthToken};
use crate::assistant::{self, Assistant, Connector, Shared, TOPIC};
use crate::legacy;
use crate::log::{Event, TurnOutcome, new_log_id, new_stream};
use crate::tokens::Tokens;

mod children;
pub(crate) mod wakes;

pub(crate) use children::Handoff;
pub use children::{
    ChildEnd, ChildTask, DEFAULT_CHILD_TURN_TIMEOUT, DEFAULT_CHILD_TURNS_PER_DAY,
    DEFAULT_CHILDREN_KEPT, DEFAULT_CHILDREN_PER_DAY, DEFAULT_CHILDREN_RUNNING, DEFAULT_MAX_DEPTH,
    DEFAULT_RESERVE, DEFAULT_WAKES_WITHOUT_OWNER, Delegation, Limits, MAX_LISTED, Pause,
    StartedChild, TurnResult, final_answer, may_access,
};
pub use fronde_protocol::usage::Background;

/// Where the agent's session id was kept before threads: the key-value table's scope and key.
/// Read once, then moved to thread 1's row.
const LEGACY_SESSION: (&str, &str) = ("agent", "session");
/// The main thread's title.
const MAIN_TITLE: &str = "Fronde";
/// Stream entries read at a time when looking for a turn a stop left open.
const SCAN_PAGE: u64 = 64;

/// Who started a thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartedBy {
    /// The user, from the chat.
    User,
    /// Another thread's agent, whose child it is.
    Thread(ThreadId),
    /// An outside MCP client, by its `clientInfo` name.
    Mcp(String),
}

impl StartedBy {
    /// As stored in `threads.started_by`.
    pub fn stored(&self) -> String {
        match self {
            Self::User => "user".into(),
            Self::Thread(id) => format!("thread:{id}"),
            Self::Mcp(name) => format!("mcp:{name}"),
        }
    }

    pub fn parse(stored: &str) -> Option<Self> {
        if stored == "user" {
            return Some(Self::User);
        }
        if let Some(id) = stored.strip_prefix("thread:") {
            return id.parse().ok().map(|id| Self::Thread(ThreadId(id)));
        }
        stored
            .strip_prefix("mcp:")
            .map(|name| Self::Mcp(name.into()))
    }
}

/// A thread as the registry describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadRow {
    pub id: ThreadId,
    /// Its stream: `conversation` for the main thread, `thread:<id>` for the others.
    pub topic: String,
    pub title: String,
    pub parent: Option<ThreadId>,
    /// 0 for a user's thread, its parent's plus one for a child.
    pub depth: u32,
    /// The user it is for.
    pub owner: String,
    pub started_by: StartedBy,
    pub provider: String,
    pub model: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// When it was archived: an archived thread refuses messages.
    pub archived_at: Option<DateTime<Utc>>,
}

impl ThreadRow {
    /// Whether `user` may see the thread, and so stream it, write to it, rename or archive it:
    /// an admin sees every thread, another user only the threads that are for them.
    pub fn visible_to(&self, user: &User) -> bool {
        user.admin || self.owner == user.name
    }

    fn from_model(row: threads::Model) -> anyhow::Result<Self> {
        Ok(Self {
            id: ThreadId(row.id),
            started_by: StartedBy::parse(&row.started_by).with_context(|| {
                format!(
                    "thread {} has an unknown starter {:?}",
                    row.id, row.started_by
                )
            })?,
            depth: u32::try_from(row.depth)
                .with_context(|| format!("thread {} has a negative depth", row.id))?,
            topic: row.topic,
            title: row.title,
            parent: row.parent_id.map(ThreadId),
            owner: row.owner,
            provider: row.provider,
            model: row.model,
            created_at: row.created_at,
            updated_at: row.updated_at,
            archived_at: row.archived_at,
        })
    }
}

/// The stream of thread `id`.
pub fn topic_of(id: ThreadId) -> String {
    if id == ThreadId::MAIN {
        TOPIC.into()
    } else {
        format!("thread:{id}")
    }
}

/// A thread's agent's directories: its working directory and its home (with its `TMPDIR`,
/// `tmp`). The only ones of the data directory its sandbox lets it read and write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadDirs {
    pub work: PathBuf,
    pub home: PathBuf,
}

/// Thread `id`'s directories in `data_dir`: `agent` and `home` for the main thread, as before
/// threads; `threads/<id>/work` and `threads/<id>/home` for the others.
pub fn thread_dirs(data_dir: &Path, id: ThreadId) -> ThreadDirs {
    if id == ThreadId::MAIN {
        return ThreadDirs {
            work: data_dir.join("agent"),
            home: data_dir.join("home"),
        };
    }
    let thread = data_dir.join("threads").join(id.to_string());
    ThreadDirs {
        work: thread.join("work"),
        home: thread.join("home"),
    }
}

/// How many agent processes run at once, how long an idle one is kept, and what threads may do
/// with sub-agents: `[threads]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// Agent processes running at once, the main thread's included; at least 1. Threads the
    /// owner did not start may hold all of them but one.
    pub max_running: usize,
    /// How long an agent is kept once its turn ended, without a new message.
    pub idle_stop: Duration,
    /// What threads may do with sub-agents.
    pub limits: Limits,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_running: 3,
            idle_stop: Duration::from_secs(10 * 60),
            limits: Limits::default(),
        }
    }
}

/// The agent processes that may run at once. Each running agent holds a [`Slot`]; a turn whose
/// thread has no agent running takes one before its agent starts. When none is free, the pool
/// stops the agent idle for the longest time, never one in a turn; when every agent is in a
/// turn, it waits for one to end.
///
/// Threads the owner did not start (a thread's children, an outside client's) also take a
/// permit of a second semaphore, one smaller, first: they never hold the last slot, so that a
/// thread the owner talks to always gets one, by stopping an idle agent or by waiting for
/// another of the owner's.
pub(crate) struct Pool {
    all: Arc<Semaphore>,
    /// For the threads the owner did not start; `None` when only one agent runs at once, which
    /// they then wait for like any other turn.
    others: Option<Arc<Semaphore>>,
    idle_stop: Duration,
    /// The open threads' assistants, among which an idle agent is stopped to make room.
    members: Mutex<Vec<Weak<Assistant>>>,
    /// Woken when a turn ends: its agent, now idle, may be stopped for a waiting turn.
    turn_ended: Notify,
}

/// A running agent's place in the [`Pool`], given back when the agent stops.
pub(crate) struct Slot {
    _all: OwnedSemaphorePermit,
    _other: Option<OwnedSemaphorePermit>,
}

impl Pool {
    fn new(budget: Budget) -> Arc<Self> {
        let max = budget.max_running.max(1);
        Arc::new(Self {
            all: Arc::new(Semaphore::new(max)),
            others: (max > 1).then(|| Arc::new(Semaphore::new(max - 1))),
            idle_stop: budget.idle_stop,
            members: Mutex::new(Vec::new()),
            turn_ended: Notify::new(),
        })
    }

    pub(crate) fn idle_stop(&self) -> Duration {
        self.idle_stop
    }

    fn register(&self, assistant: &Arc<Assistant>) {
        let mut members = lock(&self.members);
        members.retain(|member| member.strong_count() > 0);
        members.push(Arc::downgrade(assistant));
    }

    /// A turn ended: a turn waiting for a slot may now stop that agent.
    pub(crate) fn turn_ended(&self) {
        self.turn_ended.notify_waiters();
    }

    /// A slot for `me`'s agent, once one is free. A turn that has to wait says so once in its
    /// thread's stream. Dropping the future gives up the wait.
    pub(crate) async fn acquire(&self, me: &Assistant) -> anyhow::Result<Slot> {
        let mut waiting = false;
        let other = match &self.others {
            Some(others) if !me.owners() => Some(self.take(others, me, true, &mut waiting).await?),
            _ => None,
        };
        let all = self.take(&self.all, me, false, &mut waiting).await?;
        Ok(Slot {
            _all: all,
            _other: other,
        })
    }

    /// A permit of `semaphore`, stopping an idle agent to make one free; among the threads the
    /// owner did not start only, when `others_only`.
    async fn take(
        &self,
        semaphore: &Arc<Semaphore>,
        me: &Assistant,
        others_only: bool,
        waiting: &mut bool,
    ) -> anyhow::Result<OwnedSemaphorePermit> {
        loop {
            if let Ok(permit) = semaphore.clone().try_acquire_owned() {
                return Ok(permit);
            }
            // Listening before looking for an idle agent: a turn ending meanwhile is not missed.
            let ended = self.turn_ended.notified();
            tokio::pin!(ended);
            ended.as_mut().enable();
            if self.stop_idle(me, others_only).await
                && let Ok(permit) = semaphore.clone().try_acquire_owned()
            {
                return Ok(permit);
            }
            if !*waiting {
                *waiting = true;
                me.waiting_for_a_slot().await;
            }
            tokio::select! {
                permit = semaphore.clone().acquire_owned() => {
                    return permit.context("the agent pool was closed");
                }
                () = &mut ended => {}
            }
        }
    }

    /// Stops the agent idle for the longest time, other than `me`'s and never one in a turn;
    /// among the threads the owner did not start only, when `others_only`. Whether it stopped
    /// one.
    async fn stop_idle(&self, me: &Assistant, others_only: bool) -> bool {
        let mut candidates: Vec<Arc<Assistant>> = lock(&self.members)
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|member| {
                !std::ptr::eq(Arc::as_ptr(member), me)
                    && member.agent_running()
                    && !(others_only && member.owners())
            })
            .collect();
        candidates.sort_by_key(|member| member.last_active());
        for candidate in candidates {
            if candidate.stop_agent().await {
                tracing::info!(
                    "stopped the idle agent of thread {} to make room for thread {}'s",
                    candidate.thread(),
                    me.thread()
                );
                return true;
            }
        }
        false
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Tells the thread lists that a thread changed: a counter that each change moves on.
#[derive(Clone)]
pub(crate) struct Changes(Arc<watch::Sender<u64>>);

impl Changes {
    fn new() -> Self {
        Self(Arc::new(watch::channel(0).0))
    }

    pub(crate) fn notify(&self) {
        self.0.send_modify(|count| *count = count.wrapping_add(1));
    }
}

/// What the thread list shows of a thread whose assistant is not open: read once from its
/// stream, and good until it opens, since only an open thread changes.
#[derive(Clone, Copy, Debug)]
struct Tail {
    outcome: Option<TurnOutcome>,
    at: Option<DateTime<Utc>>,
}

/// Which thread a spec is bound for.
struct Bound<'a> {
    id: ThreadId,
    owner: &'a str,
    provider: &'a str,
    sub_agent: bool,
    /// Its own directories; `None` keeps the base's, the main thread's.
    dirs: Option<ThreadDirs>,
    /// Whether its owner is `[data] user`, whose agents alone get `[agent] read`.
    owners_grants: bool,
}

/// `base` bound for one thread: its directories, its system prompt (a sub-agent's own, else
/// `prompt` for its owner, else the base's) and its owner's account on its provider. Only that
/// account's token signs it in. An owner who is not a user, or has no account there, gets a
/// sign-in that fails before any process starts ([`NoOwner`], [`crate::acp::NoAccount`]):
/// nothing falls back to anyone else's account.
fn bound_spec(
    base: &AgentSpec,
    users: &Users,
    accounts: &Accounts,
    prompt: Option<&SystemPrompt>,
    bound: Bound<'_>,
) -> AgentSpec {
    let Bound {
        id,
        owner,
        provider,
        sub_agent,
        dirs,
        owners_grants,
    } = bound;
    let system_prompt = match (sub_agent, prompt) {
        (true, _) => children::SYSTEM_PROMPT.to_owned(),
        (false, Some(prompt)) => prompt(owner),
        (false, None) => base.system_prompt.clone(),
    };
    // An empty name is no account, as for `Threads::account`.
    let binding = accounts
        .of(owner, provider)
        .filter(|binding| !binding.account.is_empty());
    let (oauth_token, account) = match (users.get(owner), binding) {
        (None, _) => {
            let owner = owner.to_owned();
            (
                Some(crate::acp::oauth_token(move || {
                    let error = NoOwner(owner.clone());
                    async move { Err(error.into()) }
                })),
                None,
            )
        }
        (Some(_), Some(binding)) => (binding.token.clone(), Some(binding.account.clone())),
        (Some(_), None) => {
            let error = crate::acp::NoAccount {
                owner: owner.to_owned(),
                provider: provider.to_owned(),
            };
            (
                Some(crate::acp::oauth_token(move || {
                    let error = error.clone();
                    async move { Err(error.into()) }
                })),
                None,
            )
        }
    };
    let (cwd, home) = match dirs {
        Some(ThreadDirs { work, home }) => (work, home),
        None => (base.cwd.clone(), base.home.clone()),
    };
    AgentSpec {
        thread: id,
        cwd,
        home,
        system_prompt,
        oauth_token,
        account,
        owners_grants,
        ..base.clone()
    }
}

/// A thread's owner is no longer a configured user: its agent does not start, and nothing
/// falls back to another person's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoOwner(pub String);

impl std::fmt::Display for NoOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "this thread belongs to {}, who is no longer a user of this Fronde: it is read-only",
            self.0
        )
    }
}

impl std::error::Error for NoOwner {}

/// Why a page's thread was refused. An unknown thread and one the user may not see are the
/// same refusal, so that a page learns nothing of other users' threads.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No such thread, or not the user's.
    NotFound,
    /// The thread is archived: read-only.
    Archived,
    /// A home conversation (thread 1, the owner's, or another person's own) is never archived.
    Main,
    /// The page's `client_id` names another user's thread.
    Taken,
    /// The thread's owner is no longer a configured user: read-only.
    NoOwner,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotFound => "Conversation introuvable.",
            Self::Archived => "Cette conversation est archivée : elle ne reçoit plus de message.",
            Self::Main => "La conversation principale ne s'archive pas.",
            Self::Taken => "Cet identifiant de conversation est déjà pris.",
            Self::NoOwner => {
                "Cette conversation appartient à une personne qui n'a plus de compte : elle est en lecture seule."
            }
        })
    }
}

impl std::error::Error for Refusal {}

/// Which thread's agent holds a hub token, as the hub's gate needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    pub thread: ThreadId,
    /// The thread's stream.
    pub topic: String,
    /// The seq of the thread's running turn's user message.
    pub turn: Option<u64>,
    /// The thread's owner, from its row: whom its agent acts for.
    pub owner: User,
}

/// The system prompt of a thread a user started, for the user it names: each thread's agent is
/// told whose assistant it is.
pub type SystemPrompt = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// The account a person's agents of one provider sign in with: its name, which its usage is kept
/// under, and its token, read again at each start (`None` only for the legacy form's owner
/// account without a token: the agent starts unsigned).
#[derive(Clone)]
pub struct Binding {
    pub account: String,
    pub token: Option<OauthToken>,
}

impl std::fmt::Debug for Binding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Binding")
            .field("account", &self.account)
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

/// Each person's account per provider: the one lookup from a thread to the token its agent
/// signs in with. Built once from the configuration; a person without an account for a
/// provider has none, and nothing falls back to another's.
#[derive(Clone, Debug, Default)]
pub struct Accounts {
    by_owner: HashMap<(String, String), Binding>,
}

impl Accounts {
    /// `owner`'s account on `provider` is `binding`.
    pub fn insert(&mut self, owner: &str, provider: &str, binding: Binding) {
        self.by_owner
            .insert((owner.to_owned(), provider.to_owned()), binding);
    }

    /// `owner`'s account on `provider`, if they have one.
    pub fn of(&self, owner: &str, provider: &str) -> Option<&Binding> {
        self.by_owner.get(&(owner.to_owned(), provider.to_owned()))
    }

    /// Every account's name.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .by_owner
            .values()
            .map(|binding| binding.account.clone())
            .filter(|account| !account.is_empty())
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

/// Who the threads are for: the configured users, the owner of the main thread among them, the
/// system prompt of a thread a user started, and each person's accounts.
pub struct People {
    pub users: Users,
    /// `[data] user`: the main thread is theirs.
    pub owner: String,
    /// `None` keeps the base spec's prompt for every thread.
    pub prompt: Option<SystemPrompt>,
    pub accounts: Accounts,
}

/// The registry as every thread's assistant reaches it: set once it is made; weak, since it
/// holds the assistants.
pub(crate) type Registry = Arc<OnceLock<Weak<Threads>>>;

/// Every thread, and the [`Assistant`] of each thread opened so far.
pub struct Threads {
    db: DatabaseConnection,
    data_dir: PathBuf,
    /// The main thread's agent spec; another thread's differs by its directories.
    spec: AgentSpec,
    connector: Connector,
    /// The live hub tokens of every thread's agent processes.
    tokens: Tokens,
    /// The agent processes that may run at once.
    pool: Arc<Pool>,
    /// The people threads are for, by name: a thread's cards, hub calls and system prompt are
    /// its row's owner's.
    users: Users,
    /// The system prompt of a thread a user started, for its owner; `None` keeps the main
    /// thread's spec's for every thread.
    prompt: Option<SystemPrompt>,
    /// The owner of each thread opened so far, as [`Threads::user_of`] read it at its opening.
    owners: Mutex<HashMap<ThreadId, User>>,
    /// Each person's account per provider.
    accounts: Accounts,
    /// `[data] user`: the main thread is theirs, their threads have the pool's last slot, and
    /// only their agents get the owner's grants (`[agent] read`).
    owner: String,
    main: Arc<Assistant>,
    /// The threads opened so far, the main one among them. Locked while one is opened, so that
    /// a thread is never opened twice.
    open: tokio::sync::Mutex<HashMap<ThreadId, Arc<Assistant>>>,
    /// The requests the threads ask through; weak, since they hold the registry as their
    /// journal.
    requests: OnceLock<Weak<Requests>>,
    /// Told when a thread is made, renamed or archived, its status changes, or a card opens or
    /// closes in it.
    changes: Changes,
    /// What the thread list shows of the threads not open, read once from their streams.
    tails: Mutex<HashMap<ThreadId, Tail>>,
    /// What the threads' agents may do with sub-agents: see [`children`].
    children: children::Children,
    /// Itself, as the assistants reach it to wake a parent: see [`wakes`].
    registry: Registry,
}

impl Threads {
    /// Opens the threads stored in `db`: first imports the files that held the conversation in
    /// `data_dir` before the database, then makes sure the main thread exists, moving the
    /// agent's session id there from the key-value table. Other threads open when first needed,
    /// or now if a stop left their last turn open, so that it is closed, or left the owner's
    /// late answers waiting, so that they are given. Every thread's agent
    /// is started by `connector`, on its first message: the main thread's from `spec`, another
    /// thread's from `spec` with that thread's [`thread_dirs`].
    pub async fn open(
        db: DatabaseConnection,
        data_dir: &Path,
        spec: AgentSpec,
        connector: Connector,
        owner: User,
    ) -> anyhow::Result<Arc<Self>> {
        Self::open_with(db, data_dir, spec, connector, owner, Budget::default()).await
    }

    /// [`Threads::open`], with at most `budget.max_running` agents running at once and an idle
    /// one stopped after `budget.idle_stop`.
    pub async fn open_with(
        db: DatabaseConnection,
        data_dir: &Path,
        spec: AgentSpec,
        connector: Connector,
        owner: User,
        budget: Budget,
    ) -> anyhow::Result<Arc<Self>> {
        // One person, whose account is the spec's own, if it names one.
        let mut accounts = Accounts::default();
        if let Some(account) = spec.account.clone() {
            accounts.insert(
                &owner.name,
                spec.provider.id,
                Binding {
                    account,
                    token: spec.oauth_token.clone(),
                },
            );
        }
        let people = People {
            owner: owner.name.clone(),
            users: std::iter::once(owner).collect(),
            prompt: None,
            accounts,
        };
        Self::open_for(db, data_dir, spec, connector, people, budget).await
    }

    /// [`Threads::open_with`] for `people`: each thread acts for its row's owner, its root
    /// threads told so by `people.prompt` when given, and its agent signs in with that owner's
    /// account for the thread's provider, looked up at each start: never another's. A thread
    /// whose owner has no account starts no agent; one whose owner is not a user opens
    /// read-only. `spec`'s own token is not used.
    pub async fn open_for(
        db: DatabaseConnection,
        data_dir: &Path,
        spec: AgentSpec,
        connector: Connector,
        people: People,
        budget: Budget,
    ) -> anyhow::Result<Arc<Self>> {
        let People {
            users,
            owner,
            prompt,
            accounts,
        } = people;
        let owner = owner.as_str();
        legacy::import(&db, data_dir, TOPIC, LEGACY_SESSION).await?;
        let cancelled = ensure_main(&db, data_dir, owner, spec.provider.id)
            .await
            .context("cannot make the main thread in the database")?;
        bury_removed(&db, &users)
            .await
            .context("cannot mark the threads of people no longer configured")?;
        // The base every thread's spec is made from holds no token: each is its owner's.
        let spec = AgentSpec {
            thread: ThreadId::MAIN,
            oauth_token: None,
            owners_grants: true,
            account: None,
            ..spec
        };
        // The main thread is the owner's.
        let main_spec = bound_spec(
            &spec,
            &users,
            &accounts,
            prompt.as_ref(),
            Bound {
                id: ThreadId::MAIN,
                owner,
                provider: spec.provider.id,
                sub_agent: false,
                dirs: None,
                owners_grants: true,
            },
        );
        let tokens = Tokens::new();
        let pool = Pool::new(budget);
        let changes = Changes::new();
        let handoff = Arc::new(Handoff::default());
        let registry = Registry::default();
        let shared = Shared {
            db: db.clone(),
            connector: connector.clone(),
            tokens: tokens.clone(),
            pool: pool.clone(),
            changes: changes.clone(),
            handoff: handoff.clone(),
            registry: registry.clone(),
            limits: budget.limits,
        };
        let (main, _) = Assistant::open(
            ThreadId::MAIN,
            TOPIC.into(),
            main_spec,
            shared,
            false,
            true,
            None,
            None,
        )
        .await?;
        pool.register(&main);
        let threads = Arc::new(Self {
            db,
            data_dir: data_dir.to_path_buf(),
            spec,
            connector,
            tokens,
            pool,
            owners: Mutex::new(HashMap::from([(
                ThreadId::MAIN,
                users.get(owner).cloned().unwrap_or_else(|| User {
                    name: owner.to_owned(),
                    admin: false,
                }),
            )])),
            users,
            prompt,
            accounts,
            owner: owner.to_owned(),
            open: tokio::sync::Mutex::new(HashMap::from([(ThreadId::MAIN, main.clone())])),
            main,
            requests: OnceLock::new(),
            changes,
            tails: Mutex::new(HashMap::new()),
            children: children::Children::new(handoff, budget.limits),
            registry: registry.clone(),
        });
        // Set before any thread but the main one opens: a turn's end may wake its parent.
        let _ = registry.set(Arc::downgrade(&threads));
        // The cards `ensure_main` cancelled show their end in their threads.
        for (topic, id) in cancelled {
            if let Err(error) = threads.close(&topic, id, &Outcome::Cancelled, None).await {
                tracing::warn!("cannot show the end of request {id} in {topic}: {error:#}");
            }
        }
        let parents = threads.parents().await?;
        for row in threads.list(false).await? {
            if row.id == ThreadId::MAIN {
                continue;
            }
            // A parent may have wakes waiting, which only its whole stream tells: it opens now,
            // and its wakes run once the requests are set.
            if parents.contains(&row.id) {
                threads.assistant(row.id).await?;
                continue;
            }
            match left_waiting(&threads.db, &row.topic).await {
                Ok(true) => {
                    threads.assistant(row.id).await?;
                }
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    "cannot tell whether thread {} was stopped in the middle of a turn or with \
                     late answers waiting: {error:#}",
                    row.id
                ),
            }
        }
        Ok(threads)
    }

    /// The threads some thread is the parent of.
    async fn parents(&self) -> anyhow::Result<std::collections::HashSet<ThreadId>> {
        let parents: Vec<Option<i64>> = threads::Entity::find()
            .select_only()
            .column(threads::Column::ParentId)
            .filter(threads::Column::ParentId.is_not_null())
            .distinct()
            .into_tuple()
            .all(&self.db)
            .await?;
        Ok(parents.into_iter().flatten().map(ThreadId).collect())
    }

    /// The main thread's assistant.
    pub fn main(&self) -> Arc<Assistant> {
        self.main.clone()
    }

    /// The requests every thread asks through, for the owner: see
    /// [`Assistant::set_requests`]. Set once. The wakes a stop left waiting run from then on.
    pub async fn set_requests(&self, requests: &Arc<Requests>) {
        let open = self.open.lock().await;
        if self.requests.set(Arc::downgrade(requests)).is_err() {
            tracing::warn!("the threads' requests were already set");
            return;
        }
        for (id, assistant) in open.iter() {
            assistant.set_requests(requests, self.owner_of(*id));
        }
    }

    /// The user named `name`; one no longer configured reads as a user who is not an admin, so
    /// that nothing of theirs grants an admin's rights. See [`Threads::configured`].
    pub fn user_of(&self, name: &str) -> User {
        self.users.get(name).cloned().unwrap_or_else(|| User {
            name: name.to_owned(),
            admin: false,
        })
    }

    /// Whether `row`'s owner is a configured user: a thread whose owner is not is read-only,
    /// its agent never starting.
    pub fn configured(&self, row: &ThreadRow) -> bool {
        self.users.get(&row.owner).is_some()
    }

    /// The owner of thread `id`, open: as read when it opened.
    fn owner_of(&self, id: ThreadId) -> User {
        lock(&self.owners)
            .get(&id)
            .cloned()
            .expect("an open thread's owner is kept when it opens")
    }

    /// The assistant of thread `id`, opened now if it was not; `None` when there is no such
    /// thread. A sub-agent's turn that a stop left open is closed as failed when it opens, and
    /// its parent told so.
    pub async fn assistant(&self, id: ThreadId) -> anyhow::Result<Option<Arc<Assistant>>> {
        let (assistant, interrupted) = match self.open_assistant(id).await? {
            Some(opened) => opened,
            None => return Ok(None),
        };
        // Once the thread is in `open`, which the parent's wake goes through.
        if let Some(answer) = interrupted
            && let Err(error) = Box::pin(self.child_ended(id, TurnOutcome::Error, &answer)).await
        {
            tracing::error!(
                "cannot tell the parent of thread {id} that its interrupted turn failed: {error:#}"
            );
        }
        Ok(Some(assistant))
    }

    /// [`Threads::assistant`], with the final answer of a turn a stop left open, when it was
    /// opened now and closed so.
    async fn open_assistant(
        &self,
        id: ThreadId,
    ) -> anyhow::Result<Option<(Arc<Assistant>, Option<String>)>> {
        let mut open = self.open.lock().await;
        if let Some(assistant) = open.get(&id) {
            return Ok(Some((assistant.clone(), None)));
        }
        let Some(row) = self.get(id).await? else {
            return Ok(None);
        };
        // Every thread's agent starts from the one configured provider's spec: a thread stored
        // for another would get its permission rules and sign-in, so it does not open.
        if row.provider != self.spec.provider.id {
            bail!(
                "thread {id} uses the provider {:?}, which is not configured; only {} is",
                row.provider,
                self.spec.provider.id
            );
        }
        let shared = Shared {
            db: self.db.clone(),
            connector: self.connector.clone(),
            tokens: self.tokens.clone(),
            pool: self.pool.clone(),
            changes: self.changes.clone(),
            handoff: self.children.handoff.clone(),
            registry: self.registry.clone(),
            limits: self.limits(),
        };
        // Only a thread's sub-agent wakes its parent.
        let parent = match row.started_by {
            StartedBy::Thread(starter) if row.parent == Some(starter) => Some(starter),
            _ => None,
        };
        let turn_timeout = row.parent.map(|_| self.limits().child_turn_timeout);
        let (assistant, interrupted) = Assistant::open(
            id,
            row.topic.clone(),
            self.spec_for(&row),
            shared,
            row.archived_at.is_some(),
            // Only the owner's own threads have the pool's last slot.
            row.started_by == StartedBy::User && row.owner == self.owner,
            parent,
            turn_timeout,
        )
        .await?;
        self.pool.register(&assistant);
        let owner = self.user_of(&row.owner);
        lock(&self.owners).insert(id, owner.clone());
        if let Some(requests) = self.requests.get().and_then(Weak::upgrade) {
            assistant.set_requests(&requests, owner);
        }
        open.insert(id, assistant.clone());
        Ok(Some((assistant, interrupted.filter(|_| parent.is_some()))))
    }

    /// The subscription account the threads `owner` owns spend on `provider`: the key of their
    /// usage readings, of the reserve kept for them and of the daily counts that stand in for
    /// it, and the account their agents sign in with. The one lookup from a thread to its
    /// account; `None` when `owner` has none on `provider`, and then nothing runs on any.
    pub fn account(&self, owner: &str, provider: &str) -> Option<String> {
        self.accounts
            .of(owner, provider)
            .map(|binding| binding.account.clone())
            // An empty name is no account: never one that every account-less thread shares.
            .filter(|account| !account.is_empty())
    }

    /// Every configured account's name.
    pub fn account_names(&self) -> Vec<String> {
        self.accounts.names()
    }

    /// How thread `row`'s agent starts: the base spec, with the thread's own directories (the
    /// base's for the main thread) and scope name, a sub-agent's own system prompt or its
    /// owner's, and its owner's account for its provider: see [`bound_spec`].
    fn spec_for(&self, row: &ThreadRow) -> AgentSpec {
        let dirs = (row.id != ThreadId::MAIN).then(|| thread_dirs(&self.data_dir, row.id));
        bound_spec(
            &self.spec,
            &self.users,
            &self.accounts,
            self.prompt.as_ref(),
            Bound {
                id: row.id,
                owner: &row.owner,
                provider: &row.provider,
                sub_agent: row.parent.is_some(),
                dirs,
                owners_grants: row.owner == self.owner,
            },
        )
    }

    /// The thread whose agent process holds the hub token `given`, with its stream and running
    /// turn; `None` for any other value, a revoked token's among them. The token is the only
    /// thing that names a thread to the hub.
    pub async fn holder(&self, given: &[u8]) -> Option<Holder> {
        let thread = self.tokens.lookup(given)?;
        // A live token's agent runs, so its thread is open.
        let assistant = self.open.lock().await.get(&thread).cloned()?;
        Some(Holder {
            thread,
            topic: assistant.topic().to_owned(),
            turn: assistant.current_turn().await,
            owner: self.owner_of(thread),
        })
    }

    /// Opens `ask`, a request thread `thread`'s agent makes through the hub during its turn
    /// [`Ask::turn`]: see [`Assistant::ask_during`]. `None`, with nothing asked, when that turn
    /// is over or ending, or the thread is not open (its agent runs no turn).
    pub async fn ask_during(&self, thread: ThreadId, ask: Ask) -> anyhow::Result<Option<Waiter>> {
        let assistant = self.open.lock().await.get(&thread).cloned();
        match assistant {
            Some(assistant) => assistant.ask_during(ask).await,
            None => Ok(None),
        }
    }

    /// The live hub tokens of the threads' agent processes.
    pub fn tokens(&self) -> &Tokens {
        &self.tokens
    }

    /// The assistant of the thread over the stream `topic`.
    async fn by_topic(&self, topic: &str) -> anyhow::Result<Option<Arc<Assistant>>> {
        if topic == TOPIC {
            return Ok(Some(self.main()));
        }
        let row = threads::Entity::find()
            .filter(threads::Column::Topic.eq(topic))
            .one(&self.db)
            .await?;
        match row {
            Some(row) => self.assistant(ThreadId(row.id)).await,
            None => Ok(None),
        }
    }

    /// Makes a thread for `owner`, child of `parent` if any, over a new stream of its own.
    pub async fn create(
        &self,
        owner: &str,
        started_by: StartedBy,
        parent: Option<ThreadId>,
        title: &str,
    ) -> anyhow::Result<ThreadRow> {
        let transaction = fronde_db::begin_write(&self.db).await?;
        let row = self
            .insert(&transaction, owner, started_by, parent, title, None)
            .await?;
        transaction.commit().await?;
        self.changes.notify();
        Ok(row)
    }

    /// Makes a thread for `user` from the chat, titled after its first message `text`: the
    /// thread [`ClientMsg::Create`](fronde_protocol::ClientMsg::Create) `client_id` asks for.
    /// When that `client_id` already made one of the user's threads not archived (a retry
    /// after a lost reply), that thread, and `false`: its first message was sent already.
    pub async fn create_for(
        &self,
        user: &User,
        client_id: &str,
        text: &str,
    ) -> anyhow::Result<(ThreadRow, bool)> {
        let transaction = fronde_db::begin_write(&self.db).await?;
        let earlier = threads::Entity::find()
            .filter(threads::Column::StartedBy.eq(StartedBy::User.stored()))
            .filter(threads::Column::ClientRequestId.eq(client_id))
            .filter(threads::Column::ArchivedAt.is_null())
            .one(&transaction)
            .await?;
        if let Some(earlier) = earlier {
            let earlier = ThreadRow::from_model(earlier)?;
            if earlier.owner != user.name {
                bail!(Refusal::Taken);
            }
            return Ok((earlier, false));
        }
        let title = fronde_protocol::first_title(text);
        let row = self
            .insert(
                &transaction,
                &user.name,
                StartedBy::User,
                None,
                &title,
                Some(client_id),
            )
            .await?;
        transaction.commit().await?;
        self.changes.notify();
        Ok((row, true))
    }

    /// `user`'s home conversation: where their `/` leads, and where a message of theirs that
    /// names no thread goes. The owner's (`[data] user`) is thread 1, the main thread. Anyone
    /// else's is a root thread of theirs, made the first time it is asked for, its id kept in
    /// the key-value table ([`home_key`]); made under the write lock [`Threads::create_for`]
    /// takes, so that two pages asking at once get one. A home given to someone else since
    /// (`fronde threads reassign`) is no longer theirs: they get a new one. A home is never
    /// archived ([`Threads::set_archived`]). Refused for a name that is no configured user.
    pub async fn home(&self, user: &User) -> anyhow::Result<ThreadId> {
        if user.name == self.owner {
            return Ok(ThreadId::MAIN);
        }
        if self.users.get(&user.name).is_none() {
            bail!(
                "{} is not a user of this Fronde: they have no home conversation",
                user.name
            );
        }
        // Once made, a home is only read: no write lock for that.
        if let Some(home) = stored_home(&self.db, &user.name).await? {
            return Ok(home);
        }
        let transaction = fronde_db::begin_write(&self.db).await?;
        // Another page may have made it while this one waited for the lock.
        if let Some(home) = stored_home(&transaction, &user.name).await? {
            return Ok(home);
        }
        let row = self
            .insert(
                &transaction,
                &user.name,
                StartedBy::User,
                None,
                MAIN_TITLE,
                None,
            )
            .await?;
        let (scope, key) = home_key(&user.name);
        fronde_db::kv::set(
            &transaction,
            &scope,
            key,
            row.id.0.to_string().into_bytes(),
            None,
        )
        .await?;
        transaction.commit().await?;
        self.changes.notify();
        Ok(row.id)
    }

    /// Whether thread `id` is someone's home conversation: thread 1, or the home of the person
    /// it belongs to (see [`Threads::home`]).
    pub async fn is_home(&self, id: ThreadId) -> anyhow::Result<bool> {
        if id == ThreadId::MAIN {
            return Ok(true);
        }
        let Some(row) = self.get(id).await? else {
            return Ok(false);
        };
        // The owner's only home is thread 1.
        if row.owner == self.owner {
            return Ok(false);
        }
        Ok(stored_home(&self.db, &row.owner).await? == Some(id))
    }

    /// Inserts a thread's stream and row within `transaction`, which holds the write lock.
    async fn insert(
        &self,
        transaction: &sea_orm::DatabaseTransaction,
        owner: &str,
        started_by: StartedBy,
        parent: Option<ThreadId>,
        title: &str,
        client_request_id: Option<&str>,
    ) -> anyhow::Result<ThreadRow> {
        let depth = match parent {
            Some(parent) => {
                let row = threads::Entity::find_by_id(parent.0)
                    .one(transaction)
                    .await?
                    .with_context(|| format!("no thread {parent}"))?;
                // A sub-agent is its root's owner's, whoever asks.
                if row.owner != owner {
                    bail!(
                        "thread {parent} is {}'s: a thread under it is theirs too, never {owner}'s",
                        row.owner
                    );
                }
                row.depth + 1
            }
            None => 0,
        };
        // The topic names the id, and must exist as a stream before the row. Threads are never
        // deleted and the write lock is held, so the next id is the greatest one plus one.
        let last: Option<i64> = threads::Entity::find()
            .select_only()
            .column_as(threads::Column::Id.max(), "max")
            .into_tuple()
            .one(transaction)
            .await?
            .flatten();
        let id = ThreadId(last.unwrap_or(0) + 1);
        move_aside_leftover(&self.data_dir, id)?;
        let topic = topic_of(id);
        streams::Entity::insert(new_stream(&topic, &new_log_id()))
            .exec(transaction)
            .await?;
        let now = Utc::now();
        let row = threads::ActiveModel {
            id: Set(id.0),
            topic: Set(topic),
            title: Set(title.into()),
            parent_id: Set(parent.map(|parent| parent.0)),
            depth: Set(depth),
            owner: Set(owner.into()),
            started_by: Set(started_by.stored()),
            provider: Set(self.spec.provider.id.into()),
            model: Set(None),
            session_id: Set(None),
            client_request_id: Set(client_request_id.map(str::to_owned)),
            created_at: Set(now),
            updated_at: Set(now),
            archived_at: Set(None),
        };
        let row = threads::Entity::insert(row)
            .exec_with_returning(transaction)
            .await?;
        ThreadRow::from_model(row)
    }

    /// Sets the model thread `id`'s agent runs, `None` for the agent's default. It applies from
    /// the agent's next start, after its session opens or resumes; a model the agent does not
    /// offer then fails that start, saying which it offers.
    pub async fn set_model(&self, id: ThreadId, model: Option<&str>) -> anyhow::Result<()> {
        self.update(
            id,
            threads::Column::Model,
            Expr::value(model.map(str::to_owned)),
        )
        .await
    }

    /// The models the agent of `provider` offered when it last started, for choosing one; empty
    /// when none was seen yet.
    pub async fn models(&self, provider: &str) -> anyhow::Result<Vec<Model>> {
        cached_models(&self.db, provider).await
    }

    /// Thread `id`, if there is one.
    pub async fn get(&self, id: ThreadId) -> anyhow::Result<Option<ThreadRow>> {
        threads::Entity::find_by_id(id.0)
            .one(&self.db)
            .await?
            .map(ThreadRow::from_model)
            .transpose()
    }

    /// Every thread, oldest first; the archived ones only if asked.
    pub async fn list(&self, include_archived: bool) -> anyhow::Result<Vec<ThreadRow>> {
        let mut query = threads::Entity::find().order_by_asc(threads::Column::Id);
        if !include_archived {
            query = query.filter(threads::Column::ArchivedAt.is_null());
        }
        query
            .all(&self.db)
            .await?
            .into_iter()
            .map(ThreadRow::from_model)
            .collect()
    }

    pub async fn rename(&self, id: ThreadId, title: &str) -> anyhow::Result<()> {
        self.update(id, threads::Column::Title, Expr::value(title.to_owned()))
            .await?;
        self.changes.notify();
        Ok(())
    }

    /// Thread `id` if `user` may see it; [`Refusal::NotFound`] alike when there is no such
    /// thread and when it is not theirs.
    pub async fn visible(&self, id: ThreadId, user: &User) -> anyhow::Result<ThreadRow> {
        match self.get(id).await? {
            Some(row) if row.visible_to(user) => Ok(row),
            _ => Err(Refusal::NotFound.into()),
        }
    }

    /// Changes the thread list may show: the counter moves on at each.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changes.0.subscribe()
    }

    /// The threads `user` may see, archived ones included, oldest first, as the thread list
    /// shows them.
    pub async fn summaries(&self, user: &User) -> anyhow::Result<Vec<ThreadSummary>> {
        let rows: Vec<ThreadRow> = self
            .list(true)
            .await?
            .into_iter()
            .filter(|row| row.visible_to(user))
            .collect();
        let pending = match self.requests.get().and_then(Weak::upgrade) {
            Some(requests) => requests.pending_by_topic().await?,
            None => HashMap::new(),
        };
        let open = self.open.lock().await.clone();
        let mut summaries = Vec::with_capacity(rows.len());
        for row in rows {
            summaries.push(self.summary(row, &open, &pending).await?);
        }
        Ok(summaries)
    }

    /// What the thread list shows of `row`, given the threads open and the cards pending by
    /// stream.
    async fn summary(
        &self,
        row: ThreadRow,
        open: &HashMap<ThreadId, Arc<Assistant>>,
        pending: &HashMap<String, u32>,
    ) -> anyhow::Result<ThreadSummary> {
        let (status, at) = match open.get(&row.id) {
            Some(assistant) => (assistant.status(), assistant.last_activity()),
            None => {
                let tail = self.tail(&row).await?;
                (assistant::status_after(tail.outcome), tail.at)
            }
        };
        Ok(ThreadSummary {
            id: row.id,
            parent: row.parent,
            status,
            open_requests: pending.get(&row.topic).copied().unwrap_or(0),
            updated_at: at.map_or(row.updated_at, |at| at.max(row.updated_at)),
            archived: row.archived_at.is_some(),
            title: row.title,
        })
    }

    /// What the thread list shows of `row`, a thread not open: read from its stream the first
    /// time.
    async fn tail(&self, row: &ThreadRow) -> anyhow::Result<Tail> {
        if let Some(tail) = lock(&self.tails).get(&row.id) {
            return Ok(*tail);
        }
        let tail = read_tail(&self.db, &row.topic).await?;
        lock(&self.tails).insert(row.id, tail);
        Ok(tail)
    }

    /// Archives thread `id`, or brings it back; an archived thread refuses messages, and its
    /// agent is let go: its running turn cancelled, its hub token revoked at once, its process
    /// stopped. Every sub-agent under it is retired the same way, its turn, cards and the wakes
    /// waiting for it stopped, but stays, not archived, with what it wrote; nothing under an
    /// archived thread is woken, starts or drives a sub-agent. The secrets given in their cards
    /// "for this conversation" are forgotten. None of the turns stopped so wakes a parent; a
    /// sub-agent archived itself is noted in its parent's stream, which it does not wake. A home
    /// conversation, the main thread or another person's ([`Threads::home`]), is never
    /// archived: [`Refusal::Main`].
    ///
    /// Under the lock sub-agents start and are sent messages under: an archive and a start or
    /// a send never interleave.
    pub async fn set_archived(&self, id: ThreadId, archived: bool) -> anyhow::Result<()> {
        if self.is_home(id).await? {
            bail!(Refusal::Main);
        }
        let starting = self.children.starting.lock().await;
        let was = self
            .get(id)
            .await?
            .is_some_and(|row| row.archived_at.is_some());
        self.archive_locked(id, archived, &starting).await?;
        drop(starting);
        if archived && !was {
            self.tell_parent_archived(id).await;
        }
        Ok(())
    }

    /// [`Threads::set_archived`], `starting` held, its parent not told.
    pub(super) async fn archive_locked(
        &self,
        id: ThreadId,
        archived: bool,
        starting: &tokio::sync::MutexGuard<'_, ()>,
    ) -> anyhow::Result<()> {
        if !archived {
            self.drop_late_before_unarchive(id).await?;
        }
        let at = archived.then(Utc::now);
        self.update(id, threads::Column::ArchivedAt, Expr::value(at))
            .await?;
        let assistant = self.open.lock().await.get(&id).cloned();
        if let Some(assistant) = &assistant {
            assistant.set_archived(archived);
        }
        if archived {
            // Archived first: a result coming up the tree meanwhile wakes none of it.
            self.stop_tree(id, true, starting).await?;
        }
        self.changes.notify();
        Ok(())
    }

    /// Before thread `id` is brought back: when it is archived, not open, and its stream holds
    /// late answers a stop left waiting (Fronde stopped before the archive dropped them), it
    /// opens while still archived, which drops them in its stream ([`Assistant::open`]), so
    /// that no later start gives them.
    async fn drop_late_before_unarchive(&self, id: ThreadId) -> anyhow::Result<()> {
        if self.open.lock().await.contains_key(&id) {
            return Ok(());
        }
        let Some(row) = self.get(id).await? else {
            return Ok(());
        };
        if row.archived_at.is_some() && left_waiting(&self.db, &row.topic).await? {
            // A turn a stop left open is closed now; an archived thread's end tells no parent.
            self.open_assistant(id).await?;
        }
        Ok(())
    }

    /// Notes in thread `id`'s parent, when its agent started it, that the owner archived it:
    /// a `ChildEnded` entry with the outcome `archived`, which wakes nothing.
    async fn tell_parent_archived(&self, id: ThreadId) {
        let row = match self.get(id).await {
            Ok(Some(row)) => row,
            Ok(None) => return,
            Err(error) => {
                tracing::error!("cannot read the archived thread {id}: {error:#}");
                return;
            }
        };
        let (Some(parent), StartedBy::Thread(starter)) = (row.parent, &row.started_by) else {
            return;
        };
        if *starter != parent {
            return;
        }
        let assistant = match self.assistant(parent).await {
            Ok(Some(assistant)) => assistant,
            Ok(None) => return,
            Err(error) => {
                tracing::error!(
                    "cannot open thread {parent} to tell it {id} is archived: {error:#}"
                );
                return;
            }
        };
        // Told only to a thread of the same person, as `child_ended` wakes one.
        if self.owner_of(parent).name != row.owner {
            tracing::warn!(
                "thread {id}, {}'s, is archived: thread {parent} is another person's, not told",
                row.owner
            );
            return;
        }
        let (title, _) = fronde_protocol::text::readable(&row.title, fronde_protocol::MAX_TITLE);
        assistant
            .tell(Event::ChildEnded {
                thread: id,
                title,
                outcome: TurnOutcome::Archived,
                summary: String::new(),
                truncated: false,
            })
            .await;
    }

    /// Whether thread `id`, or a thread above it, is archived; `false` for no such thread.
    pub(crate) async fn chain_archived(&self, id: ThreadId) -> anyhow::Result<bool> {
        let mut next = Some(id);
        for _ in 0..children::MAX_CHAIN {
            let Some(id) = next else {
                return Ok(false);
            };
            let Some(row) = self.get(id).await? else {
                return Ok(false);
            };
            if row.archived_at.is_some() {
                return Ok(true);
            }
            next = row.parent;
        }
        // A chain longer than any depth allowed: a damaged database, read as archived.
        Ok(true)
    }

    /// Sets one column of thread `id`, and its `updated_at`.
    async fn update(
        &self,
        id: ThreadId,
        column: threads::Column,
        value: Expr,
    ) -> anyhow::Result<()> {
        let updated = threads::Entity::update_many()
            .col_expr(column, value)
            .col_expr(threads::Column::UpdatedAt, Expr::value(Utc::now()))
            .filter(threads::Column::Id.eq(id.0))
            .exec(&self.db)
            .await?;
        if updated.rows_affected == 0 {
            bail!("no thread {id}");
        }
        Ok(())
    }
}

/// Moves aside `threads/<id>` when it is already there, before a new thread takes the id: a
/// database restored from a backup, or made anew, counts ids again from where it stands, and
/// the new thread's agent must not find an earlier one's files, its Claude transcripts among
/// them. It becomes `threads/<id>.old-<UTC time>`, which no thread uses.
fn move_aside_leftover(data_dir: &Path, id: ThreadId) -> anyhow::Result<()> {
    let dir = thread_dirs(data_dir, id)
        .work
        .parent()
        .map(Path::to_path_buf)
        .context("a thread's directory has a parent")?;
    if dir.symlink_metadata().is_err() {
        return Ok(());
    }
    let aside = dir.with_file_name(format!(
        "{id}.old-{}",
        Utc::now().format("%Y%m%dT%H%M%S%.6fZ")
    ));
    std::fs::rename(&dir, &aside).with_context(|| {
        format!(
            "cannot move {} aside for the new thread {id}",
            dir.display()
        )
    })?;
    tracing::warn!(
        "{} was left by an earlier thread {id}: moved to {} for the new one",
        dir.display(),
        aside.display()
    );
    Ok(())
}

/// The prefix of an owner who is no longer a user: `:` is never in a user's name, so a person
/// configured later under the same name never gets their threads.
pub const REMOVED: &str = "removed:";

/// Gives every root thread of `from` (a person's name, or `removed:<name>`) but the main one,
/// with every thread under it, to `to`, in one transaction, their cards for `from` with them:
/// the way back from [`REMOVED`], or to hand a person's conversations to another. The caller
/// checks that `to` is a configured user and that Fronde is not running on `data_dir`. Given
/// to anyone but the person `from` named, each thread's agent session is forgotten and its home
/// moved aside, as when the main thread changes owner. The number of threads given.
pub async fn reassign(
    db: &DatabaseConnection,
    data_dir: &Path,
    from: &str,
    to: &str,
) -> anyhow::Result<usize> {
    if from == to {
        bail!("--from and --to both name {to}: nothing to give");
    }
    if to.starts_with(REMOVED) {
        bail!("--to {to}: threads go to a configured user, never to a removed one");
    }
    let transaction = fronde_db::begin_write(db).await?;
    let roots: Vec<i64> = threads::Entity::find()
        .select_only()
        .column(threads::Column::Id)
        .filter(threads::Column::Owner.eq(from))
        .filter(threads::Column::ParentId.is_null())
        .filter(threads::Column::Id.ne(ThreadId::MAIN.0))
        .into_tuple()
        .all(&transaction)
        .await?;
    if roots.is_empty() {
        bail!(
            "{from} has no conversation of their own to give (the main thread always is [data] \
             user's)"
        );
    }
    let mut tree = roots.clone();
    let mut level = roots;
    for _ in 0..children::MAX_CHAIN {
        if level.is_empty() {
            break;
        }
        level = threads::Entity::find()
            .select_only()
            .column(threads::Column::Id)
            .filter(threads::Column::ParentId.is_in(level))
            .into_tuple::<i64>()
            .all(&transaction)
            .await?;
        level.retain(|id| !tree.contains(id));
        tree.extend(&level);
    }
    let given: Vec<(i64, String, String)> = threads::Entity::find()
        .select_only()
        .column(threads::Column::Id)
        .column(threads::Column::Owner)
        .column(threads::Column::Topic)
        .filter(threads::Column::Id.is_in(tree))
        .into_tuple()
        .all(&transaction)
        .await?;
    // A tree is one person's: one holding another's thread (left by an earlier version, or a
    // hand edit) is not given whole, nor in part.
    if let Some((id, owner, _)) = given.iter().find(|(_, owner, _)| owner != from) {
        bail!(
            "thread {id}, under a conversation of {from}, is {owner}'s: nothing was given; give \
             {owner}'s threads first, or archive that thread"
        );
    }
    let now = Utc::now();
    // Each thread given back to the same person (`removed:marie` to `marie`) keeps its session
    // and directories; given to anyone else, it forgets them.
    let (kept, fresh): (Vec<_>, Vec<_>) = given
        .iter()
        .partition(|(_, owner, _)| owner.strip_prefix(REMOVED).unwrap_or(owner) == to);
    for (id, owner, _) in &fresh {
        move_dirs_aside(data_dir, ThreadId(*id), owner, now)?;
    }
    threads::Entity::update_many()
        .col_expr(threads::Column::Owner, Expr::value(to.to_owned()))
        .col_expr(threads::Column::UpdatedAt, Expr::value(now))
        .filter(threads::Column::Id.is_in(kept.iter().map(|(id, ..)| *id)))
        .exec(&transaction)
        .await?;
    threads::Entity::update_many()
        .col_expr(threads::Column::Owner, Expr::value(to.to_owned()))
        .col_expr(
            threads::Column::SessionId,
            Expr::value(Option::<String>::None),
        )
        .col_expr(threads::Column::UpdatedAt, Expr::value(now))
        .filter(threads::Column::Id.is_in(fresh.iter().map(|(id, ..)| *id)))
        .exec(&transaction)
        .await?;
    fronde_entity::requests::Entity::update_many()
        .col_expr(
            fronde_entity::requests::Column::ForUser,
            Expr::value(to.to_owned()),
        )
        .filter(fronde_entity::requests::Column::ForUser.eq(from))
        .filter(
            fronde_entity::requests::Column::Topic
                .is_in(given.iter().map(|(_, _, topic)| topic.clone())),
        )
        .exec(&transaction)
        .await?;
    transaction.commit().await?;
    Ok(given.len())
}

/// How many threads belong to people no longer configured (`removed:<name>`), for `fronde
/// check`.
pub async fn buried_count(db: &DatabaseConnection) -> anyhow::Result<u64> {
    Ok(threads::Entity::find()
        .filter(threads::Column::Owner.starts_with(REMOVED))
        .count(db)
        .await?)
}

/// Gives the threads (and the pending cards) of every owner who is not in `users` to
/// `removed:<name>`, in one transaction: read-only for good, an admin still seeing them, and a
/// new user of that name starting with none of them.
async fn bury_removed(db: &DatabaseConnection, users: &Users) -> anyhow::Result<()> {
    let transaction = fronde_db::begin_write(db).await?;
    let owners: Vec<String> = threads::Entity::find()
        .select_only()
        .column(threads::Column::Owner)
        .distinct()
        .into_tuple()
        .all(&transaction)
        .await?;
    for owner in owners {
        if owner.starts_with(REMOVED) || users.get(&owner).is_some() {
            continue;
        }
        let buried = format!("{REMOVED}{owner}");
        tracing::warn!(
            "{owner} is no longer a user: their threads are now {buried}'s, read-only; an admin \
             still sees them"
        );
        threads::Entity::update_many()
            .col_expr(threads::Column::Owner, Expr::value(buried.clone()))
            .filter(threads::Column::Owner.eq(owner.clone()))
            .exec(&transaction)
            .await?;
        fronde_entity::requests::Entity::update_many()
            .col_expr(
                fronde_entity::requests::Column::ForUser,
                Expr::value(buried),
            )
            .filter(fronde_entity::requests::Column::ForUser.eq(owner))
            .exec(&transaction)
            .await?;
    }
    transaction.commit().await?;
    Ok(())
}

/// Makes the main thread's stream and row when missing, and moves the agent's session id from
/// the key-value table to the row, in one transaction: the row is then the only place it is
/// kept. The main thread is `owner`'s, `[data] user`, and so is every thread under it, at every
/// start: a row of that tree made for anyone else is given to `owner`, its agent session
/// forgotten and its home moved aside ([`move_dirs_aside`]), so that the new owner's agent
/// never resumes the earlier owner's conversation nor reads their agent's files.
async fn ensure_main(
    db: &DatabaseConnection,
    data_dir: &Path,
    owner: &str,
    provider: &str,
) -> anyhow::Result<Vec<(String, RequestId)>> {
    let transaction = fronde_db::begin_write(db).await?;
    if streams::Entity::find_by_id(TOPIC)
        .one(&transaction)
        .await?
        .is_none()
    {
        streams::Entity::insert(new_stream(TOPIC, &new_log_id()))
            .exec(&transaction)
            .await?;
    }
    let main = threads::Entity::find_by_id(ThreadId::MAIN.0)
        .one(&transaction)
        .await?;
    if main.is_none() {
        let now = Utc::now();
        threads::Entity::insert(threads::ActiveModel {
            id: Set(ThreadId::MAIN.0),
            topic: Set(TOPIC.into()),
            title: Set(MAIN_TITLE.into()),
            parent_id: Set(None),
            depth: Set(0),
            owner: Set(owner.into()),
            started_by: Set(StartedBy::User.stored()),
            provider: Set(provider.into()),
            model: Set(None),
            session_id: Set(None),
            client_request_id: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            archived_at: Set(None),
        })
        .exec(&transaction)
        .await?;
    }
    let (scope, key) = LEGACY_SESSION;
    if let Some(session) = fronde_db::kv::get(&transaction, scope, key).await? {
        match String::from_utf8(session) {
            Ok(session) => set_session_id(&transaction, ThreadId::MAIN, &session).await?,
            Err(_) => tracing::warn!("the saved agent session id is not text: dropped"),
        }
        fronde_db::kv::delete(&transaction, scope, key).await?;
    }
    // The main thread and its sub-agents, all the way down: a sub-agent is its root's owner's.
    let mut tree = vec![ThreadId::MAIN.0];
    let mut level = tree.clone();
    for _ in 0..children::MAX_CHAIN {
        if level.is_empty() {
            break;
        }
        level = threads::Entity::find()
            .select_only()
            .column(threads::Column::Id)
            .filter(threads::Column::ParentId.is_in(level))
            .into_tuple::<i64>()
            .all(&transaction)
            .await?;
        level.retain(|id| !tree.contains(id));
        tree.extend(&level);
    }
    let given: Vec<(i64, String, String)> = threads::Entity::find()
        .select_only()
        .column(threads::Column::Id)
        .column(threads::Column::Owner)
        .column(threads::Column::Topic)
        .filter(threads::Column::Id.is_in(tree))
        .filter(threads::Column::Owner.ne(owner))
        .into_tuple()
        .all(&transaction)
        .await?;
    let mut cancelled = Vec::new();
    if !given.is_empty() {
        let now = Utc::now();
        // Moved before the rows change: a home that cannot be moved stops the start, the rows
        // left as they were.
        for (id, earlier, _) in &given {
            move_dirs_aside(data_dir, ThreadId(*id), earlier, now)?;
            tracing::warn!(
                "thread {id} was {earlier}'s: it is now {owner}'s, [data] user's, as the main \
                 thread and everything under it are; its agent starts a new conversation"
            );
        }
        threads::Entity::update_many()
            .col_expr(threads::Column::Owner, Expr::value(owner.to_owned()))
            .col_expr(
                threads::Column::SessionId,
                Expr::value(Option::<String>::None),
            )
            .col_expr(threads::Column::UpdatedAt, Expr::value(now))
            .filter(threads::Column::Id.is_in(given.iter().map(|(id, ..)| *id)))
            .exec(&transaction)
            .await?;
        // Their cards were asked of the earlier owner, by a session now forgotten: cancelled,
        // so that the earlier owner can never answer into the new owner's agent.
        let topics = given.iter().map(|(.., topic)| topic.clone());
        let pending: Vec<(i64, String)> = fronde_entity::requests::Entity::find()
            .select_only()
            .column(fronde_entity::requests::Column::Id)
            .column(fronde_entity::requests::Column::Topic)
            .filter(fronde_entity::requests::Column::Status.eq(fronde_requests::PENDING))
            .filter(fronde_entity::requests::Column::Topic.is_in(topics))
            .into_tuple()
            .all(&transaction)
            .await?;
        if !pending.is_empty() {
            fronde_entity::requests::Entity::update_many()
                .col_expr(
                    fronde_entity::requests::Column::Status,
                    Expr::value(fronde_requests::CANCELLED),
                )
                .col_expr(
                    fronde_entity::requests::Column::ResolvedAt,
                    Expr::value(now),
                )
                .filter(
                    fronde_entity::requests::Column::Id.is_in(pending.iter().map(|(id, _)| *id)),
                )
                .exec(&transaction)
                .await?;
        }
        for (id, topic) in pending {
            cancelled.push((
                topic,
                RequestId::try_from(id).context("negative request id")?,
            ));
        }
    }
    transaction.commit().await?;
    Ok(cancelled)
}

/// Moves thread `id`'s directories, its home (its agent's sign-in state and transcripts) and
/// its working directory (the files its agent made), to `<name>.previous-<earlier>-<UTC time>`
/// beside each, when the thread changes owner from `earlier`: no agent of the new owner reads
/// them. The main thread's are made again, empty, as the server made them; another thread's
/// are made when its agent first starts.
fn move_dirs_aside(
    data_dir: &Path,
    id: ThreadId,
    earlier: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    // A name of a user never holds a path separator; a buried owner's holds `:`.
    let earlier: String = earlier
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let ThreadDirs { work, home } = thread_dirs(data_dir, id);
    for dir in [&home, &work] {
        if dir.symlink_metadata().is_err() {
            continue;
        }
        let name = dir
            .file_name()
            .and_then(|name| name.to_str())
            .context("a thread's directory has a name")?;
        let aside = dir.with_file_name(format!(
            "{name}.previous-{earlier}-{}",
            now.format("%Y%m%dT%H%M%S%.6fZ")
        ));
        std::fs::rename(dir, &aside).with_context(|| {
            format!(
                "cannot move {} aside for thread {id}'s new owner",
                dir.display()
            )
        })?;
        tracing::warn!(
            "thread {id}'s agent directory {} was {earlier}'s: moved to {}",
            dir.display(),
            aside.display()
        );
    }
    if id == ThreadId::MAIN {
        for dir in [&work, &home, &home.join("tmp")] {
            fronde_fs::create_dir_all(dir).with_context(|| {
                format!("cannot create the agent's directory {}", dir.display())
            })?;
        }
    }
    Ok(())
}

/// Where the id of `name`'s home conversation is kept: the key-value table's scope
/// `user/<name>`, key `home`. Never the owner's, whose home is thread 1.
pub fn home_key(name: &str) -> (String, &'static str) {
    (format!("user/{name}"), "home")
}

/// The home conversation stored for `name`, while it is still theirs: a root thread they own,
/// not archived. `None` when none was made, or the one made was given to someone else since.
async fn stored_home<C: ConnectionTrait>(db: &C, name: &str) -> anyhow::Result<Option<ThreadId>> {
    let (scope, key) = home_key(name);
    let Some(value) = fronde_db::kv::get(db, &scope, key).await? else {
        return Ok(None);
    };
    let Some(id) = std::str::from_utf8(&value)
        .ok()
        .and_then(|id| id.parse::<i64>().ok())
    else {
        tracing::warn!("the home conversation stored for {name} is not a thread id: ignored");
        return Ok(None);
    };
    let row = threads::Entity::find_by_id(id).one(db).await?;
    Ok(row
        .filter(|row| row.owner == name && row.parent_id.is_none() && row.archived_at.is_none())
        .map(|row| ThreadId(row.id)))
}

/// Thread `id`'s model, `None` for the agent's default.
pub(crate) async fn model(db: &DatabaseConnection, id: ThreadId) -> anyhow::Result<Option<String>> {
    Ok(threads::Entity::find_by_id(id.0)
        .one(db)
        .await?
        .with_context(|| format!("no thread {id}"))?
        .model)
}

/// Where the models an agent offered are kept, for choosing one before it starts: the key-value
/// table's scope `provider/<id>`, key `models`.
fn models_key(provider: &str) -> (String, &'static str) {
    (format!("provider/{provider}"), "models")
}

/// Remembers the models the agent of `provider` offered at its last start.
pub(crate) async fn cache_models(
    db: &DatabaseConnection,
    provider: &str,
    models: &[Model],
) -> anyhow::Result<()> {
    let (scope, key) = models_key(provider);
    fronde_db::kv::set(db, &scope, key, serde_json::to_vec(models)?, None).await?;
    Ok(())
}

/// The models the agent of `provider` offered at its last start; empty when none was seen.
pub(crate) async fn cached_models(
    db: &DatabaseConnection,
    provider: &str,
) -> anyhow::Result<Vec<Model>> {
    let (scope, key) = models_key(provider);
    match fronde_db::kv::get(db, &scope, key).await? {
        Some(value) => Ok(serde_json::from_slice(&value)?),
        None => Ok(Vec::new()),
    }
}

/// Thread `id`'s agent session id.
pub(crate) async fn session_id(
    db: &DatabaseConnection,
    id: ThreadId,
) -> anyhow::Result<Option<String>> {
    Ok(threads::Entity::find_by_id(id.0)
        .one(db)
        .await?
        .with_context(|| format!("no thread {id}"))?
        .session_id)
}

pub(crate) async fn set_session_id<C: ConnectionTrait>(
    db: &C,
    id: ThreadId,
    session: &str,
) -> anyhow::Result<()> {
    let updated = threads::Entity::update_many()
        .col_expr(threads::Column::SessionId, Expr::value(session.to_owned()))
        .col_expr(threads::Column::UpdatedAt, Expr::value(Utc::now()))
        .filter(threads::Column::Id.eq(id.0))
        .exec(db)
        .await?;
    if updated.rows_affected == 0 {
        bail!("no thread {id}");
    }
    Ok(())
}

/// When the stream `topic`'s newest entry was written, and how its last turn ended, reading from
/// its newest entry back to the last turn's start or end.
async fn read_tail(db: &DatabaseConnection, topic: &str) -> anyhow::Result<Tail> {
    let mut pages = stream_entries::Entity::find()
        .filter(stream_entries::Column::Topic.eq(topic))
        .order_by_desc(stream_entries::Column::Seq)
        .paginate(db, SCAN_PAGE);
    let mut at = None;
    while let Some(rows) = pages.fetch_and_next().await? {
        for row in rows {
            at.get_or_insert(row.at);
            let event: Event = serde_json::from_value(row.event)
                .with_context(|| format!("entry {} of {topic} is not readable", row.seq))?;
            match event {
                Event::TurnEnded { outcome } => {
                    return Ok(Tail {
                        outcome: Some(outcome),
                        at,
                    });
                }
                Event::User { .. } => return Ok(Tail { outcome: None, at }),
                _ => {}
            }
        }
    }
    Ok(Tail { outcome: None, at })
}

/// Whether the stream `topic` ends in a turn a stop left open, or holds late answers of the
/// owner's that no message gave yet, reading from its newest entry back to its last message.
/// A late answer is logged during a turn and given by the next message, logged as soon as
/// that turn ends: one after the last message is waiting, unless an archive dropped it since.
async fn left_waiting(db: &DatabaseConnection, topic: &str) -> anyhow::Result<bool> {
    let mut pages = stream_entries::Entity::find()
        .filter(stream_entries::Column::Topic.eq(topic))
        .order_by_desc(stream_entries::Column::Seq)
        .paginate(db, SCAN_PAGE);
    let mut ended = false;
    let mut dropped = false;
    while let Some(rows) = pages.fetch_and_next().await? {
        for row in rows {
            let event: Event = serde_json::from_value(row.event)
                .with_context(|| format!("entry {} of {topic} is not readable", row.seq))?;
            match event {
                Event::LateAnswer { .. } if !dropped => return Ok(true),
                Event::LateDropped { .. } => dropped = true,
                Event::User { .. } => return Ok(!ended),
                Event::TurnEnded { .. } => ended = true,
                _ => {}
            }
        }
    }
    Ok(false)
}

/// Requests are shown in the stream of the thread they were asked in.
#[async_trait]
impl Journal for Threads {
    async fn announce(
        &self,
        topic: &str,
        id: RequestId,
        card: &Card,
        caller: &Caller,
    ) -> anyhow::Result<u64> {
        let Some(assistant) = self.by_topic(topic).await? else {
            bail!("no stream {topic:?} to show request {id} in");
        };
        let seq = assistant.announce(id, card, caller).await;
        self.changes.notify();
        seq
    }

    async fn close(
        &self,
        topic: &str,
        id: RequestId,
        outcome: &Outcome,
        by: Option<&str>,
    ) -> anyhow::Result<()> {
        let Some(assistant) = self.by_topic(topic).await? else {
            bail!("no stream {topic:?} to close request {id} in");
        };
        let closed = assistant.close(id, outcome, by).await;
        self.changes.notify();
        closed
    }

    /// The answer goes to the agent of the thread that asked, in that thread's stream only.
    async fn answered_late(
        &self,
        thread: ThreadId,
        topic: &str,
        answer: LateAnswer,
    ) -> anyhow::Result<()> {
        let Some(assistant) = self.assistant(thread).await? else {
            bail!(
                "no thread {thread} to give the late answer of request {} to",
                answer.request
            );
        };
        anyhow::ensure!(
            assistant.topic() == topic,
            "request {} was asked by thread {thread} in another stream, {topic:?}",
            answer.request
        );
        assistant.answer_late(answer).await;
        Ok(())
    }
}
