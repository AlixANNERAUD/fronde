//! Keeps Fronde's own files private to the current user (Unix only) and durable.
//!
//! The data directory holds personal data, so directories are created 0700 and files 0600
//! whatever the umask says.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

use atomic_write_file::AtomicWriteFile;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

/// Create `dir` and its missing parents with mode 0700.
pub fn create_dir_all(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

/// Make an existing directory of ours 0700, warning when it was open to others.
pub fn tighten_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mode = fs::metadata(dir)?.permissions().mode();
        if mode & 0o077 != 0 {
            tracing::warn!(
                "data directory {} had mode {:o}; restricting it to 0700",
                dir.display(),
                mode & 0o777
            );
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Options for a file created 0600 (an existing file keeps its mode, see `restrict_file`).
pub fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    options.mode(0o600);
    options
}

/// Make an existing file 0600.
pub fn restrict_file(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Write a new or replaced file with mode 0600, never readable by others even briefly.
pub fn write(path: impl AsRef<Path>, content: impl AsRef<[u8]>) -> std::io::Result<()> {
    let path = path.as_ref();
    let mut file: File = private_options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    restrict_file(path)?;
    file.write_all(content.as_ref())
}

/// Create an empty 0600 file unless one exists, and make an existing one 0600.
pub fn write_if_missing(path: &Path) -> std::io::Result<()> {
    private_options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    restrict_file(path)
}

/// Replace `path` with `content`, with Unix permissions `mode` whatever the old mode was. The
/// content goes to a temporary file that is synced, renamed over `path`, and the directory is
/// synced too: after a power cut `path` holds the old content or the new one, never an empty or
/// missing file.
pub fn write_atomic_mode(
    path: impl AsRef<Path>,
    content: impl AsRef<[u8]>,
    mode: u32,
) -> std::io::Result<()> {
    let mut options = AtomicWriteFile::options();
    #[cfg(unix)]
    {
        use atomic_write_file::unix::OpenOptionsExt as _;
        options.mode(mode).preserve_mode(false);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = options.open(path.as_ref())?;
    file.write_all(content.as_ref())?;
    #[cfg(unix)]
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    file.commit()
}

/// Replace `path` with `content`, 0600 whatever the old mode was. The content goes to a temporary
/// file that is synced, renamed over `path`, and the directory is synced too: after a power cut
/// `path` holds the old content or the new one, never an empty or missing file.
pub fn write_atomic(path: impl AsRef<Path>, content: impl AsRef<[u8]>) -> std::io::Result<()> {
    write_atomic_mode(path, content, 0o600)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn dirs_are_0700_and_files_0600() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("a/b");
        create_dir_all(&d).unwrap();
        assert_eq!(mode(&d), 0o700);
        assert_eq!(mode(&t.path().join("a")), 0o700);
        let f = d.join("f");
        write(&f, "x").unwrap();
        assert_eq!(mode(&f), 0o600);
    }

    #[test]
    fn tighten_fixes_open_dir() {
        let t = tempfile::tempdir().unwrap();
        fs::set_permissions(t.path(), fs::Permissions::from_mode(0o755)).unwrap();
        tighten_dir(t.path()).unwrap();
        assert_eq!(mode(t.path()), 0o700);
    }

    #[test]
    fn write_restricts_existing_file() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("f");
        fs::write(&f, "old").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();
        write(&f, "new").unwrap();
        assert_eq!(mode(&f), 0o600);
    }

    #[test]
    fn atomic_write_replaces_content_and_keeps_0600() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("MEMORY.md");
        fs::write(&f, "old").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();
        let content = "- one\n- two\n".repeat(1000);
        write_atomic(&f, &content).unwrap();
        assert_eq!(mode(&f), 0o600);
        assert_eq!(fs::read_to_string(&f).unwrap(), content);
        let names: Vec<_> = fs::read_dir(t.path()).unwrap().collect();
        assert_eq!(names.len(), 1, "no temporary file is left behind");
    }

    #[test]
    fn atomic_write_creates_a_missing_file_0600() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("agent.session");
        write_atomic(&f, "id").unwrap();
        assert_eq!(mode(&f), 0o600);
        assert_eq!(fs::read_to_string(&f).unwrap(), "id");
    }

    #[test]
    fn atomic_write_mode_sets_requested_permissions() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("fronde.service");
        write_atomic_mode(&f, "[Unit]\n", 0o644).unwrap();
        assert_eq!(mode(&f), 0o644);
        assert_eq!(fs::read_to_string(&f).unwrap(), "[Unit]\n");
    }
}
