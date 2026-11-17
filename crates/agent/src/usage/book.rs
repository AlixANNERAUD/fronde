//! The latest usage reading per provider, kept in `fronde.db`'s `kv` table (scope
//! `provider/<id>`, key `usage`, the JSON of a [`ProviderUsage`], no expiry), beside the models
//! cache.

use crate::provider::Provider;
use chrono::{DateTime, Utc};
use fronde_protocol::usage::{ProviderUsage, Window};
use sea_orm::{ConnectionTrait, DatabaseConnection};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tokio::sync::{mpsc, oneshot, watch};

const KEY: &str = "usage";

fn scope(provider: &str) -> String {
    format!("provider/{provider}")
}

/// The latest windows read for each provider, from every agent process: a window replaces the
/// one of its kind only when it is newer (see [`UsageBook::record`]). Each change is written to
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
    /// The book of `db`: the readings stored for each configured provider, and a task, on the
    /// current Tokio runtime, that writes the changes back. A reading that cannot be read is
    /// logged and left out: the book then starts without it.
    pub async fn load(db: DatabaseConnection) -> Arc<Self> {
        let mut readings = HashMap::new();
        for provider in Provider::ALL {
            match read_usage(&db, provider.id).await {
                Ok(Some(usage)) => {
                    readings.insert(provider.id.to_owned(), usage);
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!("cannot read the usage of {}: {error:#}", provider.id);
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

    /// Records the windows of one reading of `provider`; true when it changed the book, which is
    /// then saved and moves [`UsageBook::changes`].
    ///
    /// Per kind, a window replaces the one known when its `resets_at` is later, or the same and
    /// it was received later; one without `resets_at` replaces only one without. So readings
    /// from several agent processes, arriving in any order, end on the newest window. A window
    /// that says the same as the one known (its use, status and reset) changes nothing, and
    /// keeps the time it was first read.
    pub fn record(&self, provider: &str, windows: Vec<Window>) -> bool {
        let saved = {
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
            usage.clone()
        };
        self.changes
            .send_modify(|count| *count = count.wrapping_add(1));
        if let Some(saves) = &self.saves
            && saves.send(Save::Usage(saved)).is_err()
        {
            tracing::warn!("cannot save the usage of {provider}: its writer has stopped");
        }
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

/// The reading stored for `provider` in `db`; `None` when there is none, or when it does not
/// read (logged). `fronde check` reads it from a read-only connection.
pub async fn read_usage<C: ConnectionTrait>(
    db: &C,
    provider: &str,
) -> anyhow::Result<Option<ProviderUsage>> {
    let Some(value) = fronde_db::kv::get(db, &scope(provider), KEY).await? else {
        return Ok(None);
    };
    match serde_json::from_slice::<ProviderUsage>(&value) {
        Ok(usage) if usage.provider == provider => Ok(Some(usage)),
        Ok(_) => {
            tracing::warn!("ignored the stored usage of {provider}: it names another provider");
            Ok(None)
        }
        Err(error) => {
            tracing::warn!("ignored the stored usage of {provider}: {error}");
            Ok(None)
        }
    }
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
}
