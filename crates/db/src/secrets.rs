//! Secrets the owner stored, each sealed with the master key ([`crate::master_key`]) by
//! XChaCha20-Poly1305 under a random 24-byte nonce. The associated data binds the ciphertext to
//! the row's format version, scope, name and origin, so a row edited or moved to another name,
//! scope or origin fails to open instead of handing its secret to the wrong place.
//!
//! The first secret also stores a key check: a fixed text sealed with the same key, in `kv`
//! (`secrets`/`key_check`), under associated data of its own domain (`fronde key check`), so
//! that it can never be taken for a secret row or the other way round. A wrong key is then
//! reported as a wrong key, not as damaged rows, and a new key is never made while it exists.
//!
//! Origins are stored as the caller gives them: canonical, written by `Origin::to_string()`
//! (`fronde_hub::origin`). Values only leave this module as `SecretString`, and nothing here
//! prints one.

use crate::master_key::MasterKey;
use anyhow::{Context as _, bail};
use chacha20poly1305::aead::{Aead as _, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use chrono::{DateTime, Utc};
use fronde_entity::secrets;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, Set,
};
use secrecy::{ExposeSecret as _, SecretString};
use std::fmt;
use zeroize::Zeroizing;

/// The scope of every component credential, for now.
pub const SOURCES: &str = "sources";

/// The sealed format this version writes and reads: the associated data is
/// `fronde secret | version | scope | name | origin`, each part length-prefixed.
pub const VERSION: i64 = 1;

const NONCE_LEN: usize = 24;

/// Where the key check lives in `kv`.
const CHECK_SCOPE: &str = "secrets";
const CHECK_KEY: &str = "key_check";
/// The key check's format, its first byte; then the nonce, then the sealed text. Format 2 seals
/// it under its own domain label; format 1, written by the first version of the store, sealed it
/// under the secret rows' label: it is still accepted, and sealed again in format 2 by the next
/// write ([`set`], [`upgrade_key_check`]).
const CHECK_VERSION: u8 = 2;
const CHECK_VERSION_ROW_LABEL: u8 = 1;
const CHECK_TEXT: &[u8] = b"fronde key check v1";

/// The domain labels that start the associated data: a secret row, the key check.
const ROW_LABEL: &[u8] = b"fronde secret";
const CHECK_LABEL: &[u8] = b"fronde key check";

/// The longest value accepted, in bytes.
pub const MAX_VALUE_LEN: usize = 64 * 1024;

/// A stored secret as listed: never its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub origin: String,
    pub updated_at: DateTime<Utc>,
}

/// A stored secret still sealed, as read from the database. [`Sealed::open`] gives its value.
#[derive(Clone)]
pub struct Sealed {
    pub scope: String,
    pub name: String,
    pub origin: String,
    version: i64,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
}

impl fmt::Debug for Sealed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealed")
            .field("scope", &self.scope)
            .field("name", &self.name)
            .field("origin", &self.origin)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl From<secrets::Model> for Sealed {
    fn from(row: secrets::Model) -> Self {
        Self {
            scope: row.scope,
            name: row.name,
            origin: row.origin,
            version: row.version,
            nonce: row.nonce,
            ciphertext: row.ciphertext,
        }
    }
}

impl Sealed {
    /// The value, if `key` sealed it for this row's version, scope, name and origin.
    pub fn open(&self, key: &MasterKey) -> anyhow::Result<SecretString> {
        let damaged = || {
            format!(
                "the stored secret {} cannot be opened: its row was changed or moved; set it \
                 again with `fronde secret set {} --origin …`",
                self.name, self.name
            )
        };
        if self.version != VERSION {
            bail!(
                "the stored secret {} is sealed in format {}, which this Fronde does not read; \
                 run the newer Fronde that stored it, or set it again",
                self.name,
                self.version
            );
        }
        let plain = open_with(
            &key.cipher(),
            &self.nonce,
            &self.ciphertext,
            &associated_data(self.version, &self.scope, &self.name, &self.origin),
        )
        .with_context(damaged)?;
        let text = std::str::from_utf8(&plain).ok().with_context(damaged)?;
        // Exactly the value's length, so making the `SecretString` does not move it and leave a
        // copy behind.
        let mut value = String::with_capacity(text.len());
        value.push_str(text);
        Ok(SecretString::from(value))
    }
}

