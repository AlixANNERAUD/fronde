//! Requests to a human: a question, later a secret or a permission, shown as a card in a stream
//! and answered once. `status` moves from `pending` to `answered`, `declined`, `expired` or
//! `cancelled`, and never back. `response` holds what was answered, never a secret.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Requests {
    Table,
    Id,
    Topic,
    Turn,
    Caller,
    ClientRequestId,
    Kind,
    Card,
    ForUser,
    AdminOnly,
    Status,
    Response,
    AnsweredBy,
    CreatedAt,
    ExpiresAt,
    ResolvedAt,
    StreamSeq,
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
                    .table(Requests::Table)
                    .col(pk_auto(Requests::Id))
                    .col(text(Requests::Topic))
                    .col(big_integer_null(Requests::Turn))
                    .col(text(Requests::Caller))
                    .col(text_null(Requests::ClientRequestId))
                    .col(text(Requests::Kind))
                    .col(json(Requests::Card))
                    .col(text(Requests::ForUser))
                    .col(boolean(Requests::AdminOnly).default(false))
                    .col(text(Requests::Status))
                    .col(json_null(Requests::Response))
                    .col(text_null(Requests::AnsweredBy))
                    .col(timestamp(Requests::CreatedAt))
                    .col(timestamp_null(Requests::ExpiresAt))
                    .col(timestamp_null(Requests::ResolvedAt))
                    .col(big_integer_null(Requests::StreamSeq))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("requests_status_expires_at")
                    .table(Requests::Table)
                    .col(Requests::Status)
                    .col(Requests::ExpiresAt)
                    .to_owned(),
            )
            .await?;
        // A retried tool call names its request again; the schema builder has no partial index.
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE UNIQUE INDEX requests_caller_client_request_id \
                 ON requests (caller, client_request_id) WHERE client_request_id IS NOT NULL",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Requests::Table).to_owned())
            .await
    }
}
