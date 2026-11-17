//! Sub-agents: the threads a thread's agent starts and follows through the hub's `threads_*`
//! tools. A child is a thread like any other, with a parent, a depth one below it, and only its
//! task to start from: a new session, never its parent's.
//!
//! One rule decides what an agent may reach: [`may_access`], the target being the caller or one
//! of its descendants. An unknown thread and one out of reach get the same refusal,
//! [`Delegation::NoSuchThread`], so that an agent learns nothing of the threads it did not
//! start. The caller is always the thread whose token the hub's gate saw, never an argument.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use chrono::Utc;
use fronde_entity::threads;
use fronde_protocol::{ThreadStatus, ThreadSummary};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;

use super::{StartedBy, ThreadId, ThreadRow, Threads, lock};
use crate::assistant::{Assistant, Refused};
use crate::log::{Entry, Event, TurnOutcome};

/// A sub-agent's system prompt: it works alone, from its task, for a parent that reads its last
/// answer.
pub(super) const SYSTEM_PROMPT: &str = "You are a sub-agent of Fronde. Do the task alone; end with a short final answer that stands on its own.";

/// How deep a thread may be and still start sub-agents, by default: the threads the owner
/// starts may delegate, their children may not.
pub const DEFAULT_MAX_DEPTH: u32 = 1;

/// The longest chain of parents walked: more than any depth allowed, and a guard against a
/// cycle a damaged database could hold.
const MAX_CHAIN: usize = 64;

/// The most sub-agents [`Threads::children`] lists, newest first.
pub const MAX_LISTED: usize = 50;

/// What the registry keeps for sub-agents.
pub(super) struct Children {
    /// How deep a thread may be and still start sub-agents.
    max_depth: AtomicU32,
    /// Held while a sub-agent starts: a retry of the same call finds the first one's child with
    /// its task logged.
    starting: tokio::sync::Mutex<()>,
    /// The child each `(parent, client_request_id)` started, and in which of the parent's turns:
    /// a retry joins it only in the same turn. In memory, since no turn outlives Fronde.
    retries: Mutex<HashMap<(ThreadId, String), (ThreadId, u64)>>,
    /// For each child, the seq of the message of its last turn whose result a waiting call
    /// took: that result has reached its parent.
    delivered: Mutex<HashMap<ThreadId, u64>>,
}

impl Default for Children {
    fn default() -> Self {
        Self {
            max_depth: AtomicU32::new(DEFAULT_MAX_DEPTH),
            starting: tokio::sync::Mutex::new(()),
            retries: Mutex::new(HashMap::new()),
            delivered: Mutex::new(HashMap::new()),
        }
    }
}

/// What a thread's agent asks of a new sub-agent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChildTask {
    /// Its first message, and the only context it gets.
    pub task: String,
    /// Its title; by default the task's first words.
    pub title: Option<String>,
    /// Its provider; by default its parent's.
    pub provider: Option<String>,
    /// Its model; by default the provider's.
    pub model: Option<String>,
    /// Names the call, so that a retry in the same turn joins the child it started.
    pub client_request_id: Option<String>,
}

/// A sub-agent started, or joined by a retry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartedChild {
    pub row: ThreadRow,
    /// A retry of the call that started it: nothing new was started.
    pub joined: bool,
    /// The seq of its task's message in its stream: its first turn's.
    pub seq: u64,
}

/// How waiting for a sub-agent's turn ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChildEnd {
    /// The turn ended: how, and its final answer.
    Ended {
        outcome: TurnOutcome,
        answer: String,
    },
    /// A card opened in the sub-agent's thread: the user has to answer it first.
    WaitingForUser,
    /// The wait timed out; the sub-agent goes on.
    TimedOut,
    /// The call waiting was given up; the sub-agent goes on.
    GaveUp,
}

