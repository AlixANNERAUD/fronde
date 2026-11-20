//! The account a thread runs on, when its owner picked one: `threads.account`, nullable. `NULL`
//! is the owner's default account on the thread's provider, looked up at each start, so every
//! row from before keeps its meaning without a backfill. A name is checked again at each start.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Threads {
    Table,
    Account,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Threads::Table)
                    .add_column(text_null(Threads::Account))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Threads::Table)
                    .drop_column(Threads::Account)
                    .to_owned(),
            )
            .await
    }
}
