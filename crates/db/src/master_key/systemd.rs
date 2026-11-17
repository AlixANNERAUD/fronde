//! The master key as a systemd credential: `LoadCredentialEncrypted=fronde-master-key:…` in the
//! unit makes systemd decrypt it into `$CREDENTIALS_DIRECTORY/fronde-master-key` for the service
//! alone. Fronde only reads that file: it never runs `systemd-creds` and never writes there.

use super::{Backend, MasterKey};
use anyhow::Context as _;
use std::path::{Path, PathBuf};

/// The credential's name, in `LoadCredentialEncrypted=` and in the credentials directory.
pub const CREDENTIAL: &str = "fronde-master-key";

/// The credential's file in `dir`, the service's `$CREDENTIALS_DIRECTORY`.
pub(super) fn path(dir: &Path) -> PathBuf {
    dir.join(CREDENTIAL)
}

/// The key in the credential, or `None` when there is no credentials directory or no such
/// credential in it. The directory is systemd's, private to the service, and systemd sets the
/// file's owner and mode: only a plain file is required.
pub(super) fn read(dir: Option<&Path>) -> anyhow::Result<Option<MasterKey>> {
    let Some(dir) = dir else {
        return Ok(None);
    };
    let file = path(dir);
    let Some(text) = super::read_line(&file, false)? else {
        return Ok(None);
    };
    MasterKey::parse(&text, Backend::Systemd)
        .map(Some)
        .with_context(|| {
            format!(
                "the systemd credential {} is unreadable; encrypt the key line again with \
                 `systemd-creds encrypt --name={CREDENTIAL}`",
                file.display()
            )
        })
}