/// Why a thread's agent may not do what it asked with a sub-agent. The texts are for the agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delegation {
    /// No such thread, or not one the caller started: the same refusal.
    NoSuchThread,
    /// The caller is a sub-agent as deep as sub-agents go.
    TooDeep { max_depth: u32 },
    /// The caller named its own thread.
    Itself,
    /// The sub-agent is in a turn: one message at a time.
    Busy,
    /// The sub-agent's thread is archived.
    Archived,
    /// The call's `client_request_id` already started another sub-agent, in another turn or
    /// for another task.
    Reused {
        client_request_id: String,
        child: ThreadId,
    },
    /// A provider other than the configured one.
    Provider { given: String, configured: String },
}

impl std::fmt::Display for Delegation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuchThread => f.write_str(
                "no such thread among the sub-agents you started; threads_list shows them",
            ),
            Self::TooDeep { max_depth } => write!(
                f,
                "you are a sub-agent, and sub-agents go {max_depth} level(s) deep at most: you cannot start or drive sub-agents; do the task yourself"
            ),
            Self::Itself => f.write_str("that is your own thread; name one of your sub-agents"),
            Self::Busy => f.write_str(
                "that sub-agent is still in a turn: one message at a time; read it later with threads_read, or stop it with threads_cancel",
            ),
            Self::Archived => {
                f.write_str("that sub-agent's thread is archived: it receives no message")
            }
            Self::Reused {
                client_request_id,
                child,
            } => write!(
                f,
                "client_request_id {client_request_id:?} already started sub-agent {child}, in another turn or for another task; give a new client_request_id for a new task, or retry with exactly the same arguments in the same turn"
            ),
            Self::Provider { given, configured } => write!(
                f,
                "provider {given:?} cannot run sub-agents here; only {configured} is configured"
            ),
        }
    }
}

impl std::error::Error for Delegation {}

/// Whether the thread `caller` may reach `target`: `target` is `caller` or one of its
/// descendants, found by walking `target`'s parents with `parent_of`. A chain longer than any
/// depth allowed (a cycle) reaches nothing.
pub fn may_access(
    caller: ThreadId,
    target: ThreadId,
    parent_of: impl Fn(ThreadId) -> Option<ThreadId>,
) -> bool {
    let mut current = target;
    for _ in 0..MAX_CHAIN {
        if current == caller {
            return true;
        }
        match parent_of(current) {
            Some(parent) => current = parent,
            None => return false,
        }
    }
    false
}

