//! The master key in the platform's keychain, through keyring-core: Secret Service over zbus on
//! Linux (pure Rust, no libdbus, no OpenSSL), the Keychain on macOS, the Credential Manager on
//! Windows. One entry per data directory, service `fronde`, user `master-key@<hash of the data
//! directory>`, so that two Fronde instances never share a key. The database records the entry's
//! user with the first secret, and that record is used from then on, so that a data directory
//! moved elsewhere still finds its key.
//!
//! keyring-core is synchronous, and a locked collection may open an unlock prompt on a desktop:
//! each call runs on a blocking thread under a timeout. A blocking thread cannot be cancelled, so
//! one keychain allows one call at a time: a call that does not answer keeps its thread, and the
//! next waits for it within its own timeout instead of starting another thread and another
//! prompt. `main` does not wait for such a thread when it exits.

use super::{Backend, MasterKey};
use keyring_core::{CredentialStore, Entry, Error};
use sha2::{Digest as _, Sha256};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::Semaphore;
use zeroize::{Zeroize as _, Zeroizing};

/// The entry's service.
pub(super) const SERVICE: &str = "fronde";

/// The environment variable that, set to `off`, makes the platform's keychain absent, so that
/// `auto` keeps the key in a file. `mise.toml` sets it for every test and development run, which
/// must never reach the keychain of the machine they run on.
pub const SWITCH: &str = "FRONDE_KEYCHAIN";

/// How long a keychain call may take, waiting for an earlier one included, before the keychain
/// counts as locked behind a prompt.
const TIMEOUT: Duration = Duration::from_secs(2);

type Open = dyn Fn() -> keyring_core::Result<Arc<CredentialStore>> + Send + Sync;

/// The keychain that holds the master key of one data directory.
#[derive(Clone)]
pub struct Keychain {
    open: Arc<Open>,
    /// What the keychain is, for messages: "Secret Service", …
    kind: &'static str,
    /// The data directory whose key this is, named in the entry's user ([`Keychain::user`]).
    data_dir: PathBuf,
    /// One permit: the call in flight holds it until its blocking thread ends, timed out or not.
    calls: Arc<Semaphore>,
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
        f.write_str(&self.describe(&self.user()))
    }
}

/// Why the keychain did not give a key.
pub(super) enum KeychainError {
    /// A keychain that is there but locked, refuses access, waits on an unlock prompt or is still
    /// busy with a call that does not answer: what it said.
    Locked(String),
    /// No keychain on this system, or none that can be reached: what it said.
    Absent(String),
    /// The keychain answered, and what it holds is not a key.
    Failed(anyhow::Error),
}

impl KeychainError {
    /// The error, naming `place`, the entry ([`Keychain::describe`]).
    pub(super) fn into_anyhow(self, place: &str) -> anyhow::Error {
        match self {
            Self::Locked(why) => anyhow::anyhow!(
                "{place} {why}: unlock it (open the desktop session that unlocks it) and try \
                 again, or keep the key elsewhere with [secrets] backend = \"systemd\" or \
                 \"file\" before storing the first secret"
            ),
            Self::Absent(why) => anyhow::anyhow!(
                "{place} {why}: run Fronde where it can reach the keychain (in the desktop \
                 session), or keep the key elsewhere with [secrets] backend = \"systemd\" or \
                 \"file\" before storing the first secret"
            ),
            Self::Failed(e) => e.context(place.to_owned()),
        }
    }
}

impl Keychain {
    /// The platform's keychain, for the data directory `data_dir`, or none when
    /// `FRONDE_KEYCHAIN` is `off` ([`SWITCH`]). Every handle on it shares one call in flight: they
    /// all reach the same keychain.
    pub fn platform(data_dir: &Path) -> Self {
        Self::platform_unless(data_dir, std::env::var_os(SWITCH).as_deref())
    }

    /// Whether [`SWITCH`] turns the platform's keychain off in this process.
    pub fn switched_off() -> bool {
        is_off(std::env::var_os(SWITCH).as_deref())
    }

    /// [`Keychain::platform`], with `switch` the value of [`SWITCH`].
    fn platform_unless(data_dir: &Path, switch: Option<&std::ffi::OsStr>) -> Self {
        if is_off(switch) {
            return Self::absent(
                "no keychain is used here: FRONDE_KEYCHAIN is off; unset it to use the platform's keychain",
            );
        }
        static CALLS: OnceLock<Arc<Semaphore>> = OnceLock::new();
        Self {
            open: Arc::new(platform_store),
            kind: PLATFORM_KIND,
            data_dir: data_dir.to_path_buf(),
            calls: CALLS.get_or_init(|| Arc::new(Semaphore::new(1))).clone(),
        }
    }

