//! Threads: the conversations, each over a stream of its own. Thread 1 is the chat that came
//! before threads, over the topic `conversation`; Fronde makes its row at start, and moves the
//! agent's session id there from `kv`. A thread is archived, never deleted, so its id is never
//! given again.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Threads {
    Table,
    Id,
    Topic,
    Title,
    ParentId,
    Depth,
    Owner,
    StartedBy,
    Provider,
    Model,
    SessionId,
    ClientRequestId,
    CreatedAt,
    UpdatedAt,
    ArchivedAt,
}

#[derive(DeriveIden)]
enum Streams {
    Table,
    Topic,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    /// A table and its indexes: one transaction, so a failure leaves no half-made table.
    fn use_transaction(&self) -> Option<bool> {
        Some(true)
    }

    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Threads::Table)
                    .col(pk_auto(Threads::Id))
                    .col(text(Threads::Topic).unique_key())
                    .col(text(Threads::Title))
                    .col(integer_null(Threads::ParentId))
                    .col(integer(Threads::Depth).default(0))
                    .col(text(Threads::Owner))
                    .col(text(Threads::StartedBy))
                    .col(text(Threads::Provider))
                    .col(text_null(Threads::Model))
                    .col(text_null(Threads::SessionId))
                    .col(text_null(Threads::ClientRequestId))
                    .col(timestamp(Threads::CreatedAt))
                    .col(timestamp(Threads::UpdatedAt))
                    .col(timestamp_null(Threads::ArchivedAt))
                    .foreign_key(
                        ForeignKey::create()
                            .from(Threads::Table, Threads::Topic)
                            .to(Streams::Table, Streams::Topic),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(Threads::Table, Threads::ParentId)
                            .to(Threads::Table, Threads::Id),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("threads_parent_id")
                    .table(Threads::Table)
                    .col(Threads::ParentId)
                    .to_owned(),
            )
            .await?;
        // A retried start names its thread again while that thread is live; once archived, the
        // id may start a new one.
        manager
            .create_index(
                Index::create()
                    .name("threads_live_started_by_client_request_id")
                    .table(Threads::Table)
                    .col(Threads::StartedBy)
                    .col(Threads::ClientRequestId)
                    .unique()
                    .and_where(Expr::col(Threads::ClientRequestId).is_not_null())
                    .and_where(Expr::col(Threads::ArchivedAt).is_null())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Threads::Table).to_owned())
            .await
    }
}
