//! Full-text index of memory files, one row per non-empty line.

use crate::search::fts_query;
use rusqlite::{Connection, params};
use serde::Serialize;
use std::path::Path;
use std::sync::Mutex;

pub struct MemoryIndex {
    db: Mutex<Connection>,
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
    rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

impl MemoryIndex {
    pub fn open(db: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(db)?;
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS m USING fts5(file UNINDEXED, line UNINDEXED, text, \
             tokenize='unicode61 remove_diacritics 2');",
        )?;
        Ok(Self { db: Mutex::new(conn) })
    }

    pub fn reindex_file(&self, root: &Path, file: &Path) -> anyhow::Result<()> {
        let rel = relative(root, file);
        let content = std::fs::read(file).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        let mut db = self.db.lock().expect("index lock");
        let tx = db.transaction()?;
        tx.execute("DELETE FROM m WHERE file = ?1", params![rel])?;
        for (i, line) in content.lines().enumerate() {
            let line = line.trim_end_matches('\r');
            if !line.trim().is_empty() {
                tx.execute("INSERT INTO m(file, line, text) VALUES (?1, ?2, ?3)", params![rel, i as i64 + 1, line])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn reindex_all(&self, root: &Path) -> anyhow::Result<()> {
        self.db.lock().expect("index lock").execute("DELETE FROM m", [])?;
        let mut files = vec![root.join("USER.md"), root.join("MEMORY.md")];
        if let Ok(entries) = std::fs::read_dir(root.join("journal")) {
            files.extend(entries.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "md")));
        }
        for f in files.iter().filter(|f| f.exists()) {
            self.reindex_file(root, f)?;
        }
        Ok(())
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<Hit> {
        let Some(q) = fts_query(query) else { return Vec::new() };
        let db = self.db.lock().expect("index lock");
        let Ok(mut stmt) = db.prepare("SELECT file, line, text FROM m WHERE m MATCH ?1 ORDER BY bm25(m) LIMIT ?2") else {
            return Vec::new();
        };
        stmt.query_map(params![q, limit as i64], |r| {
            Ok(Hit { file: r.get(0)?, line: r.get::<_, i64>(1)? as usize, text: r.get(2)? })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reindexes_hand_edited_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("journal")).unwrap();
        std::fs::write(dir.path().join("MEMORY.md"), "- has a cat named Tom\r\n\r\n- likes tea\r\n").unwrap();
        std::fs::write(dir.path().join("journal/2026-01-01.md"), "- 10:00 fed the cat\n").unwrap();
        let idx = MemoryIndex::open(&dir.path().join("index.sqlite")).unwrap();
        idx.reindex_all(dir.path()).unwrap();
        let hits = idx.search("cat", 10);
        assert_eq!(hits.len(), 2);
        let mem = hits.iter().find(|h| h.file == "MEMORY.md").unwrap();
        assert_eq!((mem.line, mem.text.as_str()), (1, "- has a cat named Tom"));
        let j = hits.iter().find(|h| h.file == "journal/2026-01-01.md").unwrap();
        assert_eq!(j.line, 1);
        assert_eq!(idx.search("tea", 10)[0].line, 3);
        // Reindexing twice does not duplicate.
        idx.reindex_all(dir.path()).unwrap();
        assert_eq!(idx.search("cat", 10).len(), 2);
    }
}
