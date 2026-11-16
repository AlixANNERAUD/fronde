//! Private-file helpers live in `fronde-fs`; this adds the check on secret files Fronde reads.

use std::fs;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

pub use fronde_fs::*;

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
