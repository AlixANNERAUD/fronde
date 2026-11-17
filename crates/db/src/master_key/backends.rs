//! Which backend holds the master key. `[secrets] backend` names one, or is `auto`: then the
//! first key is made in a systemd credential if the service has one, else in the keychain if it
//! answers, else in a file, and the database records the backend that holds it
//! (`secrets`/`backend` in `kv`, written with the key check). From then on `auto` uses that one
//! only: a keychain locked at boot is an error, never a reason to make a new key elsewhere, which
//! would strand every stored secret.

use super::keychain::KeychainError;
use super::{Backend, Keychain, MasterKey, systemd};
use anyhow::bail;
use sea_orm::DatabaseConnection;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `[secrets] backend`: one backend, or `auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Choice {
    /// The recorded backend; for the first key, systemd, then the keychain, then a file.
    #[default]
    Auto,
    /// This backend alone.
    Only(Backend),
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

    /// Where `backend` keeps the key, for messages: a path or a keychain entry.
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

    /// The key that opens the stored secrets, from the backend in use, checked against the
    /// database. `None` when that backend holds no key and nothing is stored, or when, with
    /// `auto`, no key was made yet. A key missing while secrets are stored, a backend that cannot
    /// be read while they are, or a key that does not open them, is an error that says so;
    /// another backend is never tried.
    pub async fn load(&self, db: &DatabaseConnection) -> anyhow::Result<Option<Arc<MasterKey>>> {
        let stored = crate::secrets::any_stored(db).await?;
        let backend = match (self.choice, crate::secrets::recorded_backend(db).await?) {
            (Choice::Only(backend), _) | (Choice::Auto, Some(backend)) => backend,
            // Secrets stored before backends were recorded were sealed with the key file.
            (Choice::Auto, None) if stored => Backend::File,
            (Choice::Auto, None) => return Ok(None),
        };
        let mut key = match self.read(backend).await {
            Ok(key) => key,
            Err(KeychainError::Unavailable(why)) if !stored => {
                tracing::debug!("no master key read from {}: it {why}", self.place(backend));
                return Ok(None);
            }
            Err(e) => return Err(self.unavailable(backend, e)),
        };
        // A first secret stored since the key was read made the key before its row: read again.
        if key.is_none() && !stored && crate::secrets::any_stored(db).await? {
            key = self
                .read(backend)
                .await
                .map_err(|e| self.unavailable(backend, e))?;
        }
        match key {
            Some(key) => {
                crate::secrets::check_key(db, &key)
                    .await
                    .map_err(|e| e.context(format!("master key {}", self.place(backend))))?;
                Ok(Some(Arc::new(key)))
            }
            None if crate::secrets::any_stored(db).await? => Err(self.missing(backend)),
            None => Ok(None),
        }
    }

    /// [`Backends::load`], or, when no key is there and nothing is stored, a new key in the
    /// backend in use: the one `[secrets] backend` names, or, with `auto`, the recorded one, else
    /// the first of the systemd credential, the keychain and a file that can hold it. The flag
    /// says whether the key was made now.
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
        let (key, created) = match (self.choice, crate::secrets::recorded_backend(db).await?) {
            (Choice::Only(backend), _) | (Choice::Auto, Some(backend)) => {
                self.create(backend).await?
            }
            (Choice::Auto, None) => self.create_first().await?,
        };
        Ok((Arc::new(key), created))
    }

    /// The key in `backend`, if it holds one.
    async fn read(&self, backend: Backend) -> Result<Option<MasterKey>, KeychainError> {
        match backend {
            Backend::File => super::read(&self.file).map_err(KeychainError::Failed),
            Backend::Systemd => {
                systemd::read(self.credentials.as_deref()).map_err(KeychainError::Failed)
            }
            Backend::Keychain => self.keychain.read().await,
        }
    }

    /// The key in `backend`, or a new one there.
    async fn create(&self, backend: Backend) -> anyhow::Result<(MasterKey, bool)> {
        match backend {
            Backend::File => Ok((super::create(&self.file)?, true)),
            Backend::Keychain => self
                .keychain
                .read_or_create()
                .await
                .map_err(|e| e.into_anyhow(&self.keychain)),
            Backend::Systemd => match systemd::read(self.credentials.as_deref())? {
                Some(key) => Ok((key, false)),
                None => Err(self.no_credential()),
            },
        }
    }

    /// The first key with `auto`: the systemd credential when the service has one, else the
    /// keychain when it answers, else a file.
    async fn create_first(&self) -> anyhow::Result<(MasterKey, bool)> {
        if let Some(key) = systemd::read(self.credentials.as_deref())? {
            return Ok((key, false));
        }
        match self.keychain.read_or_create().await {
            Ok(made) => return Ok(made),
            Err(KeychainError::Unavailable(why)) => tracing::info!(
                "{} {why}: the master key goes to the file {} instead, for good",
                self.keychain,
                self.file.display()
            ),
            Err(e) => return Err(e.into_anyhow(&self.keychain)),
        }
        self.create(Backend::File).await
    }

    fn unavailable(&self, backend: Backend, e: KeychainError) -> anyhow::Error {
        match (backend, e) {
            (Backend::Keychain, e) => e.into_anyhow(&self.keychain),
            (_, KeychainError::Failed(e)) => e,
            (_, KeychainError::Unavailable(why)) => {
                anyhow::anyhow!("{} {why}", self.place(backend))
            }
        }
    }

    fn missing(&self, backend: Backend) -> anyhow::Error {
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
                 it; restore the entry from a backup of the key. {without}",
                self.keychain
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
