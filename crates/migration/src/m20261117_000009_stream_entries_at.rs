//! An index on `stream_entries.at`: the daily count of sub-agents' turns reads the entries of
//! the last 24 hours, under the lock every sub-agent's start and message holds, and must not
//! scan every stream to find them.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum StreamEntries {
    Table,
    At,
}

const NAME: &str = "stream_entries_at";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_index(
                Index::create()
                    .name(NAME)
                    .table(StreamEntries::Table)
                    .col(StreamEntries::At)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(
                Index::drop()
                    .name(NAME)
                    .table(StreamEntries::Table)
                    .to_owned(),
            )
            .await
    }
}
