//! The files that held the conversation before the database: imported once, at the first start
//! that finds them while the database has no conversation yet.
//!
//! The log was JSONL: a first line naming the log (`{"log_id": …}`, missing in the oldest logs),
//! then one entry per line. The agent's session id was a file of its own. Before the rename to
//! Fronde, both were named after Jarvis.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use fronde_db::DatabaseConnection;
use fronde_entity::{stream_entries, streams};
use sea_orm::EntityTrait;
use serde::Deserialize;

use crate::log::{Entry, entry_row, new_log_id, new_stream};

/// The conversation log, then its name before the rename to Fronde.
pub const LOG_FILES: [&str; 2] = ["conversation.jsonl", "jarvis.jsonl"];
/// The agent's session id, then its name before the rename to Fronde.
pub const SESSION_FILES: [&str; 2] = ["agent.session", "jarvis.session"];
/// Appended to an imported file's name.
pub const IMPORTED: &str = ".imported";

/// Rows per insert, well under SQLite's limit on bound values.
const BATCH: usize = 500;

/// The log file's first line.
#[derive(Deserialize)]
struct Header {
    log_id: String,
}

/// Imports the log and session files in `data_dir` into the stream `topic` and the key-value
/// `session` (scope, key), in one transaction, unless the stream exists. The files are renamed
/// `*.imported` once the transaction is committed: a failure before leaves them as they were,
/// and the next start tries again.
pub async fn import(
    db: &DatabaseConnection,
    data_dir: &Path,
    topic: &str,
    session: (&str, &str),
) -> anyhow::Result<()> {
    let log_file = first_existing(data_dir, &LOG_FILES);
    let session_file = first_existing(data_dir, &SESSION_FILES);
    if log_file.is_none() && session_file.is_none() {
        return Ok(());
    }
    if let Some(stream) = find_stream(db, topic).await? {
        finish_or_warn(db, data_dir, &stream, log_file, session_file, session).await;
        return Ok(());
    }

    let (log_id, entries) = match &log_file {
        Some(file) => read_log(file)?,
        None => (None, Vec::new()),
    };
    let session_id = match &session_file {
        Some(file) => read_session(file)?,
        None => None,
    };

    let files: Vec<String> = log_file
        .iter()
        .chain(&session_file)
        .map(|file| file.display().to_string())
        .collect();
    let failed = || {
        format!(
            "cannot import {} into the database; nothing was imported and the files are \
             unchanged, so the next start tries again",
            files.join(" and ")
        )
    };
    let transaction = fronde_db::begin_write(db).await.with_context(failed)?;
    // Checked again under the write lock: another Fronde on the same data may have imported.
    if stream_exists(&transaction, topic).await? {
        return Ok(());
    }
    let log_id = log_id.unwrap_or_else(new_log_id);
    async {
        streams::Entity::insert(new_stream(topic, &log_id))
            .exec(&transaction)
            .await?;
        for batch in entries.chunks(BATCH) {
            let rows = batch
                .iter()
                .map(|entry| entry_row(topic, entry))
                .collect::<anyhow::Result<Vec<_>>>()?;
            stream_entries::Entity::insert_many(rows)
                .exec(&transaction)
                .await?;
        }
        if let Some(id) = &session_id {
            fronde_db::kv::set(&transaction, session.0, session.1, id.clone().into(), None).await?;
        }
        anyhow::Ok(())
    }
    .await
    .with_context(failed)?;
    transaction.commit().await.with_context(failed)?;
    tracing::info!(
        entries = entries.len(),
        session = session_id.is_some(),
        "imported the conversation from before the database"
    );

    for file in log_file.iter().chain(&session_file) {
        mark_imported(file);
    }
    sync_dir(data_dir);
    Ok(())
}

/// Legacy files found while the stream exists. Those the database already holds (the import
/// committed, then a crash came before their rename) get the rename they missed; any other is
/// left alone with a warning, since deleting `fronde.db` would import it into a new conversation.
async fn finish_or_warn(
    db: &DatabaseConnection,
    data_dir: &Path,
    stream: &streams::Model,
    log_file: Option<PathBuf>,
    session_file: Option<PathBuf>,
    session: (&str, &str),
) {
    let log_done = log_file
        .as_ref()
        .is_some_and(|file| read_log_id(file).as_deref() == Some(stream.log_id.as_str()));
    let session_done = log_done
        || match &session_file {
            Some(file) => {
                let stored = fronde_db::kv::get(db, session.0, session.1)
                    .await
                    .ok()
                    .flatten();
                let found = read_session(file).ok().flatten();
                found.is_some_and(|id| stored.as_deref() == Some(id.as_bytes()))
            }
            None => false,
        };
    let mut renamed = false;
    for (file, done) in [(log_file, log_done), (session_file, session_done)] {
        let Some(file) = file else { continue };
        if done {
            tracing::info!(
                "{} is already in the database; finishing its rename",
                file.display()
            );
            mark_imported(&file);
            renamed = true;
        } else {
            tracing::warn!(
                "{} was not imported: the database already holds the conversation; move or \
                 delete the file",
                file.display()
            );
        }
    }
    if renamed {
        sync_dir(data_dir);
    }
}

