//! The memory's full-text index: one row per non-empty line of the Markdown memory files, which
//! stay the truth; the hub rebuilds the rows from them at each start. FTS5 has no schema-builder
//! form, hence the raw SQL.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE VIRTUAL TABLE memory_fts USING fts5(file UNINDEXED, line UNINDEXED, text, \
                 tokenize='unicode61 remove_diacritics 2')",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE memory_fts")
            .await?;
        Ok(())
    }
}
