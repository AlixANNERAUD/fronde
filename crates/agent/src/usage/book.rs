//! The latest usage reading per subscription account, kept in `fronde.db`'s `kv` table (scope
//! `provider/<account>`, key `usage`, the JSON of a [`ProviderUsage`], no expiry), beside the
//! models cache. Today an account's id is its provider's (`claude`), so the scope is the one
//! readings were first stored under.

use chrono::{DateTime, Utc};
use fronde_protocol::usage::{LimitStatus, ProviderUsage, Window, WindowKind};
use sea_orm::{ConnectionTrait, DatabaseConnection};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tokio::sync::{mpsc, oneshot, watch};

const KEY: &str = "usage";

/// The most windows of kinds Fronde has no name for ([`WindowKind::Other`]) an account keeps.
pub const MAX_OTHER_WINDOWS: usize = 4;

fn scope(account: &str) -> String {
    format!("provider/{account}")
}

/// The latest windows read for each subscription account, from every agent process, keyed by
/// the account's id ([`Threads::account`](crate::threads::Threads::account)). A window replaces
/// the one of its kind only when it is newer, and within one window its use only goes up (see
/// [`UsageBook::record`]). Each change is written to
/// the database by a task of the book's own, so that a slow write never holds an agent's
/// messages; a failed write is logged, and the reading stays in memory.
pub struct UsageBook {
    readings: Mutex<HashMap<String, ProviderUsage>>,
    changes: watch::Sender<u64>,
    /// To the task that writes the readings; `None` for a book kept in memory only.
    saves: Option<mpsc::UnboundedSender<Save>>,
}

enum Save {
    Usage(ProviderUsage),
    /// Answered once every reading sent before it is written.
    Flush(oneshot::Sender<()>),
}

impl fmt::Debug for UsageBook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UsageBook")
            .field("providers", &lock(&self.readings).len())
            .field("saved", &self.saves.is_some())
            .finish()
    }
}

impl UsageBook {
    /// The book of `db`: the readings stored for each of `accounts`, and a task, on the current
    /// Tokio runtime, that writes the changes back. A reading that cannot be read is logged and
    /// left out: the book then starts without it.
    pub async fn load(db: DatabaseConnection, accounts: &[String]) -> Arc<Self> {
        let mut readings = HashMap::new();
        for account in accounts {
            match read_usage(&db, account).await {
                Ok(Some(usage)) => {
                    readings.insert(account.clone(), usage);
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!("ignored the stored usage of {account}: {error:#}");
                }
            }
        }
        let (saves, receiver) = mpsc::unbounded_channel();
        tokio::spawn(save_loop(db, receiver));
        Arc::new(Self {
            readings: Mutex::new(readings),
            changes: watch::Sender::new(0),
            saves: Some(saves),
        })
    }

    /// A book that keeps its readings in memory only.
    pub fn in_memory() -> Arc<Self> {
        Arc::new(Self {
            readings: Mutex::default(),
            changes: watch::Sender::new(0),
            saves: None,
        })
    }

