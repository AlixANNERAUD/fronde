use fronde_db::{BUSY_TIMEOUT, approvals, kv};
use fronde_entity::kv as kv_rows;
use fronde_migration::{Migrator, MigratorTrait};
use sea_orm::{ConnectionTrait, EntityTrait, PaginatorTrait, Statement};
use std::time::Duration;

async fn pragma(db: &fronde_db::DatabaseConnection, name: &str) -> String {
    let row = db
        .query_one_raw(Statement::from_string(
            db.get_database_backend(),
            format!("PRAGMA {name}"),
        ))
        .await
        .unwrap()
        .unwrap();
    row.try_get_by_index::<i64>(0)
        .map(|n| n.to_string())
        .or_else(|_| row.try_get_by_index::<String>(0))
        .unwrap()
}

#[tokio::test]
async fn migrations_apply_on_a_fresh_file() {
    let dir = tempfile::tempdir().unwrap();
    let db = fronde_db::open(dir.path()).await.unwrap();

    assert!(dir.path().join("fronde.db").is_file());
    assert!(
        Migrator::get_pending_migrations(&db)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(kv_rows::Entity::find().count(&db).await.unwrap(), 0);
    assert_eq!(pragma(&db, "journal_mode").await, "wal");
    // FULL: a commit is synced before it returns, as the JSONL log synced each line.
    assert_eq!(pragma(&db, "synchronous").await, "2");
    assert_eq!(pragma(&db, "foreign_keys").await, "1");
    assert_eq!(
        pragma(&db, "busy_timeout").await,
        BUSY_TIMEOUT.as_millis().to_string()
    );
}

#[tokio::test]
async fn reopen_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let db = fronde_db::open(dir.path()).await.unwrap();
    kv::set(&db, "agent", "session", b"s-1".to_vec(), None)
        .await
        .unwrap();
    db.close().await.unwrap();

    let db = fronde_db::open(dir.path()).await.unwrap();
    assert!(
        Migrator::get_pending_migrations(&db)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        Migrator::get_applied_migrations(&db).await.unwrap().len(),
        Migrator::migrations().len()
    );
    assert_eq!(
        kv::get(&db, "agent", "session").await.unwrap(),
        Some(b"s-1".to_vec())
    );
}

#[cfg(unix)]
#[tokio::test]
async fn unwritable_path_explains() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();

    let locked = t.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    if std::fs::File::create(locked.join("x")).is_ok() {
        return; // running as root: permissions do not apply
    }
    let e = format!("{:#}", fronde_db::open(&locked).await.unwrap_err());
    let file = locked.join("fronde.db").display().to_string();
    assert!(e.contains(&file) && e.contains("writable"), "{e}");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();

    // A database file the user cannot write.
    let read_only = t.path().join("read-only");
    std::fs::create_dir(&read_only).unwrap();
    let file = read_only.join("fronde.db");
    std::fs::write(&file, b"").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o400)).unwrap();
    let e = format!("{:#}", fronde_db::open(&read_only).await.unwrap_err());
    assert!(
        e.contains(&file.display().to_string()) && e.contains("writable"),
        "{e}"
    );

    // Something else than a database under the name.
    let garbage = t.path().join("garbage");
    std::fs::create_dir(&garbage).unwrap();
    let file = garbage.join("fronde.db");
    std::fs::write(&file, vec![b'x'; 4096]).unwrap();
    let e = format!("{:#}", fronde_db::open(&garbage).await.unwrap_err());
    assert!(
        e.contains(&file.display().to_string()) && e.contains("SQLite file"),
        "{e}"
    );
}

/// nextest runs each test in its own process, so changing the umask touches no other test.
#[cfg(unix)]
#[tokio::test]
async fn database_files_are_private() {
    use rustix::fs::Mode;
    use std::os::unix::fs::PermissionsExt;
    rustix::process::umask(Mode::from_raw_mode(0o022));
    let dir = tempfile::tempdir().unwrap();
    let db = fronde_db::open(dir.path()).await.unwrap();
    kv::set(&db, "test", "k", b"v".to_vec(), None)
        .await
        .unwrap();

    for name in ["fronde.db", "fronde.db-wal", "fronde.db-shm"] {
        let mode = std::fs::metadata(dir.path().join(name))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{name}");
    }
}

