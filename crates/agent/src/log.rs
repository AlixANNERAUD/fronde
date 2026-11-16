//! Conversation log: an append-only JSONL file, mirrored in memory, with live broadcast.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use anyhow::Context;
use chrono::Utc;
use tokio::sync::broadcast;

pub use jarvis_protocol::{Entry, Event, ToolState, TurnOutcome};

const BROADCAST_CAPACITY: usize = 1024;

pub struct ConversationLog {
    path: PathBuf,
    entries: Mutex<Vec<Entry>>,
    live: broadcast::Sender<Entry>,
}

impl ConversationLog {
    /// Reads the JSONL file (creating it if missing); unreadable lines are skipped and logged.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let mut entries = Vec::new();
        match File::open(path) {
            Ok(file) => {
                for (index, line) in BufReader::new(file).lines().enumerate() {
                    let line = line.with_context(|| format!("reading {}", path.display()))?;
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Entry>(&line) {
                        Ok(entry) => entries.push(entry),
                        Err(error) => {
                            tracing::warn!(line = index + 1, %error, "skipping unreadable log line");
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("opening {}", path.display()));
            }
        }
        let (live, _) = broadcast::channel(BROADCAST_CAPACITY);
        Ok(Self {
            path: path.to_path_buf(),
            entries: Mutex::new(entries),
            live,
        })
    }

    /// Numbers the event, appends it as one line (flushed), then broadcasts it.
    pub fn append(&self, event: Event) -> anyhow::Result<Entry> {
        // The lock is held throughout so seqs are unique and file order matches seq order.
        let mut entries = self.lock();
        let seq = entries.last().map_or(0, |entry| entry.seq) + 1;
        let entry = Entry {
            seq,
            at: Utc::now(),
            event,
        };
        let mut line = serde_json::to_string(&entry)?;
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        file.write_all(line.as_bytes())?;
        file.flush()?;
        entries.push(entry.clone());
        // No subscriber is not an error.
        let _ = self.live.send(entry.clone());
        Ok(entry)
    }

    /// Entries with a seq greater than the given one.
    pub fn since(&self, seq: u64) -> Vec<Entry> {
        let entries = self.lock();
        let start = entries.partition_point(|entry| entry.seq <= seq);
        entries[start..].to_vec()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Entry> {
        self.live.subscribe()
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Event {
        Event::User { text: text.into() }
    }

    #[test]
    fn append_assigns_increasing_seq() {
        let dir = tempfile::tempdir().unwrap();
        let log = ConversationLog::open(&dir.path().join("log.jsonl")).unwrap();
        assert_eq!(log.append(user("a")).unwrap().seq, 1);
        assert_eq!(log.append(user("b")).unwrap().seq, 2);
    }

    #[test]
    fn reopen_keeps_entries_and_seq() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let log = ConversationLog::open(&path).unwrap();
        log.append(user("a")).unwrap();
        log.append(user("b")).unwrap();
        drop(log);
        let log = ConversationLog::open(&path).unwrap();
        assert_eq!(log.since(0).len(), 2);
        assert_eq!(log.append(user("c")).unwrap().seq, 3);
    }

    #[test]
    fn since_returns_only_newer() {
        let dir = tempfile::tempdir().unwrap();
        let log = ConversationLog::open(&dir.path().join("log.jsonl")).unwrap();
        for t in ["a", "b", "c"] {
            log.append(user(t)).unwrap();
        }
        let seqs: Vec<u64> = log.since(1).iter().map(|e| e.seq).collect();
        assert_eq!(seqs, [2, 3]);
        assert!(log.since(3).is_empty());
    }

    #[test]
    fn corrupt_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let log = ConversationLog::open(&path).unwrap();
        log.append(user("a")).unwrap();
        drop(log);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("not json\n");
        std::fs::write(&path, text).unwrap();
        let log = ConversationLog::open(&path).unwrap();
        assert_eq!(log.since(0).len(), 1);
        assert_eq!(log.append(user("b")).unwrap().seq, 2);
        drop(log);
        assert_eq!(ConversationLog::open(&path).unwrap().since(0).len(), 2);
    }

    #[tokio::test]
    async fn replay_after_reconnect_has_no_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let log = ConversationLog::open(&dir.path().join("log.jsonl")).unwrap();
        let mut live = log.subscribe();
        for t in ["a", "b", "c"] {
            log.append(user(t)).unwrap();
        }
        let mut seen: Vec<u64> = log.since(1).iter().map(|e| e.seq).collect();
        while let Ok(entry) = live.try_recv() {
            if entry.seq > *seen.last().unwrap() {
                seen.push(entry.seq);
            }
        }
        assert_eq!(seen, [2, 3]);
    }
}
