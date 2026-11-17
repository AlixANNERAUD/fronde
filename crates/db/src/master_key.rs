//! The master key that seals stored secrets ([`crate::secrets`]), and where it is kept: a
//! systemd credential, the platform's keychain, or a 0600 file, by default `master.key` beside
//! the configuration file, never in the data directory, so that a backup of the data directory or
//! a snapshot of its volume does not hold the key with the secrets it opens. Earlier versions kept
//! the file in the data directory: [`migrate`] moves it. [`Backends`] picks the place and keeps
//! it. The key never comes from the environment, which the processes Fronde starts may inherit.
//!
//! The key is written as one line, `fronde-key-v1:` followed by its 32 bytes in unpadded
//! base64url, the same in a file, a keychain entry or a systemd credential, so that anything else
//! is refused with a clear message.

mod backends;
mod keychain;
mod systemd;

pub(crate) use backends::parse_recorded;
pub use backends::{Backends, Choice, KeyLock, LOCK_FILE};
#[cfg(feature = "testing")]
pub use keychain::FakeKeychain;
pub use keychain::Keychain;
pub use systemd::CREDENTIAL;

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

/// The key file's name: beside the configuration by default, in the data directory before.
pub const FILE_NAME: &str = "master.key";

/// What starts the key's one line, naming its format.
const PREFIX: &str = "fronde-key-v1:";

/// The key's length in unpadded base64url.
const ENCODED_LEN: usize = 43;

/// Where earlier versions kept the key: in the data directory, where [`migrate`] looks for it.
pub fn legacy_path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE_NAME)
}

/// Where a master key is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// `$CREDENTIALS_DIRECTORY/fronde-master-key`, which systemd decrypts for the service.
    Systemd,
    /// The platform's keychain: Secret Service, the macOS Keychain or the Windows Credential
    /// Manager.
    Keychain,
    /// A 0600 file, `master.key` beside the configuration by default.
    File,
}

impl Backend {
    /// The name in `[secrets] backend` and in the database's record of where the key is.
    pub fn name(self) -> &'static str {
        match self {
            Self::Systemd => "systemd",
            Self::Keychain => "keychain",
            Self::File => "file",
        }
    }

    /// The backend named `name`, as [`Backend::name`] writes it.
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::Systemd, Self::Keychain, Self::File]
            .into_iter()
            .find(|b| b.name() == name)
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The 256-bit key of XChaCha20-Poly1305 that seals stored secrets. It cannot be cloned: share
/// it as an `Arc<MasterKey>`. Its `Debug` prints where it came from, never the key.
pub struct MasterKey {
    key: SecretBox<[u8; 32]>,
    backend: Backend,
    /// The keychain entry's user it was read from or written to, for a key in the keychain.
    entry: Option<String>,
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MasterKey")
            .field("backend", &self.backend.name())
            .field("entry", &self.entry)
            .finish_non_exhaustive()
    }
}

impl MasterKey {
    /// A new random key, kept by `backend`.
    pub fn generate(backend: Backend) -> anyhow::Result<Self> {
        let mut filled = Ok(());
        let key = SecretBox::init_with_mut(|key: &mut [u8; 32]| filled = getrandom::fill(key));
        filled.map_err(|e| anyhow::anyhow!("no randomness available: {e}"))?;
        Ok(Self {
            key,
            backend,
            entry: None,
        })
    }

    /// The key written `fronde-key-v1:<base64url>`, with one final line break allowed. The
    /// error never quotes the text.
    pub fn parse(text: &[u8], backend: Backend) -> anyhow::Result<Self> {
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
        Ok(Self {
            key,
            backend,
            entry: None,
        })
    }

    /// Where the key came from.
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// For a key in the keychain, the user of its entry, which the database records with the
    /// first secret ([`crate::secrets::set`]).
    pub fn keychain_entry(&self) -> Option<&str> {
        self.entry.as_deref()
    }

    /// This key, kept in the keychain entry of `user`.
    fn in_entry(mut self, user: &str) -> Self {
        self.entry = Some(user.to_owned());
        self
    }

    /// The key's one line, to store it.
    pub(crate) fn encode(&self) -> Zeroizing<Vec<u8>> {
        // Straight into a buffer zeroed on drop: `encode_string` goes through a chunked encoder
        // whose stack buffer keeps a copy of the encoded key.
        let mut encoded = Zeroizing::new([0u8; ENCODED_LEN]);
        let written = URL_SAFE_NO_PAD
            .encode_slice(self.key.expose_secret(), encoded.as_mut_slice())
            .expect("32 bytes are 43 base64url characters");
        debug_assert_eq!(written, ENCODED_LEN);
        let mut line = Zeroizing::new(Vec::with_capacity(PREFIX.len() + ENCODED_LEN + 1));
        line.extend_from_slice(PREFIX.as_bytes());
        line.extend_from_slice(encoded.as_slice());
        line.push(b'\n');
        line
    }

    pub(crate) fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(self.key.expose_secret().into())
    }
}

