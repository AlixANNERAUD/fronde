//! Component approvals the owner gave, each for one source and the SHA-256 of both of its files
//! (the `.wasm` and its manifest). `fronde_wasm::prepare` decides what they approve; nothing
//! here does.

use chrono::Utc;
use fronde_entity::approvals;
use fronde_migration::SchemaManager;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, EntityTrait, QueryOrder, Set};

pub use approvals::Model as Approval;

/// Every approval, oldest first.
pub async fn all<C: ConnectionTrait>(db: &C) -> Result<Vec<Approval>, DbErr> {
    approvals::Entity::find()
        .order_by_asc(approvals::Column::ApprovedAt)
        .all(db)
        .await
}

/// Whether the database has the approvals table. A database last opened by an older Fronde does
/// not, which only [`crate::open_read_only`] sees: [`crate::open`] adds it.
pub async fn table_exists(db: &DatabaseConnection) -> Result<bool, DbErr> {
    SchemaManager::new(db).has_table("approvals").await
}

/// Approve the files of `source` whose SHA-256 are `sha256` (the `.wasm`) and `manifest_sha256`,
/// each 64 lowercase hexadecimal characters as `fronde check` prints them: a hash in any other
/// form would never match the files, and an approval that matches nothing is refused here.
/// Approving them again keeps the first approval's time.
pub async fn add<C: ConnectionTrait>(
    db: &C,
    source: &str,
    sha256: &str,
    manifest_sha256: &str,
) -> Result<(), DbErr> {
    for (field, hash) in [("sha256", sha256), ("manifest_sha256", manifest_sha256)] {
        if !is_sha256(hash) {
            return Err(DbErr::Custom(format!(
                "{field} {hash:?} is not a SHA-256: give the 64 lowercase hexadecimal characters \
                 `fronde check` prints"
            )));
        }
    }
    let row = approvals::ActiveModel {
        source: Set(source.to_owned()),
        sha256: Set(sha256.to_owned()),
        manifest_sha256: Set(manifest_sha256.to_owned()),
        approved_at: Set(Utc::now()),
    };
    approvals::Entity::insert(row)
        .on_conflict_do_nothing()
        .exec(db)
        .await?;
    Ok(())
}

fn is_sha256(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
