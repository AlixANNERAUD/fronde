//! Which backend holds the master key. `[secrets] backend` names one, or is `auto`: then the
//! first key is made in a systemd credential if the service has one, else in the keychain if it
//! answers, else in a file, and the database records the backend that holds it
//! (`secrets`/`backend` in `kv`, written with the key check, and for the keychain the entry's
//! user beside it). From then on `auto` uses that one only: a keychain locked at boot is an
//! error, never a reason to make a new key elsewhere, which would strand every stored secret.
//! For the first key too, a keychain that is there but locked is an error: only one that is
//! absent or cannot be reached lets the key go to a file.

use super::keychain::KeychainError;
use super::{Backend, Keychain, MasterKey, systemd};
use anyhow::{Context as _, bail};
use sea_orm::{ConnectionTrait, DatabaseConnection};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The lock file of [`KeyLock`], in the data directory.
pub const LOCK_FILE: &str = "master-key.lock";

/// How long [`Backends::lock`] waits for another Fronde storing a first secret.
const LOCK_WAIT: Duration = Duration::from_secs(30);
const LOCK_POLL: Duration = Duration::from_millis(20);

/// Where a backend keeps a key: the key in use, or the next one while a rotation is under way
/// ([`super::rotation`]): `master.key.next` beside the key file, or the keychain entry's user
/// followed by `.next`. A systemd credential has no next slot: Fronde never writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Slot {
    Current,
    Next,
}

/// The user of the keychain entry that holds the next key, beside the entry of `user`.
pub(super) fn next_entry(user: &str) -> String {
    format!("{user}.next")
}

/// The file that holds the next key, beside the key file `file`.
fn next_file(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".next");
    PathBuf::from(name)
}

/// `[secrets] backend`: one backend, or `auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Choice {
    /// The recorded backend; for the first key, systemd, then the keychain, then a file.
    #[default]
    Auto,
    /// This backend alone.
    Only(Backend),
}

/// Held while a first master key may be made, until the first secret is sealed with it: an
/// exclusive lock on [`LOCK_FILE`] in the data directory, across processes, released when
/// dropped. The keychain replaces an entry it writes, so without it two first writers could each
/// make a key, the second overwriting the one the first sealed with. A key file is made without
/// replacing one and a systemd credential is only read: with no keychain, nothing is locked.
#[derive(Debug)]
#[must_use = "the lock is released when dropped"]
pub struct KeyLock {
    file: Option<std::fs::File>,
}

impl KeyLock {
    /// The lock file again, sharing the lock: a keychain call that times out keeps it on its
    /// blocking thread, so that its write, if it ever comes, still happens under the lock.
    fn share(&self) -> anyhow::Result<Option<std::fs::File>> {
        self.file
            .as_ref()
            .map(|file| {
                file.try_clone()
                    .context("cannot share the master key's lock")
            })
            .transpose()
    }
}

/// The places the master key may be kept, and which one to use.
#[derive(Debug, Clone)]
pub struct Backends {
    choice: Choice,
    file: PathBuf,
    /// `$CREDENTIALS_DIRECTORY`, set by systemd for a service that loads credentials.
    credentials: Option<PathBuf>,
    keychain: Keychain,
}

/// What the database says of the stored secrets' master key, read by [`Backends::record`].
#[derive(Clone, Debug)]
pub struct Recorded {
    /// The key check, if one is stored.
    check: Option<Vec<u8>>,
    /// How many secrets are stored.
    rows: u64,
    /// The keychain entry's user.
    entry: String,
    /// The backend recorded with the first secret.
    recorded: Backend,
    /// The backend of a rotation under way, if any.
    interrupted: Option<Backend>,
}

impl Recorded {
    /// Whether anything is sealed: a secret, or the key check alone.
    fn stored(&self) -> bool {
        self.rows > 0 || self.check.is_some()
    }
}

/// What [`Backends::find`] found.
enum Found {
    Key(Arc<MasterKey>),
    /// Nothing is sealed.
    Nothing,
    /// Secrets are sealed, and this backend holds no key.
    Missing(Backend),
}

impl Backends {
    /// The backends: `choice`, the key file `file`, the service's credentials directory, if
    /// any, and the keychain.
    pub fn new(
        choice: Choice,
        file: PathBuf,
        credentials: Option<PathBuf>,
        keychain: Keychain,
    ) -> Self {
        Self {
            choice,
            file,
            credentials,
            keychain,
        }
    }

    /// The key file `file` alone, as before backends.
    pub fn file(file: impl Into<PathBuf>) -> Self {
        Self::new(
            Choice::Only(Backend::File),
            file.into(),
            None,
            Keychain::none(),
        )
    }