/// `fronde secret | version | scope | name | origin`, each part prefixed by its length.
fn associated_data(version: i64, scope: &str, name: &str, origin: &str) -> Vec<u8> {
    labelled(ROW_LABEL, version, scope, name, origin)
}

/// `label | version | scope | name | origin`, each part prefixed by its length.
fn labelled(label: &[u8], version: i64, scope: &str, name: &str, origin: &str) -> Vec<u8> {
    let version = version.to_be_bytes();
    let parts: [&[u8]; 5] = [
        label,
        &version,
        scope.as_bytes(),
        name.as_bytes(),
        origin.as_bytes(),
    ];
    let mut aad = Vec::new();
    for part in parts {
        aad.extend_from_slice(&(part.len() as u32).to_be_bytes());
        aad.extend_from_slice(part);
    }
    aad
}

fn new_nonce() -> anyhow::Result<[u8; NONCE_LEN]> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|e| anyhow::anyhow!("no randomness available: {e}"))?;
    Ok(nonce)
}

fn seal_with(
    cipher: &XChaCha20Poly1305,
    nonce: &[u8; NONCE_LEN],
    plain: &[u8],
    aad: &[u8],
) -> anyhow::Result<Vec<u8>> {
    cipher
        .encrypt(<&XNonce>::from(nonce), Payload { msg: plain, aad })
        .map_err(|_| anyhow::anyhow!("cannot seal the secret"))
}

/// The plain text, zeroed when dropped; an error if anything does not match.
fn open_with(
    cipher: &XChaCha20Poly1305,
    nonce: &[u8],
    ciphertext: &[u8],
    aad: &[u8],
) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    let nonce: &[u8; NONCE_LEN] = nonce.try_into().context("the nonce has the wrong length")?;
    cipher
        .decrypt(
            <&XNonce>::from(nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| anyhow::anyhow!("it does not authenticate"))
}

/// The key check's associated data in `format`.
fn check_aad(format: u8) -> Vec<u8> {
    let label = if format == CHECK_VERSION_ROW_LABEL {
        ROW_LABEL
    } else {
        CHECK_LABEL
    };
    labelled(label, i64::from(format), CHECK_SCOPE, CHECK_KEY, "")
}

/// A new key check for `cipher`'s key, in the current format.
fn seal_check(cipher: &XChaCha20Poly1305) -> anyhow::Result<Vec<u8>> {
    let nonce = new_nonce()?;
    let sealed = seal_with(cipher, &nonce, CHECK_TEXT, &check_aad(CHECK_VERSION))?;
    let mut value = vec![CHECK_VERSION];
    value.extend_from_slice(&nonce);
    value.extend_from_slice(&sealed);
    Ok(value)
}

async fn write_check<C: ConnectionTrait>(db: &C, check: Vec<u8>) -> anyhow::Result<()> {
    crate::kv::set(db, CHECK_SCOPE, CHECK_KEY, check, None)
        .await
        .context("cannot store the master key's check")
}

/// Whether anything is sealed: a secret, or the key check that the first one stored.
pub async fn any_stored<C: ConnectionTrait>(db: &C) -> anyhow::Result<bool> {
    let rows = secrets::Entity::find()
        .count(db)
        .await
        .context("cannot read the stored secrets")?;
    Ok(rows > 0 || stored_check(db).await?.is_some())
}

async fn stored_check<C: ConnectionTrait>(db: &C) -> anyhow::Result<Option<Vec<u8>>> {
    crate::kv::get(db, CHECK_SCOPE, CHECK_KEY)
        .await
        .context("cannot read the master key's check in the database")
}

