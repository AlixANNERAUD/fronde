//! Component approvals given by the owner: the source they approve and the SHA-256 of both of
//! its files, so that a changed build or manifest is not approved by an older row.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Approvals {
    Table,
    Source,
    Sha256,
    ManifestSha256,
    ApprovedAt,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Approvals::Table)
                    .col(text(Approvals::Source))
                    .col(text(Approvals::Sha256))
                    .col(text(Approvals::ManifestSha256))
                    .col(timestamp(Approvals::ApprovedAt))
                    .primary_key(
                        Index::create()
                            .col(Approvals::Source)
                            .col(Approvals::Sha256)
                            .col(Approvals::ManifestSha256),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Approvals::Table).to_owned())
            .await
    }
}