    /// These backends, with the key file read from `file` instead.
    pub fn with_file(mut self, file: impl Into<PathBuf>) -> Self {
        self.file = file.into();
        self
    }

    /// The key file of the file backend.
    pub fn key_file(&self) -> &Path {
        &self.file
    }

    /// Where `backend` keeps a first key, for messages: a path or a keychain entry.
    pub fn place(&self, backend: Backend) -> String {
        match backend {
            Backend::File => self.file.display().to_string(),
            Backend::Keychain => self.keychain.to_string(),
            Backend::Systemd => match &self.credentials {
                Some(dir) => format!("the systemd credential {}", systemd::path(dir).display()),
                None => format!("the systemd credential {}", systemd::CREDENTIAL),
            },
        }
    }

    /// Where `key` is kept, for messages: its own keychain entry for a key in the keychain.
    pub fn place_of(&self, key: &MasterKey) -> String {
        match key.keychain_entry() {
            Some(entry) => self.keychain.describe(entry),
            None => self.place(key.backend()),
        }
    }

    /// The key that opens the stored secrets, from the backend in use, checked against the
    /// database; `None` when nothing is sealed, without asking any backend (a keychain could
    /// open a prompt). A key missing while secrets are stored, a backend that cannot be read
    /// while they are, or a key that does not open them, is an error that says so, and names the
    /// recorded backend when `[secrets] backend` names another; another backend is never tried.
    pub async fn load(&self, db: &DatabaseConnection) -> anyhow::Result<Option<Arc<MasterKey>>> {
        let recorded = self.record(db).await?;
        match self.find(&recorded).await? {
            Found::Key(key) => Ok(Some(key)),
            Found::Nothing => Ok(None),
            // The last secret was removed since.
            Found::Missing(_) if !crate::secrets::any_stored(db).await? => Ok(None),
            Found::Missing(backend) => Err(self.missing_for(backend, &recorded)),
        }
    }

    /// What [`Backends::load`] reads from the database, read at once: so that a reader can let
    /// the database go before asking a backend, which may take seconds (a keychain), then
    /// [`Backends::load_recorded`].
    pub async fn record<C: ConnectionTrait>(&self, db: &C) -> anyhow::Result<Recorded> {
        let (check, rows) = crate::secrets::stored_key_check(db).await?;
        Ok(Recorded {
            check,
            rows,
            entry: self.entry(db).await?,
            // Secrets stored before backends were recorded were sealed with the key file.
            recorded: crate::secrets::recorded_backend(db)
                .await?
                .unwrap_or(Backend::File),
            // Only for a message: a record that does not read says nothing of a rotation.
            interrupted: super::rotation::interrupted(db).await.ok().flatten(),
        })
    }

    /// [`Backends::load`] from what [`Backends::record`] read, without the database.
    pub async fn load_recorded(
        &self,
        recorded: &Recorded,
    ) -> anyhow::Result<Option<Arc<MasterKey>>> {
        match self.find(recorded).await? {
            Found::Key(key) => Ok(Some(key)),
            Found::Nothing => Ok(None),
            Found::Missing(backend) => Err(self.missing_for(backend, recorded)),
        }
    }

    /// The key `recorded` asks for, from the backend in use, checked against `recorded`.
    async fn find(&self, recorded: &Recorded) -> anyhow::Result<Found> {
        if !recorded.stored() {
            return Ok(Found::Nothing);
        }
        let backend = match self.choice {
            Choice::Only(chosen) => chosen,
            Choice::Auto => recorded.recorded,
        };
        // A named backend that is not the recorded one is read all the same: the owner may have
        // moved the key there, and the record then follows it ([`crate::secrets::set`]).
        let named = |e: anyhow::Error| self.named_context(e, backend, recorded);
        let key = self
            .read(backend, &recorded.entry)
            .await
            .map_err(|e| named(self.unavailable(backend, &recorded.entry, e)))?;
        let Some(key) = key else {
            return Ok(Found::Missing(backend));
        };
        if let Err(e) =
            crate::secrets::check_recorded(&key, recorded.check.as_deref(), recorded.rows)
        {
            let e = e.context(format!("master key {}", self.place_of(&key)));
            // Read-only callers such as `fronde check` cannot finish it themselves.
            let e = match recorded.interrupted {
                Some(_) => e.context(
                    "a rotation of the master key was interrupted: `fronde serve` or `fronde \
                     secret rotate-key` finishes it",
                ),
                None => e,
            };
            return Err(named(e));
        }
        Ok(Found::Key(Arc::new(key)))
    }

