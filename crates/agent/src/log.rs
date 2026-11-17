//! Conversation log: a stream of numbered entries in the database, mirrored in memory, with live
//! broadcast.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::Context;
use chrono::Utc;
use fronde_db::DatabaseConnection;
use fronde_entity::{stream_entries, streams};
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use tokio::sync::broadcast;

pub use fronde_protocol::{Entry, Event, ToolState, TurnOutcome};

const BROADCAST_CAPACITY: usize = 1024;

pub struct ConversationLog {
    db: DatabaseConnection,
    topic: String,
    log_id: String,
    /// Held while an entry is numbered and saved, so seqs are unique and saved in order.
    writer: Arc<tokio::sync::Mutex<()>>,
    mirror: Arc<Mirror>,
}

/// The saved entries, and the channel announcing each new one.
struct Mirror {
    entries: Mutex<Vec<Entry>>,
    live: broadcast::Sender<Entry>,
}

impl Mirror {
    fn entries(&self) -> MutexGuard<'_, Vec<Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn publish(&self, entry: Entry) {
        let mut entries = self.entries();
        entries.push(entry.clone());
        // No subscriber is not an error.
        let _ = self.live.send(entry);
    }
}

impl ConversationLog {
    /// Loads the stream `topic`, creating it with a new random log id when it does not exist.
    pub async fn open(db: &DatabaseConnection, topic: &str) -> anyhow::Result<Self> {
        let log_id = stream_log_id(db, topic)
            .await
            .context("cannot open the conversation in the database")?;
        let rows = stream_entries::Entity::find()
            .filter(stream_entries::Column::Topic.eq(topic))
            .order_by_asc(stream_entries::Column::Seq)
            .all(db)
            .await
            .context("cannot read the conversation from the database")?;
        let entries = rows
            .into_iter()
            .map(|row| {
                let event = serde_json::from_value(row.event).with_context(|| {
                    format!(
                        "entry {} of the conversation is not readable by this version of \
                         Fronde; run the version that wrote it, or restore a backup",
                        row.seq
                    )
                })?;
                Ok(Entry {
                    seq: u64::try_from(row.seq).context("negative seq in the conversation")?,
                    at: row.at,
                    event,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let (live, _) = broadcast::channel(BROADCAST_CAPACITY);
        Ok(Self {
            db: db.clone(),
            topic: topic.to_owned(),
            log_id,
            writer: Arc::default(),
            mirror: Arc::new(Mirror {
                entries: Mutex::new(entries),
                live,
            }),
        })
    }

    /// The random id of this log, new each time its stream is created.
    pub fn log_id(&self) -> &str {
        &self.log_id
    }

    /// Numbers the event, saves it, then broadcasts it.
    pub async fn append(&self, event: Event) -> anyhow::Result<Entry> {
        self.append_with(|_| event).await
    }

    /// [`append`](Self::append) for an event made from its seq, such as an id that must be
    /// unique for the life of the log. `event` runs under the log's lock.
    pub async fn append_with(&self, event: impl FnOnce(u64) -> Event) -> anyhow::Result<Entry> {
        let writer = self.writer.clone().lock_owned().await;
        let seq = self.last_seq() + 1;
        let entry = Entry {
            seq,
            at: Utc::now(),
            event: event(seq),
        };
        let (db, topic, mirror) = (self.db.clone(), self.topic.clone(), self.mirror.clone());
        // Saved and published by a task of its own: a caller that stops waiting cannot leave an
        // entry saved but missing from the mirror, whose last seq numbers the next entry.
        tokio::spawn(async move {
            let _writer = writer;
            save(&db, &topic, &entry)
                .await
                .context("cannot save the conversation entry in the database")?;
            // Sent only once committed: a page never shows an entry a crash could lose.
            mirror.publish(entry.clone());
            Ok(entry)
        })
        .await
        .context("saving the conversation entry stopped")?
    }

    /// Entries with a seq greater than the given one.
    pub fn since(&self, seq: u64) -> Vec<Entry> {
        let entries = self.mirror.entries();
        let start = entries.partition_point(|entry| entry.seq <= seq);
        entries[start..].to_vec()
    }

    /// The seq of the newest entry, 0 when the log is empty.
    pub fn last_seq(&self) -> u64 {
        self.mirror.entries().last().map_or(0, |entry| entry.seq)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Entry> {
        self.mirror.live.subscribe()
    }
}

/// The log id of the stream `topic`, which is created when missing.
async fn stream_log_id(db: &DatabaseConnection, topic: &str) -> Result<String, sea_orm::DbErr> {
    let transaction = fronde_db::begin_write(db).await?;
    let log_id = match streams::Entity::find_by_id(topic).one(&transaction).await? {
        Some(stream) => stream.log_id,
        None => {
            let log_id = new_log_id();
            streams::Entity::insert(new_stream(topic, &log_id))
                .exec(&transaction)
                .await?;
            log_id
        }
    };
    transaction.commit().await?;
    Ok(log_id)
}

/// A new random log id.
pub(crate) fn new_log_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

pub(crate) fn new_stream(topic: &str, log_id: &str) -> streams::ActiveModel {
    streams::ActiveModel {
        topic: Set(topic.to_owned()),
        log_id: Set(log_id.to_owned()),
        created_at: Set(Utc::now()),
    }
}

pub(crate) fn entry_row(topic: &str, entry: &Entry) -> anyhow::Result<stream_entries::ActiveModel> {
    Ok(stream_entries::ActiveModel {
        topic: Set(topic.to_owned()),
        seq: Set(i64::try_from(entry.seq).context("seq too large")?),
        at: Set(entry.at),
        event: Set(serde_json::to_value(&entry.event)?),
    })
}

/// Commits the entry; with `synchronous = FULL`, it is on disk when this returns.
async fn save(db: &DatabaseConnection, topic: &str, entry: &Entry) -> anyhow::Result<()> {
    let row = entry_row(topic, entry)?;
    let transaction = fronde_db::begin_write(db).await?;
    stream_entries::Entity::insert(row)
        .exec(&transaction)
        .await?;
    transaction.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::ConnectionTrait;
    use std::collections::BTreeSet;
    use std::path::Path;

    const TOPIC: &str = "conversation";

    fn user(text: &str) -> Event {
        Event::User { text: text.into() }
    }

    async fn db(dir: &Path) -> DatabaseConnection {
        fronde_db::open(dir).await.unwrap()
    }

    async fn open(dir: &Path) -> ConversationLog {
        ConversationLog::open(&db(dir).await, TOPIC).await.unwrap()
    }

    async fn saved_seqs(db: &DatabaseConnection) -> Vec<i64> {
        stream_entries::Entity::find()
            .filter(stream_entries::Column::Topic.eq(TOPIC))
            .order_by_asc(stream_entries::Column::Seq)
            .all(db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.seq)
            .collect()
    }

    #[tokio::test]
    async fn append_assigns_increasing_seq() {
        let dir = tempfile::tempdir().unwrap();
        let log = open(dir.path()).await;
        assert_eq!(log.append(user("a")).await.unwrap().seq, 1);
        assert_eq!(log.append(user("b")).await.unwrap().seq, 2);
    }

    #[tokio::test]
    async fn reopen_keeps_entries_and_seq() {
        let dir = tempfile::tempdir().unwrap();
        let log = open(dir.path()).await;
        log.append(user("a")).await.unwrap();
        log.append(user("b")).await.unwrap();
        drop(log);
        let log = open(dir.path()).await;
        assert_eq!(log.since(0).len(), 2);
        assert_eq!(log.append(user("c")).await.unwrap().seq, 3);
    }

    #[tokio::test]
    async fn since_returns_only_newer() {
        let dir = tempfile::tempdir().unwrap();
        let log = open(dir.path()).await;
        for t in ["a", "b", "c"] {
            log.append(user(t)).await.unwrap();
        }
        let seqs: Vec<u64> = log.since(1).iter().map(|e| e.seq).collect();
        assert_eq!(seqs, [2, 3]);
        assert!(log.since(3).is_empty());
        assert_eq!(log.last_seq(), 3);
    }

    #[tokio::test]
    async fn empty_log_last_seq_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        let log = open(dir.path()).await;
        assert_eq!(log.last_seq(), 0);
    }

    #[tokio::test]
    async fn log_id_is_kept_until_the_log_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let db = db(dir.path()).await;
        let log = ConversationLog::open(&db, TOPIC).await.unwrap();
        let id = log.log_id().to_owned();
        assert!(id.len() >= 16, "{id}");
        log.append(user("a")).await.unwrap();
        drop(log);

        let log = ConversationLog::open(&db, TOPIC).await.unwrap();
        assert_eq!(log.log_id(), id);
        assert_eq!(log.since(0).len(), 1);
        drop(log);

        // Starting a new conversation: Fronde stopped, the database deleted.
        db.close().await.unwrap();
        for name in ["fronde.db", "fronde.db-wal", "fronde.db-shm"] {
            let _ = std::fs::remove_file(dir.path().join(name));
        }
        let log = open(dir.path()).await;
        assert_ne!(log.log_id(), id);
        assert_eq!(log.last_seq(), 0);
    }

    #[tokio::test]
    async fn append_with_gives_the_event_its_seq() {
        let dir = tempfile::tempdir().unwrap();
        let log = open(dir.path()).await;
        log.append(user("a")).await.unwrap();
        let entry = log
            .append_with(|seq| Event::Permission {
                id: seq,
                title: "t".into(),
            })
            .await
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
        let log = open(dir.path()).await;
        let mut live = log.subscribe();
        for t in ["a", "b", "c"] {
            log.append(user(t)).await.unwrap();
        }
        let mut seen: Vec<u64> = log.since(1).iter().map(|e| e.seq).collect();
        while let Ok(entry) = live.try_recv() {
            if entry.seq > *seen.last().unwrap() {
                seen.push(entry.seq);
            }
        }
        assert_eq!(seen, [2, 3]);
    }

    #[tokio::test]
    async fn restart_keeps_log_id_and_entries() {
        let dir = tempfile::tempdir().unwrap();
        let db = db(dir.path()).await;
        let log = ConversationLog::open(&db, TOPIC).await.unwrap();
        log.append(user("a")).await.unwrap();
        log.append_with(|id| Event::Permission {
            id,
            title: "t".into(),
        })
        .await
        .unwrap();
        log.append(Event::TurnEnded {
            outcome: TurnOutcome::Done,
        })
        .await
        .unwrap();
        let (id, entries) = (log.log_id().to_owned(), log.since(0));
        drop(log);
        db.close().await.unwrap();

        let log = open(dir.path()).await;
        assert_eq!(log.log_id(), id);
        // Timestamps included, to the nanosecond.
        assert_eq!(log.since(0), entries);
        assert_eq!(log.append(user("b")).await.unwrap().seq, 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_appends_keep_seq_order() {
        const TASKS: u64 = 8;
        const EACH: u64 = 25;
        const TOTAL: u64 = TASKS * EACH;
        let dir = tempfile::tempdir().unwrap();
        let db = db(dir.path()).await;
        let log = Arc::new(ConversationLog::open(&db, TOPIC).await.unwrap());

        // A page connecting while the agent writes: subscribe, replay, then follow.
        let reader = tokio::spawn({
            let log = log.clone();
            async move {
                tokio::task::yield_now().await;
                let mut live = log.subscribe();
                let mut seen: Vec<u64> = log.since(0).iter().map(|e| e.seq).collect();
                while seen.last().copied().unwrap_or(0) < TOTAL {
                    let entry = live.recv().await.unwrap();
                    if entry.seq > seen.last().copied().unwrap_or(0) {
                        seen.push(entry.seq);
                    }
                }
                seen
            }
        });
        let writers: Vec<_> = (0..TASKS)
            .map(|task| {
                let log = log.clone();
                tokio::spawn(async move {
                    let mut seqs = Vec::new();
                    for n in 0..EACH {
                        let entry = log.append(user(&format!("{task}-{n}"))).await.unwrap();
                        seqs.push(entry.seq);
                    }
                    seqs
                })
            })
            .collect();
        let mut all = BTreeSet::new();
        for writer in writers {
            let seqs = writer.await.unwrap();
            assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
            all.extend(seqs);
        }

        let expected: Vec<u64> = (1..=TOTAL).collect();
        assert_eq!(all.into_iter().collect::<Vec<_>>(), expected);
        assert_eq!(reader.await.unwrap(), expected);
        let in_memory: Vec<u64> = log.since(0).iter().map(|e| e.seq).collect();
        assert_eq!(in_memory, expected);
        let saved: Vec<u64> = saved_seqs(&db)
            .await
            .into_iter()
            .map(|s| s as u64)
            .collect();
        assert_eq!(saved, expected);
        let reopened = ConversationLog::open(&db, TOPIC).await.unwrap();
        assert_eq!(reopened.since(0), log.since(0));
    }

    #[tokio::test]
    async fn entry_is_saved_before_it_is_broadcast() {
        let dir = tempfile::tempdir().unwrap();
        let db = db(dir.path()).await;
        let log = Arc::new(ConversationLog::open(&db, TOPIC).await.unwrap());
        let mut live = log.subscribe();
        let appended = tokio::spawn({
            let log = log.clone();
            async move { log.append(user("a")).await.unwrap() }
        });
        let entry = live.recv().await.unwrap();
        // Read on another connection the moment the entry is announced.
        assert_eq!(saved_seqs(&db).await, [entry.seq as i64]);
        assert_eq!(appended.await.unwrap(), entry);
    }

    #[tokio::test]
    async fn failed_save_is_neither_kept_nor_broadcast() {
        let dir = tempfile::tempdir().unwrap();
        let db = db(dir.path()).await;
        let log = ConversationLog::open(&db, TOPIC).await.unwrap();
        log.append(user("a")).await.unwrap();
        let mut live = log.subscribe();
        db.execute_unprepared(
            "CREATE TRIGGER refuse BEFORE INSERT ON stream_entries \
             BEGIN SELECT RAISE(ABORT, 'disk full'); END",
        )
        .await
        .unwrap();

        let error = format!("{:#}", log.append(user("b")).await.unwrap_err());
        assert!(error.contains("disk full"), "{error}");
        assert!(live.try_recv().is_err());
        assert_eq!(log.last_seq(), 1);

        db.execute_unprepared("DROP TRIGGER refuse").await.unwrap();
        assert_eq!(log.append(user("c")).await.unwrap().seq, 2);
        assert_eq!(saved_seqs(&db).await, [1, 2]);
    }
}
