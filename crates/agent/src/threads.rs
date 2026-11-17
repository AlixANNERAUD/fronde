//! Threads: Fronde's conversations, each a row of the `threads` table over a stream of its own,
//! with its own agent session. Thread 1 is the main thread, the chat from before threads, over
//! the stream `conversation`; the others use `thread:<id>`.
//!
//! [`Threads`] is the registry: it makes sure thread 1 exists, opens each thread's [`Assistant`]
//! when it is first needed, and shows every request in the stream of the thread it was asked in.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock, Weak};

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

pub use fronde_protocol::ThreadId;

use crate::acp::AgentSpec;
use crate::assistant::{self, Assistant, Connector, TOPIC};
use crate::legacy;
use crate::log::{Event, new_log_id, new_stream};

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

/// Every thread, and the [`Assistant`] of each thread opened so far.
pub struct Threads {
    db: DatabaseConnection,
    spec: AgentSpec,
    connector: Connector,
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
    /// is started from `spec` by `connector`, on its first message.
    pub async fn open(
        db: DatabaseConnection,
        data_dir: &Path,
        spec: AgentSpec,
        connector: Connector,
        owner: User,
    ) -> anyhow::Result<Arc<Self>> {
        legacy::import(&db, data_dir, TOPIC, LEGACY_SESSION).await?;
        ensure_main(&db, &owner.name)
            .await
            .context("cannot make the main thread in the database")?;
        let main = Assistant::open(
            ThreadId::MAIN,
            TOPIC.into(),
            spec.clone(),
            db.clone(),
            connector.clone(),
            false,
        )
        .await?;
        let threads = Arc::new(Self {
            db,
            spec,
            connector,
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
        let assistant = Assistant::open(
            id,
            row.topic,
            self.spec.clone(),
            self.db.clone(),
            self.connector.clone(),
            row.archived_at.is_some(),
        )
        .await?;
        if let Some(requests) = self.requests.get().and_then(Weak::upgrade) {
            assistant.set_requests(&requests, self.owner.clone());
        }
        open.insert(id, assistant.clone());
        Ok(Some(assistant))
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

    /// Archives thread `id`, or brings it back; an archived thread refuses messages. The main
    /// thread is never archived.
    pub async fn set_archived(&self, id: ThreadId, archived: bool) -> anyhow::Result<()> {
        if id == ThreadId::MAIN {
            bail!("the main thread cannot be archived");
        }
        let at = archived.then(Utc::now);
        self.update(id, threads::Column::ArchivedAt, Expr::value(at))
            .await?;
        if let Some(assistant) = self.open.lock().await.get(&id) {
            assistant.set_archived(archived);
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