    /// `e`, naming the recorded backend when `backend`, the one read, is another.
    fn named_context(
        &self,
        e: anyhow::Error,
        backend: Backend,
        recorded: &Recorded,
    ) -> anyhow::Error {
        match backend == recorded.recorded {
            true => e,
            false => e.context(self.contradicts(backend, recorded.recorded, &recorded.entry)),
        }
    }

    /// No key in `backend` while `recorded` says secrets are stored.
    fn missing_for(&self, backend: Backend, recorded: &Recorded) -> anyhow::Error {
        self.named_context(self.missing(backend, &recorded.entry), backend, recorded)
    }

    /// [`Backends::load`], or, when nothing is sealed, the key in the backend in use, made there
    /// if it holds none: the one `[secrets] backend` names, or, with `auto`, the recorded one,
    /// else the first of the systemd credential, the keychain and a file that can hold it. The
    /// flag says whether the key was made now. Takes the [`KeyLock`] to make a key: to seal the
    /// first secret under it too, use [`crate::secrets::store`].
    ///
    /// The backend is recorded when the first secret is stored ([`crate::secrets::set`]), in the
    /// transaction that checks, as the real guard, that no secret was sealed under another key in
    /// between.
    pub async fn load_or_create(
        &self,
        db: &DatabaseConnection,
    ) -> anyhow::Result<(Arc<MasterKey>, bool)> {
        if let Some(key) = self.load(db).await? {
            return Ok((key, false));
        }
        let lock = self.lock().await?;
        self.load_or_create_locked(db, &lock).await
    }

    /// [`Backends::load_or_create`] while the caller holds `lock`, from [`Backends::lock`].
    pub async fn load_or_create_locked(
        &self,
        db: &DatabaseConnection,
        lock: &KeyLock,
    ) -> anyhow::Result<(Arc<MasterKey>, bool)> {
        if let Some(key) = self.load(db).await? {
            return Ok((key, false));
        }
        let entry = self.entry(db).await?;
        let (key, created) = match (self.choice, crate::secrets::recorded_backend(db).await?) {
            (Choice::Only(backend), _) | (Choice::Auto, Some(backend)) => {
                self.create(backend, &entry, lock).await?
            }
            (Choice::Auto, None) => self.create_first(&entry, lock).await?,
        };
        Ok((Arc::new(key), created))
    }

