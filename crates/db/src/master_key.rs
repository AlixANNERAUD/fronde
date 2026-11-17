//! The master key that seals stored secrets ([`crate::secrets`]), and where it is kept: for now a
//! 0600 file, `master.key`, in the data directory. The keychain and systemd credentials are
//! planned. The key never comes from the environment: the agent inherits Fronde's.
//!
//! The key is written as one line, `fronde-key-v1:` followed by its 32 bytes in unpadded
//! base64url, the same in a file, a keychain entry or a systemd credential, so that anything else
//! is refused with a clear message.

use anyhow::{Context as _, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chacha20poly1305::{KeyInit as _, XChaCha20Poly1305};
use sea_orm::DatabaseConnection;
use secrecy::{ExposeSecret as _, SecretBox};
use std::fmt;
use std::io::{ErrorKind, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zeroize::Zeroizing;

/// The key file's name in the data directory.
pub const FILE_NAME: &str = "master.key";

/// What starts the key's one line, naming its format.
const PREFIX: &str = "fronde-key-v1:";

/// The key's length in unpadded base64url.
const ENCODED_LEN: usize = 43;

/// The key file in `data_dir`.
pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE_NAME)
}

/// The 256-bit key of XChaCha20-Poly1305 that seals stored secrets. It cannot be cloned: share
/// it as an `Arc<MasterKey>`. Its `Debug` prints where it came from, never the key.
pub struct MasterKey {
    key: SecretBox<[u8; 32]>,
    backend: &'static str,
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MasterKey")
            .field("backend", &self.backend)
            .finish_non_exhaustive()
    }
}

impl MasterKey {
    /// A new random key, kept by `backend`.
    pub fn generate(backend: &'static str) -> anyhow::Result<Self> {
        let mut filled = Ok(());
        let key = SecretBox::init_with_mut(|key: &mut [u8; 32]| filled = getrandom::fill(key));
        filled.map_err(|e| anyhow::anyhow!("no randomness available: {e}"))?;
        Ok(Self { key, backend })
    }

    /// The key written `fronde-key-v1:<base64url>`, with one final line break allowed. The
    /// error never quotes the text.
    pub fn parse(text: &[u8], backend: &'static str) -> anyhow::Result<Self> {
        let text = text
            .strip_suffix(b"\n")
            .map(|t| t.strip_suffix(b"\r").unwrap_or(t))
            .unwrap_or(text);
        let Some(encoded) = text.strip_prefix(PREFIX.as_bytes()) else {
            bail!("it is not a Fronde master key: it must be one line starting with {PREFIX}");
        };
        if encoded.len() != ENCODED_LEN {
            bail!(
                "it is not a Fronde master key: {PREFIX} must be followed by {ENCODED_LEN} \
                 base64url characters"
            );
        }
        // Room for the decoder's estimate (33 bytes for 43 characters).
        let mut decoded = Zeroizing::new([0u8; 36]);
        let len = URL_SAFE_NO_PAD
            .decode_slice(encoded, decoded.as_mut_slice())
            .ok()
            .filter(|&len| len == 32)
            .context("it is not a Fronde master key: what follows the prefix is not base64url")?;
        let key = SecretBox::init_with_mut(|key: &mut [u8; 32]| {
            key.copy_from_slice(&decoded[..len]);
        });
        Ok(Self { key, backend })
    }

    /// Where the key came from: `file` today.
    pub fn backend(&self) -> &'static str {
        self.backend
    }

    /// The key's one line, to store it.
    fn encode(&self) -> Zeroizing<String> {
        let mut line = Zeroizing::new(String::with_capacity(PREFIX.len() + ENCODED_LEN + 1));
        line.push_str(PREFIX);
        URL_SAFE_NO_PAD.encode_string(self.key.expose_secret(), &mut line);
        line.push('\n');
        line
    }

    pub(crate) fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(self.key.expose_secret().into())
    }
}

