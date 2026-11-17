//! The master key in the platform's keychain, through keyring-core: Secret Service over zbus on
//! Linux (pure Rust, no libdbus, no OpenSSL), the Keychain on macOS, the Credential Manager on
//! Windows. One entry per data directory, service `fronde`, user `master-key@<hash of the data
//! directory>`, so that two Fronde instances never share a key.
//!
//! keyring-core is synchronous, and a locked collection may open an unlock prompt on a desktop:
//! each call runs on a blocking thread under a timeout, so that a keychain that does not answer
//! never stalls Fronde.

use super::{Backend, MasterKey};
use keyring_core::{CredentialStore, Entry, Error};
use sha2::{Digest as _, Sha256};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use zeroize::Zeroizing;

/// The entry's service.
const SERVICE: &str = "fronde";

/// How long a keychain call may take before the keychain counts as unavailable.
const TIMEOUT: Duration = Duration::from_secs(2);

type Open = dyn Fn() -> keyring_core::Result<Arc<CredentialStore>> + Send + Sync;

/// The keychain entry that holds the master key of one data directory.
#[derive(Clone)]
pub struct Keychain {
    open: Arc<Open>,
    /// What the keychain is, for messages: "Secret Service", …
    kind: &'static str,
    /// The data directory whose key this is, named in the entry's user ([`Keychain::user`]).
    data_dir: PathBuf,
}

impl fmt::Debug for Keychain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Keychain")
            .field("kind", &self.kind)
            .field("user", &self.user())
            .finish_non_exhaustive()
    }
}

impl fmt::Display for Keychain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the keychain ({}, entry {SERVICE} / {})",
            self.kind,
            self.user()
        )
    }
}

/// Why the keychain did not give a key.
pub(super) enum KeychainError {
    /// No keychain, or one that is locked, refuses access or does not answer: what it said.
    Unavailable(String),
    /// The keychain answered, and what it holds is not a key.
    Failed(anyhow::Error),
}

impl KeychainError {
    pub(super) fn into_anyhow(self, keychain: &Keychain) -> anyhow::Error {
        match self {
            Self::Unavailable(why) => anyhow::anyhow!(
                "{keychain} {why}: unlock it (open the desktop session that unlocks it) and try \
                 again, or keep the key elsewhere with [secrets] backend = \"systemd\" or \
                 \"file\" before storing the first secret"
            ),
            Self::Failed(e) => e.context(format!("{keychain}")),
        }
    }
}

