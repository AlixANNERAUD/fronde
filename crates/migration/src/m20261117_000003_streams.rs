//! Streams: numbered entries under a topic, each stream named by a random id that changes only
//! when the stream is created anew. The chat's conversation is the topic `conversation`.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Streams {
    Table,
    Topic,
    LogId,
    CreatedAt,
}

#[derive(DeriveIden)]
enum StreamEntries {
    Table,
    Topic,
    Seq,
    At,
    Event,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    /// Two tables: SQLite migrations run outside a transaction by default, so a failure between
    /// them would leave the first table without the migration's record, and every later start
    /// would fail on "table already exists".
    fn use_transaction(&self) -> Option<bool> {
        Some(true)
    }

    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Streams::Table)
                    .col(text(Streams::Topic).primary_key())
                    .col(text(Streams::LogId))
                    .col(timestamp(Streams::CreatedAt))
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                Table::create()
                    .table(StreamEntries::Table)
                    .col(text(StreamEntries::Topic))
                    .col(big_integer(StreamEntries::Seq))
                    .col(timestamp(StreamEntries::At))
                    .col(json(StreamEntries::Event))
                    .primary_key(
                        Index::create()
                            .col(StreamEntries::Topic)
                            .col(StreamEntries::Seq),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(StreamEntries::Table, StreamEntries::Topic)
                            .to(Streams::Table, Streams::Topic)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(StreamEntries::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Streams::Table).to_owned())
            .await
    }
}
