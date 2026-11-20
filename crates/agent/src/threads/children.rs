//! Sub-agents: the threads a thread's agent starts and follows through the hub's `threads_*`
//! tools. A child is a thread like any other, with a parent, a depth one below it, and only its
//! task to start from: a new session, never its parent's.
//!
//! One rule decides what an agent may reach: [`may_access`], the target being the caller or one
//! of its descendants. An unknown thread and one out of reach get the same refusal,
//! [`Delegation::NoSuchThread`], so that an agent learns nothing of the threads it did not
//! start. The caller is always the thread whose token the hub's gate saw, never an argument.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use fronde_entity::{stream_entries, threads};
use fronde_protocol::usage::{
    Background, LimitStatus, Pause as UsagePause, WindowKind, ceil_minute,
};
use fronde_protocol::{ThreadStatus, ThreadSummary};
use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Notify, oneshot};
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

/// Sub-agents of one thread in a turn at once, by default, waiting for an agent slot included.
pub const DEFAULT_CHILDREN_RUNNING: usize = 3;

/// Sub-agents all threads together may start in 24 hours, by default.
pub const DEFAULT_CHILDREN_PER_DAY: usize = 30;

/// Sub-agents one thread keeps not archived, by default: a guard against clutter only.
pub const DEFAULT_CHILDREN_KEPT: usize = 200;

/// Turns of sub-agents all threads together may start in 24 hours, by default.
pub const DEFAULT_CHILD_TURNS_PER_DAY: usize = 100;

/// Turns a thread may be woken by its sub-agents' results in a row without the owner, by
/// default.
pub const DEFAULT_WAKES_WITHOUT_OWNER: usize = 10;

/// The share of each subscription window kept for the owner, in percent, by default.
pub const DEFAULT_RESERVE: u8 = 30;

/// How long a sub-agent's turn may run once its agent has it, by default.
pub const DEFAULT_CHILD_TURN_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The window [`Limits::children_per_day`] counts in.
const DAY: chrono::TimeDelta = chrono::TimeDelta::hours(24);

/// The longest chain of parents walked: more than any depth allowed, and a guard against a
/// cycle a damaged database could hold.
pub(super) const MAX_CHAIN: usize = 64;

/// The most sub-agents [`Threads::children`] lists, newest first.
pub const MAX_LISTED: usize = 50;

/// What threads may do with sub-agents: `[threads]` in the configuration. Together they bound
/// what an agent, a prompt-injected one included, may spend of the subscription's quota without
/// the owner; Fronde enforces them, whatever the agent's prompt says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// How deep a thread may be and still start sub-agents: the owner's threads are at depth 0,
    /// their sub-agents at 1.
    pub max_depth: u32,
    /// Sub-agents of one thread in a turn at once, those waiting for an agent slot included.
    pub children_running: usize,
    /// Sub-agents started by all threads together in the last 24 hours, archived ones
    /// included.
    pub children_per_day: usize,
    /// Sub-agents one thread keeps not archived.
    pub children_kept: usize,
    /// Turns of sub-agents all threads together started in the last 24 hours: each
    /// sub-agent's task and each message sent to one.
    pub child_turns_per_day: usize,
    /// How long a sub-agent's turn, or a turn a sub-agent's result woke, may run once its agent
    /// has it; it is then stopped, as Stop does.
    pub child_turn_timeout: Duration,
    /// Turns a thread may be woken by its sub-agents' results since the owner's last message;
    /// later results wait for the owner's next message, which carries them.
    pub wakes_without_owner: usize,
    /// The share of each guarded subscription window kept for the owner, in percent (0 to
    /// 100): past the line, sub-agents are neither started nor sent messages and their results
    /// are held for the owner's next message. With it deciding, `children_per_day` and
    /// `child_turns_per_day` do not apply; they do when no usage reading decides
    /// ([`Background::Unknown`]). 0 turns it off.
    pub reserve: u8,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            children_running: DEFAULT_CHILDREN_RUNNING,
            children_per_day: DEFAULT_CHILDREN_PER_DAY,
            children_kept: DEFAULT_CHILDREN_KEPT,
            child_turns_per_day: DEFAULT_CHILD_TURNS_PER_DAY,
            child_turn_timeout: DEFAULT_CHILD_TURN_TIMEOUT,
            wakes_without_owner: DEFAULT_WAKES_WITHOUT_OWNER,
            reserve: DEFAULT_RESERVE,
        }
    }
}

/// What the registry keeps for sub-agents.
pub(super) struct Children {
    /// What threads may do with sub-agents.
    limits: Limits,
    /// Held while a sub-agent starts: a retry of the same call finds the first one's child with
    /// its task logged, and two starts never both pass the limits.
    pub(super) starting: tokio::sync::Mutex<()>,
    /// Shared with every thread's assistant, whose turns' ends go through it.
    pub(super) handoff: Arc<Handoff>,
    /// For tests: where [`Threads::start_child`] and [`Threads::wait_child`] pause.
    pauses: Pauses,
}

impl Children {
    pub(super) fn new(handoff: Arc<Handoff>, limits: Limits) -> Self {
        Self {
            limits,
            starting: tokio::sync::Mutex::new(()),
            handoff,
            pauses: Pauses::default(),
        }
    }
}

/// A point where a call stops until a test lets it go: `reached` is told when the call gets
/// there, and the call goes on once `release` is told.
#[derive(Clone)]
pub struct Pause {
    pub reached: Arc<Notify>,
    pub release: Arc<Notify>,
}

impl Pause {
    pub fn new() -> Self {
        Self {
            reached: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        }
    }

    pub(crate) async fn hold(&self) {
        self.reached.notify_one();
        self.release.notified().await;
    }
}

impl Default for Pause {
    fn default() -> Self {
        Self::new()
    }
}

/// The pauses tests set; none in Fronde itself.
#[derive(Default)]
struct Pauses {
    /// In `start_child`, once the limits passed, before the sub-agent's row is made.
    start: Mutex<Option<Pause>>,
    /// In `wait_child`, once the wait stopped, before it withdraws.
    withdraw: Mutex<Option<Pause>>,
}

impl Pauses {
    async fn at(slot: &Mutex<Option<Pause>>) {
        let pause = lock(slot).take();
        if let Some(pause) = pause {
            pause.hold().await;
        }
    }
}

/// How a sub-agent's turn ended, as its parent gets it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnResult {
    pub outcome: TurnOutcome,
    /// [`final_answer`] of the turn.
    pub answer: String,
}

