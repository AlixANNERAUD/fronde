//! Keeps Fronde's own files private to the current user (Unix only).
//!
//! The data directory holds personal data, so directories are created 0700 and files 0600
//! whatever the umask says.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

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

/// Replace `path` through a private temporary file, so a crash never leaves half a file.
pub fn write_atomic(path: &Path, tmp: &Path, content: impl AsRef<[u8]>) -> std::io::Result<()> {
    write(tmp, content)?;
    fs::rename(tmp, path)
}

/// Refuse a secret file that other users can read, write or own.
pub fn check_secret_file(path: &Path) -> anyhow::Result<()> {
    let allow = std::env::var("FRONDE_ALLOW_SHARED_SECRET_FILES").as_deref() == Ok("1");
    check_secret_file_with(path, allow)
}

fn check_secret_file_with(path: &Path, allow_shared: bool) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        if allow_shared {
            return Ok(());
        }
        let meta = fs::metadata(path)
            .map_err(|e| anyhow::anyhow!("cannot read secret file {}: {e}", path.display()))?;
        if meta.mode() & 0o077 != 0 {
            anyhow::bail!(
                "secret file {0} is readable by other users; run chmod 600 {0}",
                path.display()
            );
        }
        if meta.uid() != rustix::process::geteuid().as_raw() {
            anyhow::bail!(
                "secret file {0} is not owned by the current user; run chown to your user, \
                 or set FRONDE_ALLOW_SHARED_SECRET_FILES=1 for Docker secrets",
                path.display()
            );
        }
    }
    #[cfg(not(unix))]
    let _ = (path, allow_shared);
    Ok(())
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
    fn secret_file_modes() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("s");
        fs::write(&f, "x").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();
        let e = check_secret_file_with(&f, false).unwrap_err().to_string();
        assert_eq!(
            e,
            format!(
                "secret file {0} is readable by other users; run chmod 600 {0}",
                f.display()
            )
        );
        assert!(check_secret_file_with(&f, true).is_ok());
        fs::set_permissions(&f, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(check_secret_file_with(&f, false).is_ok());
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
}