/// Whether `key` opens the stored key check `check`: an error if not, else whether the check is
/// in the older format and is to be sealed again.
fn opens(key: &MasterKey, check: &[u8]) -> anyhow::Result<bool> {
    let wrong = || {
        format!(
            "this master key (from {}) is not the one that sealed the stored secrets; restore \
             the right key from a backup",
            key.backend()
        )
    };
    let (&version, rest) = check.split_first().context("the key check is empty")?;
    if version != CHECK_VERSION && version != CHECK_VERSION_ROW_LABEL {
        bail!("the key check is in format {version}, which this Fronde does not read");
    }
    if rest.len() < NONCE_LEN {
        bail!("the key check is cut short");
    }
    let (nonce, sealed) = rest.split_at(NONCE_LEN);
    let plain = open_with(&key.cipher(), nonce, sealed, &check_aad(version)).with_context(wrong)?;
    if plain.as_slice() != CHECK_TEXT {
        bail!(wrong());
    }
    Ok(version != CHECK_VERSION)
}

/// Check that `key` is the one that sealed the stored secrets. With nothing stored, any key is.
/// Secrets stored without a key check cannot be checked and are refused.
pub async fn check_key<C: ConnectionTrait>(db: &C, key: &MasterKey) -> anyhow::Result<()> {
    match stored_check(db).await? {
        Some(check) => opens(key, &check).map(drop),
        None => {
            let rows = secrets::Entity::find()
                .count(db)
                .await
                .context("cannot read the stored secrets")?;
            if rows > 0 {
                bail!(
                    "the database holds stored secrets but no key check, so no key can be \
                     trusted to open them; restore the data directory from a backup"
                );
            }
            Ok(())
        }
    }
}

/// Check `key` as [`check_key`] does and, if the key check is in the older format, seal it again
/// in the current one, in one write transaction. True if it was sealed again. `fronde serve` calls
/// it at start, so that a database stays in the older format only until then.
pub async fn upgrade_key_check(db: &DatabaseConnection, key: &MasterKey) -> anyhow::Result<bool> {
    let transaction = crate::begin_write(db)
        .await
        .context("cannot write to the database")?;
    let Some(check) = stored_check(&transaction).await? else {
        return Ok(false);
    };
    if !opens(key, &check)? {
        return Ok(false);
    }
    write_check(&transaction, seal_check(&key.cipher())?).await?;
    transaction
        .commit()
        .await
        .context("cannot store the master key's check")?;
    Ok(true)
}

/// Whether the database has the secrets table. A database last opened by an older Fronde does
/// not, which only [`crate::open_read_only`] sees: [`crate::open`] adds it.
pub async fn table_exists(db: &DatabaseConnection) -> Result<bool, sea_orm::DbErr> {
    fronde_migration::SchemaManager::new(db)
        .has_table("secrets")
        .await
}

/// A secret's name: 1 to 64 ASCII letters, digits, `-`, `_` or `.`.
pub fn check_name(name: &str) -> anyhow::Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if !ok {
        bail!("secret name {name:?} is not allowed: use 1 to 64 letters, digits, '-', '_' or '.'");
    }
    Ok(())
}

