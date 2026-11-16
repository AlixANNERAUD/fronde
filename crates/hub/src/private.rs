//! Private-file helpers live in `fronde-fs`; this adds the check on secret files Fronde reads.

use std::fs;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

pub use fronde_fs::*;

/// Create the data directory (0700), make sure it is private, and prove it is writable by
/// creating and removing a probe file. Used by `fronde check` and at start, so a directory the
/// user cannot write fails early with the fix.
pub fn prepare_data_dir(dir: &Path) -> anyhow::Result<()> {
    let fix = "set [data] dir to a directory you own, e.g. ~/.local/share/fronde";
    fronde_fs::create_dir_all(dir)
        .map_err(|e| anyhow::anyhow!("cannot create data dir {}: {e}; {fix}", dir.display()))?;
    fronde_fs::tighten_dir(dir)
        .map_err(|e| anyhow::anyhow!("cannot restrict data dir {}: {e}; {fix}", dir.display()))?;
    let probe = dir.join(format!(".fronde-write-test-{}", std::process::id()));
    fronde_fs::write(&probe, b"")
        .and_then(|()| fs::remove_file(&probe))
        .map_err(|e| anyhow::anyhow!("data dir {} is not writable: {e}; {fix}", dir.display()))
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
    use std::os::unix::fs::PermissionsExt;

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
}
