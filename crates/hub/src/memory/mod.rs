//! Memory files: USER.md, MEMORY.md (capped core) and a dated journal.

use crate::config::MemoryConfig;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use std::fmt;
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;

pub mod git;
pub mod index;
pub mod tools;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MemoryFile {
    User,
    Memory,
    Journal,
}

#[derive(Debug)]
pub enum MemoryError {
    Full { file: &'static str, would_be: usize, limit: usize },
    NotFound,
    Ambiguous(usize),
    Empty,
    AppendOnly,
    Io(std::io::Error),
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoryError::Full { file, would_be, limit } => write!(
                f,
                "{file} would be {would_be}/{limit} characters; merge or remove lines with memory_replace first"
            ),
            MemoryError::NotFound => write!(f, "text not found; copy it exactly from the memory"),
            MemoryError::Ambiguous(n) => {
                write!(f, "text found {n} times; include more of the line so it is unique")
            }
            MemoryError::Empty => write!(f, "text is empty"),
            MemoryError::AppendOnly => write!(f, "the journal is append-only; use memory_add with file = journal"),
            MemoryError::Io(e) => write!(f, "memory storage error: {e}"),
        }
    }
}

impl std::error::Error for MemoryError {}

impl From<std::io::Error> for MemoryError {
    fn from(e: std::io::Error) -> Self {
        MemoryError::Io(e)
    }
}

pub struct Core {
    pub user: String,
    pub memory: String,
}

pub struct MemoryStore {
    dir: PathBuf,
    memory_limit: usize,
    user_limit: usize,
    tz: Tz,
    lock: Mutex<()>,
}

fn read(path: &Path) -> String {
    std::fs::read(path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
}

fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(tmp, path)
}

