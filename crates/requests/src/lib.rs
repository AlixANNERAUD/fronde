//! Requests to the user. Anything that waits on a human is a request: a row in `requests`,
//! announced as a card in its stream by a [`Journal`], answered once.
//!
//! A request is written before it is shown, and closed in the database before its asker wakes.
//! Every change of status is a compare-and-set on `status = 'pending'`, so the first answer
//! wins and a closed request never reopens. Each pending request has a timer that expires it:
//! no answer means no.
//!
//! A secret answered in a card goes to the [`SecretStore`] before the request closes, and only
//! where it was kept comes back to the asker ([`Saved`]): an ephemeral reference for the
//! conversation, or the name it was stored under. The value never reaches the request's row, the
//! journal, the asker or the log.

use anyhow::Context as _;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use fronde_db::DatabaseConnection;
use fronde_db::ephemeral::{EphemeralScope, EphemeralSecrets};
use fronde_db::master_key::{Backend, Backends};
use fronde_entity::requests;
use fronde_protocol::request::{
    self, Card, Invalid, Keep, KeyPlace, Outcome, RequestId, Response, SecretValue,
};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder,
};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;
use tokio::sync::oneshot;
use zeroize::Zeroize as _;

const PENDING: &str = "pending";
const ANSWERED: &str = "answered";
const DECLINED: &str = "declined";
const EXPIRED: &str = "expired";
const CANCELLED: &str = "cancelled";
/// The kind of a secret card's row, whose answer is where the secret was kept.
const SECRET: &str = "secret";
/// How long an asker waits past its request's expiry for the timer to close it, before it
/// gives up on the timer: a database that failed to close the request must not leave the asker
/// waiting forever.
const EXPIRY_GRACE: Duration = Duration::from_secs(2);

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
    /// A retried call with the same id from the same caller joins the same request, as long as
    /// it asks the same thing of the same user in the same place: for the assistant in the same
    /// turn, for anyone else while the first request is pending or within its timeout.
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
    /// A secret card was answered: where the secret is, never what it is.
    Saved(Saved),
    Declined,
    Expired,
    Cancelled,
}

/// Where a secret answered in a card was kept: what its asker gets instead of the value, and
/// what the request's row keeps as its answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kept", rename_all = "snake_case")]
pub enum Saved {
    /// In memory, for the asker in this conversation, until used once or `expires_at`.
    Conversation {
        secret_ref: String,
        expires_at: DateTime<Utc>,
    },
    /// Sealed in the database under `name`, for `origin` only.
    Stored { name: String, origin: String },
}

impl Saved {
    pub fn keep(&self) -> Keep {
        match self {
            Self::Conversation { .. } => Keep::Conversation,
            Self::Stored { .. } => Keep::Stored,
        }
    }
}

/// Where secrets answered in cards go: the ephemeral store for "this conversation", the sealed
/// store (its master key from `backends`) for "keep for this origin".
pub struct SecretStore {
    backends: Backends,
    ephemeral: Arc<EphemeralSecrets>,
}

impl SecretStore {
    pub fn new(backends: Backends, ephemeral: Arc<EphemeralSecrets>) -> Self {
        Self {
            backends,
            ephemeral,
        }
    }

    pub fn ephemeral(&self) -> &Arc<EphemeralSecrets> {
        &self.ephemeral
    }
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

/// A `client_request_id` already names another request that is still pending: a new question
/// needs a new id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reused {
    pub client_request_id: String,
}

impl fmt::Display for Reused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "client_request_id {:?} already names another request that is still pending",
            self.client_request_id
        )
    }
}

impl std::error::Error for Reused {}

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
    /// When to stop waiting for the expiry timer: the request's expiry plus [`EXPIRY_GRACE`].
    deadline: Option<tokio::time::Instant>,
    requests: Weak<Requests>,
}

impl Waiter {
    pub fn id(&self) -> RequestId {
        self.id
    }