impl Keychain {
    /// The platform's keychain, for the data directory `data_dir`.
    pub fn platform(data_dir: &Path) -> Self {
        Self {
            open: Arc::new(platform_store),
            kind: PLATFORM_KIND,
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// The keychain that `open` connects to, for the data directory `data_dir`.
    #[cfg(feature = "testing")]
    fn with(
        open: impl Fn() -> keyring_core::Result<Arc<CredentialStore>> + Send + Sync + 'static,
        kind: &'static str,
        data_dir: &Path,
    ) -> Self {
        Self {
            open: Arc::new(open),
            kind,
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// No keychain at all: every call says so.
    pub fn none() -> Self {
        Self {
            open: Arc::new(|| {
                Err(Error::NotSupportedByStore(
                    "no keychain is used here".to_owned(),
                ))
            }),
            kind: "none",
            data_dir: PathBuf::new(),
        }
    }

    /// The entry's user name: `master-key@` and the start of the data directory's hash. Worked
    /// out at each call, so that a data directory created since is resolved as it is now.
    pub fn user(&self) -> String {
        user(&self.data_dir)
    }

    /// The key in the keychain, or `None` when it holds none.
    pub(super) async fn read(&self) -> Result<Option<MasterKey>, KeychainError> {
        let line = self
            .run(|entry| match entry.get_password() {
                Ok(line) => Ok(Some(Zeroizing::new(line))),
                Err(Error::NoEntry) => Ok(None),
                Err(e) => Err(e),
            })
            .await?;
        line.map(|line| parse(&line)).transpose()
    }

    /// The key in the keychain, or a new one stored there when it holds none, and whether it
    /// was made now. The new key is read back, so that of two writers both use the one stored.
    pub(super) async fn read_or_create(&self) -> Result<(MasterKey, bool), KeychainError> {
        let new = MasterKey::generate(Backend::Keychain).map_err(KeychainError::Failed)?;
        let (line, created) = self
            .run(move |entry| {
                match entry.get_password() {
                    Ok(line) => return Ok((Zeroizing::new(line), false)),
                    Err(Error::NoEntry) => {}
                    Err(e) => return Err(e),
                }
                let encoded = new.encode();
                let text =
                    std::str::from_utf8(encoded.trim_ascii_end()).expect("a key line is ASCII");
                entry.set_password(text)?;
                Ok((Zeroizing::new(entry.get_password()?), true))
            })
            .await?;
        Ok((parse(&line)?, created))
    }

    /// `op` on the entry, on a blocking thread, within [`TIMEOUT`].
    async fn run<T: Send + 'static>(
        &self,
        op: impl FnOnce(Entry) -> keyring_core::Result<T> + Send + 'static,
    ) -> Result<T, KeychainError> {
        let open = self.open.clone();
        let user = self.user();
        let task = tokio::task::spawn_blocking(move || {
            let store = open()?;
            op(store.build(SERVICE, &user, None)?)
        });
        match tokio::time::timeout(TIMEOUT, task).await {
            Err(_) => Err(KeychainError::Unavailable(format!(
                "did not answer within {} s (locked behind an unlock prompt?)",
                TIMEOUT.as_secs()
            ))),
            Ok(Err(e)) => Err(KeychainError::Failed(anyhow::anyhow!(
                "the keychain call failed: {e}"
            ))),
            Ok(Ok(result)) => result.map_err(classify),
        }
    }
}

/// The key in `line`, as the keychain held it.
fn parse(line: &str) -> Result<MasterKey, KeychainError> {
    MasterKey::parse(line.as_bytes(), Backend::Keychain).map_err(|e| {
        KeychainError::Failed(
            e.context("its master key entry is unreadable; restore it from a backup of the key"),
        )
    })
}

/// What a keychain error means for Fronde. Nothing the keychain returned is quoted: a malformed
/// entry's bytes could be a secret.
fn classify(e: Error) -> KeychainError {
    match e {
        Error::NoStorageAccess(why) => {
            KeychainError::Unavailable(format!("is locked or refuses access ({why})"))
        }
        Error::PlatformFailure(why) => {
            KeychainError::Unavailable(format!("cannot be reached ({why})"))
        }
        Error::NoDefaultStore | Error::NotSupportedByStore(_) => {
            KeychainError::Unavailable("is not available on this system".to_owned())
        }
        Error::BadEncoding(_) | Error::BadDataFormat(..) => KeychainError::Failed(anyhow::anyhow!(
            "its master key entry is not text; restore it from a backup of the key"
        )),
        other => KeychainError::Failed(anyhow::anyhow!("the keychain refused the call: {other}")),
    }
}

/// `master-key@` and the first 8 bytes, in hex, of the SHA-256 of the data directory's path,
/// resolved when it exists.
fn user(data_dir: &Path) -> String {
    let dir = data_dir
        .canonicalize()
        .unwrap_or_else(|_| data_dir.to_path_buf());
    let hash = Sha256::digest(dir.as_os_str().as_encoded_bytes());
    let mut user = String::from("master-key@");
    for byte in &hash[..8] {
        user.push_str(&format!("{byte:02x}"));
    }
    user
}

#[cfg(target_os = "linux")]
const PLATFORM_KIND: &str = "Secret Service";
#[cfg(target_os = "macos")]
const PLATFORM_KIND: &str = "macOS Keychain";
#[cfg(target_os = "windows")]
const PLATFORM_KIND: &str = "Windows Credential Manager";
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
const PLATFORM_KIND: &str = "none on this platform";

#[cfg(target_os = "linux")]
fn platform_store() -> keyring_core::Result<Arc<CredentialStore>> {
    let store: Arc<CredentialStore> = zbus_secret_service_keyring_store::Store::new()?;
    Ok(store)
}

#[cfg(target_os = "macos")]
fn platform_store() -> keyring_core::Result<Arc<CredentialStore>> {
    let store: Arc<CredentialStore> = apple_native_keyring_store::keychain::Store::new()?;
    Ok(store)
}

#[cfg(target_os = "windows")]
fn platform_store() -> keyring_core::Result<Arc<CredentialStore>> {
    let store: Arc<CredentialStore> = windows_native_keyring_store::Store::new()?;
    Ok(store)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn platform_store() -> keyring_core::Result<Arc<CredentialStore>> {
    Err(Error::NotSupportedByStore(
        "no keychain on this platform".to_owned(),
    ))
}

/// An in-memory keychain for tests, on keyring-core's mock store, that can be locked or made
/// unreachable at will. Its entries live as long as it does.
#[cfg(feature = "testing")]
#[derive(Clone)]
pub struct FakeKeychain {
    store: Arc<keyring_core::mock::Store>,
    state: Arc<std::sync::atomic::AtomicU8>,
}

#[cfg(feature = "testing")]
impl Default for FakeKeychain {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "testing")]
impl FakeKeychain {
    const OPEN: u8 = 0;
    const LOCKED: u8 = 1;
    const UNREACHABLE: u8 = 2;

    /// An empty, unlocked keychain.
    pub fn new() -> Self {
        Self {
            store: keyring_core::mock::Store::new().expect("the mock store"),
            state: Arc::new(std::sync::atomic::AtomicU8::new(Self::OPEN)),
        }
    }

    /// This keychain, for the data directory `data_dir`.
    pub fn keychain(&self, data_dir: &Path) -> Keychain {
        let (store, state) = (self.store.clone(), self.state.clone());
        Keychain::with(
            move || match state.load(std::sync::atomic::Ordering::SeqCst) {
                Self::LOCKED => Err(Error::NoStorageAccess("the collection is locked".into())),
                Self::UNREACHABLE => Err(Error::PlatformFailure("no D-Bus session bus".into())),
                _ => {
                    let store: Arc<CredentialStore> = store.clone();
                    Ok(store)
                }
            },
            "fake",
            data_dir,
        )
    }

    /// Locked, as a desktop keychain is before its session unlocks it.
    pub fn lock(&self) {
        self.set(Self::LOCKED);
    }

    /// Unreachable, as on a server with no session bus.
    pub fn make_unreachable(&self) {
        self.set(Self::UNREACHABLE);
    }

    /// Open again.
    pub fn unlock(&self) {
        self.set(Self::OPEN);
    }

    fn set(&self, state: u8) {
        self.state.store(state, std::sync::atomic::Ordering::SeqCst);
    }

    /// The text of the entry of `data_dir`'s key, whatever the state, if there is one.
    pub fn entry(&self, data_dir: &Path) -> Option<String> {
        use keyring_core::api::CredentialStoreApi as _;
        let entry = self
            .store
            .build(SERVICE, &user(data_dir), None)
            .expect("a mock entry");
        entry.get_password().ok()
    }

    /// Put `line` in `data_dir`'s entry, whatever the state.
    pub fn put(&self, data_dir: &Path, line: &str) {
        use keyring_core::api::CredentialStoreApi as _;
        self.store
            .build(SERVICE, &user(data_dir), None)
            .expect("a mock entry")
            .set_password(line)
            .expect("the mock stores it");
    }
}
