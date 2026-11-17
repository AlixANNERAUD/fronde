//! Threads: Fronde's conversations, each a row of the `threads` table over a stream of its own,
//! with its own agent session. Thread 1 is the main thread, the chat from before threads, over
//! the stream `conversation`; the others use `thread:<id>`.
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
use fronde_protocol::{Card, Outcome, RequestId};
use fronde_requests::{Caller, Journal, Requests, User};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect,
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

pub use fronde_protocol::ThreadId;

use crate::acp::AgentSpec;
use crate::assistant::{self, Assistant, Connector, Shared, TOPIC};
use crate::legacy;
use crate::log::{Event, new_log_id, new_stream};
use crate::tokens::Tokens;

/// Where the agent's session id was kept before threads: the key-value table's scope and key.
/// Read once, then moved to thread 1's row.
const LEGACY_SESSION: (&str, &str) = ("agent", "session");
/// The main thread's title.
const MAIN_TITLE: &str = "Fronde";
/// The only provider so far.
const PROVIDER: &str = "claude";
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

/// How many agent processes run at once, and how long an idle one is kept: `[threads]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// Agent processes running at once, the main thread's included; at least 1. Threads the
    /// owner did not start may hold all of them but one.
    pub max_running: usize,
    /// How long an agent is kept once its turn ended, without a new message.
    pub idle_stop: Duration,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_running: 3,
            idle_stop: Duration::from_secs(10 * 60),
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

/// Which thread's agent holds a hub token, as the hub's gate needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    pub thread: ThreadId,
    /// The thread's stream.
    pub topic: String,
    /// The seq of the thread's running turn's user message.
    pub turn: Option<u64>,
}

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
    /// The user the threads are for: the owner, the one user so far.
    owner: User,
    main: Arc<Assistant>,
    /// The threads opened so far, the main one among them. Locked while one is opened, so that
    /// a thread is never opened twice.
    open: tokio::sync::Mutex<HashMap<ThreadId, Arc<Assistant>>>,
    /// The requests the threads ask through; weak, since they hold the registry as their
    /// journal.
    requests: OnceLock<Weak<Requests>>,
}

