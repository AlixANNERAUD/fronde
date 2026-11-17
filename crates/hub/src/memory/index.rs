//! Full-text index of memory files, one row per non-empty line, in the database's `memory_fts`
//! table (FTS5; raw SQL, as SeaORM has no FTS5 support).

use crate::search::fts_query;
use fronde_db::DatabaseConnection;
use sea_orm::{ConnectionTrait, DbBackend, Statement, Value};
use serde::Serialize;
use std::path::Path;

pub struct MemoryIndex {
    db: DatabaseConnection,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    /// Path relative to the memory directory, with '/'.
    pub file: String,
    pub line: usize,
    pub text: String,
}

fn relative(root: &Path, file: &Path) -> String {
    let rel = file.strip_prefix(root).unwrap_or(file);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn statement(sql: &str, values: impl IntoIterator<Item = Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Sqlite, sql, values)
}

/// What identifies one version of a memory file. Memory writes replace files atomically and a
/// hand edit changes the modification time, so the same stamp means the same content.
#[derive(Debug, PartialEq)]
struct Stamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    ino: u64,
}

impl Stamp {
    fn of(meta: &std::fs::Metadata) -> Self {
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            #[cfg(unix)]
            ino: std::os::unix::fs::MetadataExt::ino(meta),
        }
    }
}

/// A file's content as read, with the stamp of the version read (`None`: unreadable or missing,
/// indexed as empty).
struct Snapshot {
    content: String,
    stamp: Option<Stamp>,
}

impl Snapshot {
    /// Read `file`, taking the stamp from the open file so it names the version read.
    async fn read(file: &Path) -> Self {
        let file = file.to_path_buf();
        let read = tokio::task::spawn_blocking(move || -> std::io::Result<Self> {
            let mut f = std::fs::File::open(&file)?;
            let stamp = Stamp::of(&f.metadata()?);
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut f, &mut bytes)?;
            Ok(Self {
                content: String::from_utf8_lossy(&bytes).into_owned(),
                stamp: Some(stamp),
            })
        })
        .await;
        match read {
            Ok(Ok(snapshot)) => snapshot,
            _ => Self {
                content: String::new(),
                stamp: None,
            },
        }
    }

    /// The file as it is now: `self` when it has not changed since it was read, else a new read.
    /// Called under the write lock, so of two writes racing on one file, the last to index it
    /// indexes its latest content; only a race pays for a read under the lock.
    async fn current(self, file: &Path) -> Self {
        let now = tokio::fs::metadata(file).await.ok().map(|m| Stamp::of(&m));
        if now == self.stamp {
            self
        } else {
            Self::read(file).await
        }
    }
}

/// Replace the rows of `file` with the lines of `snapshot`, inside the caller's transaction.
async fn index_file<C: ConnectionTrait>(
    db: &C,
    root: &Path,
    file: &Path,
    snapshot: Snapshot,
) -> anyhow::Result<()> {
    let rel = relative(root, file);
    let content = snapshot.current(file).await.content;
    db.execute_raw(statement(
        "DELETE FROM memory_fts WHERE file = ?",
        [rel.clone().into()],
    ))
    .await?;
    for (i, line) in content.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if !line.trim().is_empty() {
            db.execute_raw(statement(
                "INSERT INTO memory_fts(file, line, text) VALUES (?, ?, ?)",
                [rel.clone().into(), (i as i64 + 1).into(), line.into()],
            ))
            .await?;
        }
    }
    Ok(())
}

