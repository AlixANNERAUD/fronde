//! The key-value table: small values grouped by scope (the agent's session id, later others),
//! each optionally expiring. Expired rows are ignored on read and purged in the background.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Kv {
    Table,
    Scope,
    Key,
    Value,
    ExpiresAt,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Kv::Table)
                    .col(text(Kv::Scope))
                    .col(text(Kv::Key))
                    .col(blob(Kv::Value))
                    .col(timestamp_null(Kv::ExpiresAt))
                    .primary_key(Index::create().col(Kv::Scope).col(Kv::Key))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Kv::Table).to_owned())
            .await
    }
}
