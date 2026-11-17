use fronde_migration::{Migrator, MigratorTrait};
use sea_orm_migration::sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseConnection};

/// An in-memory database: one connection, or each would see a database of its own.
async fn memory() -> DatabaseConnection {
    let mut options = ConnectOptions::new("sqlite::memory:");
    options
        .max_connections(1)
        .min_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .sqlx_logging(false);
    Database::connect(options).await.unwrap()
}

async fn has_table(db: &DatabaseConnection, name: &str) -> bool {
    db.query_one_raw(sea_orm_migration::sea_orm::Statement::from_string(
        db.get_database_backend(),
        format!("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '{name}'"),
    ))
    .await
    .unwrap()
    .is_some()
}

#[tokio::test]
async fn streams_migration_is_all_or_nothing() {
    let db = memory().await;
    Migrator::up(&db, Some(2)).await.unwrap();
    // The second table cannot be created: the migration fails after its first table.
    db.execute_unprepared("CREATE TABLE stream_entries (x)")
        .await
        .unwrap();

    let error = Migrator::up(&db, None).await.unwrap_err().to_string();
    assert!(error.contains("stream_entries"), "{error}");
    assert!(!has_table(&db, "streams").await, "the first table was kept");
    assert_eq!(
        Migrator::get_pending_migrations(&db).await.unwrap().len(),
        1
    );

    // Once the obstacle is gone, the same migration applies.
    db.execute_unprepared("DROP TABLE stream_entries")
        .await
        .unwrap();
    Migrator::up(&db, None).await.unwrap();
    assert!(has_table(&db, "streams").await);
    assert!(has_table(&db, "stream_entries").await);
    assert!(
        Migrator::get_pending_migrations(&db)
            .await
            .unwrap()
            .is_empty()
    );
}