#[tokio::test]
async fn kv_roundtrip_with_ttl() {
    let dir = tempfile::tempdir().unwrap();
    let db = fronde_db::open(dir.path()).await.unwrap();

    assert_eq!(kv::get(&db, "s", "k").await.unwrap(), None);
    kv::set(&db, "s", "k", b"forever".to_vec(), None)
        .await
        .unwrap();
    assert_eq!(
        kv::get(&db, "s", "k").await.unwrap(),
        Some(b"forever".to_vec())
    );
    // Same key, other scope: a separate value.
    assert_eq!(kv::get(&db, "other", "k").await.unwrap(), None);

    kv::set(
        &db,
        "s",
        "k",
        b"an hour".to_vec(),
        Some(Duration::from_secs(3600)),
    )
    .await
    .unwrap();
    assert_eq!(
        kv::get(&db, "s", "k").await.unwrap(),
        Some(b"an hour".to_vec())
    );

    kv::set(&db, "s", "k", b"gone".to_vec(), Some(Duration::ZERO))
        .await
        .unwrap();
    assert_eq!(kv::get(&db, "s", "k").await.unwrap(), None, "expired");

    assert!(kv::delete(&db, "s", "k").await.unwrap());
    assert!(!kv::delete(&db, "s", "k").await.unwrap());
    assert_eq!(kv_rows::Entity::find().count(&db).await.unwrap(), 0);

    let e = kv::set(&db, "s", "k", vec![], Some(Duration::MAX))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("too long"), "{e}");
}

#[tokio::test]
async fn expired_kv_rows_are_purged() {
    let dir = tempfile::tempdir().unwrap();
    let db = fronde_db::open(dir.path()).await.unwrap();
    kv::set(&db, "s", "expired", vec![1], Some(Duration::ZERO))
        .await
        .unwrap();
    kv::set(&db, "s", "forever", vec![2], None).await.unwrap();
    kv::set(&db, "s", "later", vec![3], Some(Duration::from_secs(3600)))
        .await
        .unwrap();

    assert_eq!(kv::purge_expired(&db).await.unwrap(), 1);
    assert_eq!(kv_rows::Entity::find().count(&db).await.unwrap(), 2);
    assert_eq!(kv::get(&db, "s", "forever").await.unwrap(), Some(vec![2]));
    assert_eq!(kv::get(&db, "s", "later").await.unwrap(), Some(vec![3]));
    assert_eq!(kv::purge_expired(&db).await.unwrap(), 0);
}

#[tokio::test]
async fn begin_write_commits_and_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let db = fronde_db::open(dir.path()).await.unwrap();

    let tx = fronde_db::begin_write(&db).await.unwrap();
    kv::set(&tx, "s", "kept", vec![1], None).await.unwrap();
    tx.commit().await.unwrap();

    let tx = fronde_db::begin_write(&db).await.unwrap();
    kv::set(&tx, "s", "dropped", vec![2], None).await.unwrap();
    tx.rollback().await.unwrap();

    assert_eq!(kv::get(&db, "s", "kept").await.unwrap(), Some(vec![1]));
    assert_eq!(kv::get(&db, "s", "dropped").await.unwrap(), None);
}

#[tokio::test]
async fn newer_schema_explains() {
    let dir = tempfile::tempdir().unwrap();
    let db = fronde_db::open(dir.path()).await.unwrap();
    db.execute_unprepared(
        "INSERT INTO seaql_migrations (version, applied_at) VALUES ('m29991231_000001_future', 0)",
    )
    .await
    .unwrap();
    db.close().await.unwrap();

    let e = format!("{:#}", fronde_db::open(dir.path()).await.unwrap_err());
    let file = dir.path().join("fronde.db").display().to_string();
    assert!(
        e.contains(&file) && e.contains("newer Fronde") && e.contains("restore a backup"),
        "{e}"
    );
}