/// Where the end of a turn meets the calls waiting for it, so that its result reaches the parent
/// once: either a waiting call takes it, or nobody does and the parent is told otherwise.
///
/// The contract, every step under the one lock:
///
/// - a call that waits for turn `seq` of thread `child` [`Handoff::register`]s before it
///   blocks; registering a turn whose end was already decided is refused
///   ([`AlreadyEnded`]), saying whether a waiting call took it: if one did, that call reads the
///   result from the stream, the end being logged there before it was decided; if none did,
///   the result went to the parent as a wake, and the call does not return it again;
/// - the turn's end, once logged, calls [`Handoff::turn_ended`]: the result goes through a
///   oneshot to every call registered for that turn, which is then marked delivered, and the
///   function says whether one took it. A turn nobody took wakes the parent instead, with a
///   `ChildEnded` entry (see [`super::wakes`]): never both;
/// - a call that stops waiting (timeout, given up, a card, its turn over)
///   [`Handoff::withdraw`]s: if the result was handed over meanwhile, it gets it then, so that
///   a result marked delivered is never lost; a call whose caller is gone gives it back
///   ([`Handoff::give_back`]), and it wakes the parent instead, unless another call it was
///   handed to returns it ([`Handoff::returned`]): the last of those calls to settle decides.
///
/// It also remembers which child each `(parent, client_request_id)` started, in which of the
/// parent's turns, so that a retry joins it only in that turn; those entries go when that turn
/// ends. Everything is in memory: no turn outlives Fronde.
#[derive(Default)]
pub struct Handoff {
    state: Mutex<HandoffState>,
}

/// The calls waiting for one turn: each one's id and where its result goes.
type Waiting = Vec<(u64, oneshot::Sender<TurnResult>)>;

/// How many ended turns of each thread [`Handoff`] remembers the fate of.
const ENDS_KEPT: usize = 32;

#[derive(Default)]
struct HandoffState {
    next: u64,
    /// The calls waiting, by thread and turn.
    waiting: HashMap<(ThreadId, u64), Waiting>,
    /// The last turns of each thread whose end was decided, by seq.
    ended: HashMap<ThreadId, std::collections::BTreeMap<u64, Fate>>,
    /// `(parent, client_request_id)` → (child, the parent's turn that started it).
    retries: HashMap<(ThreadId, String), (ThreadId, u64)>,
}

/// What became of an ended turn's result.
#[derive(Clone, Copy, Debug, Default)]
struct Fate {
    /// A waiting call took the result (or the turn was settled); else it went to the parent as
    /// a wake.
    taken: bool,
    /// The calls it was handed to that have not said yet whether they return it.
    handed: usize,
    /// One of those calls returned it to its caller: it is taken for good.
    returned: bool,
}

/// The turn's end was decided before the call registered: its result is in the stream.
/// `taken`: whether a waiting call took it (`Some(false)`: it woke the parent); `None` when
/// that turn is too old to be remembered, or no turn's seq.
#[derive(Debug, PartialEq, Eq)]
pub struct AlreadyEnded {
    pub taken: Option<bool>,
}

/// A call's place among those waiting for a turn.
pub struct Registration {
    id: u64,
    result: oneshot::Receiver<TurnResult>,
}

impl Handoff {
    /// Registers a call waiting for turn `seq` of `thread`.
    pub fn register(&self, thread: ThreadId, seq: u64) -> Result<Registration, AlreadyEnded> {
        let mut state = lock(&self.state);
        if let Some(ended) = state.ended.get(&thread)
            && ended.last_key_value().is_some_and(|(last, _)| *last >= seq)
        {
            return Err(AlreadyEnded {
                taken: ended.get(&seq).map(|fate| fate.taken),
            });
        }
        state.next += 1;
        let id = state.next;
        let (sender, result) = oneshot::channel();
        state
            .waiting
            .entry((thread, seq))
            .or_default()
            .push((id, sender));
        Ok(Registration { id, result })
    }

    /// The call of `registration` stops waiting for turn `seq` of `thread`: the result, if it
    /// was handed over meanwhile.
    pub fn withdraw(
        &self,
        thread: ThreadId,
        seq: u64,
        mut registration: Registration,
    ) -> Option<TurnResult> {
        let mut state = lock(&self.state);
        if let Some(waiting) = state.waiting.get_mut(&(thread, seq)) {
            let before = waiting.len();
            waiting.retain(|(id, _)| *id != registration.id);
            let withdrawn = waiting.len() < before;
            if waiting.is_empty() {
                state.waiting.remove(&(thread, seq));
            }
            if withdrawn {
                return None;
            }
        }
        registration.result.try_recv().ok()
    }

    /// Turn `seq` of `thread` ended, logged as `result`: hands it to every call registered for
    /// it, and forgets the retries the turn started. Whether a call took it.
    #[cfg(test)]
    pub fn turn_ended(&self, thread: ThreadId, seq: u64, result: TurnResult) -> bool {
        self.turn_ended_with(thread, seq, result, false)
    }

    /// [`Handoff::turn_ended`]; `settled`: the turn was stopped by the one its result is for (a
    /// `threads_cancel` or an archive up its tree), so that its result is marked delivered even
    /// when no call took it, and wakes nobody: a call registering later reads it from the
    /// stream. Whether a call took it, or `settled`.
    pub fn turn_ended_with(
        &self,
        thread: ThreadId,
        seq: u64,
        result: TurnResult,
        settled: bool,
    ) -> bool {
        let mut state = lock(&self.state);
        state
            .retries
            .retain(|(parent, _), (_, turn)| !(*parent == thread && *turn <= seq));
        let mut handed = 0;
        for (_, sender) in state.waiting.remove(&(thread, seq)).unwrap_or_default() {
            if sender.send(result.clone()).is_ok() {
                handed += 1;
            }
        }
        let delivered = handed > 0;
        let ended = state.ended.entry(thread).or_default();
        ended.insert(
            seq,
            Fate {
                taken: delivered || settled,
                handed,
                returned: false,
            },
        );
        while ended.len() > ENDS_KEPT {
            ended.pop_first();
        }
        delivered || settled
    }

    /// The result of turn `seq` of `thread` was handed to a call whose caller is gone: unless
    /// another call it was handed to returned it, or may still, it is no longer taken, and goes
    /// to the parent as a wake. Whether it goes there now: then the caller wakes the parent.
    pub fn give_back(&self, thread: ThreadId, seq: u64) -> bool {
        let mut state = lock(&self.state);
        let Some(fate) = state
            .ended
            .get_mut(&thread)
            .and_then(|ended| ended.get_mut(&seq))
        else {
            return false;
        };
        fate.handed = fate.handed.saturating_sub(1);
        if !fate.taken || fate.returned || fate.handed > 0 {
            return false;
        }
        fate.taken = false;
        true
    }