    /// Records the windows of one reading of `account`; true when it changed the book, which is
    /// then saved and moves [`UsageBook::changes`].
    ///
    /// Per kind, a window replaces the one known when its `resets_at` is later: the window
    /// started again. With the same `resets_at`, the two say how one window stood at two times,
    /// and use only goes up until the reset: the greater use and the worse status are kept, so
    /// that a reading held back by another agent process never lifts a pause. A window without
    /// `resets_at` replaces only one without, received earlier. So readings from several agent
    /// processes, arriving in any order, end on the newest window. A window that says the same
    /// as the one known changes nothing, and keeps the time it was first read.
    ///
    /// A full reading (both the 5-hour and the weekly window) drops the windows of kinds Fronde
    /// has no name for that it does not list, and at most [`MAX_OTHER_WINDOWS`] of those are
    /// kept, the latest read.
    pub fn record(&self, account: &str, windows: Vec<Window>) -> bool {
        let saved = {
            let mut readings = lock(&self.readings);
            let mut changed = false;
            let mut latest: Option<DateTime<Utc>> = None;
            let usage = readings
                .entry(account.to_owned())
                .or_insert_with(|| ProviderUsage {
                    provider: account.to_owned(),
                    known_since: DateTime::<Utc>::MIN_UTC,
                    windows: Vec::new(),
                });
            let full = [WindowKind::Session, WindowKind::Weekly]
                .iter()
                .all(|kind| windows.iter().any(|window| &window.kind == kind));
            if full {
                let before = usage.windows.len();
                usage.windows.retain(|known| {
                    !matches!(known.kind, WindowKind::Other(_))
                        || windows.iter().any(|window| window.kind == known.kind)
                });
                changed |= usage.windows.len() != before;
            }
            for window in windows {
                let as_of = window.as_of;
                if merge(&mut usage.windows, window) {
                    changed = true;
                    latest = latest.max(Some(as_of));
                }
            }
            changed |= cap_others(&mut usage.windows);
            if let Some(latest) = latest {
                usage.known_since = usage.known_since.max(latest);
            }
            if usage.windows.is_empty() {
                readings.remove(account);
                return false;
            }
            if !changed {
                return false;
            }
            usage.clone()
        };
        self.changes
            .send_modify(|count| *count = count.wrapping_add(1));
        if let Some(saves) = &self.saves
            && saves.send(Save::Usage(saved)).is_err()
        {
            tracing::warn!("cannot save the usage of {account}: its writer has stopped");
        }
        true
    }

    /// The latest windows of `account`, as read (see [`ProviderUsage::now`] for how they stand
    /// now), and since when they are known; `None` before its first reading.
    pub fn get(&self, account: &str) -> Option<ProviderUsage> {
        lock(&self.readings).get(account).cloned()
    }

    /// Every account's latest windows, by account id.
    pub fn all(&self) -> Vec<ProviderUsage> {
        let mut all: Vec<_> = lock(&self.readings).values().cloned().collect();
        all.sort_by(|a, b| a.provider.cmp(&b.provider));
        all
    }

    /// Moves at each change of the book.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// Waits until every change recorded so far is written (or failed to be).
    pub async fn flush(&self) {
        let Some(saves) = &self.saves else {
            return;
        };
        let (done, written) = oneshot::channel();
        if saves.send(Save::Flush(done)).is_ok() {
            let _ = written.await;
        }
    }
}

/// Puts `window` in `windows` when it is new, newer than the one of its kind, or says that
/// window is used more; true if it changed them.
fn merge(windows: &mut Vec<Window>, window: Window) -> bool {
    let Some(known) = windows.iter_mut().find(|known| known.kind == window.kind) else {
        windows.push(window);
        return true;
    };
    let merged = match (known.resets_at, window.resets_at) {
        (Some(was), Some(is)) if is > was => window,
        (Some(was), Some(is)) if is < was => return false,
        // The same window at two times: it is used at least as much as either says.
        (Some(_), Some(_)) => Window {
            used: match (known.used, window.used) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            },
            status: worse(known.status, window.status),
            as_of: known.as_of.max(window.as_of),
            ..window
        },
        (None, Some(_)) => window,
        (Some(_), None) => return false,
        (None, None) if window.as_of >= known.as_of => window,
        (None, None) => return false,
    };
    if known.same_reading(&merged) {
        return false;
    }
    *known = merged;
    true
}

/// The status nearer the limit: rejected, then warning, then allowed.
fn worse(a: Option<LimitStatus>, b: Option<LimitStatus>) -> Option<LimitStatus> {
    let rank = |status: Option<LimitStatus>| match status {
        None => 0,
        Some(LimitStatus::Allowed) => 1,
        Some(LimitStatus::Warning) => 2,
        Some(LimitStatus::Rejected) => 3,
    };
    if rank(b) > rank(a) { b } else { a }
}

/// Keeps at most [`MAX_OTHER_WINDOWS`] windows of kinds Fronde has no name for, the latest read;
/// true if it dropped some.
fn cap_others(windows: &mut Vec<Window>) -> bool {
    let mut others: Vec<(DateTime<Utc>, usize)> = windows
        .iter()
        .enumerate()
        .filter(|(_, window)| matches!(window.kind, WindowKind::Other(_)))
        .map(|(index, window)| (window.as_of, index))
        .collect();
    if others.len() <= MAX_OTHER_WINDOWS {
        return false;
    }
    // The latest read first; of those read at once, the first listed.
    others.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let dropped: std::collections::HashSet<usize> = others[MAX_OTHER_WINDOWS..]
        .iter()
        .map(|(_, index)| *index)
        .collect();
    let mut index = 0;
    windows.retain(|_| {
        let keep = !dropped.contains(&index);
        index += 1;
        keep
    });
    true
}