    /// The keychain that `open` connects to, for the data directory `data_dir`, with a call in
    /// flight of its own, as another process would have.
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
            calls: Arc::new(Semaphore::new(1)),
        }
    }

    /// No keychain at all: every call says so.
    pub fn none() -> Self {
        Self::absent("no keychain is used here")
    }

    /// No keychain, for the reason `why`, which every call gives.
    fn absent(why: &'static str) -> Self {
        Self {
            open: Arc::new(move || Err(Error::NotSupportedByStore(why.to_owned()))),
            kind: "none",
            data_dir: PathBuf::new(),
            calls: Arc::new(Semaphore::new(1)),
        }
    }

    /// The data directory whose key this keychain holds; `None` for [`Keychain::none`].
    pub(super) fn data_dir(&self) -> Option<&Path> {
        (!self.data_dir.as_os_str().is_empty()).then_some(self.data_dir.as_path())
    }

    /// The user name of a first key's entry: `master-key@` and the start of the data
    /// directory's hash. Worked out at each call, so that a data directory created since is
    /// resolved as it is now. Once a secret is stored, the database's record of the entry wins.
    pub fn user(&self) -> String {
        user(&self.data_dir)
    }

    /// The entry of `user` in this keychain, for messages.
    pub(super) fn describe(&self, user: &str) -> String {
        format!("the keychain ({}, entry {SERVICE} / {user})", self.kind)
    }

    /// The key in the entry of `user`, or `None` when the keychain holds none there.
    pub(super) async fn read(&self, user: &str) -> Result<Option<MasterKey>, KeychainError> {
        let line = self
            .run(user, |entry| match entry.get_password() {
                Ok(line) => Ok(Some(Zeroizing::new(line))),
                Err(Error::NoEntry) => Ok(None),
                Err(e) => Err(e),
            })
            .await?;
        line.map(|line| parse(&line, user)).transpose()
    }

    /// The key in the entry of `user`, or a new one stored there when it holds none, and whether
    /// it was made now. The store replaces an entry it writes, so the caller holds the
    /// [`super::KeyLock`]: another Fronde never writes between this read and this write. `lock`
    /// is that lock's file, kept until the call ends, even after it timed out.
    pub(super) async fn read_or_create(
        &self,
        user: &str,
        lock: Option<std::fs::File>,
    ) -> Result<(MasterKey, bool), KeychainError> {
        let new = MasterKey::generate(Backend::Keychain).map_err(KeychainError::Failed)?;
        let (line, created) = self
            .run(user, move |entry| {
                let _lock = lock;
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
        Ok((parse(&line, user)?, created))
    }

    /// Once no call of this keychain is in flight, within [`TIMEOUT`]. A call that timed out may
    /// still hold the [`super::KeyLock`] on its thread: waiting for the lock would then wait on
    /// this process itself.
    pub(super) async fn wait_idle(&self) -> Result<(), KeychainError> {
        match tokio::time::timeout(TIMEOUT, self.calls.acquire()).await {
            Ok(_) => Ok(()),
            Err(_) => Err(busy()),
        }
    }

    /// `op` on the entry of `user`, on a blocking thread, once no other call of this keychain
    /// is in flight, all within [`TIMEOUT`]. The thread keeps the permit until it ends: a call
    /// that times out still blocks the next ones, which then time out without a thread of their
    /// own.
    async fn run<T: Send + 'static>(
        &self,
        user: &str,
        op: impl FnOnce(Entry) -> keyring_core::Result<T> + Send + 'static,
    ) -> Result<T, KeychainError> {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        let permit =
            match tokio::time::timeout_at(deadline, self.calls.clone().acquire_owned()).await {
                Ok(Ok(permit)) => permit,
                Ok(Err(_)) => unreachable!("the semaphore is never closed"),
                Err(_) => return Err(busy()),
            };
        let open = self.open.clone();
        let user = user.to_owned();
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let store = open()?;
            op(store.build(SERVICE, &user, None)?)
        });
        match tokio::time::timeout_at(deadline, task).await {
            Err(_) => Err(KeychainError::Locked(format!(
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

/// An earlier call that has not answered holds the keychain.
fn busy() -> KeychainError {
    KeychainError::Locked(format!(
        "is busy: an earlier call has not answered for {} s (an unlock prompt left open?)",
        TIMEOUT.as_secs()
    ))
}

/// The key in `line`, as the keychain held it in the entry of `user`.
fn parse(line: &str, user: &str) -> Result<MasterKey, KeychainError> {
    MasterKey::parse(line.as_bytes(), Backend::Keychain)
        .map(|key| key.in_entry(user))
        .map_err(|e| {
            KeychainError::Failed(
                e.context(
                    "its master key entry is unreadable; restore it from a backup of the key",
                ),
            )
        })
}

/// What a keychain error means for Fronde. Nothing the keychain returned is quoted: a malformed
/// entry's bytes could be a secret, and they are zeroed here (keyring-core's copies before this
/// one are not).
fn classify(e: Error) -> KeychainError {
    match e {
        Error::NoStorageAccess(why) => {
            KeychainError::Locked(format!("is locked or refuses access ({why})"))
        }
        Error::PlatformFailure(why) => KeychainError::Absent(format!("cannot be reached ({why})")),
        Error::NoDefaultStore => {
            KeychainError::Absent("is not available on this system".to_owned())
        }
        Error::NotSupportedByStore(why) => {
            KeychainError::Absent(format!("is not available on this system ({why})"))
        }
        Error::BadEncoding(mut bytes) | Error::BadDataFormat(mut bytes, _) => {
            bytes.zeroize();
            KeychainError::Failed(anyhow::anyhow!(
                "its master key entry is not text; restore it from a backup of the key"
            ))
        }
        other => KeychainError::Failed(anyhow::anyhow!("the keychain refused the call: {other}")),
    }
}

/// Whether `switch`, the value of [`SWITCH`], turns the platform's keychain off.
fn is_off(switch: Option<&std::ffi::OsStr>) -> bool {
    switch.is_some_and(|value| value.eq_ignore_ascii_case("off"))
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

/// An in-memory keychain for tests, on keyring-core's mock store, that can be locked, made
/// unreachable, made to hang, or made slow at will. Its entries live as long as it does.
#[cfg(feature = "testing")]
#[derive(Clone)]
pub struct FakeKeychain {
    store: Arc<keyring_core::mock::Store>,
    shared: Arc<FakeShared>,
}

#[cfg(feature = "testing")]
#[derive(Default)]
struct FakeShared {
    state: std::sync::Mutex<FakeState>,
    changed: std::sync::Condvar,
    /// How many times a call connected to the keychain: one per blocking thread.
    opened: std::sync::atomic::AtomicUsize,
}

#[cfg(feature = "testing")]
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum FakeState {
    #[default]
    Open,
    Locked,
    Unreachable,
    /// Connecting blocks until the state changes, as behind an unlock prompt nobody answers.
    Hung,
    /// Open, and a read that finds no entry takes this long to say so.
    SlowWhenEmpty(Duration),
}

#[cfg(feature = "testing")]
impl Default for FakeKeychain {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "testing")]
impl FakeKeychain {
    /// An empty, unlocked keychain.
    pub fn new() -> Self {
        Self {
            store: keyring_core::mock::Store::new().expect("the mock store"),
            shared: Arc::default(),
        }
    }

    /// A handle on this keychain for the data directory `data_dir`, as one process would have:
    /// each handle (and its clones) has its own call in flight.
    pub fn keychain(&self, data_dir: &Path) -> Keychain {
        let (store, shared) = (self.store.clone(), self.shared.clone());
        Keychain::with(
            move || {
                shared
                    .opened
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let state = shared
                    .changed
                    .wait_while(shared.state.lock().unwrap(), |s| *s == FakeState::Hung)
                    .unwrap();
                match *state {
                    FakeState::Locked => {
                        Err(Error::NoStorageAccess("the collection is locked".into()))
                    }
                    FakeState::Unreachable => {
                        Err(Error::PlatformFailure("no D-Bus session bus".into()))
                    }
                    FakeState::SlowWhenEmpty(delay) => {
                        let store: Arc<CredentialStore> = Arc::new(SlowWhenEmpty {
                            store: store.clone(),
                            delay,
                        });
                        Ok(store)
                    }
                    FakeState::Open | FakeState::Hung => {
                        let store: Arc<CredentialStore> = store.clone();
                        Ok(store)
                    }
                }
            },
            "fake",
            data_dir,
        )
    }

    /// Locked, as a desktop keychain is before its session unlocks it.
    pub fn lock(&self) {
        self.set(FakeState::Locked);
    }

    /// Unreachable, as on a server with no session bus.
    pub fn make_unreachable(&self) {
        self.set(FakeState::Unreachable);
    }

    /// Every call blocks until the keychain is unlocked, as behind a prompt nobody answers.
    pub fn hang(&self) {
        self.set(FakeState::Hung);
    }

    /// Open, but a read that finds no entry answers only after `delay`: two first writers then
    /// both read before either writes.
    pub fn slow_when_empty(&self, delay: Duration) {
        self.set(FakeState::SlowWhenEmpty(delay));
    }

    /// Open again; a hung call goes on.
    pub fn unlock(&self) {
        self.set(FakeState::Open);
    }

    fn set(&self, state: FakeState) {
        *self.shared.state.lock().unwrap() = state;
        self.shared.changed.notify_all();
    }

    /// How many calls connected to the keychain, each on a blocking thread of its own.
    pub fn opened(&self) -> usize {
        self.shared.opened.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The text of the entry of `data_dir`'s first key, whatever the state, if there is one.
    pub fn entry(&self, data_dir: &Path) -> Option<String> {
        use keyring_core::api::CredentialStoreApi as _;
        let entry = self
            .store
            .build(SERVICE, &user(data_dir), None)
            .expect("a mock entry");
        entry.get_password().ok()
    }

    /// Put `line` in the entry of `data_dir`'s first key, whatever the state.
    pub fn put(&self, data_dir: &Path, line: &str) {
        use keyring_core::api::CredentialStoreApi as _;
        self.store
            .build(SERVICE, &user(data_dir), None)
            .expect("a mock entry")
            .set_password(line)
            .expect("the mock stores it");
    }
}

/// The mock store, with reads that find no entry slowed down.
#[cfg(feature = "testing")]
#[derive(Debug)]
struct SlowWhenEmpty {
    store: Arc<keyring_core::mock::Store>,
    delay: Duration,
}

#[cfg(feature = "testing")]
impl keyring_core::api::CredentialStoreApi for SlowWhenEmpty {
    fn vendor(&self) -> String {
        self.store.vendor()
    }

    fn id(&self) -> String {
        self.store.id()
    }

    fn build(
        &self,
        service: &str,
        user: &str,
        modifiers: Option<&std::collections::HashMap<&str, &str>>,
    ) -> keyring_core::Result<Entry> {
        let entry = self.store.build(service, user, modifiers)?;
        Ok(Entry::new_with_credential(Arc::new(SlowEntry {
            entry,
            delay: self.delay,
        })))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(feature = "testing")]
#[derive(Debug)]
struct SlowEntry {
    entry: Entry,
    delay: Duration,
}

#[cfg(feature = "testing")]
impl keyring_core::api::CredentialApi for SlowEntry {
    fn set_secret(&self, secret: &[u8]) -> keyring_core::Result<()> {
        self.entry.set_secret(secret)
    }

    fn get_secret(&self) -> keyring_core::Result<Vec<u8>> {
        let read = self.entry.get_secret();
        if matches!(read, Err(Error::NoEntry)) {
            std::thread::sleep(self.delay);
        }
        read
    }

    fn delete_credential(&self) -> keyring_core::Result<()> {
        self.entry.delete_credential()
    }

    fn get_credential(&self) -> keyring_core::Result<Option<Arc<keyring_core::Credential>>> {
        Ok(None)
    }

    fn get_specifiers(&self) -> Option<(String, String)> {
        self.entry.get_specifiers()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[tokio::test]
    async fn switched_off_the_platform_keychain_is_absent() {
        let dir = Path::new("/srv/fronde/data");
        for off in ["off", "OFF"] {
            let keychain = Keychain::platform_unless(dir, Some(OsStr::new(off)));
            assert_eq!(keychain.kind, "none");
            assert!(keychain.data_dir().is_none());
            for result in [
                keychain.read("master-key@x").await.map(drop),
                keychain
                    .read_or_create("master-key@x", None)
                    .await
                    .map(drop),
            ] {
                let Err(KeychainError::Absent(why)) = result else {
                    panic!("{off}: not absent");
                };
                // Whoever meets it learns what turned the keychain off.
                assert!(why.contains("FRONDE_KEYCHAIN is off"), "{off}: {why}");
                let error = KeychainError::Absent(why).into_anyhow(&keychain.to_string());
                assert!(error.to_string().contains("FRONDE_KEYCHAIN"), "{error}");
            }
            assert!(is_off(Some(OsStr::new(off))));
        }
        // Anything else leaves the platform's keychain in place, untouched by this test.
        for other in [None, Some(OsStr::new("on")), Some(OsStr::new(""))] {
            assert!(!is_off(other));
            let keychain = Keychain::platform_unless(dir, other);
            assert_eq!(keychain.kind, PLATFORM_KIND, "{other:?}");
            assert_eq!(keychain.data_dir(), Some(dir));
        }
    }
}
