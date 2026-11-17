//! Requests to the user. Anything that waits on a human is a request: a row in `requests`,
//! announced as a card in its stream by a [`Journal`], answered once.
//!
//! A request is written before it is shown, and closed in the database before its asker wakes.
//! Every change of status is a compare-and-set on `status = 'pending'`, so the first answer
//! wins and a closed request never reopens. Each pending request has a timer that expires it:
//! no answer means no.

use anyhow::Context as _;
use async_trait::async_trait;
use chrono::Utc;
use fronde_db::DatabaseConnection;
use fronde_entity::requests;
use fronde_protocol::request::{self, Card, Invalid, Outcome, RequestId, Response};
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;
use tokio::sync::oneshot;

const PENDING: &str = "pending";
const ANSWERED: &str = "answered";
const DECLINED: &str = "declined";
const EXPIRED: &str = "expired";
const CANCELLED: &str = "cancelled";

/// Who asked: Fronde's own assistant, an outside MCP client, or a tool source (an upstream
/// server's elicitation).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caller {
    Assistant,
    /// An MCP client other than the assistant, by the name it gave (`clientInfo.name`).
    Mcp(String),
    Source(String),
}

impl Caller {
    /// How the caller is stored: `assistant`, `mcp:<name>` or `source:<name>`.
    pub fn id(&self) -> String {
        match self {
            Self::Assistant => "assistant".into(),
            Self::Mcp(name) => format!("mcp:{name}"),
            Self::Source(name) => format!("source:{name}"),
        }
    }

    fn parse(id: &str) -> Self {
        if let Some(name) = id.strip_prefix("mcp:") {
            Self::Mcp(name.into())
        } else if let Some(name) = id.strip_prefix("source:") {
            Self::Source(name.into())
        } else {
            Self::Assistant
        }
    }

    /// The name a card shows for an asker other than the assistant.
    pub fn from(&self) -> Option<&str> {
        match self {
            Self::Assistant => None,
            Self::Mcp(name) | Self::Source(name) => Some(name),
        }
    }
}

/// Someone answering from a signed-in page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub admin: bool,
}

/// A new request.
#[derive(Clone, Debug)]
pub struct Ask {
    /// The stream that shows the card.
    pub topic: String,
    /// The seq of the user message of the turn that asks, if a turn asks.
    pub turn: Option<u64>,
    pub caller: Caller,
    /// A retried call with the same id from the same caller gets the same request.
    pub client_request_id: Option<String>,
    pub card: Card,
    /// The only user who may answer.
    pub for_user: String,
    /// `for_user` may answer only while an admin: a card that grants something lasting.
    pub admin_only: bool,
    pub timeout: Duration,
}

/// How a request ended, for the one who asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    Answered(Response),
    Declined,
    Expired,
    Cancelled,
}

/// Why an answer was refused; the request did not change. Displayed in French, for the page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    Unknown,
    NotYours,
    Closed,
    Invalid(Invalid),
    /// The database failed; the cause is in the server's log.
    Failed,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown => f.write_str("Cette demande n'existe pas."),
            Self::NotYours => f.write_str("Cette demande ne t'est pas adressée."),
            Self::Closed => f.write_str("Cette demande est déjà close."),
            Self::Invalid(invalid) => invalid.fmt(f),
            Self::Failed => f.write_str("La réponse n'a pas pu être enregistrée ; réessaie."),
        }
    }
}

/// Where requests are shown: a stream of entries, such as the conversation log.
#[async_trait]
pub trait Journal: Send + Sync {
    /// Shows the request `id` in `topic`; returns the seq of its entry.
    async fn announce(
        &self,
        topic: &str,
        id: RequestId,
        card: &Card,
        from: Option<&str>,
    ) -> anyhow::Result<u64>;

    /// Shows in `topic` that the request `id` ended.
    async fn close(
        &self,
        topic: &str,
        id: RequestId,
        outcome: &Outcome,
        by: Option<&str>,
    ) -> anyhow::Result<()>;
}

/// The asker's side of a request: resolves once the request ends.
#[derive(Debug)]
pub struct Waiter {
    id: RequestId,
    resolution: oneshot::Receiver<Resolution>,
}

