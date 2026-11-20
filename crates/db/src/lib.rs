//! Fronde's database: one SQLite file, `<data>/fronde.db`, accessed through SeaORM. [`open`]
//! creates it private, sets it up and applies the migrations of `fronde-migration`; the tables
//! are the entities of `fronde-entity`.

/// Whether this build has the `testing` feature, whose test hooks a served Fronde must not
/// have: `fronde serve` refuses to start with it.
pub const TESTING: bool = cfg!(feature = "testing");

pub mod approvals;
pub mod ephemeral;
pub mod kv;
pub mod master_key;
pub mod running;
pub mod secrets;

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
            // The path itself, not a URL, so that no character in it is read as part of one
            // (sqlx refuses a path that is not UTF-8). Never created here: SQLite would follow
            // the umask, the file made above does not.
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

/// Open `<data_dir>/fronde.db`, which must exist, to read only: no migration, no write. The
/// schema is the one the last Fronde to open it left: a table added since may be missing. A
/// read-only connection to a write-ahead-logged database creates SQLite's `-wal` and `-shm`
/// files when they are missing, and cannot remove them when it closes: to leave the data
/// directory as it was when Fronde is not running, use [`read_only`].
pub async fn open_read_only(data_dir: &Path) -> anyhow::Result<DatabaseConnection> {
    connect_read_only(data_dir, false).await
}

/// Runs `read` on `<data_dir>/fronde.db`, which must exist, opened to read only as
/// [`open_read_only`] does, and closes it. For `fronde check`, which must decide what serving
/// would without touching the database: when Fronde is not running, it leaves the data
/// directory as it was, creating no `-wal` or `-shm`.
///
/// When the database [`is_at_rest`], it is first read as immutable: SQLite then reads the main
/// file alone, takes no lock and creates nothing. Immutable, it must not read a file another
/// process writes meanwhile, which `fronde.lock` cannot rule out: `fronde secret` writes
/// without it. One that opens the database while `read` runs writes to a new `-wal`, and
/// writes the main file only at a checkpoint, at its close at the latest. So once `read` is
/// done, the database must still be at rest, its file the same as before (size, times, inode):
/// otherwise what was read is dropped, and `read` runs again on a plain read-only connection,
/// which reads beside a writer as any reader does, through the `-wal` and `-shm`. `read` may
/// therefore run twice: it only reads, and should not wait on anything but the database.
pub async fn read_only<T>(
    data_dir: &Path,
    read: impl AsyncFn(&DatabaseConnection) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    if let Some(before) = Rest::now(data_dir) {
        let db = connect_read_only(data_dir, true).await?;
        let result = read(&db).await;
        let closed = db.close().await;
        if closed.is_ok() && Rest::now(data_dir).as_ref() == Some(&before) {
            return result;
        }
        tracing::debug!(
            "the database {} changed while it was read as immutable: read again",
            path(data_dir).display()
        );
    }
    let db = connect_read_only(data_dir, false).await?;
    let result = read(&db).await;
    // Read only: a failed close loses nothing.
    if let Err(error) = db.close().await {
        tracing::debug!(
            "cannot close the database {} read only: {error:#}",
            path(data_dir).display()
        );
    }
    result
}

/// The connection of [`open_read_only`], immutable or not.
async fn connect_read_only(data_dir: &Path, immutable: bool) -> anyhow::Result<DatabaseConnection> {
    let file = path(data_dir);
    let mut options = ConnectOptions::new(url(&file));
    let filename = file.clone();
    options
        .max_connections(1)
        .min_connections(0)
        .connect_timeout(BUSY_TIMEOUT)
        .sqlx_logging(false)
        .map_sqlx_sqlite_opts(move |options| {
            options
                .filename(&filename)
                .read_only(true)
                .immutable(immutable)
                .create_if_missing(false)
                .busy_timeout(BUSY_TIMEOUT)
        });
    Database::connect(options).await.with_context(|| {
        format!(
            "cannot read the database {}; check that it is a SQLite file the user running Fronde \
             can read",
            file.display()
        )
    })
}

/// The database file of a database at rest, as [`read_only`] compares it before and after an
/// immutable read: a checkpoint, or a new file under the name, changes one of these.
#[derive(Debug, PartialEq, Eq)]
struct Rest {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    changed: (i64, i64),
    #[cfg(unix)]
    inode: (u64, u64),
}

impl Rest {
    /// The database file now, if the database [`is_at_rest`].
    fn now(data_dir: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path(data_dir)).ok()?;
        if !is_at_rest(data_dir) {
            return None;
        }
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        Some(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            #[cfg(unix)]
            inode: (metadata.dev(), metadata.ino()),
        })
    }
}

/// Whether nothing has `<data_dir>/fronde.db` open now: it is in write-ahead-logging mode, as
/// [`open`] leaves it, neither its `-wal` nor its `-shm` exists (every connection to such a
/// database keeps both while it is open, and the last to close removes them; a commit a crash
/// left in a `-wal` keeps it from being at rest, so it is never missed), and no `fronde serve`
/// holds the data directory ([`running::is_free`]). `false` when any of it cannot be told. A
/// database in another journal mode is written in place: it is never at rest, and its readers
/// create no file anyway. Something may open it the moment after: see [`read_only`].
pub fn is_at_rest(data_dir: &Path) -> bool {
    let file = path(data_dir);
    let mut header = [0u8; 20];
    let wal = std::fs::File::open(&file)
        .and_then(|mut opened| std::io::Read::read_exact(&mut opened, &mut header))
        .is_ok_and(|()| header[18] == 2 && header[19] == 2);
    let quiet = ["-wal", "-shm"].iter().all(|suffix| {
        let mut sibling = file.clone().into_os_string();
        sibling.push(suffix);
        // Present, or unknown: in use, as far as this can tell.
        matches!(
            std::fs::symlink_metadata(PathBuf::from(sibling)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        )
    });
    wal && quiet && running::is_free(data_dir)
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