/// The key in `file`, or `None` when there is no such file. A key file that other users could
/// read, or that another user owns, is refused, and so is anything but a plain file.
pub fn read(file: &Path) -> anyhow::Result<Option<MasterKey>> {
    let Some(text) = read_text(file)? else {
        return Ok(None);
    };
    MasterKey::parse(&text, Backend::File)
        .map(Some)
        .with_context(|| {
            format!(
                "the master key {} is unreadable; restore it from a backup",
                file.display()
            )
        })
}

/// The text of the key file `file`, not parsed, after the checks of [`read`].
fn read_text(file: &Path) -> anyhow::Result<Option<Zeroizing<Vec<u8>>>> {
    read_line(file, true)
}

/// The text of the key file `file`, not parsed: a plain file, never followed through a link, and,
/// if `private`, one that only Fronde's user owns and reads.
fn read_line(file: &Path, private: bool) -> anyhow::Result<Option<Zeroizing<Vec<u8>>>> {
    let shown = file.display();
    let mut opened = match open_plain(file) {
        Ok(opened) => opened,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) if is_link_refused(&e) => bail!(
            "the master key {shown} is a symbolic link; Fronde reads the key only from a plain \
             file: put the file itself there"
        ),
        Err(e) => return Err(e).with_context(|| format!("cannot read the master key {shown}")),
    };
    if private {
        check_private(&opened, file)?;
    } else {
        check_plain(&opened, file)?;
    }
    // A key file is one short line and its line break: read one byte more, enough to refuse a
    // longer file, into a buffer that never grows.
    let limit = PREFIX.len() + ENCODED_LEN + 3;
    let mut text = Zeroizing::new(Vec::with_capacity(limit + 1));
    (&mut opened)
        .take(limit as u64)
        .read_to_end(&mut text)
        .with_context(|| format!("cannot read the master key {shown}"))?;
    Ok(Some(text))
}

/// Whether opening without following links failed on a symbolic link (`ELOOP`).
fn is_link_refused(e: &std::io::Error) -> bool {
    #[cfg(unix)]
    return e.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error());
    #[cfg(not(unix))]
    {
        let _ = e;
        false
    }
}

/// `file` opened for reading, without following a symbolic link, and without waiting on a FIFO
/// (no writer would ever come): [`check_private`] then refuses anything but a plain file.
fn open_plain(file: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use rustix::fs::OFlags;
        use std::os::unix::fs::OpenOptionsExt as _;
        let flags = OFlags::NOFOLLOW | OFlags::NONBLOCK;
        options.custom_flags(flags.bits() as i32);
    }
    options.open(file)
}

#[cfg(unix)]
fn check_private(opened: &std::fs::File, file: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = opened
        .metadata()
        .with_context(|| format!("cannot read the master key {}", file.display()))?;
    if !meta.is_file() {
        bail!(
            "the master key {} is not a plain file; Fronde reads the key only from a plain file",
            file.display()
        );
    }
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
fn check_private(opened: &std::fs::File, file: &Path) -> anyhow::Result<()> {
    check_plain(opened, file)
}

/// Refuses anything but a plain file.
fn check_plain(opened: &std::fs::File, file: &Path) -> anyhow::Result<()> {
    let meta = opened
        .metadata()
        .with_context(|| format!("cannot read the master key {}", file.display()))?;
    if !meta.is_file() {
        bail!(
            "the master key {} is not a plain file; Fronde reads the key only from a plain file",
            file.display()
        );
    }
    Ok(())
}

/// Write a new key to `file`, 0600, unless the file exists. Returns the key in the file: the new
/// one, or the one another writer put there first.
fn create(file: &Path) -> anyhow::Result<MasterKey> {
    let key = MasterKey::generate(Backend::File)?;
    match place(file, &key.encode()) {
        Ok(()) => Ok(key),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            read(file)?.with_context(|| format!("the master key {} vanished", file.display()))
        }
        Err(e) if unwritable(&e) => Err(not_writable(file, e)),
        Err(e) => {
            Err(e).with_context(|| format!("cannot create the master key {}", file.display()))
        }
    }
}

/// Put `text` in a new file `file`, 0600, unless the file exists (`AlreadyExists`). The text is
/// written and synced to a temporary file, which is then linked to `file`: the link fails if
/// `file` exists, and a crash never leaves `file` empty or half written. The directory is synced
/// so that the new name survives a power cut.
fn place(file: &Path, text: &[u8]) -> std::io::Result<()> {
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
            opened.write_all(text)?;
            opened.sync_all()
        });
    let linked = written.and_then(|()| std::fs::hard_link(&staged, file));
    // The staged name goes whatever happened; the key stays in `file` once linked.
    let _ = std::fs::remove_file(&staged);
    linked?;
    sync_dir(file)
}

/// Whether `e` says the directory cannot be written to: a read-only mount, or a directory that
/// Fronde's user may not write.
fn unwritable(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem
    )
}

