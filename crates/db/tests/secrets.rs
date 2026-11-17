use fronde_db::master_key::{self, MasterKey};
use fronde_db::secrets::{self, SOURCES};
use fronde_entity::secrets as rows;
use sea_orm::{ActiveModelTrait, EntityTrait, IntoActiveModel, Set};
use secrecy::{ExposeSecret, SecretString};
use std::path::Path;

const ORIGIN: &str = "https://cloud.example";
const CANARY: &str = "canary-stored-secret-7f3e9a1c5b";

async fn setup() -> (tempfile::TempDir, fronde_db::DatabaseConnection) {
    let dir = tempfile::tempdir().unwrap();
    let db = fronde_db::open(dir.path()).await.unwrap();
    (dir, db)
}

async fn store(db: &fronde_db::DatabaseConnection, data: &Path, name: &str, value: &str) {
    let (key, _) = master_key::load_or_create(db, data).await.unwrap();
    secrets::set(db, &key, SOURCES, name, ORIGIN, &SecretString::from(value))
        .await
        .unwrap();
}

async fn open_only(db: &fronde_db::DatabaseConnection, data: &Path) -> anyhow::Result<String> {
    let key = master_key::load(db, data).await?.expect("a key");
    let sealed = secrets::load_sealed(db, SOURCES).await?;
    assert_eq!(sealed.len(), 1, "{sealed:?}");
    Ok(sealed[0].open(&key)?.expose_secret().to_owned())
}

/// Changes the one stored row with `edit`.
async fn tamper(db: &fronde_db::DatabaseConnection, edit: impl FnOnce(&mut rows::ActiveModel)) {
    let row = rows::Entity::find().one(db).await.unwrap().unwrap();
    let mut row = row.into_active_model();
    edit(&mut row);
    row.update(db).await.unwrap();
}

fn flip_first(bytes: &[u8]) -> Vec<u8> {
    let mut bytes = bytes.to_vec();
    bytes[0] ^= 1;
    bytes
}

fn other_key_line() -> String {
    // 32 bytes of 0x01 in unpadded base64url.
    format!(
        "fronde-key-v1:{}\n",
        "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE"
    )
}

#[tokio::test]
async fn roundtrip() {
    let (dir, db) = setup().await;
    store(&db, dir.path(), "caldav", CANARY).await;
    assert_eq!(open_only(&db, dir.path()).await.unwrap(), CANARY);

    // Setting it again replaces the value, with a new nonce, and keeps one row.
    let before = rows::Entity::find().one(&db).await.unwrap().unwrap();
    store(&db, dir.path(), "caldav", "second value").await;
    let after = rows::Entity::find().one(&db).await.unwrap().unwrap();
    assert_ne!(before.nonce, after.nonce);
    assert_eq!(before.created_at, after.created_at);
    assert_eq!(after.version, secrets::VERSION);
    assert_eq!(open_only(&db, dir.path()).await.unwrap(), "second value");
}

#[tokio::test]
async fn list_never_returns_values() {
    let (dir, db) = setup().await;
    store(&db, dir.path(), "caldav", CANARY).await;
    store(&db, dir.path(), "another", CANARY).await;
    let listed = secrets::list(&db, SOURCES).await.unwrap();
    let names: Vec<_> = listed
        .iter()
        .map(|s| (s.name.as_str(), s.origin.as_str()))
        .collect();
    assert_eq!(names, [("another", ORIGIN), ("caldav", ORIGIN)]);
    assert!(!format!("{listed:?}").contains(CANARY));
    assert!(secrets::list(&db, "elsewhere").await.unwrap().is_empty());
}

