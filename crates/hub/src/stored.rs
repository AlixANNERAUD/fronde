//! The secrets stored in `fronde.db` for a source's credentials (`{ stored = "name" }`), still
//! sealed, with the master key that opens them.

use crate::origin::Origin;
use crate::source::Waiting;
use anyhow::Context;
use fronde_db::master_key::{Backend, Backends, MasterKey};
use fronde_db::secrets::Sealed;
use secrecy::SecretString;
use std::sync::Arc;

/// The secrets stored for components' credentials and upstream servers' tokens, still sealed, and the master key that opens
/// them. Nothing is opened until [`prepare`] needs one: the fields are private, so that only
/// [`StoredSecrets::load`] fills them and only `prepare` opens a secret from them.
#[derive(Debug, Clone, Default)]
pub struct StoredSecrets {
    /// `None` when nothing is stored and there is no key file.
    key: Option<Arc<MasterKey>>,
    sealed: Vec<Sealed>,
}

impl StoredSecrets {
    /// The rows of `db` in the `sources` scope, and the master key from `backends`, checked
    /// against `db`: a key that did not seal them, or no key while secrets are stored, is an
    /// error. Only reads.
    pub async fn load(
        db: &fronde_db::DatabaseConnection,
        backends: &Backends,
    ) -> anyhow::Result<Self> {
        let key = backends.load(db).await?;
        let sealed = fronde_db::secrets::load_sealed(db, fronde_db::secrets::SOURCES).await?;
        Ok(Self { key, sealed })
    }

    /// [`StoredSecrets::load`] from `recorded` and `sealed`, read with the database before it was
    /// let go: only the key comes from `backends` now, which may take seconds (a keychain).
    pub async fn load_recorded(
        recorded: &fronde_db::master_key::Recorded,
        sealed: Vec<Sealed>,
        backends: &Backends,
    ) -> anyhow::Result<Self> {
        let key = backends.load_recorded(recorded).await?;
        Ok(Self { key, sealed })
    }

    /// Where the master key came from, if one was read.
    pub fn key_backend(&self) -> Option<Backend> {
        self.key.as_ref().map(|key| key.backend())
    }

    /// Whether a secret named `name` is stored, for any origin.
    pub fn is_stored(&self, name: &str) -> bool {
        self.sealed.iter().any(|s| s.name == name)
    }

    /// The value of the secret `name`, which must have been stored for `origin`. A secret not
    /// stored yet, or stored for another origin, is [`Waiting`]: `fronde secret set` fixes it
    /// while Fronde runs.
    pub fn open(&self, name: &str, origin: &Origin) -> anyhow::Result<SecretString> {
        let set = format!(
            "`fronde secret set {name} --origin {origin} --config <your fronde.toml>`, the value \
             on standard input"
        );
        let Some(sealed) = self.sealed.iter().find(|s| s.name == name) else {
            return Err(Waiting(format!(
                "no secret named {name} is stored; store it with {set}"
            ))
            .into());
        };
        let stored = Origin::parse(&sealed.origin).with_context(|| {
            format!("the stored secret {name} has an unreadable origin; store it again with {set}")
        })?;
        if stored != *origin {
            return Err(Waiting(format!(
                "the stored secret {name} is for {stored}, not {origin}, and a stored secret goes \
                 only to its own origin; store it again for {origin} with {set}, or name the \
                 secret stored for {origin}"
            ))
            .into());
        }
        // The key is read before the rows: a first secret stored in between leaves a row without
        // its key, which the next try reads.
        let Some(key) = self.key.as_deref() else {
            return Err(Waiting(format!(
                "the stored secret {name} was stored while Fronde read the stored secrets; \
                 trying again"
            ))
            .into());
        };
        sealed.open(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row read without its key, as when a first secret is stored between the read of the key
    /// and the read of the rows, is waited for: the next try reads both.
    #[tokio::test]
    async fn a_row_without_its_key_is_waited_for() {
        let dir = tempfile::tempdir().unwrap();
        let db = fronde_db::open(dir.path()).await.unwrap();
        let file = dir.path().join("master.key");
        let (key, _) = fronde_db::master_key::load_or_create(&db, &file)
            .await
            .unwrap();
        let origin = "https://cloud.example";
        let value = secrecy::SecretString::from("value".to_owned());
        fronde_db::secrets::set(
            &db,
            &key,
            fronde_db::secrets::SOURCES,
            "api",
            origin,
            &value,
        )
        .await
        .unwrap();
        let secrets = StoredSecrets {
            key: None,
            sealed: fronde_db::secrets::load_sealed(&db, fronde_db::secrets::SOURCES)
                .await
                .unwrap(),
        };
        let e = secrets
            .open("api", &Origin::parse(origin).unwrap())
            .unwrap_err();
        assert!(e.downcast_ref::<Waiting>().is_some(), "{e:#}");
        let loaded = StoredSecrets::load(&db, &Backends::file(&file))
            .await
            .unwrap();
        assert!(loaded.open("api", &Origin::parse(origin).unwrap()).is_ok());
    }
}
