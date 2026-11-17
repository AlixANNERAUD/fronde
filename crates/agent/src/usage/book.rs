//! The latest usage reading per provider, in memory.

use chrono::{DateTime, Utc};
use fronde_protocol::usage::{ProviderUsage, Window};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tokio::sync::watch;

/// The latest windows read for each provider, from every agent process: a window replaces the
/// one of its kind only when it is newer (see [`UsageBook::record`]).
pub struct UsageBook {
    readings: Mutex<HashMap<String, ProviderUsage>>,
    changes: watch::Sender<u64>,
}

impl fmt::Debug for UsageBook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UsageBook")
            .field("providers", &lock(&self.readings).len())
            .finish()
    }
}

impl UsageBook {
    /// A book that keeps its readings in memory only.
    pub fn in_memory() -> Arc<Self> {
        Arc::new(Self {
            readings: Mutex::default(),
            changes: watch::Sender::new(0),
        })
    }

    /// Records the windows of one reading of `provider`; true when it changed the book, which
    /// then moves [`UsageBook::changes`].
    ///
    /// Per kind, a window replaces the one known when its `resets_at` is later, or the same and
    /// it was received later; one without `resets_at` replaces only one without. So readings
    /// from several agent processes, arriving in any order, end on the newest window. A window
    /// that says the same as the one known (its use, status and reset) changes nothing, and
    /// keeps the time it was first read.
    pub fn record(&self, provider: &str, windows: Vec<Window>) -> bool {
        {
            let mut readings = lock(&self.readings);
            let mut changed = false;
            let mut latest: Option<DateTime<Utc>> = None;
            let usage = readings
                .entry(provider.to_owned())
                .or_insert_with(|| ProviderUsage {
                    provider: provider.to_owned(),
                    known_since: DateTime::<Utc>::MIN_UTC,
                    windows: Vec::new(),
                });
            for window in windows {
                let as_of = window.as_of;
                if merge(&mut usage.windows, window) {
                    changed = true;
                    latest = latest.max(Some(as_of));
                }
            }
            if let Some(latest) = latest {
                usage.known_since = usage.known_since.max(latest);
            }
            if usage.windows.is_empty() {
                readings.remove(provider);
                return false;
            }
            if !changed {
                return false;
            }
        }
        self.changes
            .send_modify(|count| *count = count.wrapping_add(1));
        true
    }

    /// The latest windows of `provider`, as read (see [`ProviderUsage::now`] for how they stand
    /// now), and since when they are known; `None` before its first reading.
    pub fn get(&self, provider: &str) -> Option<ProviderUsage> {
        lock(&self.readings).get(provider).cloned()
    }

    /// Every provider's latest windows, by provider id.
    pub fn all(&self) -> Vec<ProviderUsage> {
        let mut all: Vec<_> = lock(&self.readings).values().cloned().collect();
        all.sort_by(|a, b| a.provider.cmp(&b.provider));
        all
    }

    /// Moves at each change of the book.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }
}

/// Puts `window` in `windows` when it is new or newer than the one of its kind; true if it was.
fn merge(windows: &mut Vec<Window>, window: Window) -> bool {
    let Some(known) = windows.iter_mut().find(|known| known.kind == window.kind) else {
        windows.push(window);
        return true;
    };
    let newer = match (known.resets_at, window.resets_at) {
        (Some(was), Some(is)) => is > was || (is == was && window.as_of >= known.as_of),
        (None, Some(_)) => true,
        (Some(_), None) => false,
        (None, None) => window.as_of >= known.as_of,
    };
    if !newer || known.same_reading(&window) {
        return false;
    }
    *known = window;
    true
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
}