impl Waiter {
    pub fn id(&self) -> RequestId {
        self.id
    }

    pub async fn wait(self) -> Resolution {
        // The service went away with the request still open: nobody can answer it any more.
        self.resolution.await.unwrap_or(Resolution::Cancelled)
    }
}

pub struct Requests {
    db: DatabaseConnection,
    journal: Arc<dyn Journal>,
    /// The askers waiting, by request. Several when a retried call joined the request.
    waiters: Mutex<HashMap<RequestId, Vec<oneshot::Sender<Resolution>>>>,
    this: Weak<Self>,
}

impl Requests {
    /// Opens the requests stored in `db`, shown by `journal`. Pending requests whose time is up
    /// expire; those never announced (a crash between their row and their entry) are announced;
    /// the others get their timer again.
    pub async fn open(
        db: DatabaseConnection,
        journal: Arc<dyn Journal>,
    ) -> anyhow::Result<Arc<Self>> {
        let requests = Arc::new_cyclic(|this| Self {
            db,
            journal,
            waiters: Mutex::default(),
            this: this.clone(),
        });
        requests.expire_due().await?;
        let pending = requests::Entity::find()
            .filter(requests::Column::Status.eq(PENDING))
            .all(&requests.db)
            .await
            .context("cannot read the pending requests")?;
        for row in pending {
            let id = row_id(&row)?;
            if row.stream_seq.is_none() {
                requests.announce(&row).await?;
            }
            requests.expire_at(id, row.expires_at);
        }
        Ok(requests)
    }

    /// Opens a request, shows its card and returns its waiter. A retry (same caller and
    /// `client_request_id`) gets a waiter on the first request instead, already resolved if
    /// that one ended.
    pub async fn ask(&self, ask: Ask) -> anyhow::Result<Waiter> {
        let caller = ask.caller.id();
        if let Some(waiter) = self
            .retried(&caller, ask.client_request_id.as_deref())
            .await?
        {
            return Ok(waiter);
        }
        let now = Utc::now();
        let expires_at = chrono::Duration::from_std(ask.timeout)
            .ok()
            .and_then(|timeout| now.checked_add_signed(timeout))
            .with_context(|| format!("timeout {:?} is too long", ask.timeout))?;
        let row = requests::ActiveModel {
            topic: Set(ask.topic.clone()),
            turn: Set(ask.turn.map(i64::try_from).transpose()?),
            caller: Set(caller.clone()),
            client_request_id: Set(ask.client_request_id.clone()),
            kind: Set(ask.card.kind().to_owned()),
            card: Set(serde_json::to_value(&ask.card)?),
            for_user: Set(ask.for_user.clone()),
            admin_only: Set(ask.admin_only),
            status: Set(PENDING.to_owned()),
            created_at: Set(now),
            expires_at: Set(Some(expires_at)),
            ..Default::default()
        };
        let row = match row.insert(&self.db).await {
            Ok(row) => row,
            Err(error) => {
                // The same retry, sent twice at once: the other one inserted first.
                if let Some(waiter) = self
                    .retried(&caller, ask.client_request_id.as_deref())
                    .await?
                {
                    return Ok(waiter);
                }
                return Err(anyhow::Error::new(error).context("cannot save the request"));
            }
        };
        let id = row_id(&row)?;
        // Waiting before the card is shown, so that the quickest answer finds the asker.
        let waiter = self.wait_on(id);
        if let Err(error) = self.announce(&row).await {
            // Nobody can answer a card that is not shown.
            if let Err(cancel) = self.cancel(id).await {
                tracing::error!("cannot cancel request {id}: {cancel:#}");
            }
            return Err(error);
        }
        self.expire_at(id, Some(expires_at));
        Ok(waiter)
    }

