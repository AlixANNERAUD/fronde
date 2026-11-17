//! Database migrations of Fronde, written with SeaORM's schema builder (raw SQL only for FTS5).
//! They are embedded in the binary and applied when `fronde_db::open` opens the database.
//!
//! To change the schema, add a migration rather than editing one that was pushed: databases
//! that already applied it would never see the change. Name it `mYYYYMMDD_NNNNNN_purpose`,
//! append it to [`Migrator::migrations`], then run `mise run entities` to regenerate the
//! `fronde-entity` crate from the migrated schema (entities are never edited by hand).

pub use sea_orm_migration::prelude::*;

mod m20261117_000001_kv;
mod m20261117_000002_memory_fts;
mod m20261117_000003_streams;
mod m20261117_000004_approvals;
mod m20261117_000005_secrets;

/// Every migration of Fronde, in the order they apply.
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20261117_000001_kv::Migration),
            Box::new(m20261117_000002_memory_fts::Migration),
            Box::new(m20261117_000003_streams::Migration),
            Box::new(m20261117_000004_approvals::Migration),
            Box::new(m20261117_000005_secrets::Migration),
        ]
    }
}
