//! Component approvals the owner gave, each for one source and the SHA-256 of both of its files
//! (the `.wasm` and its manifest). `fronde_wasm::prepare` decides what they approve; nothing
//! here does.

use chrono::Utc;
use fronde_entity::approvals;
use sea_orm::{ConnectionTrait, DbErr, EntityTrait, QueryOrder, Set};

pub use approvals::Model as Approval;

/// Every approval, oldest first.
pub async fn all<C: ConnectionTrait>(db: &C) -> Result<Vec<Approval>, DbErr> {
    approvals::Entity::find()
        .order_by_asc(approvals::Column::ApprovedAt)
        .all(db)
        .await
}

/// Approve the files of `source` whose SHA-256 are `sha256` (the `.wasm`) and `manifest_sha256`.
/// Approving them again keeps the first approval's time.
pub async fn add<C: ConnectionTrait>(
    db: &C,
    source: &str,
    sha256: &str,
    manifest_sha256: &str,
) -> Result<(), DbErr> {
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