/// Seal `value` with `key` and store it under `scope` and `name`, for `origin` (canonical),
/// replacing any earlier value. The first secret also stores the key check; later ones are
/// refused if `key` does not open it.
pub async fn set(
    db: &DatabaseConnection,
    key: &MasterKey,
    scope: &str,
    name: &str,
    origin: &str,
    value: &SecretString,
) -> anyhow::Result<()> {
    check_name(name)?;
    let plain = value.expose_secret().as_bytes();
    if plain.is_empty() {
        bail!("the secret {name} is empty");
    }
    if plain.len() > MAX_VALUE_LEN {
        bail!("the secret {name} is longer than {MAX_VALUE_LEN} bytes");
    }
    let cipher = key.cipher();
    let nonce = new_nonce()?;
    let ciphertext = seal_with(
        &cipher,
        &nonce,
        plain,
        &associated_data(VERSION, scope, name, origin),
    )?;

    let transaction = crate::begin_write(db)
        .await
        .context("cannot write to the database")?;
    // The real guard against sealing under a key that is not the stored secrets' one: inside the
    // write transaction, so no other writer can store a first secret under another key between
    // this check and this write.
    match stored_check(&transaction).await? {
        Some(check) => {
            if opens(key, &check)? {
                write_check(&transaction, seal_check(&cipher)?).await?;
            }
        }
        None => {
            check_key(&transaction, key).await?;
            write_check(&transaction, seal_check(&cipher)?).await?;
        }
    }
    let now = Utc::now();
    let created_at = secrets::Entity::find_by_id((scope.to_owned(), name.to_owned()))
        .one(&transaction)
        .await
        .context("cannot read the stored secrets")?
        .map_or(now, |row| row.created_at);
    let row = secrets::ActiveModel {
        scope: Set(scope.to_owned()),
        name: Set(name.to_owned()),
        origin: Set(origin.to_owned()),
        version: Set(VERSION),
        nonce: Set(nonce.to_vec()),
        ciphertext: Set(ciphertext),
        created_at: Set(created_at),
        updated_at: Set(now),
    };
    secrets::Entity::insert(row)
        .on_conflict(
            sea_orm::sea_query::OnConflict::columns([
                secrets::Column::Scope,
                secrets::Column::Name,
            ])
            .update_columns([
                secrets::Column::Origin,
                secrets::Column::Version,
                secrets::Column::Nonce,
                secrets::Column::Ciphertext,
                secrets::Column::UpdatedAt,
            ])
            .to_owned(),
        )
        .exec(&transaction)
        .await
        .context("cannot store the secret")?;
    transaction
        .commit()
        .await
        .context("cannot store the secret")
}

/// Remove the secret under `scope` and `name`; true if there was one. Removing the last secret
/// also removes the key check, so that a lost key no longer blocks a new one.
pub async fn remove(db: &DatabaseConnection, scope: &str, name: &str) -> anyhow::Result<bool> {
    let transaction = crate::begin_write(db)
        .await
        .context("cannot write to the database")?;
    let removed = secrets::Entity::delete_by_id((scope.to_owned(), name.to_owned()))
        .exec(&transaction)
        .await
        .context("cannot remove the secret")?
        .rows_affected
        > 0;
    let left = secrets::Entity::find()
        .count(&transaction)
        .await
        .context("cannot read the stored secrets")?;
    if left == 0 {
        crate::kv::delete(&transaction, CHECK_SCOPE, CHECK_KEY)
            .await
            .context("cannot remove the master key's check")?;
    }
    transaction
        .commit()
        .await
        .context("cannot remove the secret")?;
    Ok(removed)
}

/// The secrets under `scope`, by name: names, origins and times, never values.
pub async fn list<C: ConnectionTrait>(db: &C, scope: &str) -> anyhow::Result<Vec<Listed>> {
    Ok(secrets::Entity::find()
        .filter(secrets::Column::Scope.eq(scope))
        .order_by_asc(secrets::Column::Name)
        .all(db)
        .await
        .context("cannot read the stored secrets")?
        .into_iter()
        .map(|row| Listed {
            name: row.name,
            origin: row.origin,
            updated_at: row.updated_at,
        })
        .collect())
}

