//! Conversation log: an append-only JSONL file, mirrored in memory, with live broadcast.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::Context;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

pub use fronde_protocol::{Entry, Event, ToolState, TurnOutcome};

const BROADCAST_CAPACITY: usize = 1024;

/// The log file's first line.
#[derive(Serialize, Deserialize)]
struct Header {
    log_id: String,
}

struct State {
    entries: Vec<Entry>,
    file: File,
    /// The file may end in a torn line: terminate it before the next entry.
    torn: bool,
}

pub struct ConversationLog {
    log_id: String,
    state: Mutex<State>,
    live: broadcast::Sender<Entry>,
}

impl ConversationLog {
    /// Reads the JSONL file (creating it if missing); unreadable lines are skipped and logged.
    /// The first line names the log with a random id, written when the file is created.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let mut entries = Vec::new();
        let mut log_id = None;
        match File::open(path) {
            Ok(file) => {
                for (index, line) in BufReader::new(file).lines().enumerate() {
                    let line = line.with_context(|| format!("reading {}", path.display()))?;
                    if line.trim().is_empty() {
                        continue;
                    }
                    if index == 0
                        && let Ok(header) = serde_json::from_str::<Header>(&line)
                    {
                        log_id = Some(header.log_id);
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
        let log_id = match log_id {
            Some(id) => id,
            None => write_header(path)?,
        };
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let length = file.metadata()?.len();
        let torn = length > 0 && {
            let mut last = [0u8; 1];
            file.seek(SeekFrom::Start(length - 1))?;
            file.read_exact(&mut last)?;
            last[0] != b'\n'
        };
        let (live, _) = broadcast::channel(BROADCAST_CAPACITY);
        Ok(Self {
            log_id,
            state: Mutex::new(State {
                entries,
                file,
                torn,
            }),
            live,
        })
    }

    /// The random id of this log, new each time the log file is created.
    pub fn log_id(&self) -> &str {
        &self.log_id
    }

    /// Numbers the event, appends it as one line, then broadcasts it.
    pub fn append(&self, event: Event) -> anyhow::Result<Entry> {
        self.append_with(|_| event)
    }

    /// [`append`](Self::append) for an event made from its seq, such as an id that must be
    /// unique for the life of the log. `event` runs under the log's lock.
    pub fn append_with(&self, event: impl FnOnce(u64) -> Event) -> anyhow::Result<Entry> {
        // The lock is held throughout so seqs are unique and file order matches seq order.
        let mut state = self.lock();
        let seq = state.entries.last().map_or(0, |entry| entry.seq) + 1;
        let entry = Entry {
            seq,
            at: Utc::now(),
            event: event(seq),
        };
        let mut line = String::new();
        if state.torn {
            line.push('\n');
        }
        line.push_str(&serde_json::to_string(&entry)?);
        line.push('\n');
        // A failed write may leave partial bytes behind.
        state.torn = true;
        state.file.write_all(line.as_bytes())?;
        state.torn = false;
        state.entries.push(entry.clone());
        // No subscriber is not an error.
        let _ = self.live.send(entry.clone());
        Ok(entry)
    }

    /// Entries with a seq greater than the given one.
    pub fn since(&self, seq: u64) -> Vec<Entry> {
        let state = self.lock();
        let start = state.entries.partition_point(|entry| entry.seq <= seq);
        state.entries[start..].to_vec()
    }

    /// The seq of the newest entry, 0 when the log is empty.
    pub fn last_seq(&self) -> u64 {
        self.lock().entries.last().map_or(0, |entry| entry.seq)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Entry> {
        self.live.subscribe()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Names the log at `path` with a new random id, written as its first line, and returns the id.
/// Entries already there (a log from before ids) are kept; the file is replaced in one rename.
fn write_header(path: &Path) -> anyhow::Result<String> {
    let log_id = uuid::Uuid::new_v4().simple().to_string();
    let mut text = serde_json::to_string(&Header {
        log_id: log_id.clone(),
    })?;
    text.push('\n');
    let mut old = Vec::new();
    match File::open(path) {
        Ok(mut file) => {
            file.read_to_end(&mut old)
                .with_context(|| format!("reading {}", path.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("opening {}", path.display())),
    }
    let mut bytes = text.into_bytes();
    bytes.extend(old);
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = std::path::PathBuf::from(temporary);
    std::fs::write(&temporary, bytes)
        .with_context(|| format!("writing {}", temporary.display()))?;
    std::fs::rename(&temporary, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(log_id)
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
        assert_eq!(log.last_seq(), 3);
    }

    #[test]
    fn empty_log_last_seq_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        let log = ConversationLog::open(&dir.path().join("log.jsonl")).unwrap();
        assert_eq!(log.last_seq(), 0);
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

    #[test]
    fn torn_trailing_line_does_not_swallow_next_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let log = ConversationLog::open(&path).unwrap();
        log.append(user("a")).unwrap();
        drop(log);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(r#"{"seq":2,"at":"2026"#);
        std::fs::write(&path, text).unwrap();
        let log = ConversationLog::open(&path).unwrap();
        assert_eq!(log.append(user("b")).unwrap().seq, 2);
        drop(log);
        let seqs: Vec<u64> = ConversationLog::open(&path)
            .unwrap()
            .since(0)
            .iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(seqs, [1, 2]);
    }

    #[test]
    fn log_id_is_kept_until_the_log_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let log = ConversationLog::open(&path).unwrap();
        let id = log.log_id().to_owned();
        assert!(id.len() >= 16, "{id}");
        log.append(user("a")).unwrap();
        drop(log);

        let log = ConversationLog::open(&path).unwrap();
        assert_eq!(log.log_id(), id);
        assert_eq!(log.since(0).len(), 1);
        drop(log);

        std::fs::remove_file(&path).unwrap();
        let log = ConversationLog::open(&path).unwrap();
        assert_ne!(log.log_id(), id);
        assert_eq!(log.last_seq(), 0);
    }

    #[test]
    fn log_without_an_id_gets_one_and_keeps_its_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let entry = r#"{"seq":1,"at":"2026-11-16T10:00:00Z","event":{"type":"user","text":"a"}}"#;
        std::fs::write(&path, format!("{entry}\n")).unwrap();

        let log = ConversationLog::open(&path).unwrap();
        let id = log.log_id().to_owned();
        assert_eq!(log.since(0).len(), 1);
        assert_eq!(log.append(user("b")).unwrap().seq, 2);
        drop(log);

        let log = ConversationLog::open(&path).unwrap();
        assert_eq!(log.log_id(), id);
        let seqs: Vec<u64> = log.since(0).iter().map(|e| e.seq).collect();
        assert_eq!(seqs, [1, 2]);
    }

    #[test]
    fn append_with_gives_the_event_its_seq() {
        let dir = tempfile::tempdir().unwrap();
        let log = ConversationLog::open(&dir.path().join("log.jsonl")).unwrap();
        log.append(user("a")).unwrap();
        let entry = log
            .append_with(|seq| Event::Permission {
                id: seq,
                title: "t".into(),
            })
            .unwrap();
        assert_eq!(entry.seq, 2);
        assert_eq!(
            entry.event,
            Event::Permission {
                id: 2,
                title: "t".into()
            }
        );
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