#[tokio::test]
async fn names_are_checked() {
    let (dir, db) = setup().await;
    let (key, _) = master_key::load_or_create(&db, dir.path()).await.unwrap();
    for bad in ["", "with space", "slash/name", "é", &"a".repeat(65)] {
        let err = secrets::set(&db, &key, SOURCES, bad, ORIGIN, &SecretString::from("v"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not allowed"), "{err}");
    }
    let err = secrets::set(&db, &key, SOURCES, "empty", ORIGIN, &SecretString::from(""))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("empty"), "{err}");
    assert!(!secrets::any_stored(&db).await.unwrap());
}

#[cfg(unix)]
#[tokio::test]
async fn master_key_file_is_created_private() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, db) = setup().await;
    let _umask = rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o022));
    let file = master_key::path(dir.path());
    assert!(master_key::load(&db, dir.path()).await.unwrap().is_none());
    assert!(!file.exists(), "loading never creates a key");

    let (_, created) = master_key::load_or_create(&db, dir.path()).await.unwrap();
    assert!(created);
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let line = std::fs::read_to_string(&file).unwrap();
    assert!(
        line.starts_with("fronde-key-v1:") && line.ends_with('\n'),
        "{line:?}"
    );
    assert_eq!(line.trim_end().len(), "fronde-key-v1:".len() + 43);
    // Nothing else is left beside it.
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("master.key"))
        .collect();
    assert_eq!(names, ["master.key"]);

    let (_, created) = master_key::load_or_create(&db, dir.path()).await.unwrap();
    assert!(!created, "an existing key is kept");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), line);
}

#[cfg(unix)]
#[tokio::test]
async fn a_key_file_open_to_others_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, db) = setup().await;
    master_key::load_or_create(&db, dir.path()).await.unwrap();
    let file = master_key::path(dir.path());
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = master_key::load(&db, dir.path()).await.unwrap_err();
    assert!(format!("{err:#}").contains("chmod 600"), "{err:#}");
}

#[test]
fn key_format_is_checked() {
    let good = "fronde-key-v1:AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE";
    for text in [good.to_owned(), format!("{good}\n"), format!("{good}\r\n")] {
        let key = MasterKey::parse(text.as_bytes(), "test").unwrap();
        assert_eq!(key.backend(), "test");
    }
    for bad in [
        String::new(),
        "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE".to_owned(),
        "fronde-key-v2:AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE".to_owned(),
        format!("{good}A"),
        good[..good.len() - 1].to_owned(),
        format!("{good}\n\n"),
        format!("{good}\r\nmore"),
        format!(" {good}"),
        // Standard base64, not base64url.
        "fronde-key-v1:+QEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE".to_owned(),
        // 43 characters whose last one carries bits beyond the 32 bytes.
        "fronde-key-v1:AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQF".to_owned(),
    ] {
        let err = MasterKey::parse(bad.as_bytes(), "test")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a Fronde master key"), "{bad:?}: {err}");
        assert!(!err.contains("AQEB"), "the error quotes the key: {err}");
    }
}

#[tokio::test]
async fn wrong_key_is_reported_as_such() {
    let (dir, db) = setup().await;
    store(&db, dir.path(), "caldav", CANARY).await;
    let file = master_key::path(dir.path());
    fronde_fs::write(&file, other_key_line()).unwrap();

    let err = format!("{:#}", master_key::load(&db, dir.path()).await.unwrap_err());
    assert!(
        err.contains("is not the one that sealed the stored secrets"),
        "{err}"
    );
    assert!(err.contains(&file.display().to_string()), "{err}");
    let err = format!(
        "{:#}",
        master_key::load_or_create(&db, dir.path())
            .await
            .unwrap_err()
    );
    assert!(err.contains("is not the one that sealed"), "{err}");

    // Writing with the wrong key is refused, and leaves the row as it was.
    let wrong = MasterKey::parse(other_key_line().as_bytes(), "file").unwrap();
    let before = rows::Entity::find().all(&db).await.unwrap();
    let err = secrets::set(
        &db,
        &wrong,
        SOURCES,
        "other",
        ORIGIN,
        &SecretString::from("v"),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("is not the one that sealed"), "{err}");
    assert_eq!(rows::Entity::find().all(&db).await.unwrap(), before);
    // And the row does not open with it.
    let sealed = secrets::load_sealed(&db, SOURCES).await.unwrap();
    assert!(sealed[0].open(&wrong).is_err());
}