/// The secrets under `scope`, still sealed.
pub async fn load_sealed<C: ConnectionTrait>(db: &C, scope: &str) -> anyhow::Result<Vec<Sealed>> {
    Ok(secrets::Entity::find()
        .filter(secrets::Column::Scope.eq(scope))
        .order_by_asc(secrets::Column::Name)
        .all(db)
        .await
        .context("cannot read the stored secrets")?
        .into_iter()
        .map(Sealed::from)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The version is part of what is authenticated: a row sealed in one format does not open
    /// as another, whatever reads it.
    #[test]
    fn version_is_bound() {
        let key = MasterKey::generate("test").unwrap();
        let cipher = key.cipher();
        let nonce = new_nonce().unwrap();
        let sealed = seal_with(
            &cipher,
            &nonce,
            b"value",
            &associated_data(1, "sources", "caldav", "https://cloud.example"),
        )
        .unwrap();
        let opened = open_with(
            &cipher,
            &nonce,
            &sealed,
            &associated_data(1, "sources", "caldav", "https://cloud.example"),
        )
        .unwrap();
        assert_eq!(opened.as_slice(), b"value");
        assert!(
            open_with(
                &cipher,
                &nonce,
                &sealed,
                &associated_data(2, "sources", "caldav", "https://cloud.example"),
            )
            .is_err()
        );
    }

    /// The key check and the secret rows are sealed under different domain labels: a check does
    /// not open as a row with the same scope, name and origin, nor the other way round.
    #[test]
    fn key_check_has_its_own_domain() {
        let key = MasterKey::generate("test").unwrap();
        let check = seal_check(&key.cipher()).unwrap();
        assert_eq!(check[0], CHECK_VERSION);
        assert!(
            !opens(&key, &check).unwrap(),
            "a current check needs no new seal"
        );
        let (nonce, sealed) = check[1..].split_at(NONCE_LEN);
        let as_row = associated_data(i64::from(CHECK_VERSION), CHECK_SCOPE, CHECK_KEY, "");
        assert!(open_with(&key.cipher(), nonce, sealed, &as_row).is_err());
        assert_ne!(check_aad(CHECK_VERSION), as_row);
    }

    /// A key check written by the first version of the store, under the rows' label.
    fn row_labelled_check(key: &MasterKey) -> Vec<u8> {
        let nonce = new_nonce().unwrap();
        let aad = associated_data(1, CHECK_SCOPE, CHECK_KEY, "");
        let sealed = seal_with(&key.cipher(), &nonce, CHECK_TEXT, &aad).unwrap();
        let mut check = vec![CHECK_VERSION_ROW_LABEL];
        check.extend_from_slice(&nonce);
        check.extend_from_slice(&sealed);
        check
    }

    /// A check in the older format still opens with its key, and is sealed again in the current
    /// format by the next write, in the same transaction, or by `upgrade_key_check`; a wrong key
    /// is still refused.
    #[tokio::test]
    async fn older_key_check_is_accepted_and_sealed_again() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::open(dir.path()).await.unwrap();
        let key = MasterKey::generate("test").unwrap();
        let other = MasterKey::generate("test").unwrap();
        let old = row_labelled_check(&key);
        assert!(
            opens(&key, &old).unwrap(),
            "the older format is to be sealed again"
        );
        assert!(opens(&other, &old).is_err());

        // Through `set`.
        write_check(&db, old.clone()).await.unwrap();
        check_key(&db, &key).await.unwrap();
        assert!(check_key(&db, &other).await.is_err());
        let value = SecretString::from("value");
        assert!(
            set(&db, &other, SOURCES, "a", "https://x", &value)
                .await
                .is_err()
        );
        assert_eq!(
            stored_check(&db).await.unwrap().unwrap(),
            old,
            "refused, unchanged"
        );
        set(&db, &key, SOURCES, "a", "https://x", &value)
            .await
            .unwrap();
        let new = stored_check(&db).await.unwrap().unwrap();
        assert_eq!(new[0], CHECK_VERSION);
        assert!(!opens(&key, &new).unwrap());

        // Through `upgrade_key_check`, which `serve` runs at start.
        write_check(&db, old.clone()).await.unwrap();
        assert!(upgrade_key_check(&db, &other).await.is_err());
        assert_eq!(stored_check(&db).await.unwrap().unwrap(), old);
        assert!(upgrade_key_check(&db, &key).await.unwrap());
        let new = stored_check(&db).await.unwrap().unwrap();
        assert_eq!(new[0], CHECK_VERSION);
        assert!(!upgrade_key_check(&db, &key).await.unwrap(), "only once");
        assert_eq!(stored_check(&db).await.unwrap().unwrap(), new);
    }

    /// Length prefixes keep parts apart: moving a character from one part to the next changes
    /// the associated data.
    #[test]
    fn parts_do_not_run_together() {
        assert_ne!(
            associated_data(1, "sources", "a", "bhttps://x"),
            associated_data(1, "sources", "ab", "https://x")
        );
    }
}