    /// Answers the request `id` as `by`: checked against the request's user, then its card,
    /// then saved if it is still pending. The asker wakes once the request is closed.
    pub async fn answer(
        &self,
        id: RequestId,
        response: Response,
        by: &User,
    ) -> Result<(), Refusal> {
        let row = self
            .find(id)
            .await
            .map_err(failed)?
            .ok_or(Refusal::Unknown)?;
        if row.for_user != by.name || row.admin_only && !by.admin {
            return Err(Refusal::NotYours);
        }
        if row.status != PENDING {
            return Err(Refusal::Closed);
        }
        let card: Card = serde_json::from_value(row.card)
            .context("the request's card is not readable by this version of Fronde")
            .map_err(failed)?;
        request::check(&card, &response).map_err(Refusal::Invalid)?;
        let (status, outcome, resolution) = match response {
            Response::Decline => (DECLINED, Outcome::Declined, Resolution::Declined),
            response => (
                ANSWERED,
                Outcome::Answered {
                    summary: request::summary(&card, &response),
                },
                Resolution::Answered(response),
            ),
        };
        let stored = match &resolution {
            Resolution::Answered(response) => Some(response),
            _ => None,
        };
        let won = self
            .close(id, status, stored, outcome, Some(&by.name))
            .await
            .map_err(failed)?;
        if !won {
            return Err(Refusal::Closed);
        }
        self.wake(id, resolution);
        Ok(())
    }

    /// Cancels the request `id` if it is pending; true if it was.
    pub async fn cancel(&self, id: RequestId) -> anyhow::Result<bool> {
        self.end(id, CANCELLED, Outcome::Cancelled, Resolution::Cancelled)
            .await
    }

    /// Cancels the pending requests of `turn` in `topic`; returns how many.
    pub async fn cancel_turn(&self, topic: &str, turn: u64) -> anyhow::Result<usize> {
        let rows = requests::Entity::find()
            .filter(requests::Column::Status.eq(PENDING))
            .filter(requests::Column::Topic.eq(topic))
            .filter(requests::Column::Turn.eq(i64::try_from(turn)?))
            .all(&self.db)
            .await?;
        let mut cancelled = 0;
        for row in rows {
            cancelled += usize::from(self.cancel(row_id(&row)?).await?);
        }
        Ok(cancelled)
    }

    /// Expires the pending requests whose time is up; returns how many.
    pub async fn expire_due(&self) -> anyhow::Result<usize> {
        let rows = requests::Entity::find()
            .filter(requests::Column::Status.eq(PENDING))
            .filter(requests::Column::ExpiresAt.lte(Utc::now()))
            .all(&self.db)
            .await
            .context("cannot read the requests due")?;
        let mut expired = 0;
        for row in rows {
            expired += usize::from(self.expire(row_id(&row)?).await?);
        }
        Ok(expired)
    }

    async fn expire(&self, id: RequestId) -> anyhow::Result<bool> {
        self.end(id, EXPIRED, Outcome::Expired, Resolution::Expired)
            .await
    }

    /// Ends a pending request with no answer.
    async fn end(
        &self,
        id: RequestId,
        status: &str,
        outcome: Outcome,
        resolution: Resolution,
    ) -> anyhow::Result<bool> {
        let won = self.close(id, status, None, outcome, None).await?;
        if won {
            self.wake(id, resolution);
        }
        Ok(won)
    }

    /// Moves the request from pending to `status` and shows its end; false if it was not
    /// pending (any more).
    async fn close(
        &self,
        id: RequestId,
        status: &str,
        response: Option<&Response>,
        outcome: Outcome,
        by: Option<&str>,
    ) -> anyhow::Result<bool> {
        let Some(row) = self.find(id).await? else {
            return Ok(false);
        };
        let response = response.map(serde_json::to_value).transpose()?;
        let changed = requests::Entity::update_many()
            .col_expr(requests::Column::Status, Expr::value(status))
            .col_expr(requests::Column::Response, Expr::value(response))
            .col_expr(requests::Column::AnsweredBy, Expr::value(by))
            .col_expr(requests::Column::ResolvedAt, Expr::value(Utc::now()))
            .filter(requests::Column::Id.eq(row.id))
            .filter(requests::Column::Status.eq(PENDING))
            .exec(&self.db)
            .await
            .context("cannot close the request")?;
        if changed.rows_affected == 0 {
            return Ok(false);
        }
        // The row is the truth; a card left open on the pages answers "already closed".
        if let Err(error) = self.journal.close(&row.topic, id, &outcome, by).await {
            tracing::error!("cannot show that request {id} ended: {error:#}");
        }
        Ok(true)
    }