/// The error for a key file that cannot be made because its directory is not writable, such as a
/// configuration directory mounted read-only in a container, with the two ways out.
fn not_writable(file: &Path, e: std::io::Error) -> anyhow::Error {
    let dir = file.parent().unwrap_or(Path::new("/"));
    anyhow::Error::new(e).context(format!(
        "cannot write the master key {}: Fronde's user cannot write to {} (a read-only mount, \
         such as a configuration directory in a container?). Make that directory writable, or \
         set [secrets] master_key = {{ file = \"…\" }} in the configuration to a file in a \
         writable directory kept apart from the data directory",
        file.display(),
        dir.display()
    ))
}

/// Move the key an earlier version kept in `data_dir` to `file`, where it now belongs. Returns
/// whether it moved (or whether a copy left by an interrupted move was removed).
///
/// The key is checked as [`read`] checks it, then linked to its new name, or, from another
/// filesystem, copied, synced and read back before the old file is removed: a crash leaves the
/// key in one place or both, never in neither. Two different keys, one in each place, are refused:
/// only the owner knows which one sealed the secrets.
pub fn migrate(file: &Path, data_dir: &Path) -> anyhow::Result<bool> {
    let legacy = legacy_path(data_dir);
    if same_place(&legacy, file) {
        return Ok(false);
    }
    let Some(old) = read_text(&legacy)? else {
        return Ok(false);
    };
    MasterKey::parse(&old, Backend::File).with_context(|| {
        format!(
            "the master key {} is unreadable; restore it from a backup",
            legacy.display()
        )
    })?;
    match read_text(file)? {
        Some(new) if new == old => {}
        Some(_) => return Err(conflict(file, &legacy)),
        None => match std::fs::hard_link(&legacy, file) {
            Ok(()) => sync_dir(file)
                .with_context(|| format!("cannot write the master key {}", file.display()))?,
            Err(e) if e.kind() == ErrorKind::CrossesDevices => match place(file, &old) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) if unwritable(&e) => return Err(not_writable(file, e)),
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("cannot copy the master key to {}", file.display())
                    });
                }
            },
            // Another Fronde moved it at the same time: checked below.
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) if unwritable(&e) => return Err(not_writable(file, e)),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("cannot move the master key to {}", file.display()));
            }
        },
    }
    // The old file goes only once the new one reads back the same.
    if read_text(file)?.as_ref().map(|new| new.as_slice()) != Some(old.as_slice()) {
        return Err(conflict(file, &legacy));
    }
    match std::fs::remove_file(&legacy) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "the master key is now in {}, but its old copy {} cannot be removed; remove \
                     it by hand",
                    file.display(),
                    legacy.display()
                )
            });
        }
    }
    sync_dir(&legacy).with_context(|| format!("cannot remove {}", legacy.display()))?;
    tracing::info!(
        "moved the master key from {} to {}: it is kept out of the data directory, so that \
         backups of the data directory do not hold it; back it up apart",
        legacy.display(),
        file.display()
    );
    Ok(true)
}

/// Where the key is to be read from now, moving nothing: `file`, or the data directory's key
/// that [`migrate`] would move there. For `fronde check`, which changes nothing.
pub fn locate(file: &Path, data_dir: &Path) -> anyhow::Result<PathBuf> {
    let legacy = legacy_path(data_dir);
    if same_place(&legacy, file) {
        return Ok(file.to_path_buf());
    }
    let Some(old) = read_text(&legacy)? else {
        return Ok(file.to_path_buf());
    };
    match read_text(file)? {
        None => Ok(legacy),
        Some(new) if new == old => Ok(file.to_path_buf()),
        Some(_) => Err(conflict(file, &legacy)),
    }
}

fn conflict(file: &Path, legacy: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "there are two different master keys, {} and {} (where earlier versions kept it); \
         Fronde cannot tell which one sealed the stored secrets. Keep the right one at {} and \
         move the other out of the way (try each: `fronde check` says when a key does not open \
         the stored secrets)",
        file.display(),
        legacy.display(),
        file.display()
    )
}

/// Whether `a` and `b` name the same file, as written or once resolved.
fn same_place(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    let resolved = |p: &Path| {
        let dir = p.parent()?.canonicalize().ok()?;
        Some(dir.join(p.file_name()?))
    };
    matches!((resolved(a), resolved(b)), (Some(a), Some(b)) if a == b)
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

/// The key that opens the stored secrets, from the file `file` alone ([`Backends::file`]).
pub async fn load(db: &DatabaseConnection, file: &Path) -> anyhow::Result<Option<Arc<MasterKey>>> {
    Backends::file(file).load(db).await
}

/// [`load`], or, when nothing is stored and there is no key file yet, a new key in a new file
/// ([`Backends::load_or_create`] for the file alone).
pub async fn load_or_create(
    db: &DatabaseConnection,
    file: &Path,
) -> anyhow::Result<(Arc<MasterKey>, bool)> {
    Backends::file(file).load_or_create(db).await
}
