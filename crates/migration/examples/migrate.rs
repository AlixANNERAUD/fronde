//! Applies Fronde's migrations to the SQLite database at the URL given as the only argument,
//! such as `sqlite://target/entities-schema/schema.db?mode=rwc`. `mise run entities` uses it to
//! get a schema to generate the entities from, building only the SeaORM stack.

use fronde_migration::{Migrator, MigratorTrait};
use sea_orm_migration::sea_orm::Database;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::args()
        .nth(1)
        .ok_or("usage: migrate <database URL>, such as sqlite://schema.db?mode=rwc")?;
    let database = Database::connect(&url).await?;
    Migrator::up(&database, None).await?;
    Ok(())
}