/// The key in `file`, or `None` when there is no such file. A key file that other users could
/// read, or that another user owns, is refused.
pub fn read(file: &Path) -> anyhow::Result<Option<MasterKey>> {
    let shown = file.display();
    let mut opened = match std::fs::File::open(file) {
        Ok(opened) => opened,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read the master key {shown}")),
    };
    check_private(&opened, file)?;
    // A key file is one short line and its line break: read one byte more, enough to refuse a
    // longer file, into a buffer that never grows.
    let limit = PREFIX.len() + ENCODED_LEN + 3;
    let mut text = Zeroizing::new(Vec::with_capacity(limit + 1));
    (&mut opened)
        .take(limit as u64)
        .read_to_end(&mut text)
        .with_context(|| format!("cannot read the master key {shown}"))?;
    MasterKey::parse(&text, "file")
        .map(Some)
        .with_context(|| format!("the master key {shown} is unreadable; restore it from a backup"))
}

#[cfg(unix)]
fn check_private(opened: &std::fs::File, file: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = opened
        .metadata()
        .with_context(|| format!("cannot read the master key {}", file.display()))?;
    if meta.mode() & 0o077 != 0 {
        bail!(
            "the master key {0} is open to other users; run chmod 600 {0}",
            file.display()
        );
    }
    if meta.uid() != rustix::process::geteuid().as_raw() {
        bail!(
            "the master key {} is not owned by the user running Fronde; run chown to that user",
            file.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private(_opened: &std::fs::File, _file: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// Write a new key to `file`, 0600, unless the file exists. Returns the key in the file: the new
/// one, or the one another writer put there first.
///
/// The key is written and synced to a temporary file, which is then linked to `file`: the link
/// fails if `file` exists, and a crash never leaves `file` empty or half written.
fn create(file: &Path) -> anyhow::Result<MasterKey> {
    let key = MasterKey::generate("file")?;
    let shown = file.display();
    let mut staged = file.as_os_str().to_owned();
    staged.push(format!(".{}.new", std::process::id()));
    let staged = PathBuf::from(staged);
    // Left by a crash of an earlier process with the same id, if any.
    let _ = std::fs::remove_file(&staged);
    let written = fronde_fs::private_options()
        .write(true)
        .create_new(true)
        .open(&staged)
        .and_then(|mut opened| {
            opened.write_all(key.encode().as_bytes())?;
            opened.sync_all()
        });
    let linked = written.and_then(|()| std::fs::hard_link(&staged, file));
    // The staged name goes whatever happened; the key stays in `file` once linked.
    let _ = std::fs::remove_file(&staged);
    match linked {
        Ok(()) => {
            sync_dir(file).with_context(|| format!("cannot write the master key {shown}"))?;
            Ok(key)
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            read(file)?.with_context(|| format!("the master key {shown} vanished"))
        }
        Err(e) => Err(e).with_context(|| format!("cannot create the master key {shown}")),
    }
}

/// Sync the directory of `file`, so that its new name survives a power cut.
fn sync_dir(file: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    if let Some(dir) = file.parent() {
        std::fs::File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

/// The key that opens the stored secrets, from `<data_dir>/master.key`, checked against the
/// database. `None` when the file is missing and nothing is stored. A missing file while secrets
/// are stored, or a key that does not open them, is an error that says so.
pub async fn load(
    db: &DatabaseConnection,
    data_dir: &Path,
) -> anyhow::Result<Option<Arc<MasterKey>>> {
    let file = path(data_dir);
    match read(&file)? {
        Some(key) => {
            crate::secrets::check_key(db, &key)
                .await
                .with_context(|| format!("master key {}", file.display()))?;
            Ok(Some(Arc::new(key)))
        }
        None if crate::secrets::any_stored(db).await? => bail!(
            "the master key {} is missing, but the database holds secrets sealed with it; \
             restore the file from a backup. Fronde does not make a new key while secrets are \
             stored: it could not open them",
            file.display()
        ),
        None => Ok(None),
    }
}

/// [`load`], or, when nothing is stored and there is no key file yet, a new key in a new file.
/// The flag says whether the key was created now.
pub async fn load_or_create(
    db: &DatabaseConnection,
    data_dir: &Path,
) -> anyhow::Result<(Arc<MasterKey>, bool)> {
    if let Some(key) = load(db, data_dir).await? {
        return Ok((key, false));
    }
    let key = create(&path(data_dir))?;
    Ok((Arc::new(key), true))
}