fn first_existing(dir: &Path, names: &[&str]) -> Option<PathBuf> {
    names
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.exists())
}

async fn find_stream<C: sea_orm::ConnectionTrait>(
    db: &C,
    topic: &str,
) -> anyhow::Result<Option<streams::Model>> {
    streams::Entity::find_by_id(topic)
        .one(db)
        .await
        .context("cannot read the conversation from the database")
}

async fn stream_exists<C: sea_orm::ConnectionTrait>(db: &C, topic: &str) -> anyhow::Result<bool> {
    Ok(find_stream(db, topic).await?.is_some())
}

/// The log id in the log file's first line, if it has one.
fn read_log_id(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(file), &mut first).ok()?;
    serde_json::from_str::<Header>(&first)
        .ok()
        .map(|h| h.log_id)
}

/// Makes the renames in `dir` durable. A failure is only a warning: a rename lost in a crash
/// is finished at the next start.
fn sync_dir(dir: &Path) {
    if let Err(error) = std::fs::File::open(dir).and_then(|dir| dir.sync_all()) {
        tracing::warn!("cannot sync {}: {error}", dir.display());
    }
}

/// The log id in the header, if any, and the entries. A line cut short by a crash mid-write was
/// never shown and is skipped; any other unreadable line stops the import, naming it.
fn read_log(path: &Path) -> anyhow::Result<(Option<String>, Vec<Entry>)> {
    let shown = path.display();
    let bytes = std::fs::read(path).with_context(|| {
        format!("cannot read {shown}; make it readable by the user running Fronde")
    })?;
    let mut log_id = None;
    let mut entries: Vec<Entry> = Vec::new();
    let lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    for (index, line) in lines.iter().enumerate() {
        let number = index + 1;
        if line.trim_ascii().is_empty() {
            continue;
        }
        if index == 0
            && let Ok(header) = serde_json::from_slice::<Header>(line)
        {
            log_id = Some(header.log_id);
            continue;
        }
        let entry = match serde_json::from_slice::<Entry>(line) {
            Ok(entry) => entry,
            // The last line has no newline, or the line stops mid-entry: a write the crash
            // interrupted, which the log never counted.
            Err(error) if index + 1 == lines.len() || error.is_eof() => {
                tracing::warn!(
                    line = number,
                    "skipping a line of {shown} cut short: {error}"
                );
                continue;
            }
            Err(error) => bail!(
                "line {number} of {shown} is not a conversation entry ({error}); nothing was \
                 imported: fix or delete that line, then start Fronde again (or move the file \
                 away to start a new conversation)"
            ),
        };
        if let Some(previous) = entries.last()
            && entry.seq <= previous.seq
        {
            bail!(
                "line {number} of {shown} has seq {}, not after the previous entry's {}; \
                 nothing was imported: delete the line that repeats an entry, then start Fronde \
                 again",
                entry.seq,
                previous.seq
            );
        }
        entries.push(entry);
    }
    Ok((log_id, entries))
}

fn read_session(path: &Path) -> anyhow::Result<Option<String>> {
    let id = std::fs::read_to_string(path).with_context(|| {
        format!(
            "cannot read {}; make it readable by the user running Fronde, or delete it to start \
             a new agent session",
            path.display()
        )
    })?;
    Ok(Some(id.trim().to_owned()).filter(|id| !id.is_empty()))
}