    pub async fn wait(self) -> Resolution {
        let Self {
            id,
            mut resolution,
            deadline,
            requests,
        } = self;
        let Some(deadline) = deadline else {
            return resolution.await.unwrap_or(Resolution::Cancelled);
        };
        tokio::select! {
            // The service went away with the request still open: nobody can answer it any more.
            resolved = &mut resolution => return resolved.unwrap_or(Resolution::Cancelled),
            () = tokio::time::sleep_until(deadline) => {}
        }
        // The timer could not close the request: try once more, and expire anyway.
        let closed = match requests.upgrade() {
            Some(requests) => requests.expire(id).await,
            None => Ok(false),
        };
        match closed {
            // Closed now, by this call or by an answer that came just before: its outcome.
            Ok(_) => tokio::time::timeout(EXPIRY_GRACE, resolution)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(Resolution::Expired),
            Err(error) => {
                tracing::error!("cannot expire request {id}, the asker stops waiting: {error:#}");
                Resolution::Expired
            }
        }
    }
}

pub struct Requests {
    db: DatabaseConnection,
    journal: Arc<dyn Journal>,
    /// The askers waiting, by request. Several when a retried call joined the request.
    waiters: Mutex<HashMap<RequestId, Vec<oneshot::Sender<Resolution>>>>,
    secrets: Option<SecretStore>,
    /// Held while a secret is kept and its request closed, so that two pages answering at once
    /// never both keep one.
    saving: tokio::sync::Mutex<()>,
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
        Self::open_with(db, journal, None).await
    }

    /// [`Requests::open`], with `secrets` keeping what secret cards are answered with; without
    /// it, a secret answer is refused.
    pub async fn open_with(
        db: DatabaseConnection,
        journal: Arc<dyn Journal>,
        secrets: Option<SecretStore>,
    ) -> anyhow::Result<Arc<Self>> {
        let requests = Arc::new_cyclic(|this| Self {
            db,
            journal,
            waiters: Mutex::default(),
            secrets,
            saving: tokio::sync::Mutex::default(),
            this: this.clone(),
        });
        requests.expire_due().await?;
        let pending = requests::Entity::find()
            .filter(requests::Column::Status.eq(PENDING))
            .all(&requests.db)
            .await
            .context("cannot read the pending requests")?;
        for row in pending {
            // One row this version cannot show must not keep Fronde from starting.
            if let Err(error) = requests.reopen(&row).await {
                tracing::error!("cannot reopen request {}, cancelling it: {error:#}", row.id);
                if let Ok(id) = row_id(&row)
                    && let Err(error) = requests.cancel(id).await
                {
                    tracing::error!("cannot cancel request {id}: {error:#}");
                }
            }
        }
        Ok(requests)
    }

    /// Where secret answers go, when the service keeps them.
    pub fn secrets(&self) -> Option<&SecretStore> {
        self.secrets.as_ref()
    }

    /// The origin the secret `name` is stored for, if one is.
    pub async fn stored_origin(&self, name: &str) -> anyhow::Result<Option<String>> {
        Ok(
            fronde_db::secrets::list(&self.db, fronde_db::secrets::SOURCES)
                .await?
                .into_iter()
                .find(|listed| listed.name == name)
                .map(|listed| listed.origin),
        )
    }

    /// Shows a pending request again if it never was, and starts its timer.
    async fn reopen(&self, row: &requests::Model) -> anyhow::Result<()> {
        let id = row_id(row)?;
        if row.stream_seq.is_none() {
            self.announce(row).await?;
        }
        self.expire_at(id, row.expires_at);
        Ok(())
    }

    /// Opens a request, shows its card and returns its waiter. A retry (see
    /// [`Ask::client_request_id`]) gets a waiter on the first request instead, already resolved
    /// if that one ended. A [`Reused`] error when the id names another pending request.
    pub async fn ask(&self, ask: Ask) -> anyhow::Result<Waiter> {
        let caller = ask.caller.id();
        if let Some(waiter) = self.retried(&ask).await? {
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
                if let Some(waiter) = self.retried(&ask).await? {
                    return Ok(waiter);
                }
                return Err(anyhow::Error::new(error).context("cannot save the request"));
            }
        };
        let id = row_id(&row)?;
        // Waiting before the card is shown, so that the quickest answer finds the asker.
        let waiter = self.wait_on(id, Some(expires_at));
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
        let card: Card = serde_json::from_value(row.card.clone())
            .context("the request's card is not readable by this version of Fronde")
            .map_err(failed)?;
        request::check(&card, &response).map_err(Refusal::Invalid)?;
        let (status, outcome, resolution) = match response {
            Response::Secret { value, keep } => {
                return self.save_secret(&row, &card, value, keep, by).await;
            }
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
            Resolution::Answered(response) => Some(
                serde_json::to_value(response)
                    .context("cannot encode the answer")
                    .map_err(failed)?,
            ),
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

    /// Keeps the secret answering the request `row` as `keep` says, then closes the request and
    /// wakes its asker with where it was kept. The value is wiped when this returns.
    async fn save_secret(
        &self,
        row: &requests::Model,
        card: &Card,
        value: SecretValue,
        keep: Keep,
        by: &User,
    ) -> Result<(), Refusal> {
        let id = row_id(row).map_err(failed)?;
        let Some(store) = &self.secrets else {
            return Err(failed(anyhow::anyhow!(
                "request {id} got a secret, but this Fronde keeps none"
            )));
        };
        if keep == Keep::Stored && !by.admin {
            return Err(refused("Seul un administrateur peut garder un secret."));
        }
        let value = secret_string(value);
        let _saving = self.saving.lock().await;
        // Another page may have answered while this one waited.
        let current = self.find(id).await.map_err(failed)?;
        if current.is_none_or(|current| current.status != PENDING) {
            return Err(Refusal::Closed);
        }
        let mut new_key = None;
        let saved = match (keep, card) {
            (Keep::Conversation, _) => {
                let scope = EphemeralScope {
                    topic: row.topic.clone(),
                    asker: row.caller.clone(),
                    turn: None,
                };
                let (reference, expires_at) = store
                    .ephemeral
                    .put(scope, value, fronde_db::ephemeral::DEFAULT_TTL, true)
                    .map_err(failed)?;
                Saved::Conversation {
                    secret_ref: reference.as_str().to_owned(),
                    expires_at,
                }
            }
            (
                Keep::Stored,
                Card::Secret {
                    remember: Some(remember),
                    ..
                },
            ) => {
                // Never moved to another origin behind the owner's back: a secret stored for one
                // origin is replaced only for that origin.
                if let Some(other) = self.stored_origin(&remember.name).await.map_err(failed)?
                    && other != remember.origin
                {
                    return Err(refused(format!(
                        "Un secret « {} » est déjà gardé pour {other} : garde celui-ci pour cette \
                         conversation seulement, ou retire l'autre avec « fronde secret remove {} ».",
                        remember.name, remember.name
                    )));
                }
                // Under the backends' key lock: a first key is made and seals in one go.
                let (key, created) = fronde_db::secrets::store(
                    &self.db,
                    &store.backends,
                    fronde_db::secrets::SOURCES,
                    &remember.name,
                    &remember.origin,
                    &value,
                )
                .await
                .map_err(failed)?;
                if created {
                    tracing::warn!(
                        "made the master key in {}: back it up apart from the data directory, \
                         without it the stored secrets are lost",
                        store.backends.place(key.backend())
                    );
                    new_key = Some(match key.backend() {
                        Backend::Keychain => KeyPlace::Keychain,
                        Backend::File => KeyPlace::File,
                        Backend::Systemd => KeyPlace::Systemd,
                    });
                }
                Saved::Stored {
                    name: remember.name.clone(),
                    origin: remember.origin.clone(),
                }
            }
            // `check` refuses it before.
            (Keep::Stored, _) => {
                return Err(refused("Cette demande ne propose pas de garder le secret."));
            }
        };
        let answer = serde_json::to_value(&saved)
            .context("cannot encode where the secret was kept")
            .map_err(failed)?;
        let outcome = Outcome::Saved {
            kept: keep,
            new_key,
        };
        let won = self
            .close(id, ANSWERED, Some(answer), outcome, Some(&by.name))
            .await;
        if !matches!(won, Ok(true)) {
            match &saved {
                Saved::Conversation { secret_ref, .. } => {
                    store.ephemeral.forget(secret_ref);
                }
                Saved::Stored { name, .. } => tracing::warn!(
                    "request {id} closed while the secret {name} was being stored: it stays stored"
                ),
            }
            return Err(won.map_err(failed).err().unwrap_or(Refusal::Closed));
        }
        self.wake(id, Resolution::Saved(saved));
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
        response: Option<serde_json::Value>,
        outcome: Outcome,
        by: Option<&str>,
    ) -> anyhow::Result<bool> {
        let Some(row) = self.find(id).await? else {
            return Ok(false);
        };
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

    /// A waiter on the request a retry names, if `ask` is a retry of it; [`Reused`] if its
    /// `client_request_id` names another request still pending.
    async fn retried(&self, ask: &Ask) -> anyhow::Result<Option<Waiter>> {
        let Some(client_request_id) = ask.client_request_id.as_deref() else {
            return Ok(None);
        };
        let rows = requests::Entity::find()
            .filter(requests::Column::Caller.eq(ask.caller.id()))
            .filter(requests::Column::ClientRequestId.eq(client_request_id))
            .order_by_desc(requests::Column::Id)
            .all(&self.db)
            .await
            .context("cannot look for the request to retry")?;
        let now = Utc::now();
        let Some(row) = rows.iter().find(|row| retries(ask, row, now)) else {
            if rows.iter().any(|row| row.status == PENDING) {
                return Err(Reused {
                    client_request_id: client_request_id.to_owned(),
                }
                .into());
            }
            return Ok(None);
        };
        let id = row_id(row)?;
        let waiter = self.wait_on(id, row.expires_at);
        // Closed before the waiter was registered: it would never be woken.
        if let Some(row) = self.find(id).await?
            && row.status != PENDING
        {
            self.wake(id, resolution_of(&row)?);
        }
        Ok(Some(waiter))
    }

    fn wait_on(&self, id: RequestId, expires_at: Option<chrono::DateTime<Utc>>) -> Waiter {
        let (sender, resolution) = oneshot::channel();
        self.waiters().entry(id).or_default().push(sender);
        let deadline = expires_at.map(|expires_at| {
            let left = (expires_at - Utc::now()).to_std().unwrap_or_default();
            tokio::time::Instant::now() + left + EXPIRY_GRACE
        });
        Waiter {
            id,
            resolution,
            deadline,
            requests: self.this.clone(),
        }
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

/// Whether `ask` retries the request `row`: the same caller and id (already matched), the same
/// card for the same user in the same stream, not cancelled, and for the assistant the same
/// turn, for anyone else while the request is pending or within its timeout. Callers share
/// ids across turns and outside clients share a name, so the id alone proves nothing.
fn retries(ask: &Ask, row: &requests::Model, now: chrono::DateTime<Utc>) -> bool {
    let same_question = row.status != CANCELLED
        && row.topic == ask.topic
        && row.for_user == ask.for_user
        && row.admin_only == ask.admin_only
        && serde_json::from_value::<Card>(row.card.clone()).is_ok_and(|card| card == ask.card);
    let still_current = match ask.caller {
        Caller::Assistant => {
            ask.turn.is_some() && row.turn == ask.turn.and_then(|turn| i64::try_from(turn).ok())
        }
        Caller::Mcp(_) | Caller::Source(_) => {
            row.status == PENDING || row.expires_at.is_some_and(|expires_at| expires_at > now)
        }
    };
    same_question && still_current
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
            if row.kind == SECRET {
                Resolution::Saved(serde_json::from_value(response)?)
            } else {
                Resolution::Answered(serde_json::from_value(response)?)
            }
        }
        DECLINED => Resolution::Declined,
        EXPIRED => Resolution::Expired,
        _ => Resolution::Cancelled,
    })
}

fn refused(message: impl Into<String>) -> Refusal {
    Refusal::Invalid(Invalid {
        key: None,
        message: message.into(),
    })
}

/// The value as a `SecretString`, moved, never copied: an allocation larger than the text would
/// be copied by the conversion, so it is copied here first and the original wiped.
fn secret_string(value: SecretValue) -> SecretString {
    let mut text = value.into_inner();
    if text.capacity() != text.len() {
        let mut exact = String::with_capacity(text.len());
        exact.push_str(&text);
        text.zeroize();
        text = exact;
    }
    SecretString::from(text)
}

fn failed(error: anyhow::Error) -> Refusal {
    tracing::error!("cannot answer a request: {error:#}");
    Refusal::Failed
}

#[cfg(test)]
mod tests {
    use super::Caller;

    #[test]
    fn a_client_named_like_the_assistant_stays_a_client() {
        for name in ["assistant", "Fronde", "source:x", ""] {
            let caller = Caller::Mcp(name.into());
            assert_eq!(Caller::parse(&caller.id()), caller, "{name}");
            assert_eq!(caller.from(), Some(name));
        }
        assert_eq!(Caller::parse("assistant"), Caller::Assistant);
        assert_eq!(Caller::Assistant.from(), None);
    }
}