/// The reading stored for `account` in `db`; `None` when there is none, an error when it does
/// not read or names another account. `fronde check` reads it from a read-only connection.
pub async fn read_usage<C: ConnectionTrait>(
    db: &C,
    account: &str,
) -> anyhow::Result<Option<ProviderUsage>> {
    let Some(value) = fronde_db::kv::get(db, &scope(account), KEY).await? else {
        return Ok(None);
    };
    let usage = serde_json::from_slice::<ProviderUsage>(&value)
        .map_err(|error| anyhow::anyhow!("the stored usage does not read: {error}"))?;
    if usage.provider != account {
        anyhow::bail!("the stored usage names another account");
    }
    Ok(Some(usage))
}

/// Writes the readings sent, the latest of each provider when several wait, until the book is
/// dropped.
async fn save_loop(db: DatabaseConnection, mut saves: mpsc::UnboundedReceiver<Save>) {
    while let Some(first) = saves.recv().await {
        let mut latest: HashMap<String, ProviderUsage> = HashMap::new();
        let mut flushes = Vec::new();
        let mut take = |save| match save {
            Save::Usage(usage) => {
                latest.insert(usage.provider.clone(), usage);
            }
            Save::Flush(done) => flushes.push(done),
        };
        take(first);
        while let Ok(save) = saves.try_recv() {
            take(save);
        }
        for usage in latest.into_values() {
            if let Err(error) = save(&db, &usage).await {
                tracing::warn!("cannot save the usage of {}: {error:#}", usage.provider);
            }
        }
        for done in flushes {
            let _ = done.send(());
        }
    }
}