    async fn announce(&self, row: &requests::Model) -> anyhow::Result<()> {
        let id = row_id(row)?;
        let card: Card = serde_json::from_value(row.card.clone())
            .with_context(|| format!("request {id} is not readable by this version of Fronde"))?;
        let caller = Caller::parse(&row.caller);
        let seq = self
            .journal
            .announce(&row.topic, id, &card, caller.from())
            .await
            .with_context(|| format!("cannot show request {id}"))?;
        requests::Entity::update_many()
            .col_expr(
                requests::Column::StreamSeq,
                Expr::value(i64::try_from(seq)?),
            )
            .filter(requests::Column::Id.eq(row.id))
            .exec(&self.db)
            .await
            .context("cannot save where the request is shown")?;
        Ok(())
    }

    /// A waiter on the request a retry names, if there is one.
    async fn retried(
        &self,
        caller: &str,
        client_request_id: Option<&str>,
    ) -> anyhow::Result<Option<Waiter>> {
        let Some(client_request_id) = client_request_id else {
            return Ok(None);
        };
        let row = requests::Entity::find()
            .filter(requests::Column::Caller.eq(caller))
            .filter(requests::Column::ClientRequestId.eq(client_request_id))
            .one(&self.db)
            .await
            .context("cannot look for the request to retry")?;
        let Some(row) = row else {
            return Ok(None);
        };
        let id = row_id(&row)?;
        let waiter = self.wait_on(id);
        // Closed before the waiter was registered: it would never be woken.
        if let Some(row) = self.find(id).await?
            && row.status != PENDING
        {
            self.wake(id, resolution_of(&row)?);
        }
        Ok(Some(waiter))
    }

    fn wait_on(&self, id: RequestId) -> Waiter {
        let (sender, resolution) = oneshot::channel();
        self.waiters().entry(id).or_default().push(sender);
        Waiter { id, resolution }
    }

    fn wake(&self, id: RequestId, resolution: Resolution) {
        for waiter in self.waiters().remove(&id).unwrap_or_default() {
            // An asker that gave up is not an error.
            let _ = waiter.send(resolution.clone());
        }
    }

    /// Expires the request at `expires_at`, unless it ended before.
    fn expire_at(&self, id: RequestId, expires_at: Option<chrono::DateTime<Utc>>) {
        let Some(expires_at) = expires_at else {
            return;
        };
        let after = (expires_at - Utc::now()).to_std().unwrap_or_default();
        let this = self.this.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            let Some(requests) = this.upgrade() else {
                return;
            };
            if let Err(error) = requests.expire(id).await {
                tracing::error!("cannot expire request {id}: {error:#}");
            }
        });
    }

    async fn find(&self, id: RequestId) -> anyhow::Result<Option<requests::Model>> {
        let Ok(key) = i64::try_from(id) else {
            return Ok(None);
        };
        requests::Entity::find_by_id(key)
            .one(&self.db)
            .await
            .context("cannot read the request")
    }

    fn waiters(&self) -> MutexGuard<'_, HashMap<RequestId, Vec<oneshot::Sender<Resolution>>>> {
        self.waiters.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn row_id(row: &requests::Model) -> anyhow::Result<RequestId> {
    RequestId::try_from(row.id).context("negative request id")
}

/// How a closed request ended, read back from its row.
fn resolution_of(row: &requests::Model) -> anyhow::Result<Resolution> {
    Ok(match row.status.as_str() {
        ANSWERED => {
            let response = row
                .response
                .clone()
                .context("an answered request without its answer")?;
            Resolution::Answered(serde_json::from_value(response)?)
        }
        DECLINED => Resolution::Declined,
        EXPIRED => Resolution::Expired,
        _ => Resolution::Cancelled,
    })
}

fn failed(error: anyhow::Error) -> Refusal {
    tracing::error!("cannot answer a request: {error:#}");
    Refusal::Failed
}
