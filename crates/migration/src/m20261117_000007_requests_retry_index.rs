//! A retried call joins its request only while that request is the same question, so the key
//! `(caller, client_request_id)` is unique among pending requests only: once a request ends,
//! its id may name a new question. Replaces the permanent unique index of
//! `m20261117_000006_requests`, which was raw SQL.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Requests {
    Table,
    Caller,
    ClientRequestId,
    Status,
}

const OLD: &str = "requests_caller_client_request_id";
const NEW: &str = "requests_pending_caller_client_request_id";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    /// Dropped and made again in one transaction, so no write finds the key unguarded.
    fn use_transaction(&self) -> Option<bool> {
        Some(true)
    }

    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(Index::drop().name(OLD).table(Requests::Table).to_owned())
            .await?;
        manager
            .create_index(
                Index::create()
                    .name(NEW)
                    .table(Requests::Table)
                    .col(Requests::Caller)
                    .col(Requests::ClientRequestId)
                    .unique()
                    .and_where(Expr::col(Requests::ClientRequestId).is_not_null())
                    .and_where(Expr::col(Requests::Status).eq("pending"))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(Index::drop().name(NEW).table(Requests::Table).to_owned())
            .await?;
        manager
            .create_index(
                Index::create()
                    .name(OLD)
                    .table(Requests::Table)
                    .col(Requests::Caller)
                    .col(Requests::ClientRequestId)
                    .unique()
                    .and_where(Expr::col(Requests::ClientRequestId).is_not_null())
                    .to_owned(),
            )
            .await
    }
}