async fn save(db: &DatabaseConnection, usage: &ProviderUsage) -> anyhow::Result<()> {
    let value = serde_json::to_vec(usage)?;
    fronde_db::kv::set(db, &scope(&usage.provider), KEY, value, None).await?;
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fronde_protocol::usage::{LimitStatus, WindowKind};

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).unwrap()
    }

    fn window(kind: WindowKind, used: f64, resets_at: Option<i64>, as_of: i64) -> Window {
        Window {
            kind,
            used: Some(used),
            status: Some(LimitStatus::Allowed),
            resets_at: resets_at.map(at),
            as_of: at(as_of),
            reset_since_reading: false,
        }
    }

    #[test]
    fn merge_follows_the_reset_then_the_receipt() {
        let book = UsageBook::in_memory();
        let mut changes = book.changes();
        assert!(book.record(
            "claude",
            vec![window(WindowKind::Session, 0.2, Some(100), 10)]
        ));
        assert!(changes.has_changed().unwrap());
        changes.mark_unchanged();
        // Received later, same reset: replaces.
        assert!(book.record(
            "claude",
            vec![window(WindowKind::Session, 0.3, Some(100), 20)]
        ));
        // Received later, older reset: an earlier window from another process, ignored.
        assert!(!book.record(
            "claude",
            vec![window(WindowKind::Session, 0.9, Some(50), 30)]
        ));
        // No reset replaces only one without.
        assert!(!book.record("claude", vec![window(WindowKind::Session, 0.5, None, 40)]));
        assert!(book.record("claude", vec![window(WindowKind::Weekly, 0.1, None, 40)]));
        assert!(book.record("claude", vec![window(WindowKind::Weekly, 0.2, None, 50)]));
        assert!(book.record(
            "claude",
            vec![window(WindowKind::Weekly, 0.3, Some(900), 60)]
        ));
        let usage = book.get("claude").unwrap();
        assert_eq!(usage.known_since, at(60));
        assert_eq!(
            usage.windows,
            [
                window(WindowKind::Session, 0.3, Some(100), 20),
                window(WindowKind::Weekly, 0.3, Some(900), 60),
            ]
        );
        assert_eq!(book.get("codex"), None);
        assert_eq!(book.all(), [usage]);
        // An empty reading leaves no row.
        assert!(!book.record("codex", Vec::new()));
        assert_eq!(book.get("codex"), None);
    }

    /// Readings of one window arriving out of order never lower it: until its reset, the
    /// greater use and the worse status stay, so that a reading another agent process held
    /// back cannot lift a pause.
    #[test]
    fn a_window_only_goes_down_at_its_reset() {
        let book = UsageBook::in_memory();
        assert!(book.record(
            "claude",
            vec![window(WindowKind::Session, 0.75, Some(100), 20)]
        ));
        // An older reading of the same window, received later.
        assert!(!book.record(
            "claude",
            vec![window(WindowKind::Session, 0.40, Some(100), 30)]
        ));
        let session = |book: &UsageBook| book.get("claude").unwrap().windows[0].clone();
        assert_eq!(session(&book).used, Some(0.75));
        // A rejected status stays, whatever a later reading of that window says.
        let rejected = Window {
            status: Some(LimitStatus::Rejected),
            ..window(WindowKind::Session, 0.75, Some(100), 40)
        };
        assert!(book.record("claude", vec![rejected]));
        assert!(!book.record(
            "claude",
            vec![window(WindowKind::Session, 0.2, Some(100), 50)]
        ));
        assert_eq!(session(&book).status, Some(LimitStatus::Rejected));
        assert_eq!(session(&book).used, Some(0.75));
        // More use is taken.
        assert!(book.record(
            "claude",
            vec![window(WindowKind::Session, 0.8, Some(100), 60)]
        ));
        assert_eq!(session(&book).used, Some(0.8));
        assert_eq!(session(&book).status, Some(LimitStatus::Rejected));
        // The next window, after the reset, starts low.
        assert!(book.record(
            "claude",
            vec![window(WindowKind::Session, 0.05, Some(200), 70)]
        ));
        assert_eq!(session(&book).used, Some(0.05));
        assert_eq!(session(&book).status, Some(LimitStatus::Allowed));
    }

    fn other(id: &str, as_of: i64) -> Window {
        window(WindowKind::Other(id.into()), 0.1, Some(1_000), as_of)
    }

    /// At most four windows of unknown kinds are kept, the latest read.
    #[test]
    fn other_windows_are_capped() {
        let book = UsageBook::in_memory();
        let reading: Vec<Window> = (0..6).map(|n| other(&format!("k{n}"), 10 + n)).collect();
        assert!(book.record("claude", reading));
        let kinds: Vec<String> = book
            .get("claude")
            .unwrap()
            .windows
            .iter()
            .map(|window| window.kind.to_string())
            .collect();
        assert_eq!(kinds, ["other:k2", "other:k3", "other:k4", "other:k5"]);
        // A seventh pushes out the oldest.
        assert!(book.record("claude", vec![other("k6", 30)]));
        let kinds: Vec<String> = book
            .get("claude")
            .unwrap()
            .windows
            .iter()
            .map(|window| window.kind.to_string())
            .collect();
        assert_eq!(kinds, ["other:k3", "other:k4", "other:k5", "other:k6"]);
    }

    /// A full reading (both guarded windows) drops the unknown kinds it no longer lists; a
    /// partial one keeps them.
    #[test]
    fn a_full_reading_drops_unknown_kinds_it_leaves_out() {
        let book = UsageBook::in_memory();
        assert!(book.record(
            "claude",
            vec![
                window(WindowKind::Session, 0.1, Some(100), 10),
                window(WindowKind::Weekly, 0.1, Some(900), 10),
                other("gone", 10),
                other("kept", 10),
            ]
        ));
        // Partial: nothing dropped.
        assert!(book.record(
            "claude",
            vec![window(WindowKind::Session, 0.2, Some(100), 20)]
        ));
        assert_eq!(book.get("claude").unwrap().windows.len(), 4);
        // Full, without "gone": dropped, even though nothing else changed.
        assert!(book.record(
            "claude",
            vec![
                window(WindowKind::Session, 0.2, Some(100), 30),
                window(WindowKind::Weekly, 0.1, Some(900), 30),
                other("kept", 10),
            ]
        ));
        let kinds: Vec<String> = book
            .get("claude")
            .unwrap()
            .windows
            .iter()
            .map(|window| window.kind.to_string())
            .collect();
        assert_eq!(kinds, ["session", "weekly", "other:kept"]);
    }
}
