//! Fronde's database: one SQLite file, `<data>/fronde.db`, accessed through SeaORM. [`open`]
//! creates it private, sets it up and applies the migrations of `fronde-migration`; the tables
//! are the entities of `fronde-entity`.

pub mod kv;

use anyhow::Context as _;
use fronde_migration::{Migrator, MigratorTrait};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use sea_orm::{
    ConnectOptions, Database, DbErr, SqliteTransactionMode, TransactionOptions, TransactionTrait,
};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use sea_orm::DatabaseConnection;

/// The database's file name in the data directory.
pub const FILE_NAME: &str = "fronde.db";

/// How long a connection waits for the write lock, held by another connection, before failing
/// with `SQLITE_BUSY` ("database is locked"). Writes are short, so a wait this long only happens
/// if something is stuck.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// The database file in `data_dir`.
pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE_NAME)
}

/// Open `<data_dir>/fronde.db`, creating it if missing, and apply pending migrations. The
/// directory must exist (`fronde_hub::private::prepare_data_dir` makes it).
///
/// The file is created 0600 before SQLite opens it, and SQLite gives its `-wal` and `-shm`
/// files the main file's mode. Write-ahead logging lets readers run beside the writer;
/// `synchronous = FULL` syncs each commit, so a committed row survives a power cut. Every
/// error names the file and says how to fix it.
pub async fn open(data_dir: &Path) -> anyhow::Result<DatabaseConnection> {
    let file = path(data_dir);
    let shown = file.display();
    fronde_fs::write_if_missing(&file).with_context(|| {
        format!(
            "cannot create the database {shown}; make it and [data] dir writable by the user \
             running Fronde"
        )
    })?;
    for suffix in ["-wal", "-shm"] {
        let mut sibling = file.clone().into_os_string();
        sibling.push(suffix);
        let sibling = PathBuf::from(sibling);
        if sibling.exists() {
            fronde_fs::restrict_file(&sibling).with_context(|| {
                format!(
                    "cannot make {} private; it must belong to the user running Fronde (stop \
                     Fronde, then fix its owner or remove it together with the database)",
                    sibling.display()
                )
            })?;
        }
    }

    let mut options = ConnectOptions::new(url(&file));
    let filename = file.clone();
    options
        .max_connections(8)
        .min_connections(1)
        .connect_timeout(BUSY_TIMEOUT)
        .sqlx_logging(false)
        .map_sqlx_sqlite_opts(move |options| {
            // The path itself, exact even when it is not UTF-8. Never created here: SQLite would
            // follow the umask, the file made above does not.
            options
                .filename(&filename)
                .create_if_missing(false)
                .foreign_keys(true)
                .busy_timeout(BUSY_TIMEOUT)
                .pragma("journal_mode", "WAL")
                .pragma("synchronous", "FULL")
        });
    let db = Database::connect(options).await.with_context(|| {
        format!(
            "cannot open the database {shown}; check that it is a SQLite file the user running \
             Fronde can write, and that no other Fronde is using it"
        )
    })?;
    migrate(&db).await.with_context(|| {
        format!(
            "cannot update the database {shown} to this version of Fronde; if a newer Fronde \
             used it, run that version again or restore a backup of the data directory"
        )
    })?;
    Ok(db)
}

/// Applies the pending migrations in one transaction. sea-orm-migration runs a SQLite migration
/// outside any transaction unless it asks for one, so a failure between a migration's change and
/// its record would leave a schema that every later start fails to migrate again.
async fn migrate(db: &DatabaseConnection) -> Result<(), DbErr> {
    let transaction = begin_write(db).await?;
    Migrator::up(&transaction, None).await?;
    transaction.commit().await
}

/// `file` as a `sqlite://` URL. sqlx percent-decodes the path, so a `?`, `#` or `%` in a
/// directory name stays part of it instead of starting the URL's parameters.
fn url(file: &Path) -> String {
    let path = file.to_string_lossy();
    format!("sqlite://{}", utf8_percent_encode(&path, NON_ALPHANUMERIC))
}

/// Start a transaction that will write, with `BEGIN IMMEDIATE`: it takes the write lock up
/// front, waiting up to [`BUSY_TIMEOUT`] for it. A plain (deferred) `BEGIN` would only take it
/// at the first write, and under write-ahead logging, if another connection committed since the
/// transaction's first read, that upgrade fails at once with `SQLITE_BUSY_SNAPSHOT`, which the
/// busy timeout does not cover. Inside a transaction, this starts a savepoint instead.
pub async fn begin_write<C>(db: &C) -> Result<C::Transaction, DbErr>
where
    C: TransactionTrait + ?Sized,
{
    db.begin_with_options(TransactionOptions {
        sqlite_transaction_mode: Some(SqliteTransactionMode::Immediate),
        ..TransactionOptions::default()
    })
    .await
}