impl MemoryIndex {
    /// The index in `db`, whose migrations created the table.
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn reindex_file(&self, root: &Path, file: &Path) -> anyhow::Result<()> {
        // Read before taking the write lock, which every database writer waits for.
        let snapshot = Snapshot::read(file).await;
        let tx = fronde_db::begin_write(&self.db).await?;
        index_file(&tx, root, file, snapshot).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Rebuild the whole index from the files, in one transaction.
    pub async fn reindex_all(&self, root: &Path) -> anyhow::Result<()> {
        let mut files = vec![root.join("USER.md"), root.join("MEMORY.md")];
        if let Ok(entries) = std::fs::read_dir(root.join("journal")) {
            files.extend(
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "md")),
            );
        }
        let mut snapshots = Vec::new();
        for f in files.into_iter().filter(|f| f.exists()) {
            let snapshot = Snapshot::read(&f).await;
            snapshots.push((f, snapshot));
        }
        let tx = fronde_db::begin_write(&self.db).await?;
        tx.execute_unprepared("DELETE FROM memory_fts").await?;
        for (f, snapshot) in snapshots {
            index_file(&tx, root, &f, snapshot).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn search(&self, query: &str, limit: usize) -> Vec<Hit> {
        let Some(q) = fts_query(query) else {
            return Vec::new();
        };
        let rows = match self
            .db
            .query_all_raw(statement(
                "SELECT file, line, text FROM memory_fts WHERE memory_fts MATCH ? \
                 ORDER BY bm25(memory_fts) LIMIT ?",
                [q.into(), (limit as i64).into()],
            ))
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("memory search: {e}");
                return Vec::new();
            }
        };
        rows.iter()
            .filter_map(|r| {
                Some(Hit {
                    file: r.try_get_by_index(0).ok()?,
                    line: r.try_get_by_index::<i64>(1).ok()? as usize,
                    text: r.try_get_by_index(2).ok()?,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reindexes_hand_edited_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("journal")).unwrap();
        std::fs::write(
            dir.path().join("MEMORY.md"),
            "- has a cat named Tom\r\n\r\n- likes tea\r\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("journal/2026-01-01.md"),
            "- 10:00 fed the cat\n",
        )
        .unwrap();
        let idx = MemoryIndex::new(fronde_db::open(dir.path()).await.unwrap());
        idx.reindex_all(dir.path()).await.unwrap();
        let hits = idx.search("cat", 10).await;
        assert_eq!(hits.len(), 2);
        let mem = hits.iter().find(|h| h.file == "MEMORY.md").unwrap();
        assert_eq!((mem.line, mem.text.as_str()), (1, "- has a cat named Tom"));
        let j = hits
            .iter()
            .find(|h| h.file == "journal/2026-01-01.md")
            .unwrap();
        assert_eq!(j.line, 1);
        assert_eq!(idx.search("tea", 10).await[0].line, 3);
        // Reindexing twice does not duplicate.
        idx.reindex_all(dir.path()).await.unwrap();
        assert_eq!(idx.search("cat", 10).await.len(), 2);
    }

    #[tokio::test]
    async fn a_file_changed_after_its_read_is_indexed_as_it_is_now() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("MEMORY.md");
        std::fs::write(&file, "- likes tea\n").unwrap();
        let idx = MemoryIndex::new(fronde_db::open(dir.path()).await.unwrap());
        let stale = Snapshot::read(&file).await;
        // Another write replaces the file, as memory writes do, between the read and the lock.
        let next = dir.path().join("MEMORY.md.next");
        std::fs::write(&next, "- likes coffee\n").unwrap();
        std::fs::rename(&next, &file).unwrap();
        let tx = fronde_db::begin_write(&idx.db).await.unwrap();
        index_file(&tx, dir.path(), &file, stale).await.unwrap();
        tx.commit().await.unwrap();
        assert!(idx.search("tea", 10).await.is_empty());
        assert_eq!(idx.search("coffee", 10).await[0].text, "- likes coffee");
    }

    #[tokio::test]
    async fn an_unchanged_file_is_indexed_from_its_read() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("MEMORY.md");
        std::fs::write(&file, "- likes tea\n").unwrap();
        let snapshot = Snapshot::read(&file).await;
        assert_eq!(snapshot.content, "- likes tea\n");
        let kept = snapshot.current(&file).await;
        assert_eq!(kept.content, "- likes tea\n");
        // A missing file reads as empty, and stays so.
        let missing = Snapshot::read(&dir.path().join("USER.md")).await;
        assert!(missing.stamp.is_none() && missing.content.is_empty());
    }
}