/// The text of the last turn in `entries` after its last message: what a sub-agent answered.
pub fn final_answer(entries: &[Entry]) -> String {
    let start = entries
        .iter()
        .rposition(|entry| matches!(entry.event, Event::User { .. }))
        .map_or(0, |index| index + 1);
    entries[start..]
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::Assistant { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

impl Threads {
    /// How deep a thread may be and still start sub-agents.
    pub fn max_depth(&self) -> u32 {
        self.children.max_depth.load(Ordering::Relaxed)
    }

    /// Sets how deep a thread may be and still start sub-agents.
    pub fn set_max_depth(&self, max_depth: u32) {
        self.children.max_depth.store(max_depth, Ordering::Relaxed);
    }

    /// Whether thread `caller` may start sub-agents and send them messages: it exists, is not
    /// archived and is less deep than [`Threads::max_depth`].
    pub async fn may_delegate(&self, caller: ThreadId) -> anyhow::Result<bool> {
        Ok(self
            .get(caller)
            .await?
            .is_some_and(|row| row.archived_at.is_none() && row.depth < self.max_depth()))
    }

    /// The providers a sub-agent may run on: the configured one.
    pub fn providers(&self) -> Vec<&'static str> {
        vec![self.spec.provider.id]
    }

    /// For each child of `parent`, the seq of the message of the last turn whose result a
    /// waiting call took: that result reached the parent already.
    pub fn delivered(&self, child: ThreadId) -> Option<u64> {
        lock(&self.children.delivered).get(&child).copied()
    }

    /// Thread `target` if `caller` may reach it ([`may_access`]); else
    /// [`Delegation::NoSuchThread`], as when there is no such thread.
    async fn reachable(&self, caller: ThreadId, target: ThreadId) -> anyhow::Result<ThreadRow> {
        let Some(row) = self.get(target).await? else {
            bail!(Delegation::NoSuchThread);
        };
        let mut parents = HashMap::from([(row.id, row.parent)]);
        let mut next = row.parent;
        for _ in 0..MAX_CHAIN {
            let Some(id) = next else { break };
            if id == caller || parents.contains_key(&id) {
                break;
            }
            let parent = self.get(id).await?.and_then(|row| row.parent);
            parents.insert(id, parent);
            next = parent;
        }
        if may_access(caller, target, |id| parents.get(&id).copied().flatten()) {
            Ok(row)
        } else {
            bail!(Delegation::NoSuchThread)
        }
    }

    /// A sub-agent of `caller`, not `caller` itself.
    async fn descendant(&self, caller: ThreadId, target: ThreadId) -> anyhow::Result<ThreadRow> {
        let row = self.reachable(caller, target).await?;
        if row.id == caller {
            bail!(Delegation::Itself);
        }
        Ok(row)
    }

    /// The open assistant of thread `id`, which exists.
    async fn opened(&self, id: ThreadId) -> anyhow::Result<Arc<Assistant>> {
        self.assistant(id)
            .await?
            .with_context(|| format!("thread {id} vanished"))
    }

    /// Starts a sub-agent of thread `parent`, whose running turn is `turn`: a new thread whose
    /// first message is the task, written by the parent, in a new session of its own. A call
    /// whose `client_request_id` already started a sub-agent in the same turn, for the same task,
    /// title, provider and model, joins that one instead ([`StartedChild::joined`]); one that
    /// started another is [`Delegation::Reused`].
    pub async fn start_child(
        &self,
        parent: ThreadId,
        turn: Option<u64>,
        child: ChildTask,
    ) -> anyhow::Result<StartedChild> {
        let _starting = self.children.starting.lock().await;
        let Some(parent_row) = self.get(parent).await? else {
            bail!(Delegation::NoSuchThread);
        };
        let max_depth = self.max_depth();
        if parent_row.archived_at.is_some() || parent_row.depth >= max_depth {
            bail!(Delegation::TooDeep { max_depth });
        }
        let provider = child
            .provider
            .clone()
            .unwrap_or_else(|| parent_row.provider.clone());
        if provider != self.spec.provider.id {
            bail!(Delegation::Provider {
                given: provider,
                configured: self.spec.provider.id.into(),
            });
        }
        let title = child
            .title
            .clone()
            .unwrap_or_else(|| fronde_protocol::first_title(&child.task));
        let started_by = StartedBy::Thread(parent);
        if let Some(client_request_id) = &child.client_request_id
            && let Some(earlier) = threads::Entity::find()
                .filter(threads::Column::StartedBy.eq(started_by.stored()))
                .filter(threads::Column::ClientRequestId.eq(client_request_id))
                .filter(threads::Column::ArchivedAt.is_null())
                .one(&self.db)
                .await?
        {
            let earlier = ThreadRow::from_model(earlier)?;
            if let Some(seq) = self
                .retried(parent, turn, &child, &title, &provider, &earlier)
                .await?
            {
                return Ok(StartedChild {
                    row: earlier,
                    joined: true,
                    seq,
                });
            }
            bail!(Delegation::Reused {
                client_request_id: client_request_id.clone(),
                child: earlier.id,
            });
        }
        let transaction = fronde_db::begin_write(&self.db).await?;
        let mut row = self
            .insert(
                &transaction,
                &parent_row.owner,
                started_by,
                Some(parent),
                &title,
                child.client_request_id.as_deref(),
            )
            .await?;
        if let Some(model) = &child.model {
            threads::Entity::update_many()
                .col_expr(threads::Column::Model, Expr::value(model.clone()))
                .col_expr(threads::Column::UpdatedAt, Expr::value(Utc::now()))
                .filter(threads::Column::Id.eq(row.id.0))
                .exec(&transaction)
                .await?;
            row.model = Some(model.clone());
        }
        transaction.commit().await?;
        self.changes.notify();
        if let (Some(client_request_id), Some(turn)) = (&child.client_request_id, turn) {
            lock(&self.children.retries)
                .insert((parent, client_request_id.clone()), (row.id, turn));
        }
        let assistant = self.opened(row.id).await?;
        let seq = match assistant
            .try_send(child.task, Some(parent_row.title.clone()))
            .await
        {
            Ok(Some(seq)) => seq,
            Ok(None) => bail!("the task of sub-agent {} could not be logged", row.id),
            Err(Refused::Busy) => bail!(Delegation::Busy),
            Err(Refused::Archived) => bail!(Delegation::Archived),
        };
        tracing::info!("thread {parent} started the sub-agent {}", row.id);
        Ok(StartedChild {
            row,
            joined: false,
            seq,
        })
    }

    /// The seq of `earlier`'s task message, if this call retries the one that started it: the
    /// same parent's turn, and the same task, title, provider and model.
    async fn retried(
        &self,
        parent: ThreadId,
        turn: Option<u64>,
        child: &ChildTask,
        title: &str,
        provider: &str,
        earlier: &ThreadRow,
    ) -> anyhow::Result<Option<u64>> {
        let Some(turn) = turn else {
            return Ok(None);
        };
        let Some(client_request_id) = &child.client_request_id else {
            return Ok(None);
        };
        let same_turn = lock(&self.children.retries)
            .get(&(parent, client_request_id.clone()))
            .is_some_and(|started| *started == (earlier.id, turn));
        if !same_turn
            || earlier.title != title
            || earlier.provider != provider
            || earlier.model != child.model
        {
            return Ok(None);
        }
        let assistant = self.opened(earlier.id).await?;
        let first = assistant
            .log()
            .since(0)
            .into_iter()
            .find_map(|entry| match entry.event {
                Event::User { text, .. } => Some((entry.seq, text)),
                _ => None,
            });
        Ok(first
            .filter(|(_, task)| *task == child.task)
            .map(|(seq, _)| seq))
    }

    /// Sends `text` to `caller`'s sub-agent `target`, as a new turn written by `caller`: the
    /// seq of the message. Refused while the sub-agent is in a turn.
    pub async fn send_child(
        &self,
        caller: ThreadId,
        target: ThreadId,
        text: String,
    ) -> anyhow::Result<u64> {
        let row = self.descendant(caller, target).await?;
        if !self.may_delegate(caller).await? {
            bail!(Delegation::TooDeep {
                max_depth: self.max_depth()
            });
        }
        let by = self.get(caller).await?.map(|caller| caller.title);
        let assistant = self.opened(row.id).await?;
        match assistant.try_send(text, by).await {
            Ok(Some(seq)) => Ok(seq),
            Ok(None) => bail!("the message to sub-agent {target} could not be logged"),
            Err(Refused::Busy) => bail!(Delegation::Busy),
            Err(Refused::Archived) => bail!(Delegation::Archived),
        }
    }

    /// `caller`'s sub-agent `target`, with its status and the entries of its stream after
    /// `after`.
    pub async fn read_child(
        &self,
        caller: ThreadId,
        target: ThreadId,
        after: u64,
    ) -> anyhow::Result<(ThreadRow, ThreadStatus, Vec<Entry>)> {
        let row = self.descendant(caller, target).await?;
        let assistant = self.opened(row.id).await?;
        let entries = assistant.log().since(after);
        Ok((row, assistant.status(), entries))
    }

    /// `caller`'s sub-agents, theirs included, newest first: at most [`MAX_LISTED`], with how
    /// many more there are.
    pub async fn children(
        &self,
        caller: ThreadId,
    ) -> anyhow::Result<(Vec<(ThreadRow, ThreadSummary)>, usize)> {
        let mut rows = self.descendants(caller).await?;
        rows.sort_by_key(|row| std::cmp::Reverse(row.id));
        let more = rows.len().saturating_sub(MAX_LISTED);
        rows.truncate(MAX_LISTED);
        let pending = match self.requests.get().and_then(std::sync::Weak::upgrade) {
            Some(requests) => requests.pending_by_topic().await?,
            None => HashMap::new(),
        };
        let open = self.open.lock().await.clone();
        let mut listed = Vec::with_capacity(rows.len());
        for row in rows {
            let summary = self.summary(row.clone(), &open, &pending).await?;
            listed.push((row, summary));
        }
        Ok((listed, more))
    }

    /// Every descendant of `id`, archived ones included.
    async fn descendants(&self, id: ThreadId) -> anyhow::Result<Vec<ThreadRow>> {
        let mut found = Vec::new();
        let mut seen = HashSet::from([id]);
        let mut frontier = vec![id.0];
        for _ in 0..MAX_CHAIN {
            if frontier.is_empty() {
                break;
            }
            let rows = threads::Entity::find()
                .filter(threads::Column::ParentId.is_in(frontier))
                .order_by_asc(threads::Column::Id)
                .all(&self.db)
                .await?;
            frontier = Vec::new();
            for row in rows {
                let row = ThreadRow::from_model(row)?;
                if seen.insert(row.id) {
                    frontier.push(row.id.0);
                    found.push(row);
                }
            }
        }
        Ok(found)
    }

    /// Stops the running turn of `caller`'s sub-agent `target`, if any, with its pending cards,
    /// and those of its own sub-agents; a turn that ended keeps its result. `reason`, if any, is
    /// shown in the stopped thread. Whether `target` was in a turn.
    pub async fn cancel_child(
        &self,
        caller: ThreadId,
        target: ThreadId,
        reason: Option<&str>,
    ) -> anyhow::Result<bool> {
        let row = self.descendant(caller, target).await?;
        let mut tree = vec![row.clone()];
        tree.extend(self.descendants(row.id).await?);
        let mut stopped = false;
        for thread in tree {
            let Some(assistant) = self.open.lock().await.get(&thread.id).cloned() else {
                // A thread not open runs no turn.
                continue;
            };
            if !matches!(
                assistant.status(),
                ThreadStatus::Running | ThreadStatus::Queued
            ) {
                continue;
            }
            if thread.id == row.id {
                stopped = true;
                if let Some(reason) = reason {
                    assistant
                        .append(Event::Notice {
                            text: format!("Arrêté par la conversation parente : {reason}"),
                        })
                        .await;
                }
            }
            assistant.cancel().await;
            tracing::info!(
                "thread {caller} stopped the turn of its sub-agent {}",
                thread.id
            );
        }
        Ok(stopped)
    }

    /// Waits for the turn of `caller`'s sub-agent `target` whose message is `seq` to end, at
    /// most `timeout`, and while `cancelled` is not: ends early when a card opens in that turn,
    /// for the user to answer. A result returned here is marked delivered
    /// ([`Threads::delivered`]).
    pub async fn wait_child(
        &self,
        caller: ThreadId,
        target: ThreadId,
        seq: u64,
        timeout: Duration,
        cancelled: &CancellationToken,
    ) -> anyhow::Result<ChildEnd> {
        let row = self.descendant(caller, target).await?;
        let assistant = self.opened(row.id).await?;
        // Listening before reading: an entry written in between is not missed.
        let mut live = assistant.log().subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        let mut seen = seq;
        let mut open_cards = HashSet::new();
        loop {
            for entry in assistant.log().since(seen) {
                seen = entry.seq;
                match entry.event {
                    Event::Request { id, .. } => {
                        open_cards.insert(id);
                    }
                    Event::RequestClosed { id, .. } => {
                        open_cards.remove(&id);
                    }
                    Event::TurnEnded { outcome } => {
                        let turn: Vec<Entry> = assistant
                            .log()
                            .since(seq.saturating_sub(1))
                            .into_iter()
                            .take_while(|entry| entry.seq <= seen)
                            .collect();
                        lock(&self.children.delivered).insert(row.id, seq);
                        return Ok(ChildEnd::Ended {
                            outcome,
                            answer: final_answer(&turn),
                        });
                    }
                    _ => {}
                }
            }
            if !open_cards.is_empty() {
                return Ok(ChildEnd::WaitingForUser);
            }
            tokio::select! {
                biased;
                () = cancelled.cancelled() => return Ok(ChildEnd::GaveUp),
                () = tokio::time::sleep_until(deadline) => return Ok(ChildEnd::TimedOut),
                received = live.recv() => {
                    if let Err(RecvError::Closed) = received {
                        bail!("the stream of sub-agent {target} closed");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> HashMap<ThreadId, Option<ThreadId>> {
        // 1 ── 2 ── 4
        //  └── 3
        // 5 (another user thread) ── 6
        HashMap::from([
            (ThreadId(1), None),
            (ThreadId(2), Some(ThreadId(1))),
            (ThreadId(3), Some(ThreadId(1))),
            (ThreadId(4), Some(ThreadId(2))),
            (ThreadId(5), None),
            (ThreadId(6), Some(ThreadId(5))),
        ])
    }

    fn reaches(caller: i64, target: i64) -> bool {
        let tree = tree();
        may_access(ThreadId(caller), ThreadId(target), |id| {
            tree.get(&id).copied().flatten()
        })
    }

    #[test]
    fn a_thread_reaches_itself_and_its_descendants_only() {
        assert!(reaches(1, 1));
        assert!(reaches(1, 2));
        assert!(reaches(1, 3));
        assert!(reaches(1, 4), "a grandchild");
        assert!(reaches(2, 4));
        assert!(!reaches(2, 1), "a child does not reach its parent");
        assert!(!reaches(2, 3), "nor its sibling");
        assert!(!reaches(4, 2), "nor its grandparent's line upwards");
        assert!(!reaches(3, 4), "nor its sibling's child");
        assert!(!reaches(1, 6), "nor another thread's children");
        assert!(!reaches(5, 2));
        assert!(!reaches(1, 99), "nor an unknown thread");
    }

    #[test]
    fn a_cycle_reaches_nothing() {
        let cycle = HashMap::from([(ThreadId(7), ThreadId(8)), (ThreadId(8), ThreadId(7))]);
        assert!(!may_access(ThreadId(1), ThreadId(7), |id| cycle
            .get(&id)
            .copied()));
    }

    fn entry(seq: u64, event: Event) -> Entry {
        Entry {
            seq,
            at: Utc::now(),
            event,
        }
    }

    #[test]
    fn the_final_answer_is_the_last_turns_text() {
        let text = |text: &str| Event::Assistant { text: text.into() };
        let user = |text: &str| Event::User {
            text: text.into(),
            by: None,
        };
        let entries = vec![
            entry(1, user("Tâche")),
            entry(2, text("Avant.")),
            entry(
                3,
                Event::TurnEnded {
                    outcome: TurnOutcome::Done,
                },
            ),
            entry(4, user("Encore")),
            entry(5, text("Voici ")),
            entry(
                6,
                Event::Tool {
                    id: "t".into(),
                    title: "Lire".into(),
                    state: crate::log::ToolState::Done,
                },
            ),
            entry(7, text("la réponse.")),
            entry(
                8,
                Event::TurnEnded {
                    outcome: TurnOutcome::Done,
                },
            ),
        ];
        assert_eq!(final_answer(&entries), "Voici la réponse.");
        assert_eq!(final_answer(&entries[..3]), "Avant.");
        assert_eq!(final_answer(&[]), "");
    }
}