#[tokio::test]
async fn tampered_ciphertext_is_refused() {
    let (dir, db) = setup().await;
    store(&db, dir.path(), "caldav", CANARY).await;
    let row = rows::Entity::find().one(&db).await.unwrap().unwrap();
    tamper(&db, |r| r.ciphertext = Set(flip_first(&row.ciphertext))).await;
    let err = format!("{:#}", open_only(&db, dir.path()).await.unwrap_err());
    assert!(err.contains("cannot be opened"), "{err}");
    assert!(!err.contains(CANARY));
}

#[tokio::test]
async fn tampered_nonce_is_refused() {
    let (dir, db) = setup().await;
    store(&db, dir.path(), "caldav", CANARY).await;
    let row = rows::Entity::find().one(&db).await.unwrap().unwrap();
    tamper(&db, |r| r.nonce = Set(flip_first(&row.nonce))).await;
    let err = format!("{:#}", open_only(&db, dir.path()).await.unwrap_err());
    assert!(err.contains("cannot be opened"), "{err}");
    tamper(&db, |r| r.nonce = Set(row.nonce[..12].to_vec())).await;
    assert!(open_only(&db, dir.path()).await.is_err());
}

#[tokio::test]
async fn tampered_version_is_refused() {
    let (dir, db) = setup().await;
    store(&db, dir.path(), "caldav", CANARY).await;
    for version in [0, 2] {
        tamper(&db, |r| r.version = Set(version)).await;
        let err = format!("{:#}", open_only(&db, dir.path()).await.unwrap_err());
        assert!(err.contains(&format!("format {version}")), "{err}");
    }
    tamper(&db, |r| r.version = Set(secrets::VERSION)).await;
    assert_eq!(open_only(&db, dir.path()).await.unwrap(), CANARY);
}

#[tokio::test]
async fn row_moved_to_another_name_scope_or_origin_fails() {
    let (dir, db) = setup().await;
    store(&db, dir.path(), "caldav", CANARY).await;
    type Move = fn(&mut rows::ActiveModel);
    let moves: [(&str, Move); 3] = [
        ("name", |r| r.name = Set("github".into())),
        ("origin", |r| r.origin = Set("https://evil.example".into())),
        ("scope", |r| r.scope = Set("agent".into())),
    ];
    for (what, edit) in moves {
        let row = rows::Entity::find().one(&db).await.unwrap().unwrap();
        let original = row.clone();
        rows::Entity::delete_by_id((row.scope.clone(), row.name.clone()))
            .exec(&db)
            .await
            .unwrap();
        let mut moved = row.into_active_model().reset_all();
        edit(&mut moved);
        let moved = moved.insert(&db).await.unwrap();

        let key = master_key::load(&db, dir.path()).await.unwrap().unwrap();
        let sealed = secrets::load_sealed(&db, &moved.scope).await.unwrap();
        let err = sealed[0].open(&key).unwrap_err().to_string();
        assert!(err.contains("cannot be opened"), "{what}: {err}");

        rows::Entity::delete_many().exec(&db).await.unwrap();
        original
            .into_active_model()
            .reset_all()
            .insert(&db)
            .await
            .unwrap();
    }
    assert_eq!(open_only(&db, dir.path()).await.unwrap(), CANARY);
}

#[tokio::test]
async fn missing_key_file_while_secrets_exist_is_an_error_and_makes_no_key() {
    let (dir, db) = setup().await;
    store(&db, dir.path(), "caldav", CANARY).await;
    let file = master_key::path(dir.path());
    std::fs::remove_file(&file).unwrap();

    let err = format!("{:#}", master_key::load(&db, dir.path()).await.unwrap_err());
    assert!(
        err.contains("is missing") && err.contains("restore"),
        "{err}"
    );
    let err = format!(
        "{:#}",
        master_key::load_or_create(&db, dir.path())
            .await
            .unwrap_err()
    );
    assert!(err.contains("does not make a new key"), "{err}");
    assert!(
        !file.exists(),
        "a new key was made while secrets are stored"
    );
}

