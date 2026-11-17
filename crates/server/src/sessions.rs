//! The web sessions' store: `tower-sessions-sqlx-store` in `fronde.db`, with every id hashed
//! before it reaches the database, so a copy of the file signs no one in.

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tower_sessions::session::{Id, Record};
use tower_sessions::session_store::{self, ExpiredDeletion, SessionStore};
use tower_sessions_sqlx_store::SqliteStore;
use tower_sessions_sqlx_store::sqlx::SqlitePool;

/// Sessions in the table `tower_sessions`, each row keyed by the hash of its id
/// ([`stored_id`]). A cookie's id is random and 128 bits long, so a plain hash is enough: there
/// is nothing to guess from.
#[derive(Clone, Debug)]
pub struct Sessions(SqliteStore);

/// The id a session's row is stored under: the first 16 bytes of the SHA-256 of its real id.
pub fn stored_id(id: &Id) -> Id {
    let digest = Sha256::digest(id.0.to_le_bytes());
    let mut first = [0; 16];
    first.copy_from_slice(&digest[..16]);
    Id(i128::from_le_bytes(first))
}

/// `record` as stored: under its hashed id, which its serialized copy in the row also carries.
fn stored(record: &Record) -> Record {
    Record {
        id: stored_id(&record.id),
        ..record.clone()
    }
}

impl Sessions {
    pub fn new(pool: SqlitePool) -> Self {
        Self(SqliteStore::new(pool))
    }

    /// Creates the table when missing.
    pub async fn migrate(&self) -> Result<(), tower_sessions_sqlx_store::sqlx::Error> {
        self.0.migrate().await
    }
}

#[async_trait]
impl SessionStore for Sessions {
    async fn create(&self, record: &mut Record) -> session_store::Result<()> {
        loop {
            let wanted = stored_id(&record.id);
            let mut row = stored(record);
            // On a taken id, the inner store saves the row under a random id of its own.
            self.0.create(&mut row).await?;
            if row.id == wanted {
                return Ok(());
            }
            // No cookie maps to that random id: drop the row and try a new id, which the
            // trait leaves to the store.
            self.0.delete(&row.id).await?;
            record.id = Id::default();
        }
    }

    async fn save(&self, record: &Record) -> session_store::Result<()> {
        self.0.save(&stored(record)).await
    }

    async fn load(&self, id: &Id) -> session_store::Result<Option<Record>> {
        Ok(self
            .0
            .load(&stored_id(id))
            .await?
            .map(|record| Record { id: *id, ..record }))
    }

    async fn delete(&self, id: &Id) -> session_store::Result<()> {
        self.0.delete(&stored_id(id)).await
    }
}

#[async_trait]
impl ExpiredDeletion for Sessions {
    async fn delete_expired(&self) -> session_store::Result<()> {
        self.0.delete_expired().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_sessions::cookie::time::{Duration, OffsetDateTime};
    use tower_sessions_sqlx_store::sqlx;

    async fn sessions(dir: &std::path::Path) -> (fronde_db::DatabaseConnection, Sessions) {
        let db = fronde_db::open(dir).await.unwrap();
        let sessions = Sessions::new(db.get_sqlite_connection_pool().clone());
        sessions.migrate().await.unwrap();
        (db, sessions)
    }

    fn record(id: i128) -> Record {
        Record {
            id: Id(id),
            data: [("signed_in".to_owned(), serde_json::Value::Bool(true))].into(),
            expiry_date: OffsetDateTime::now_utc() + Duration::days(1),
        }
    }

    async fn stored_ids(db: &fronde_db::DatabaseConnection) -> Vec<String> {
        sqlx::query_scalar("SELECT id FROM tower_sessions ORDER BY id")
            .fetch_all(db.get_sqlite_connection_pool())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn ids_are_stored_hashed() {
        let dir = tempfile::tempdir().unwrap();
        let (db, sessions) = sessions(dir.path()).await;
        let mut created = record(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        sessions.create(&mut created).await.unwrap();
        let id = created.id;
        let blob: Vec<u8> = sqlx::query_scalar("SELECT data FROM tower_sessions")
            .fetch_one(db.get_sqlite_connection_pool())
            .await
            .unwrap();
        assert_eq!(stored_ids(&db).await, [stored_id(&id).to_string()]);
        let raw = id.0.to_le_bytes();
        assert!(
            !blob.windows(raw.len()).any(|w| w == raw)
                && !blob.windows(16).any(|w| w == id.0.to_be_bytes()),
            "the row's data holds the real id"
        );

        // Each operation finds the row by the real id, and hands that id back.
        assert_eq!(sessions.load(&id).await.unwrap(), Some(created.clone()));
        assert_eq!(sessions.load(&stored_id(&id)).await.unwrap(), None);
        let mut renewed = created.clone();
        renewed.expiry_date += Duration::days(1);
        sessions.save(&renewed).await.unwrap();
        assert_eq!(stored_ids(&db).await.len(), 1);
        assert_eq!(sessions.load(&id).await.unwrap(), Some(renewed));
        sessions.delete(&id).await.unwrap();
        assert!(stored_ids(&db).await.is_empty());
    }

    #[tokio::test]
    async fn a_taken_id_gets_a_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let (db, sessions) = sessions(dir.path()).await;
        let mut first = record(1);
        sessions.create(&mut first).await.unwrap();
        let mut second = record(1);
        sessions.create(&mut second).await.unwrap();
        assert_ne!(second.id, Id(1));
        let mut ids = vec![
            stored_id(&Id(1)).to_string(),
            stored_id(&second.id).to_string(),
        ];
        ids.sort();
        assert_eq!(
            stored_ids(&db).await,
            ids,
            "no row under an id no cookie maps to"
        );
        assert_eq!(sessions.load(&second.id).await.unwrap(), Some(second));
        assert_eq!(sessions.load(&Id(1)).await.unwrap(), Some(first));
    }
}