#[tokio::test]
async fn opens_a_path_with_url_characters() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("a?mode=ro%20#b");
    std::fs::create_dir(&dir).unwrap();
    let db = fronde_db::open(&dir).await.unwrap();
    kv::set(&db, "s", "k", vec![1], None).await.unwrap();
    assert!(dir.join("fronde.db").is_file());
    assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn failed_migration_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("fronde.db");
    let url = format!("sqlite://{}?mode=rwc", file.display());
    // A table in the way of the last migration's second table.
    let raw = sea_orm::Database::connect(&url).await.unwrap();
    raw.execute_unprepared("CREATE TABLE stream_entries (x)")
        .await
        .unwrap();
    raw.close().await.unwrap();

    assert!(fronde_db::open(dir.path()).await.is_err());
    let raw = sea_orm::Database::connect(&url).await.unwrap();
    let tables = |raw: sea_orm::DatabaseConnection| async move {
        let rows = raw
            .query_all_raw(Statement::from_string(
                raw.get_database_backend(),
                "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name",
            ))
            .await
            .unwrap();
        raw.close().await.unwrap();
        rows.iter()
            .map(|row| row.try_get_by_index::<String>(0).unwrap())
            .collect::<Vec<_>>()
    };
    // Every migration rolled back, the earlier ones too.
    assert_eq!(tables(raw).await, ["stream_entries"]);

    let raw = sea_orm::Database::connect(&url).await.unwrap();
    raw.execute_unprepared("DROP TABLE stream_entries")
        .await
        .unwrap();
    raw.close().await.unwrap();
    let db = fronde_db::open(dir.path()).await.unwrap();
    assert!(
        Migrator::get_pending_migrations(&db)
            .await
            .unwrap()
            .is_empty()
    );
    kv::set(&db, "s", "k", vec![1], None).await.unwrap();
}

/// A 64-character hexadecimal hash made of `c`, as `fronde check` prints them.
fn hash(c: char) -> String {
    c.to_string().repeat(64)
}

#[tokio::test]
async fn approvals_are_kept_once_with_their_first_time() {
    let dir = tempfile::tempdir().unwrap();
    let db = fronde_db::open(dir.path()).await.unwrap();
    let (aa, bb, cc) = (hash('a'), hash('b'), hash('c'));
    assert!(approvals::all(&db).await.unwrap().is_empty());
    approvals::add(&db, "calendar", &aa, &bb).await.unwrap();
    let first = approvals::all(&db).await.unwrap();
    approvals::add(&db, "calendar", &aa, &bb).await.unwrap();
    // Another build of the same source is another approval.
    approvals::add(&db, "calendar", &cc, &bb).await.unwrap();
    let all = approvals::all(&db).await.unwrap();
    assert_eq!(all.len(), 2, "{all:?}");
    assert_eq!(all[0], first[0]);
    assert_eq!(
        (
            all[1].source.as_str(),
            all[1].sha256.as_str(),
            all[1].manifest_sha256.as_str()
        ),
        ("calendar", cc.as_str(), bb.as_str())
    );
}

#[tokio::test]
async fn approvals_refuse_what_is_not_a_sha256() {
    let dir = tempfile::tempdir().unwrap();
    let db = fronde_db::open(dir.path()).await.unwrap();
    let good = hash('0');
    let upper = "A".repeat(64);
    let short = "a".repeat(63);
    let long = "a".repeat(65);
    let not_hex = format!("{}g", "a".repeat(63));
    let spaced = format!(" {}", "a".repeat(63));
    for bad in ["", upper.as_str(), &short, &long, &not_hex, &spaced] {
        let err = approvals::add(&db, "calendar", bad, &good)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("sha256") && err.contains("not a SHA-256"),
            "{err}"
        );
        let err = approvals::add(&db, "calendar", &good, bad)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("manifest_sha256"), "{err}");
    }
    assert!(approvals::all(&db).await.unwrap().is_empty());
    let digits = "0123456789abcdef".repeat(4);
    approvals::add(&db, "calendar", &digits, &good)
        .await
        .unwrap();
    assert_eq!(approvals::all(&db).await.unwrap().len(), 1);
}