/// Renames the file `*.imported`. A failure is only a warning: the stream exists now, so the file
/// is never imported again.
fn mark_imported(file: &Path) {
    let mut target = file.as_os_str().to_owned();
    target.push(IMPORTED);
    let target = PathBuf::from(target);
    if target.exists() {
        tracing::warn!(
            "{} is imported, but {} already exists: move or delete the file",
            file.display(),
            target.display()
        );
        return;
    }
    if let Err(error) = std::fs::rename(file, &target) {
        tracing::warn!(
            "{} is imported, but cannot be renamed {}: {error}",
            file.display(),
            target.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{ConversationLog, Event};
    use sea_orm::ConnectionTrait;

    const TOPIC: &str = "conversation";
    const SESSION: (&str, &str) = ("agent", "session");

    fn user(text: &str) -> Event {
        Event::User {
            text: text.into(),
            by: None,
        }
    }

    fn line(seq: u64, text: &str) -> String {
        format!(
            r#"{{"seq":{seq},"at":"2026-11-16T10:00:0{seq}Z","event":{{"type":"user","text":"{text}"}}}}"#
        )
    }

    fn header(log_id: &str) -> String {
        format!(r#"{{"log_id":"{log_id}"}}"#)
    }

    /// Imports what `dir` holds, then opens the log, as `Assistant::open` does.
    async fn open(dir: &Path) -> anyhow::Result<(DatabaseConnection, ConversationLog)> {
        let db = fronde_db::open(dir).await?;
        import(&db, dir, TOPIC, SESSION).await?;
        let log = ConversationLog::open(&db, TOPIC).await?;
        Ok((db, log))
    }

    fn texts(log: &ConversationLog) -> Vec<(u64, String)> {
        log.since(0)
            .into_iter()
            .map(|entry| match entry.event {
                Event::User { text, .. } => (entry.seq, text),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    async fn session(db: &DatabaseConnection) -> Option<Vec<u8>> {
        fronde_db::kv::get(db, SESSION.0, SESSION.1).await.unwrap()
    }

    #[tokio::test]
    async fn imports_legacy_jsonl_once() {
        for (log_name, session_name) in LOG_FILES.into_iter().zip(SESSION_FILES) {
            let dir = tempfile::tempdir().unwrap();
            let (log_file, session_file) =
                (dir.path().join(log_name), dir.path().join(session_name));
            let text = format!(
                "{}\n{}\n{}\n",
                header("old-log"),
                line(1, "a"),
                line(2, "b")
            );
            std::fs::write(&log_file, &text).unwrap();
            std::fs::write(&session_file, "s-1\n").unwrap();

            let (db, log) = open(dir.path()).await.unwrap();
            assert_eq!(log.log_id(), "old-log", "{log_name}");
            assert_eq!(texts(&log), [(1, "a".into()), (2, "b".into())]);
            assert_eq!(log.since(0)[0].at.to_rfc3339(), "2026-11-16T10:00:01+00:00");
            assert_eq!(session(&db).await, Some(b"s-1".to_vec()));
            assert!(!log_file.exists() && !session_file.exists(), "{log_name}");
            let imported = |file: &Path| {
                let mut name = file.as_os_str().to_owned();
                name.push(IMPORTED);
                std::fs::read_to_string(PathBuf::from(name)).unwrap()
            };
            assert_eq!(imported(&log_file), text);
            assert_eq!(imported(&session_file), "s-1\n");
            assert_eq!(log.append(user("c")).await.unwrap().seq, 3);
            drop(log);
            db.close().await.unwrap();

            // A second start imports nothing, even if a log file shows up again.
            std::fs::write(
                &log_file,
                format!("{}\n{}\n", header("other"), line(1, "x")),
            )
            .unwrap();
            let (_db, log) = open(dir.path()).await.unwrap();
            assert_eq!(log.log_id(), "old-log");
            assert_eq!(
                texts(&log),
                [(1, "a".into()), (2, "b".into()), (3, "c".into())]
            );
            assert!(log_file.exists(), "a file not imported is left alone");
        }
    }

    #[tokio::test]
    async fn the_newer_names_win() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("conversation.jsonl"), line(1, "new") + "\n").unwrap();
        std::fs::write(dir.path().join("jarvis.jsonl"), line(1, "old") + "\n").unwrap();
        std::fs::write(dir.path().join("agent.session"), "new").unwrap();
        std::fs::write(dir.path().join("jarvis.session"), "old").unwrap();

        let (db, log) = open(dir.path()).await.unwrap();
        assert_eq!(texts(&log), [(1, "new".into())]);
        assert_eq!(session(&db).await, Some(b"new".to_vec()));
        assert!(dir.path().join("jarvis.jsonl").exists());
        assert!(dir.path().join("jarvis.session").exists());
    }

    #[tokio::test]
    async fn a_session_file_alone_is_imported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("agent.session"), "s-1").unwrap();
        let (db, log) = open(dir.path()).await.unwrap();
        assert_eq!(log.last_seq(), 0);
        assert_eq!(session(&db).await, Some(b"s-1".to_vec()));
        assert!(dir.path().join("agent.session.imported").exists());
    }

    #[tokio::test]
    async fn corrupt_line_stops_the_import_until_fixed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversation.jsonl");
        let good = format!("{}\n{}\n", header("old-log"), line(1, "a"));
        std::fs::write(&path, format!("{good}not json\n")).unwrap();
        std::fs::write(dir.path().join("agent.session"), "s-1").unwrap();

        let error = format!("{:#}", open(dir.path()).await.err().unwrap());
        assert!(
            error.contains(&format!("line 3 of {}", path.display()))
                && error.contains("nothing was imported")
                && error.contains("fix or delete that line"),
            "{error}"
        );
        // Nothing changed: the files are where they were, the database holds no conversation.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{good}not json\n")
        );
        assert!(dir.path().join("agent.session").exists());
        let db = fronde_db::open(dir.path()).await.unwrap();
        assert!(
            streams::Entity::find_by_id(TOPIC)
                .one(&db)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(session(&db).await, None);
        db.close().await.unwrap();

        // The owner deletes the line: the entries before it are kept and the log goes on.
        std::fs::write(&path, &good).unwrap();
        let (db, log) = open(dir.path()).await.unwrap();
        assert_eq!(log.since(0).len(), 1);
        assert_eq!(log.append(user("b")).await.unwrap().seq, 2);
        drop(log);
        assert_eq!(
            ConversationLog::open(&db, TOPIC)
                .await
                .unwrap()
                .since(0)
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn repeated_seq_stops_the_import() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversation.jsonl");
        std::fs::write(&path, format!("{}\n{}\n", line(1, "a"), line(1, "b"))).unwrap();
        let error = format!("{:#}", open(dir.path()).await.err().unwrap());
        assert!(
            error.contains(&format!("line 2 of {}", path.display())) && error.contains("seq 1"),
            "{error}"
        );
        assert!(path.exists());
    }

    #[tokio::test]
    async fn torn_trailing_line_does_not_swallow_next_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversation.jsonl");
        // The server stopped in the middle of writing entry 2.
        let torn = r#"{"seq":2,"at":"2026"#;
        std::fs::write(&path, format!("{}\n{}\n{torn}", header("l"), line(1, "a"))).unwrap();
        let (db, log) = open(dir.path()).await.unwrap();
        assert_eq!(log.append(user("b")).await.unwrap().seq, 2);
        drop(log);
        let seqs: Vec<u64> = ConversationLog::open(&db, TOPIC)
            .await
            .unwrap()
            .since(0)
            .iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(seqs, [1, 2]);

        // The old writer ended a torn line before the next entry: both entries are kept.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversation.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n{torn}\n{}\n",
                header("l"),
                line(1, "a"),
                line(2, "b")
            ),
        )
        .unwrap();
        let (_db, log) = open(dir.path()).await.unwrap();
        assert_eq!(texts(&log), [(1, "a".into()), (2, "b".into())]);
    }

    #[tokio::test]
    async fn log_without_an_id_gets_one_and_keeps_its_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversation.jsonl");
        let entry = r#"{"seq":1,"at":"2026-11-16T10:00:00Z","event":{"type":"user","text":"a"}}"#;
        std::fs::write(&path, format!("{entry}\n")).unwrap();

        let (db, log) = open(dir.path()).await.unwrap();
        let id = log.log_id().to_owned();
        assert!(id.len() >= 16, "{id}");
        assert_eq!(log.since(0).len(), 1);
        assert_eq!(log.append(user("b")).await.unwrap().seq, 2);
        drop(log);

        let log = ConversationLog::open(&db, TOPIC).await.unwrap();
        assert_eq!(log.log_id(), id);
        let seqs: Vec<u64> = log.since(0).iter().map(|e| e.seq).collect();
        assert_eq!(seqs, [1, 2]);
    }

    #[tokio::test]
    async fn import_failing_midway_leaves_the_files_and_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversation.jsonl");
        let text = format!(
            "{}\n{}\n{}\n{}\n",
            header("old-log"),
            line(1, "a"),
            line(2, "b"),
            line(3, "c")
        );
        std::fs::write(&path, &text).unwrap();
        std::fs::write(dir.path().join("agent.session"), "s-1").unwrap();
        let db = fronde_db::open(dir.path()).await.unwrap();
        // The stream row and the first entry are written when the import fails.
        db.execute_unprepared(
            "CREATE TRIGGER crash BEFORE INSERT ON stream_entries WHEN NEW.seq = 2 \
             BEGIN SELECT RAISE(ABORT, 'simulated crash'); END",
        )
        .await
        .unwrap();

        let error = format!(
            "{:#}",
            import(&db, dir.path(), TOPIC, SESSION).await.unwrap_err()
        );
        assert!(
            error.contains("nothing was imported") && error.contains("simulated crash"),
            "{error}"
        );
        assert!(
            streams::Entity::find_by_id(TOPIC)
                .one(&db)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            stream_entries::Entity::find()
                .all(&db)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(session(&db).await, None);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert!(dir.path().join("agent.session").exists());

        // The next start imports everything.
        db.execute_unprepared("DROP TRIGGER crash").await.unwrap();
        db.close().await.unwrap();
        let (db, log) = open(dir.path()).await.unwrap();
        assert_eq!(log.log_id(), "old-log");
        assert_eq!(
            texts(&log),
            [(1, "a".into()), (2, "b".into()), (3, "c".into())]
        );
        assert_eq!(session(&db).await, Some(b"s-1".to_vec()));
        assert!(!path.exists());
    }

    fn imported_name(file: &Path) -> PathBuf {
        let mut name = file.as_os_str().to_owned();
        name.push(IMPORTED);
        PathBuf::from(name)
    }

    #[tokio::test]
    async fn a_rename_lost_in_a_crash_is_finished() {
        let dir = tempfile::tempdir().unwrap();
        let (log_file, session_file) = (
            dir.path().join("conversation.jsonl"),
            dir.path().join("agent.session"),
        );
        let text = format!("{}\n{}\n", header("old-log"), line(1, "a"));
        std::fs::write(&log_file, &text).unwrap();
        std::fs::write(&session_file, "s-1\n").unwrap();
        let (db, log) = open(dir.path()).await.unwrap();
        drop(log);
        db.close().await.unwrap();

        // The import committed, then the crash came before both renames.
        std::fs::rename(imported_name(&log_file), &log_file).unwrap();
        std::fs::rename(imported_name(&session_file), &session_file).unwrap();
        let (db, log) = open(dir.path()).await.unwrap();
        assert_eq!(texts(&log), [(1, "a".into())], "nothing imported twice");
        assert!(!log_file.exists() && !session_file.exists());
        assert_eq!(
            std::fs::read_to_string(imported_name(&log_file)).unwrap(),
            text
        );
        assert_eq!(
            std::fs::read_to_string(imported_name(&session_file)).unwrap(),
            "s-1\n"
        );
        drop(log);
        db.close().await.unwrap();

        // The crash came between the two renames: the session file, held by the database, is
        // finished alone.
        std::fs::rename(imported_name(&session_file), &session_file).unwrap();
        let (db, _log) = open(dir.path()).await.unwrap();
        assert!(!session_file.exists());
        assert!(imported_name(&session_file).exists());
        assert_eq!(session(&db).await, Some(b"s-1".to_vec()));
    }

    #[tokio::test]
    async fn files_of_another_conversation_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (log_file, session_file) = (
            dir.path().join("conversation.jsonl"),
            dir.path().join("agent.session"),
        );
        std::fs::write(
            &log_file,
            format!("{}\n{}\n", header("old-log"), line(1, "a")),
        )
        .unwrap();
        std::fs::write(&session_file, "s-1").unwrap();
        let (db, log) = open(dir.path()).await.unwrap();
        drop(log);
        db.close().await.unwrap();

        for (log_text, session_text) in [
            (format!("{}\n{}\n", header("other"), line(1, "x")), "s-2"),
            // No header: nothing ties the log to the stream.
            (line(1, "a") + "\n", "s-2"),
        ] {
            std::fs::write(&log_file, &log_text).unwrap();
            std::fs::write(&session_file, session_text).unwrap();
            let (db, log) = open(dir.path()).await.unwrap();
            assert_eq!(log.log_id(), "old-log");
            assert_eq!(texts(&log), [(1, "a".into())]);
            assert_eq!(session(&db).await, Some(b"s-1".to_vec()));
            assert_eq!(std::fs::read_to_string(&log_file).unwrap(), log_text);
            assert_eq!(
                std::fs::read_to_string(&session_file).unwrap(),
                session_text
            );
            drop(log);
            db.close().await.unwrap();
        }
    }
}