    /// A call handed the result of turn `seq` of `thread` returned it to its caller: it stays
    /// taken, whatever the other calls handed it do.
    pub fn returned(&self, thread: ThreadId, seq: u64) {
        let mut state = lock(&self.state);
        if let Some(fate) = state
            .ended
            .get_mut(&thread)
            .and_then(|ended| ended.get_mut(&seq))
        {
            fate.handed = fate.handed.saturating_sub(1);
            fate.returned = true;
        }
    }

    /// The last turn of `thread` whose end was decided, if a waiting call took its result.
    pub fn delivered(&self, thread: ThreadId) -> Option<u64> {
        lock(&self.state)
            .ended
            .get(&thread)
            .and_then(|ended| ended.last_key_value())
            .and_then(|(seq, fate)| fate.taken.then_some(*seq))
    }

    /// How many calls wait for turns of `thread`.
    pub fn waiting(&self, thread: ThreadId) -> usize {
        lock(&self.state)
            .waiting
            .iter()
            .filter(|((waited, _), _)| *waited == thread)
            .map(|(_, calls)| calls.len())
            .sum()
    }

    /// Remembers that `client_request_id` started `child` in `parent`'s turn `turn`.
    fn remember_retry(
        &self,
        parent: ThreadId,
        client_request_id: &str,
        child: ThreadId,
        turn: u64,
    ) {
        lock(&self.state)
            .retries
            .insert((parent, client_request_id.to_owned()), (child, turn));
    }

    /// Whether `client_request_id` started `child` in `parent`'s turn `turn`.
    fn started_in(
        &self,
        parent: ThreadId,
        client_request_id: &str,
        child: ThreadId,
        turn: u64,
    ) -> bool {
        lock(&self.state)
            .retries
            .get(&(parent, client_request_id.to_owned()))
            .is_some_and(|started| *started == (child, turn))
    }

    fn forget_retry(&self, parent: ThreadId, client_request_id: &str) {
        lock(&self.state)
            .retries
            .remove(&(parent, client_request_id.to_owned()));
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
    /// The sub-agent waits for an agent slot: it starts once another agent stops.
    Queued,
    /// The turn had already ended, and its result went to the caller as a wake: not given
    /// twice.
    Woken,
    /// The wait timed out; the sub-agent goes on.
    TimedOut,
    /// The call waiting was given up, or the caller's turn is over; the sub-agent goes on.
    GaveUp,
}

/// Why a thread's agent may not do what it asked with a sub-agent. The texts are for the agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delegation {
    /// No such thread, or not one the caller started: the same refusal.
    NoSuchThread,
    /// The caller is a sub-agent as deep as sub-agents go.
    TooDeep { max_depth: u32 },
    /// The caller's own thread is archived.
    CallerArchived,
    /// The caller has as many sub-agents in a turn as it may.
    TooManyRunning { limit: usize },
    /// The caller keeps as many sub-agents as it may.
    TooManyKept { limit: usize },
    /// The threads started as many sub-agents in the last 24 hours as they may.
    TooManyToday { limit: usize },
    /// The threads started as many turns of sub-agents in the last 24 hours as they may.
    TooManyTurnsToday { limit: usize },
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
    /// The caller's turn that made the call ended before it went through.
    TurnOver,
    /// The thread's owner has no account on the provider: no sub-agent runs for them.
    NoAccount { owner: String, provider: String },
    /// The thread's owner is no longer a user: it is read-only.
    NoOwner,
    /// The owner's reserve of a subscription window is reached (`[threads] reserve`): `window`,
    /// its use in percent and status as read, the reserve in percent and when the window resets.
    Reserve {
        window: WindowKind,
        used: Option<u8>,
        status: Option<LimitStatus>,
        reserve: u8,
        resets_at: Option<DateTime<Utc>>,
    },
}

impl Delegation {
    /// The refusal of a start or a send while `pause` holds background work, under a reserve
    /// of `reserve` percent.
    pub fn reserve(pause: UsagePause, reserve: u8) -> Self {
        Self::Reserve {
            used: pause.used_percent(),
            window: pause.window,
            status: pause.status,
            reserve,
            resets_at: pause.resets_at,
        }
    }
}

