//! Secrets the owner stored, sealed with the master key: each row holds the nonce and the
//! ciphertext, never the value. The format `version`, the scope, the name and the canonical
//! origin are authenticated with the ciphertext, so a row changed or moved does not open.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Secrets {
    Table,
    Scope,
    Name,
    Origin,
    Version,
    Nonce,
    Ciphertext,
    CreatedAt,
    UpdatedAt,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Secrets::Table)
                    .col(text(Secrets::Scope))
                    .col(text(Secrets::Name))
                    .col(text(Secrets::Origin))
                    .col(integer(Secrets::Version))
                    .col(blob(Secrets::Nonce))
                    .col(blob(Secrets::Ciphertext))
                    .col(timestamp(Secrets::CreatedAt))
                    .col(timestamp(Secrets::UpdatedAt))
                    .primary_key(Index::create().col(Secrets::Scope).col(Secrets::Name))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Secrets::Table).to_owned())
            .await
    }
}