/// Once the last secret is removed, its key check goes too: a lost key no longer blocks a new
/// one, since nothing sealed with it remains.
#[tokio::test]
async fn removing_the_last_secret_frees_the_key() {
    let (dir, db) = setup().await;
    store(&db, dir.path(), "caldav", CANARY).await;
    store(&db, dir.path(), "other", CANARY).await;
    assert!(secrets::remove(&db, SOURCES, "caldav").await.unwrap());
    assert!(!secrets::remove(&db, SOURCES, "caldav").await.unwrap());
    assert!(secrets::any_stored(&db).await.unwrap());
    std::fs::remove_file(master_key::path(dir.path())).unwrap();
    assert!(master_key::load(&db, dir.path()).await.is_err());

    assert!(secrets::remove(&db, SOURCES, "other").await.unwrap());
    assert!(!secrets::any_stored(&db).await.unwrap());
    let (_, created) = master_key::load_or_create(&db, dir.path()).await.unwrap();
    assert!(created);
}

/// The value never reaches the database's files in clear: not the database, not its write-ahead
/// log or its index, while open or after closing. Nor does the key.
#[tokio::test]
async fn database_files_hold_no_plaintext() {
    let (dir, db) = setup().await;
    store(&db, dir.path(), "caldav", CANARY).await;
    let key_line = std::fs::read_to_string(master_key::path(dir.path())).unwrap();
    let encoded_key = key_line.trim_end().strip_prefix("fronde-key-v1:").unwrap();
    let scan = |when: &str| {
        let mut scanned = 0;
        for name in ["fronde.db", "fronde.db-wal", "fronde.db-shm"] {
            let Ok(bytes) = std::fs::read(dir.path().join(name)) else {
                continue;
            };
            scanned += 1;
            for needle in [CANARY.as_bytes(), encoded_key.as_bytes()] {
                assert!(
                    !bytes.windows(needle.len()).any(|w| w == needle),
                    "{when}: {name} holds {}",
                    String::from_utf8_lossy(needle)
                );
            }
        }
        scanned
    };
    // While open, the new row is in the write-ahead log.
    assert_eq!(scan("open"), 3);
    db.close().await.unwrap();
    assert!(scan("closed") >= 1);
}

/// `Debug` of everything that holds a key or a value prints neither.
#[tokio::test]
async fn debug_never_shows_a_key_or_a_value() {
    let (dir, db) = setup().await;
    store(&db, dir.path(), "caldav", CANARY).await;
    let key_line = std::fs::read_to_string(master_key::path(dir.path())).unwrap();
    let encoded_key = key_line.trim_end().strip_prefix("fronde-key-v1:").unwrap();
    let key = master_key::load(&db, dir.path()).await.unwrap().unwrap();
    let sealed = secrets::load_sealed(&db, SOURCES).await.unwrap();
    let value = sealed[0].open(&key).unwrap();
    let listed = secrets::list(&db, SOURCES).await.unwrap();

    let printed = [
        format!("{key:?}"),
        format!("{key:#?}"),
        format!("{sealed:?}"),
        format!("{sealed:#?}"),
        format!("{value:?}"),
        format!("{listed:?}"),
    ];
    for text in &printed {
        assert!(!text.contains(CANARY), "{text}");
        assert!(!text.contains(encoded_key), "{text}");
    }
    assert!(printed[0].contains("\"file\""), "{}", printed[0]);
}

#[tokio::test]
async fn a_key_file_with_more_than_one_line_is_refused() {
    let (dir, db) = setup().await;
    let file = master_key::path(dir.path());
    fronde_fs::write(&file, format!("{}more", other_key_line())).unwrap();
    let err = format!("{:#}", master_key::load(&db, dir.path()).await.unwrap_err());
    assert!(
        err.contains("is unreadable") && err.contains("not a Fronde master key"),
        "{err}"
    );
    assert!(!err.contains("AQEB"), "{err}");
}