impl std::fmt::Display for Delegation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuchThread => f.write_str(
                "no such thread among the sub-agents you started; threads_list shows them",
            ),
            Self::TooDeep { max_depth: 0 } => f.write_str(
                "sub-agents are turned off here (the setting [threads] max_depth is 0): you cannot start or drive sub-agents; do the task yourself",
            ),
            Self::TooDeep { max_depth } => write!(
                f,
                "you are a sub-agent, and sub-agents go {max_depth} level(s) deep at most (the setting [threads] max_depth): you cannot start or drive sub-agents; do the task yourself"
            ),
            Self::CallerArchived => f.write_str(
                "your thread, or a thread above it, is archived: it cannot start or drive sub-agents",
            ),
            Self::TooManyRunning { limit } => write!(
                f,
                "you already have {limit} sub-agents running or waiting for an agent, the most at once (the setting [threads] children_running); wait for one to end, or stop one with threads_cancel"
            ),
            Self::TooManyKept { limit } => write!(
                f,
                "you already have {limit} sub-agents not archived, the most a thread keeps (the setting [threads] children_kept); reuse one with threads_send, or ask the user to archive some"
            ),
            Self::TooManyToday { limit } => write!(
                f,
                "{limit} sub-agents were started in the last 24 hours, the most allowed (the setting [threads] children_per_day); do the task yourself, or ask the user"
            ),
            Self::TooManyTurnsToday { limit } => write!(
                f,
                "{limit} turns of sub-agents were started in the last 24 hours, the most allowed (the setting [threads] child_turns_per_day); do the task yourself, or ask the user"
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
            Self::TurnOver => f.write_str(
                "your turn ended before this call went through: nothing was started or sent",
            ),
            Self::NoOwner => f.write_str(
                "this thread belongs to someone who is no longer a user: it is read-only",
            ),
            Self::NoAccount { owner, provider } => write!(
                f,
                "{owner} has no {provider} account, so no sub-agent runs for this conversation; \
                 do the task yourself"
            ),
            Self::Reserve {
                window,
                used,
                status,
                reserve,
                resets_at,
            } => {
                let what = match window {
                    WindowKind::Extra => "paid extra usage is being spent".to_owned(),
                    window => format!(
                        "the owner's reserve of the {} is reached",
                        window.describe()
                    ),
                };
                let used = match (used, status) {
                    (Some(used), _) => format!("{used} % used, "),
                    (None, Some(LimitStatus::Rejected)) => "its limit reached, ".to_owned(),
                    (None, Some(LimitStatus::Warning)) => {
                        "close to its limit, no percentage given, ".to_owned()
                    }
                    (None, _) => String::new(),
                };
                let reset = match resets_at {
                    Some(at) => format!(
                        "it resets at {}",
                        ceil_minute(*at).format("%Y-%m-%d %H:%M UTC")
                    ),
                    None => "its reset time is not known".to_owned(),
                };
                write!(
                    f,
                    "{what} ({used}{reserve} % kept for the owner, the setting [threads] reserve); {reset}; sub-agents can neither start nor get messages until then: do the task yourself or tell the user"
                )
            }
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
    /// What threads may do with sub-agents.
    pub fn limits(&self) -> Limits {
        self.children.limits
    }

    /// Whether background work may spend subscription account `account` now
    /// ([`Threads::account`]), under [`Limits::reserve`]: [`Background::Unknown`] without a
    /// usage book or a reading.
    pub fn background(&self, account: &str) -> Background {
        let usage = self.spec.usage.as_ref().and_then(|book| book.get(account));
        fronde_protocol::usage::background(usage.as_ref(), self.limits().reserve, Utc::now())
    }

    /// The latest usage of `user`'s own subscription accounts ([`Threads::usage_is_own`]), one
    /// per configured provider read so far: what the chat's meter shows that user. Empty
    /// without a usage book or a reading, and for a user without an account.
    pub fn usage_of(&self, user: &str) -> Vec<fronde_protocol::usage::AccountUsage> {
        self.usage_where(|account| self.usage_is_own(user, account))
    }

    /// The latest usage of every configured account but `user`'s own
    /// ([`Threads::usage_is_own`]), by account name: what an admin's meter lists besides their
    /// own. Never ask it for anyone else.
    pub fn usage_of_others(&self, user: &str) -> Vec<fronde_protocol::usage::AccountUsage> {
        self.usage_where(|account| !self.usage_is_own(user, account))
    }

    /// Whether `account` is one of `user`'s own, whose usage their meter's pill shows: the one
    /// predicate [`Threads::usage_of`] and [`Threads::usage_of_others`] split the accounts by.
    fn usage_is_own(&self, user: &str, account: &str) -> bool {
        self.providers()
            .into_iter()
            .any(|provider| self.account(user, provider).as_deref() == Some(account))
    }

    /// The latest reading of each configured account `keep` keeps, by account name.
    fn usage_where(
        &self,
        keep: impl Fn(&str) -> bool,
    ) -> Vec<fronde_protocol::usage::AccountUsage> {
        let Some(book) = &self.spec.usage else {
            return Vec::new();
        };
        self.account_names()
            .into_iter()
            .filter(|account| keep(account))
            .filter_map(|account| book.get(&account))
            .collect()
    }

    /// Moves on each new usage reading; `None` without a usage book.
    pub fn usage_changes(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        self.spec.usage.as_ref().map(|book| book.changes())
    }

    /// `owner`'s account on `provider`, or [`Delegation::NoAccount`].
    fn account_or_refuse(&self, owner: &str, provider: &str) -> Result<String, Delegation> {
        self.account(owner, provider)
            .ok_or_else(|| Delegation::NoAccount {
                owner: owner.to_owned(),
                provider: provider.to_owned(),
            })
    }

    /// Refuses background work past the owner's reserve of `account`; whether the daily counts
    /// apply: only when no usage reading decides.
    fn check_reserve(&self, account: &str) -> Result<bool, Delegation> {
        match self.background(account) {
            Background::Paused(pause) => Err(Delegation::reserve(pause, self.limits().reserve)),
            Background::Allowed { .. } => Ok(false),
            Background::Unknown => Ok(true),
        }
    }

    /// How deep a thread may be and still start sub-agents.
    pub fn max_depth(&self) -> u32 {
        self.children.limits.max_depth
    }

    /// Whether thread `caller` may start sub-agents and send them messages: it exists, is not
    /// archived and is less deep than [`Threads::max_depth`].
    pub async fn may_delegate(&self, caller: ThreadId) -> anyhow::Result<bool> {
        match self.check_delegate(caller).await {
            Ok(()) => Ok(true),
            Err(error) if error.is::<Delegation>() => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// [`Threads::may_delegate`], with the refusal: [`Delegation::CallerArchived`] or
    /// [`Delegation::TooDeep`].
    pub async fn check_delegate(&self, caller: ThreadId) -> anyhow::Result<()> {
        let Some(row) = self.get(caller).await? else {
            bail!(Delegation::NoSuchThread);
        };
        self.delegates(&row).await
    }

    /// Whether thread `row` may start and drive sub-agents: neither it nor a thread above it
    /// is archived, and it is less deep than [`Threads::max_depth`].
    async fn delegates(&self, row: &ThreadRow) -> anyhow::Result<()> {
        if !self.configured(row) {
            bail!(Delegation::NoOwner);
        }
        if row.archived_at.is_some() || self.chain_archived(row.id).await? {
            bail!(Delegation::CallerArchived);
        }
        let max_depth = self.max_depth();
        if row.depth >= max_depth {
            bail!(Delegation::TooDeep { max_depth });
        }
        Ok(())
    }

    /// For tests: the next [`Threads::start_child`] stops at `pause`, under the lock that
    /// starts, sends and archives hold, once its limits passed.
    #[doc(hidden)]
    pub fn pause_next_start(&self, pause: Pause) {
        *lock(&self.children.pauses.start) = Some(pause);
    }

    /// For tests: the next [`Threads::wait_child`] that stops waiting stops at `pause`, before
    /// it withdraws.
    #[doc(hidden)]
    pub fn pause_next_withdraw(&self, pause: Pause) {
        *lock(&self.children.pauses.withdraw) = Some(pause);
    }

    /// `parent`'s own sub-agents not archived.
    async fn kept_children(&self, parent: ThreadId) -> anyhow::Result<Vec<ThreadId>> {
        Ok(threads::Entity::find()
            .filter(threads::Column::ParentId.eq(parent.0))
            .filter(threads::Column::ArchivedAt.is_null())
            .all(&self.db)
            .await?
            .into_iter()
            .map(|row| ThreadId(row.id))
            .collect())
    }

    /// How many sub-agents on subscription account `account` all threads together started
    /// since `since`, archived ones included.
    async fn started_since(
        &self,
        account: &str,
        since: chrono::DateTime<Utc>,
    ) -> anyhow::Result<u64> {
        let started: Vec<(String, String)> = threads::Entity::find()
            .select_only()
            .column(threads::Column::Owner)
            .column(threads::Column::Provider)
            .filter(threads::Column::StartedBy.starts_with("thread:"))
            .filter(threads::Column::CreatedAt.gt(since))
            .into_tuple()
            .all(&self.db)
            .await?;
        let on_account = started
            .iter()
            .filter(|(owner, provider)| self.account(owner, provider).as_deref() == Some(account))
            .count();
        Ok(u64::try_from(on_account).unwrap_or(u64::MAX))
    }

    /// How many turns of sub-agents on subscription account `account` all threads together
    /// started since `since`: the messages a thread wrote (a task, or a `threads_send`), read
    /// from the streams, by the account of the thread each went to.
    async fn turns_since(
        &self,
        account: &str,
        since: chrono::DateTime<Utc>,
    ) -> anyhow::Result<u64> {
        let by_topic: Vec<(String, i64)> = stream_entries::Entity::find()
            .select_only()
            .column(stream_entries::Column::Topic)
            .column_as(stream_entries::Column::Seq.count(), "turns")
            .filter(stream_entries::Column::At.gt(since))
            .filter(Expr::cust("json_extract(event, '$.type')").eq("user"))
            .filter(Expr::cust("json_extract(event, '$.by')").is_not_null())
            .group_by(stream_entries::Column::Topic)
            .into_tuple()
            .all(&self.db)
            .await?;
        if by_topic.is_empty() {
            return Ok(0);
        }
        let accounts: HashMap<String, Option<String>> = threads::Entity::find()
            .select_only()
            .column(threads::Column::Topic)
            .column(threads::Column::Owner)
            .column(threads::Column::Provider)
            .filter(threads::Column::Topic.is_in(by_topic.iter().map(|(topic, _)| topic.clone())))
            .into_tuple::<(String, String, String)>()
            .all(&self.db)
            .await?
            .into_iter()
            .map(|(topic, owner, provider)| (topic, self.account(&owner, &provider)))
            .collect();
        Ok(by_topic
            .into_iter()
            .filter(|(topic, _)| {
                accounts
                    .get(topic)
                    .is_some_and(|on| on.as_deref() == Some(account))
            })
            .map(|(_, turns)| u64::try_from(turns).unwrap_or(0))
            .sum())
    }

    /// Refuses a new turn of a sub-agent on `account` past [`Limits::child_turns_per_day`].
    async fn check_turns_today(&self, account: &str) -> anyhow::Result<()> {
        let limit = self.limits().child_turns_per_day;
        let today = self.turns_since(account, Utc::now() - DAY).await?;
        if today >= u64::try_from(limit).unwrap_or(u64::MAX) {
            bail!(Delegation::TooManyTurnsToday { limit });
        }
        Ok(())
    }

    /// How many of `children` are in a turn or waiting for an agent: only an open thread runs
    /// one.
    async fn running_among(&self, children: &[ThreadId]) -> usize {
        let open = self.open.lock().await;
        children
            .iter()
            .filter_map(|id| open.get(id))
            .filter(|assistant| {
                matches!(
                    assistant.status(),
                    ThreadStatus::Running | ThreadStatus::Queued
                )
            })
            .count()
    }

    /// The providers a sub-agent may run on: the configured one.
    pub fn providers(&self) -> Vec<&'static str> {
        vec![self.spec.provider.id]
    }

    /// For each child of `parent`, the seq of the message of the last turn whose result a
    /// waiting call took: that result reached the parent already.
    pub fn delivered(&self, child: ThreadId) -> Option<u64> {
        self.children.handoff.delivered(child)
    }

    /// How many calls wait for turns of thread `child`.
    pub fn waiting(&self, child: ThreadId) -> usize {
        self.children.handoff.waiting(child)
    }

    /// Thread `target` if `caller` may reach it ([`may_access`]) and both are one person's;
    /// else [`Delegation::NoSuchThread`], as when there is no such thread. An agent never
    /// reaches another person's thread, whatever the tree says.
    async fn reachable(&self, caller: ThreadId, target: ThreadId) -> anyhow::Result<ThreadRow> {
        let (Some(row), Some(calling)) = (self.get(target).await?, self.get(caller).await?) else {
            bail!(Delegation::NoSuchThread);
        };
        if row.owner != calling.owner {
            tracing::warn!(
                "thread {caller}, {}'s, reached for thread {target}, {}'s: refused",
                calling.owner,
                row.owner
            );
            bail!(Delegation::NoSuchThread);
        }
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
        // Read-only: nothing drives a thread whose owner is no longer a user.
        if !self.configured(&row) {
            bail!(Delegation::NoOwner);
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
        let starting = self.children.starting.lock().await;
        let Some(parent_row) = self.get(parent).await? else {
            bail!(Delegation::NoSuchThread);
        };
        self.delegates(&parent_row).await?;
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
        // The sub-agent's turn follows what the calling turn followed: a turn of wake origin
        // gives its own to it (see `Assistant::origin_of`).
        let origin = self.opened(parent).await?.origin_of(turn).await?;
        // Under the start lock: two starts never both pass. The owner's reserve first, from
        // Fronde's own book: nothing the agent says changes it.
        // The sub-agent spends the account of its parent's owner on its provider, and runs on
        // nothing when that owner has none.
        let account = self.account_or_refuse(&parent_row.owner, &provider)?;
        let counts = self.check_reserve(&account)?;
        let limits = self.limits();
        let kept = self.kept_children(parent).await?;
        if self.running_among(&kept).await >= limits.children_running {
            bail!(Delegation::TooManyRunning {
                limit: limits.children_running
            });
        }
        if kept.len() >= limits.children_kept {
            bail!(Delegation::TooManyKept {
                limit: limits.children_kept
            });
        }
        // The daily counts are the safety net when no usage reading decides.
        if counts {
            let today = self.started_since(&account, Utc::now() - DAY).await?;
            if today >= u64::try_from(limits.children_per_day).unwrap_or(u64::MAX) {
                bail!(Delegation::TooManyToday {
                    limit: limits.children_per_day
                });
            }
            self.check_turns_today(&account).await?;
        }
        Pauses::at(&self.children.pauses.start).await;
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
            self.children
                .handoff
                .remember_retry(parent, client_request_id, row.id, turn);
        }
        let sent = match self.opened(row.id).await {
            Ok(assistant) => assistant
                .try_send_from(child.task, Some(parent_row.title.clone()), origin)
                .await
                .map_err(|refused| match refused {
                    Refused::Busy => anyhow::Error::new(Delegation::Busy),
                    Refused::Archived => anyhow::Error::new(Delegation::Archived),
                }),
            Err(error) => Err(error),
        };
        let seq = match sent {
            Ok(Some(seq)) => seq,
            failed => {
                // A child without its task: archived, so that a retry does not find its
                // `client_request_id` taken and starts anew.
                if let Some(client_request_id) = &child.client_request_id {
                    self.children
                        .handoff
                        .forget_retry(parent, client_request_id);
                }
                if let Err(error) = self.archive_locked(row.id, true, &starting).await {
                    tracing::error!(
                        "cannot archive the sub-agent {} left without its task: {error:#}",
                        row.id
                    );
                }
                match failed {
                    Err(error) => return Err(error),
                    _ => bail!("the task of sub-agent {} could not be logged", row.id),
                }
            }
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
        let same_turn =
            self.children
                .handoff
                .started_in(parent, client_request_id, earlier.id, turn);
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

    /// Sends `text` to `caller`'s sub-agent `target`, as a new turn written by `caller` in its
    /// turn `turn`: the seq of the message. Refused while the sub-agent is in a turn, and once
    /// `turn` is over. A turn of wake origin gives its own to the sub-agent's turn.
    pub async fn send_child(
        &self,
        caller: ThreadId,
        turn: Option<u64>,
        target: ThreadId,
        text: String,
    ) -> anyhow::Result<u64> {
        let row = self.descendant(caller, target).await?;
        // Under the lock an archive takes: the checks hold until the message is logged. The
        // sub-agent itself is idle, or the send is refused below as busy.
        let _starting = self.children.starting.lock().await;
        self.check_delegate(caller).await?;
        let account = self.account_or_refuse(&row.owner, &row.provider)?;
        let counts = self.check_reserve(&account)?;
        let kept = self.kept_children(caller).await?;
        let limit = self.limits().children_running;
        if self.running_among(&kept).await >= limit {
            bail!(Delegation::TooManyRunning { limit });
        }
        if counts {
            self.check_turns_today(&account).await?;
        }
        let origin = self.opened(caller).await?.origin_of(turn).await?;
        let by = self.get(caller).await?.map(|caller| caller.title);
        let assistant = self.opened(row.id).await?;
        match assistant.try_send_from(text, by, origin).await {
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
        let mut rows = self.own_descendants(caller).await?;
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

    /// Every descendant of `id` that is `id`'s owner's, archived ones included: what an agent
    /// of `id` may list or stop.
    async fn own_descendants(&self, id: ThreadId) -> anyhow::Result<Vec<ThreadRow>> {
        let Some(row) = self.get(id).await? else {
            return Ok(Vec::new());
        };
        let mut rows = self.descendants(id).await?;
        rows.retain(|descendant| descendant.owner == row.owner);
        Ok(rows)
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

    /// Stops the running turn of `caller`'s sub-agent `target`, if any, with its pending cards
    /// and the wakes waiting for it, and those of every sub-agent under it; a turn that ended
    /// keeps its result. `reason`, if any, is shown in the stopped thread. The turns stopped so
    /// wake nobody: the caller asked for it. Whether `target` was in a turn.
    pub async fn cancel_child(
        &self,
        caller: ThreadId,
        target: ThreadId,
        reason: Option<&str>,
    ) -> anyhow::Result<bool> {
        let row = self.descendant(caller, target).await?;
        let starting = self.children.starting.lock().await;
        let assistant = self.open.lock().await.get(&row.id).cloned();
        if let Some(reason) = reason
            && let Some(assistant) = assistant
            && matches!(
                assistant.status(),
                ThreadStatus::Running | ThreadStatus::Queued
            )
        {
            assistant
                .append(Event::Notice {
                    text: format!("Arrêté par la conversation parente : {reason}"),
                })
                .await;
        }
        let stopped = self.stop_tree(row.id, false, &starting).await?;
        tracing::info!("thread {caller} stopped its sub-agent {target} and those under it");
        Ok(stopped)
    }

    /// Stops the running turn of thread `root` and of every thread under it, deepest first,
    /// with their pending cards and the wakes waiting for them; none of those turns wakes its
    /// parent. Deepest first, so that a result coming up meanwhile finds its parent's turn
    /// still to be stopped. With `retire`, as an archive does, each agent is also let go, its
    /// token revoked, the late answers waiting for its turn dropped, and the secrets given "for
    /// this conversation" in each thread forgotten.
    /// Whether `root` was in a turn. `starting` is held throughout: no sub-agent starts, and
    /// none is sent a message, while the tree stops.
    pub(super) async fn stop_tree(
        &self,
        root: ThreadId,
        retire: bool,
        _starting: &tokio::sync::MutexGuard<'_, ()>,
    ) -> anyhow::Result<bool> {
        let mut tree = vec![(root, super::topic_of(root))];
        // An archive stops everything under the thread; an agent's cancel stops only what is
        // its owner's.
        let under = if retire {
            self.descendants(root).await?
        } else {
            self.own_descendants(root).await?
        };
        tree.extend(under.into_iter().map(|row| (row.id, row.topic)));
        let mut stopped = false;
        // `descendants` goes level by level: reversed, the deepest come first.
        for (thread, topic) in tree.into_iter().rev() {
            if retire {
                self.forget_secrets(thread, &topic);
            }
            let Some(assistant) = self.open.lock().await.get(&thread).cloned() else {
                // A thread not open runs no turn and no agent.
                continue;
            };
            let was_running = if retire {
                // Before the turn is stopped: its end may never be logged.
                assistant.drop_waiting_late().await;
                assistant.retire().await
            } else {
                assistant.cancel_quietly().await
            };
            if was_running {
                stopped |= thread == root;
                tracing::info!("the turn of thread {thread} was stopped with its tree");
            }
        }
        Ok(stopped)
    }

    /// Forgets the secrets given in thread `thread`'s cards "for this conversation".
    fn forget_secrets(&self, thread: ThreadId, topic: &str) {
        if let Some(requests) = self.requests.get().and_then(std::sync::Weak::upgrade)
            && let Some(secrets) = requests.secrets()
        {
            let forgotten = secrets.ephemeral().forget_topic(topic);
            if forgotten > 0 {
                tracing::info!(
                    "thread {thread} is archived or under an archived thread: {forgotten} \
                     secret(s) given for it forgotten"
                );
            }
        }
    }

    /// Waits for the turn of `caller`'s sub-agent `target` whose message is `seq` to end, at
    /// most `timeout`, while `cancelled` is not and while `caller`'s turn is still `caller_turn`
    /// with its agent running. Ends early when a card opens in that turn, for the user to answer,
    /// or when the sub-agent waits for an agent slot. The result comes through the [`Handoff`],
    /// which marks it delivered ([`Threads::delivered`]); a turn whose end was decided before
    /// this call registered is read from the stream, and is not marked, unless its result woke
    /// the caller: then it is [`ChildEnd::Woken`], so that it never reaches the caller twice.
    pub async fn wait_child(
        &self,
        caller: ThreadId,
        caller_turn: Option<u64>,
        target: ThreadId,
        seq: u64,
        timeout: Duration,
        cancelled: &CancellationToken,
    ) -> anyhow::Result<ChildEnd> {
        let row = self.descendant(caller, target).await?;
        let assistant = self.opened(row.id).await?;
        let parent = self.opened(caller).await?;
        // Listening before registering and reading: an entry written in between is not missed.
        let mut live = assistant.log().subscribe();
        let mut parent_live = parent.log().subscribe();
        let handoff = &self.children.handoff;
        let mut registration = match handoff.register(row.id, seq) {
            Ok(registration) => registration,
            // Nobody took it: it woke the sub-agent's parent, maybe this caller.
            Err(AlreadyEnded { taken: Some(false) }) if row.parent == Some(caller) => {
                return Ok(ChildEnd::Woken);
            }
            Err(AlreadyEnded { .. }) => return Ok(ended_in_log(&assistant, seq)),
        };
        let deadline = tokio::time::Instant::now() + timeout;
        let mut seen = seq;
        let mut open_cards = HashSet::new();
        let stopped = loop {
            for entry in assistant.log().since(seen) {
                seen = entry.seq;
                match entry.event {
                    Event::Request { id, .. } => {
                        open_cards.insert(id);
                    }
                    Event::RequestClosed { id, .. } => {
                        open_cards.remove(&id);
                    }
                    // The turn's end comes through the handoff, once decided.
                    _ => {}
                }
            }
            if !open_cards.is_empty() {
                break ChildEnd::WaitingForUser;
            }
            if assistant.status() == ThreadStatus::Queued {
                break ChildEnd::Queued;
            }
            if parent.current_turn().await != caller_turn || !parent.agent_running() {
                break ChildEnd::GaveUp;
            }
            tokio::select! {
                biased;
                () = cancelled.cancelled() => break ChildEnd::GaveUp,
                () = tokio::time::sleep_until(deadline) => break ChildEnd::TimedOut,
                result = &mut registration.result => match result {
                    Ok(TurnResult { outcome, answer }) => {
                        handoff.returned(row.id, seq);
                        return Ok(ChildEnd::Ended { outcome, answer });
                    }
                    Err(_) => bail!("the end of sub-agent {target}'s turn was lost"),
                },
                received = live.recv() => {
                    if let Err(RecvError::Closed) = received {
                        break ChildEnd::GaveUp;
                    }
                }
                received = parent_live.recv() => {
                    if let Err(RecvError::Closed) = received {
                        break ChildEnd::GaveUp;
                    }
                }
            }
        };
        Pauses::at(&self.children.pauses.withdraw).await;
        // Handed over while this call was stopping: it takes the result, marked delivered;
        // unless the call was given up, its caller gone, when it goes to the parent as a wake.
        Ok(match handoff.withdraw(row.id, seq, registration) {
            Some(TurnResult { outcome, answer }) if stopped == ChildEnd::GaveUp => {
                if handoff.give_back(row.id, seq) {
                    tracing::info!(
                        "a wait for sub-agent {target} was given up as its result came: it \
                         wakes the parent instead"
                    );
                    self.child_ended(row.id, outcome, &answer).await?;
                }
                stopped
            }
            Some(TurnResult { outcome, answer }) => {
                handoff.returned(row.id, seq);
                ChildEnd::Ended { outcome, answer }
            }
            None => stopped,
        })
    }
}

/// The end of turn `seq` of `assistant`, read from its stream: logged before it was decided.
fn ended_in_log(assistant: &Assistant, seq: u64) -> ChildEnd {
    let turn: Vec<Entry> = assistant.log().since(seq.saturating_sub(1));
    let end = turn
        .iter()
        .position(|entry| matches!(entry.event, Event::TurnEnded { .. }));
    match end {
        Some(end) => {
            let Event::TurnEnded { outcome } = turn[end].event else {
                unreachable!("found as a turn's end");
            };
            ChildEnd::Ended {
                outcome,
                answer: final_answer(&turn[..end]),
            }
        }
        None => ChildEnd::GaveUp,
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

    fn result(answer: &str) -> TurnResult {
        TurnResult {
            outcome: TurnOutcome::Done,
            answer: answer.into(),
        }
    }

    #[test]
    fn a_registered_wait_gets_the_result_once_delivered() {
        let handoff = Handoff::default();
        let (child, other) = (ThreadId(2), ThreadId(3));
        let mut waiting = handoff.register(child, 5).unwrap();
        assert!(
            !handoff.turn_ended(other, 5, result("autre")),
            "another thread's turn"
        );
        assert!(handoff.turn_ended(child, 5, result("Fini.")));
        assert_eq!(waiting.result.try_recv().unwrap(), result("Fini."));
        assert_eq!(handoff.delivered(child), Some(5));
        // A turn whose end was decided is read from the stream: registering is refused.
        assert!(matches!(
            handoff.register(child, 5),
            Err(AlreadyEnded { taken: Some(true) })
        ));
        // An older seq is refused too, its fate not known: no turn of it ended here.
        assert!(matches!(
            handoff.register(child, 3),
            Err(AlreadyEnded { taken: None })
        ));
        assert!(handoff.register(child, 9).is_ok());
    }

    #[test]
    fn a_wait_that_stops_takes_a_result_handed_meanwhile() {
        let handoff = Handoff::default();
        let child = ThreadId(2);
        // Withdrawn before the end: nobody takes it, nothing is delivered.
        let early = handoff.register(child, 5).unwrap();
        assert_eq!(handoff.withdraw(child, 5, early), None);
        assert!(!handoff.turn_ended(child, 5, result("Fini.")));
        assert_eq!(handoff.delivered(child), None);
        // Handed over while it was stopping: it gets it.
        let late = handoff.register(child, 7).unwrap();
        assert!(handoff.turn_ended(child, 7, result("Encore.")));
        assert_eq!(handoff.withdraw(child, 7, late), Some(result("Encore.")));
        // Two waits for the same turn both get it.
        let (first, second) = (
            handoff.register(child, 9).unwrap(),
            handoff.register(child, 9).unwrap(),
        );
        assert!(handoff.turn_ended(child, 9, result("Deux.")));
        assert_eq!(handoff.withdraw(child, 9, first), Some(result("Deux.")));
        assert_eq!(handoff.withdraw(child, 9, second), Some(result("Deux.")));
        // A later turn nobody waited for clears the mark.
        assert!(!handoff.turn_ended(child, 11, result("Seul.")));
        assert_eq!(handoff.delivered(child), None);
    }

    /// The race between a wait and the end of the turn it waits for, forced both ways: the
    /// result goes to the wait or to a wake, never to both.
    #[test]
    fn a_result_goes_to_the_wait_or_to_the_wake_never_both() {
        let handoff = Handoff::default();
        let child = ThreadId(2);
        // The wait registers first: it takes the result, so the end wakes nobody.
        let first = handoff.register(child, 5).unwrap();
        assert_eq!(handoff.waiting(child), 1);
        assert!(handoff.turn_ended(child, 5, result("Pris.")), "no wake");
        assert_eq!(handoff.withdraw(child, 5, first), Some(result("Pris.")));
        assert_eq!(handoff.waiting(child), 0);
        // A retry of that wait reads it again: it is the same caller's.
        assert_eq!(
            handoff.register(child, 5).err(),
            Some(AlreadyEnded { taken: Some(true) })
        );
        // The end comes first: nobody took it, so it wakes the parent, and a wait registering
        // after it learns so instead of getting the result again.
        assert!(!handoff.turn_ended(child, 7, result("Réveil.")), "a wake");
        assert_eq!(
            handoff.register(child, 7).err(),
            Some(AlreadyEnded { taken: Some(false) })
        );
        assert_eq!(handoff.waiting(child), 0);
    }

    /// A turn stopped with its tree is settled: no call took it, yet it wakes nobody, and a
    /// call registering afterwards reads it from the stream rather than being told it woke.
    #[test]
    fn a_settled_end_wakes_nobody_and_is_read_from_the_stream() {
        let handoff = Handoff::default();
        let child = ThreadId(2);
        assert!(handoff.turn_ended_with(child, 5, result("Arrêté."), true));
        assert_eq!(handoff.delivered(child), Some(5));
        assert_eq!(
            handoff.register(child, 5).err(),
            Some(AlreadyEnded { taken: Some(true) })
        );
        // A waiting call still gets it.
        let waiting = handoff.register(child, 7).unwrap();
        assert!(handoff.turn_ended_with(child, 7, result("Encore."), true));
        assert_eq!(handoff.withdraw(child, 7, waiting), Some(result("Encore.")));
    }

    /// Each ended turn keeps its own fate: a wait for an older turn that a call took is not
    /// told it woke the parent because a later turn did.
    #[test]
    fn an_older_turns_fate_is_its_own() {
        let handoff = Handoff::default();
        let child = ThreadId(2);
        let taken = handoff.register(child, 5).unwrap();
        assert!(handoff.turn_ended(child, 5, result("Pris.")));
        assert!(!handoff.turn_ended(child, 7, result("Réveil.")));
        assert_eq!(handoff.withdraw(child, 5, taken), Some(result("Pris.")));
        assert_eq!(
            handoff.register(child, 5).err(),
            Some(AlreadyEnded { taken: Some(true) })
        );
        assert_eq!(
            handoff.register(child, 7).err(),
            Some(AlreadyEnded { taken: Some(false) })
        );
        // Only the last few are kept; an older one is not known, never said to have woken.
        for seq in 0..(ENDS_KEPT as u64) {
            handoff.turn_ended(child, 100 + seq, result(""));
        }
        assert_eq!(
            handoff.register(child, 5).err(),
            Some(AlreadyEnded { taken: None })
        );
    }

    /// A result handed to a call whose caller is gone is given back, once: it wakes the parent.
    #[test]
    fn a_result_given_back_is_no_longer_taken() {
        let handoff = Handoff::default();
        let child = ThreadId(2);
        let waiting = handoff.register(child, 5).unwrap();
        assert!(handoff.turn_ended(child, 5, result("Pris.")));
        assert_eq!(handoff.withdraw(child, 5, waiting), Some(result("Pris.")));
        assert!(handoff.give_back(child, 5));
        assert!(!handoff.give_back(child, 5), "once");
        assert_eq!(handoff.delivered(child), None);
        assert_eq!(
            handoff.register(child, 5).err(),
            Some(AlreadyEnded { taken: Some(false) })
        );
    }

    /// Two calls handed the same result: once one returns it, it stays taken, whichever gives
    /// it back; only when every call handed it gives it back does it wake the parent, once.
    #[test]
    fn a_result_returned_by_one_of_two_waits_is_never_given_back() {
        let handoff = Handoff::default();
        let child = ThreadId(2);
        // The first gives it back while the second may still return it: no wake yet; the
        // second returns it.
        let (first, second) = (
            handoff.register(child, 5).unwrap(),
            handoff.register(child, 5).unwrap(),
        );
        assert!(handoff.turn_ended(child, 5, result("Deux.")));
        assert_eq!(handoff.withdraw(child, 5, first), Some(result("Deux.")));
        assert!(!handoff.give_back(child, 5), "the other wait may return it");
        assert_eq!(handoff.withdraw(child, 5, second), Some(result("Deux.")));
        handoff.returned(child, 5);
        assert_eq!(handoff.delivered(child), Some(5));
        assert_eq!(
            handoff.register(child, 5).err(),
            Some(AlreadyEnded { taken: Some(true) })
        );
        // The other order: returned first, then given back.
        let (first, second) = (
            handoff.register(child, 7).unwrap(),
            handoff.register(child, 7).unwrap(),
        );
        assert!(handoff.turn_ended(child, 7, result("Trois.")));
        assert_eq!(handoff.withdraw(child, 7, first), Some(result("Trois.")));
        handoff.returned(child, 7);
        assert_eq!(handoff.withdraw(child, 7, second), Some(result("Trois.")));
        assert!(!handoff.give_back(child, 7), "returned by the first");
        assert_eq!(handoff.delivered(child), Some(7));
        // Both give it back: the last one wakes the parent, once.
        let (first, second) = (
            handoff.register(child, 9).unwrap(),
            handoff.register(child, 9).unwrap(),
        );
        assert!(handoff.turn_ended(child, 9, result("Quatre.")));
        assert_eq!(handoff.withdraw(child, 9, first), Some(result("Quatre.")));
        assert_eq!(handoff.withdraw(child, 9, second), Some(result("Quatre.")));
        assert!(!handoff.give_back(child, 9));
        assert!(handoff.give_back(child, 9), "the last one gives it back");
        assert!(!handoff.give_back(child, 9), "once");
        assert_eq!(
            handoff.register(child, 9).err(),
            Some(AlreadyEnded { taken: Some(false) })
        );
    }

    #[test]
    fn retries_go_with_the_turn_that_started_them() {
        let handoff = Handoff::default();
        let (parent, other, child) = (ThreadId(1), ThreadId(5), ThreadId(2));
        handoff.remember_retry(parent, "a", child, 4);
        handoff.remember_retry(other, "a", ThreadId(6), 4);
        assert!(handoff.started_in(parent, "a", child, 4));
        assert!(!handoff.started_in(parent, "a", child, 5), "another turn");
        handoff.turn_ended(parent, 4, result(""));
        assert!(!handoff.started_in(parent, "a", child, 4));
        assert!(
            handoff.started_in(other, "a", ThreadId(6), 4),
            "another parent's"
        );
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