    /// The [`KeyLock`] of the keychain's data directory, once no other process holds it; an
    /// error after [`LOCK_WAIT`], or at once when a keychain call of this process that timed
    /// out still holds it.
    pub async fn lock(&self) -> anyhow::Result<KeyLock> {
        let Some(dir) = self.keychain.data_dir() else {
            return Ok(KeyLock { file: None });
        };
        self.keychain
            .wait_idle()
            .await
            .map_err(|e| e.into_anyhow(&self.keychain.to_string()))?;
        let path = dir.join(LOCK_FILE);
        let file = fronde_fs::private_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("cannot open the lock file {}", path.display()))?;
        let deadline = tokio::time::Instant::now() + LOCK_WAIT;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(KeyLock { file: Some(file) }),
                Err(std::fs::TryLockError::WouldBlock)
                    if tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(LOCK_POLL).await;
                }
                Err(std::fs::TryLockError::WouldBlock) => bail!(
                    "another Fronde has held {} for {} s while storing the first secret; try \
                     again once it is done",
                    path.display(),
                    LOCK_WAIT.as_secs()
                ),
                Err(std::fs::TryLockError::Error(e)) => {
                    return Err(e).with_context(|| format!("cannot lock {}", path.display()));
                }
            }
        }
    }

    /// The keychain entry's user: the one recorded with the first secret, else the one this data
    /// directory's path gives.
    pub(super) async fn entry<C: ConnectionTrait>(&self, db: &C) -> anyhow::Result<String> {
        Ok(crate::secrets::recorded_keychain_entry(db)
            .await?
            .unwrap_or_else(|| self.keychain.user()))
    }

    /// The key in `backend`, if it holds one; `entry` is the keychain entry's user.
    async fn read(
        &self,
        backend: Backend,
        entry: &str,
    ) -> Result<Option<MasterKey>, KeychainError> {
        match backend {
            Backend::File => super::read(&self.file).map_err(KeychainError::Failed),
            Backend::Systemd => {
                systemd::read(self.credentials.as_deref()).map_err(KeychainError::Failed)
            }
            Backend::Keychain => self.keychain.read(entry).await,
        }
    }

    /// The backend `[secrets] backend` names, if it names one.
    pub(super) fn named(&self) -> Option<Backend> {
        match self.choice {
            Choice::Only(backend) => Some(backend),
            Choice::Auto => None,
        }
    }

    /// Where `backend` keeps the key of `slot`, for messages; `entry` is the keychain entry's
    /// user.
    pub(super) fn slot_place(&self, backend: Backend, entry: &str, slot: Slot) -> String {
        match (backend, slot) {
            (Backend::Keychain, Slot::Current) => self.keychain.describe(entry),
            (Backend::Keychain, Slot::Next) => self.keychain.describe(&next_entry(entry)),
            (Backend::File, Slot::Next) => next_file(&self.file).display().to_string(),
            (backend, _) => self.place(backend),
        }
    }

    /// The key in `slot` of `backend`, if it holds one; `entry` is the keychain entry's user.
    pub(super) async fn read_slot(
        &self,
        backend: Backend,
        entry: &str,
        slot: Slot,
    ) -> anyhow::Result<Option<MasterKey>> {
        match (backend, slot) {
            (_, Slot::Current) => self
                .read(backend, entry)
                .await
                .map_err(|e| self.unavailable(backend, entry, e)),
            (Backend::File, Slot::Next) => super::read(&next_file(&self.file)),
            (Backend::Keychain, Slot::Next) => {
                let next = next_entry(entry);
                self.keychain
                    .read(&next)
                    .await
                    .map_err(|e| e.into_anyhow(&self.keychain.describe(&next)))
            }
            (Backend::Systemd, Slot::Next) => Ok(None),
        }
    }

    /// Put `key` in `slot` of `backend`, replacing what it held, durably, and check that it
    /// reads back the same. Fronde never writes a systemd credential.
    pub(super) async fn write_slot(
        &self,
        backend: Backend,
        entry: &str,
        slot: Slot,
        key: &MasterKey,
    ) -> anyhow::Result<()> {
        let place = self.slot_place(backend, entry, slot);
        match backend {
            Backend::Systemd => bail!(
                "Fronde never writes the systemd credential {}",
                systemd::CREDENTIAL
            ),
            Backend::Keychain => {
                let user = match slot {
                    Slot::Current => entry.to_owned(),
                    Slot::Next => next_entry(entry),
                };
                self.keychain
                    .put(&user, key)
                    .await
                    .map_err(|e| e.into_anyhow(&place))?;
            }
            Backend::File => {
                let file = match slot {
                    Slot::Current => self.file.clone(),
                    Slot::Next => next_file(&self.file),
                };
                fronde_fs::write_atomic(&file, key.encode().as_slice()).map_err(|e| {
                    if super::unwritable(&e) {
                        super::not_writable(&file, e)
                    } else {
                        anyhow::Error::new(e)
                            .context(format!("cannot write the master key {place}"))
                    }
                })?;
                let back = super::read(&file)?;
                if back.is_none_or(|back| back.encode() != key.encode()) {
                    bail!("the master key read back from {place} is not the one written");
                }
            }
        }
        Ok(())
    }

    /// Remove the key in `slot` of `backend`; false if it held none. A systemd credential is
    /// never removed by Fronde: the unit loads it.
    pub(super) async fn delete_slot(
        &self,
        backend: Backend,
        entry: &str,
        slot: Slot,
    ) -> anyhow::Result<bool> {
        let place = self.slot_place(backend, entry, slot);
        match backend {
            Backend::Systemd => Ok(false),
            Backend::Keychain => {
                let user = match slot {
                    Slot::Current => entry.to_owned(),
                    Slot::Next => next_entry(entry),
                };
                self.keychain
                    .delete(&user)
                    .await
                    .map_err(|e| e.into_anyhow(&place))
            }
            Backend::File => {
                let file = match slot {
                    Slot::Current => self.file.clone(),
                    Slot::Next => next_file(&self.file),
                };
                match std::fs::remove_file(&file) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                    Err(e) => {
                        return Err(e).with_context(|| format!("cannot remove {place}"));
                    }
                }
                super::sync_dir(&file).with_context(|| format!("cannot remove {place}"))?;
                Ok(true)
            }
        }
    }

    /// The key in `backend`, or a new one there, and whether it was made now.
    async fn create(
        &self,
        backend: Backend,
        entry: &str,
        lock: &KeyLock,
    ) -> anyhow::Result<(MasterKey, bool)> {
        match backend {
            Backend::File => match super::read(&self.file)? {
                Some(key) => Ok((key, false)),
                None => Ok((super::create(&self.file)?, true)),
            },
            Backend::Keychain => self
                .keychain
                .read_or_create(entry, lock.share()?)
                .await
                .map_err(|e| e.into_anyhow(&self.keychain.describe(entry))),
            Backend::Systemd => match systemd::read(self.credentials.as_deref())? {
                Some(key) => Ok((key, false)),
                None => Err(self.no_credential()),
            },
        }
    }

    /// The first key with `auto`: the systemd credential when the service has one, else the
    /// keychain when it answers, else a file. A keychain that is there but locked is an error:
    /// unlocked later, it would hold no key while the file holds one, and the owner, who has a
    /// keychain, did not choose a file.
    async fn create_first(&self, entry: &str, lock: &KeyLock) -> anyhow::Result<(MasterKey, bool)> {
        if let Some(key) = systemd::read(self.credentials.as_deref())? {
            return Ok((key, false));
        }
        match self.keychain.read_or_create(entry, lock.share()?).await {
            Ok(made) => return Ok(made),
            Err(KeychainError::Absent(why)) => tracing::info!(
                "{} {why}: the master key goes to the file {} instead, for good",
                self.keychain.describe(entry),
                self.file.display()
            ),
            Err(e) => return Err(e.into_anyhow(&self.keychain.describe(entry))),
        }
        self.create(Backend::File, entry, lock).await
    }

    fn unavailable(&self, backend: Backend, entry: &str, e: KeychainError) -> anyhow::Error {
        match (backend, e) {
            (Backend::Keychain, e) => e.into_anyhow(&self.keychain.describe(entry)),
            (_, KeychainError::Failed(e)) => e,
            (_, KeychainError::Locked(why) | KeychainError::Absent(why)) => {
                anyhow::anyhow!("{} {why}", self.place(backend))
            }
        }
    }

    /// `[secrets] backend` names `chosen`, but the database records that `recorded` holds the
    /// key that sealed the stored secrets.
    fn contradicts(&self, chosen: Backend, recorded: Backend, entry: &str) -> String {
        let place = match recorded {
            Backend::Keychain => self.keychain.describe(entry),
            other => self.place(other),
        };
        format!(
            "[secrets] backend = \"{chosen}\", but the database records that the master key of \
             the stored secrets is kept in the {recorded} backend ({place}); set [secrets] \
             backend = \"auto\" or \"{recorded}\", or put that key in the {chosen} backend"
        )
    }

    fn missing(&self, backend: Backend, entry: &str) -> anyhow::Error {
        let without = "Fronde does not make a new key while secrets are stored: it could not \
                       open them. Without the key, remove each stored secret (`fronde secret \
                       list`, then `fronde secret remove <name>`) and store them again: once the \
                       last one is removed, the first one stored makes a new key";
        match backend {
            Backend::File => anyhow::anyhow!(
                "the master key {} is missing, but the database holds secrets sealed with it; \
                 restore the file from a backup. {without}",
                self.file.display()
            ),
            Backend::Keychain => anyhow::anyhow!(
                "the master key is missing from {}, but the database holds secrets sealed with \
                 it; restore the entry from a backup of the key (on Linux, `secret-tool search \
                 service fronde` lists the entries Fronde made). {without}",
                self.keychain.describe(entry)
            ),
            Backend::Systemd => self.no_credential().context(format!(
                "the database holds secrets sealed with the master key, which is not there. \
                 {without}"
            )),
        }
    }

    fn no_credential(&self) -> anyhow::Error {
        match &self.credentials {
            None => anyhow::anyhow!(
                "the master key is kept in the systemd credential {}, but $CREDENTIALS_DIRECTORY \
                 is not set: run Fronde under the unit that loads it \
                 (LoadCredentialEncrypted={}:…), or, for a command, under \
                 `systemd-run --user --pipe --wait -p LoadCredentialEncrypted={}:<file>`",
                systemd::CREDENTIAL,
                systemd::CREDENTIAL,
                systemd::CREDENTIAL
            ),
            Some(dir) => anyhow::anyhow!(
                "there is no systemd credential {}: Fronde never writes it. Encrypt a key line \
                 with `systemd-creds encrypt --name={}` and load it with \
                 LoadCredentialEncrypted= in the unit",
                systemd::path(dir).display(),
                systemd::CREDENTIAL
            ),
        }
    }
}

/// Refuses a recorded backend name this Fronde does not know.
pub(crate) fn parse_recorded(name: &[u8]) -> anyhow::Result<Backend> {
    match std::str::from_utf8(name).ok().and_then(Backend::from_name) {
        Some(backend) => Ok(backend),
        None => bail!(
            "the database records the master key in a backend this Fronde does not know ({}); \
             run the newer Fronde that recorded it",
            String::from_utf8_lossy(name)
        ),
    }
}