impl Threads {
    /// Opens the threads stored in `db`: first imports the files that held the conversation in
    /// `data_dir` before the database, then makes sure the main thread exists, moving the
    /// agent's session id there from the key-value table. Other threads open when first needed,
    /// or now if a stop left their last turn open, so that it is closed. Every thread's agent
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
        legacy::import(&db, data_dir, TOPIC, LEGACY_SESSION).await?;
        ensure_main(&db, &owner.name)
            .await
            .context("cannot make the main thread in the database")?;
        let spec = AgentSpec {
            thread: ThreadId::MAIN,
            ..spec
        };
        let tokens = Tokens::new();
        let pool = Pool::new(budget);
        let shared = Shared {
            db: db.clone(),
            connector: connector.clone(),
            tokens: tokens.clone(),
            pool: pool.clone(),
        };
        let main = Assistant::open(
            ThreadId::MAIN,
            TOPIC.into(),
            spec.clone(),
            shared,
            false,
            true,
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
            owner,
            open: tokio::sync::Mutex::new(HashMap::from([(ThreadId::MAIN, main.clone())])),
            main,
            requests: OnceLock::new(),
        });
        for row in threads.list(false).await? {
            if row.id == ThreadId::MAIN {
                continue;
            }
            match turn_left_open(&threads.db, &row.topic).await {
                Ok(true) => {
                    threads.assistant(row.id).await?;
                }
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    "cannot tell whether thread {} was stopped in the middle of a turn: {error:#}",
                    row.id
                ),
            }
        }
        Ok(threads)
    }

    /// The main thread's assistant.
    pub fn main(&self) -> Arc<Assistant> {
        self.main.clone()
    }

    /// The requests every thread asks through, for the owner: see
    /// [`Assistant::set_requests`]. Set once.
    pub async fn set_requests(&self, requests: &Arc<Requests>) {
        let open = self.open.lock().await;
        if self.requests.set(Arc::downgrade(requests)).is_err() {
            tracing::warn!("the threads' requests were already set");
            return;
        }
        for assistant in open.values() {
            assistant.set_requests(requests, self.owner.clone());
        }
    }

    /// The assistant of thread `id`, opened now if it was not; `None` when there is no such
    /// thread.
    pub async fn assistant(&self, id: ThreadId) -> anyhow::Result<Option<Arc<Assistant>>> {
        let mut open = self.open.lock().await;
        if let Some(assistant) = open.get(&id) {
            return Ok(Some(assistant.clone()));
        }
        let Some(row) = self.get(id).await? else {
            return Ok(None);
        };
        let shared = Shared {
            db: self.db.clone(),
            connector: self.connector.clone(),
            tokens: self.tokens.clone(),
            pool: self.pool.clone(),
        };
        let assistant = Assistant::open(
            id,
            row.topic,
            self.spec_for(id),
            shared,
            row.archived_at.is_some(),
            row.started_by == StartedBy::User,
        )
        .await?;
        self.pool.register(&assistant);
        if let Some(requests) = self.requests.get().and_then(Weak::upgrade) {
            assistant.set_requests(&requests, self.owner.clone());
        }
        open.insert(id, assistant.clone());
        Ok(Some(assistant))
    }

    /// How thread `id`'s agent starts: the main thread's spec, with another thread's own
    /// directories and scope name.
    fn spec_for(&self, id: ThreadId) -> AgentSpec {
        if id == ThreadId::MAIN {
            return self.spec.clone();
        }
        let ThreadDirs { work, home } = thread_dirs(&self.data_dir, id);
        AgentSpec {
            thread: id,
            cwd: work,
            home,
            ..self.spec.clone()
        }
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
        })
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
        let depth = match parent {
            Some(parent) => {
                let parent = threads::Entity::find_by_id(parent.0)
                    .one(&transaction)
                    .await?
                    .with_context(|| format!("no thread {parent}"))?;
                parent.depth + 1
            }
            None => 0,
        };
        // The topic names the id, and must exist as a stream before the row. Threads are never
        // deleted and the write lock is held, so the next id is the greatest one plus one.
        let last: Option<i64> = threads::Entity::find()
            .select_only()
            .column_as(threads::Column::Id.max(), "max")
            .into_tuple()
            .one(&transaction)
            .await?
            .flatten();
        let id = ThreadId(last.unwrap_or(0) + 1);
        move_aside_leftover(&self.data_dir, id)?;
        let topic = topic_of(id);
        streams::Entity::insert(new_stream(&topic, &new_log_id()))
            .exec(&transaction)
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
            provider: Set(PROVIDER.into()),
            model: Set(None),
            session_id: Set(None),
            client_request_id: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            archived_at: Set(None),
        };
        let row = threads::Entity::insert(row)
            .exec_with_returning(&transaction)
            .await?;
        transaction.commit().await?;
        ThreadRow::from_model(row)
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
            .await
    }

    /// Archives thread `id`, or brings it back; an archived thread refuses messages, and its
    /// agent is let go: its running turn cancelled, its hub token revoked at once, its process
    /// stopped. The main thread is never archived.
    pub async fn set_archived(&self, id: ThreadId, archived: bool) -> anyhow::Result<()> {
        if id == ThreadId::MAIN {
            bail!("the main thread cannot be archived");
        }
        let at = archived.then(Utc::now);
        self.update(id, threads::Column::ArchivedAt, Expr::value(at))
            .await?;
        let assistant = self.open.lock().await.get(&id).cloned();
        if let Some(assistant) = assistant {
            assistant.set_archived(archived);
            if archived {
                assistant.retire().await;
            }
        }
        Ok(())
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

/// Makes the main thread's stream and row when missing, and moves the agent's session id from
/// the key-value table to the row, in one transaction: the row is then the only place it is
/// kept.
async fn ensure_main(db: &DatabaseConnection, owner: &str) -> anyhow::Result<()> {
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
    if threads::Entity::find_by_id(ThreadId::MAIN.0)
        .one(&transaction)
        .await?
        .is_none()
    {
        let now = Utc::now();
        threads::Entity::insert(threads::ActiveModel {
            id: Set(ThreadId::MAIN.0),
            topic: Set(TOPIC.into()),
            title: Set(MAIN_TITLE.into()),
            parent_id: Set(None),
            depth: Set(0),
            owner: Set(owner.into()),
            started_by: Set(StartedBy::User.stored()),
            provider: Set(PROVIDER.into()),
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
    transaction.commit().await?;
    Ok(())
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

/// Whether the stream `topic` ends in a turn a stop left open, reading from its newest entry
/// back to the last turn's start or end.
async fn turn_left_open(db: &DatabaseConnection, topic: &str) -> anyhow::Result<bool> {
    let mut pages = stream_entries::Entity::find()
        .filter(stream_entries::Column::Topic.eq(topic))
        .order_by_desc(stream_entries::Column::Seq)
        .paginate(db, SCAN_PAGE);
    while let Some(rows) = pages.fetch_and_next().await? {
        for row in rows {
            let event: Event = serde_json::from_value(row.event)
                .with_context(|| format!("entry {} of {topic} is not readable", row.seq))?;
            if let Some(open) = assistant::turn_open_at(&event) {
                return Ok(open);
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
        assistant.announce(id, card, caller).await
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
        assistant.close(id, outcome, by).await
    }
}