impl MemoryStore {
    pub fn open(dir: &Path, cfg: &MemoryConfig, tz: Tz) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir.join("journal"))?;
        for f in ["USER.md", "MEMORY.md"] {
            let p = dir.join(f);
            if !p.exists() {
                std::fs::write(p, "")?;
            }
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            memory_limit: cfg.memory_limit,
            user_limit: cfg.user_limit,
            tz,
            lock: Mutex::new(()),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path, display name and cap of a capped file.
    fn capped(&self, file: MemoryFile) -> Option<(PathBuf, &'static str, usize)> {
        match file {
            MemoryFile::User => Some((self.dir.join("USER.md"), "USER.md", self.user_limit)),
            MemoryFile::Memory => Some((self.dir.join("MEMORY.md"), "MEMORY.md", self.memory_limit)),
            MemoryFile::Journal => None,
        }
    }

    pub fn limit(&self, file: MemoryFile) -> Option<usize> {
        self.capped(file).map(|(_, _, l)| l)
    }

    /// Read the core without waiting for writers (files are replaced atomically).
    pub fn core_now(&self) -> Core {
        Core { user: read(&self.dir.join("USER.md")), memory: read(&self.dir.join("MEMORY.md")) }
    }

    pub async fn core(&self) -> Core {
        let _g = self.lock.lock().await;
        Core { user: read(&self.dir.join("USER.md")), memory: read(&self.dir.join("MEMORY.md")) }
    }

    pub async fn add(&self, file: MemoryFile, text: &str, now: DateTime<Utc>) -> Result<PathBuf, MemoryError> {
        let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            return Err(MemoryError::Empty);
        }
        let _g = self.lock.lock().await;
        match self.capped(file) {
            Some((path, name, limit)) => {
                let mut content = read(&path);
                if !content.is_empty() && !content.ends_with('\n') {
                    content.push('\n');
                }
                content.push_str(&format!("- {line}\n"));
                let would_be = content.chars().count();
                if would_be > limit {
                    return Err(MemoryError::Full { file: name, would_be, limit });
                }
                write_atomic(&path, &content)?;
                Ok(path)
            }
            None => {
                let local = now.with_timezone(&self.tz);
                let path = self.dir.join("journal").join(format!("{}.md", local.format("%Y-%m-%d")));
                let mut content = read(&path);
                if !content.is_empty() && !content.ends_with('\n') {
                    content.push('\n');
                }
                content.push_str(&format!("- {} {line}\n", local.format("%H:%M")));
                write_atomic(&path, &content)?;
                Ok(path)
            }
        }
    }

    pub async fn replace(&self, file: MemoryFile, old: &str, new: &str) -> Result<PathBuf, MemoryError> {
        let Some((path, name, limit)) = self.capped(file) else { return Err(MemoryError::AppendOnly) };
        if old.is_empty() {
            return Err(MemoryError::Empty);
        }
        let _g = self.lock.lock().await;
        let content = read(&path);
        let count = content.matches(old).count();
        let start = match count {
            0 => return Err(MemoryError::NotFound),
            1 => content.find(old).expect("counted once"),
            n => return Err(MemoryError::Ambiguous(n)),
        };
        let updated = if new.is_empty() {
            let end = start + old.len();
            let line_start = content[..start].rfind('\n').map_or(0, |i| i + 1);
            let line_end = content[end..].find('\n').map_or(content.len(), |i| end + i + 1);
            format!("{}{}", &content[..line_start], &content[line_end..])
        } else {
            content.replacen(old, new, 1)
        };
        let would_be = updated.chars().count();
        if would_be > limit {
            return Err(MemoryError::Full { file: name, would_be, limit });
        }
        write_atomic(&path, &updated)?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MemoryConfig;
    use chrono::TimeZone;
    use std::sync::Arc;

    fn store(limit: usize) -> (tempfile::TempDir, MemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = MemoryConfig { memory_limit: limit, user_limit: limit, git: false };
        let s = MemoryStore::open(dir.path(), &cfg, chrono_tz::Europe::Paris).unwrap();
        (dir, s)
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 16, 9, 0, 0).unwrap()
    }

    #[tokio::test]
    async fn open_creates_files() {
        let (dir, _s) = store(100);
        assert!(dir.path().join("USER.md").exists());
        assert!(dir.path().join("MEMORY.md").exists());
        assert!(dir.path().join("journal").is_dir());
    }

    #[tokio::test]
    async fn add_appends_bullet() {
        let (_d, s) = store(100);
        s.add(MemoryFile::Memory, "likes tea", now()).await.unwrap();
        s.add(MemoryFile::User, "is called Alix", now()).await.unwrap();
        let core = s.core().await;
        assert_eq!(core.memory, "- likes tea\n");
        assert_eq!(core.user, "- is called Alix\n");
    }

    #[tokio::test]
    async fn add_multiline_text_is_flattened() {
        let (_d, s) = store(100);
        s.add(MemoryFile::Memory, "a\r\nb\nc", now()).await.unwrap();
        assert_eq!(s.core().await.memory, "- a b c\n");
        assert!(matches!(s.add(MemoryFile::Memory, "  \n", now()).await, Err(MemoryError::Empty)));
    }

    #[tokio::test]
    async fn full_error_tells_how_to_fix() {
        let (_d, s) = store(50);
        s.add(MemoryFile::Memory, &"x".repeat(40), now()).await.unwrap();
        let e = s.add(MemoryFile::Memory, "one more fact", now()).await.unwrap_err().to_string();
        assert!(e.contains("MEMORY.md") && e.contains("50") && e.contains("memory_replace"), "{e}");
    }

    #[tokio::test]
    async fn cap_counts_chars_not_bytes() {
        let (_d, s) = store(45);
        s.add(MemoryFile::Memory, &"é".repeat(40), now()).await.unwrap();
    }

    #[tokio::test]
    async fn replace_unique_ambiguous_missing_and_delete_line() {
        let (_d, s) = store(200);
        for t in ["likes tea", "likes coffee", "owns a cat"] {
            s.add(MemoryFile::Memory, t, now()).await.unwrap();
        }
        s.replace(MemoryFile::Memory, "tea", "green tea").await.unwrap();
        assert!(matches!(s.replace(MemoryFile::Memory, "likes", "x").await, Err(MemoryError::Ambiguous(2))));
        assert!(matches!(s.replace(MemoryFile::Memory, "dog", "x").await, Err(MemoryError::NotFound)));
        s.replace(MemoryFile::Memory, "a cat", "").await.unwrap();
        assert_eq!(s.core().await.memory, "- likes green tea\n- likes coffee\n");
        let e = s.replace(MemoryFile::Journal, "a", "b").await.unwrap_err().to_string();
        assert!(e.contains("append-only"), "{e}");
    }

    #[tokio::test]
    async fn journal_uses_local_date() {
        let (dir, s) = store(10);
        let t = Utc.with_ymd_and_hms(2026, 11, 16, 23, 30, 0).unwrap();
        let p = s.add(MemoryFile::Journal, "went to bed late", t).await.unwrap();
        assert_eq!(p, dir.path().join("journal/2026-11-17.md"));
        assert_eq!(std::fs::read_to_string(p).unwrap(), "- 00:30 went to bed late\n");
    }

    #[tokio::test]
    async fn concurrent_adds_respect_cap() {
        let (_d, s) = store(200);
        let s = Arc::new(s);
        let tasks: Vec<_> = (0..50)
            .map(|i| {
                let s = s.clone();
                tokio::spawn(async move { s.add(MemoryFile::Memory, &format!("fact {i:03}"), now()).await.is_ok() })
            })
            .collect();
        let mut oks = 0;
        for t in tasks {
            oks += t.await.unwrap() as usize;
        }
        let mem = s.core().await.memory;
        assert!(mem.chars().count() <= 200);
        assert_eq!(mem.lines().count(), oks);
        assert!(oks > 0);
    }
}
