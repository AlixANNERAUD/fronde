//! `fronde check`: what can be validated without serving.

use anyhow::Context;
use fronde_hub::config::{Config, SourceConfig};
use std::io::Write;

/// Check the secrets and sources of a loaded configuration. For each component, write the hashes
/// that approve its files to `out` first, so that a component not approved yet shows them, then
/// what approved it. Approvals stored in the database count as when serving; a data directory
/// without a database has none, and the database is never changed by this check.
pub async fn check(config: &Config, out: &mut impl Write) -> anyhow::Result<()> {
    crate::check_token(&config.server.token.resolve().context("server.token")?)?;
    if let Some(web) = &config.web {
        crate::check_password(&web.password.resolve().context("web.password")?)?;
    }
    let checks = approval_checks(config).await?;
    for source in &config.sources {
        let name = source.name();
        match source {
            SourceConfig::Mcp(m) => {
                if let Some(t) = &m.token {
                    t.resolve()
                        .with_context(|| format!("source {name}: token"))?;
                }
            }
            SourceConfig::Component(c) => {
                let hashes =
                    fronde_wasm::Hashes::of(c).with_context(|| format!("source {name}"))?;
                writeln!(out, "source {name}: sha256 = \"{}\"", hashes.sha256)?;
                writeln!(
                    out,
                    "source {name}: manifest_sha256 = \"{}\"",
                    hashes.manifest_sha256
                )?;
                let prepared =
                    fronde_wasm::prepare(c, &checks).with_context(|| format!("source {name}"))?;
                writeln!(out, "source {name}: {}", prepared.approval())?;
            }
        }
    }
    Ok(())
}

/// The checks serving would make. The database is read, never created, migrated or written: a
/// missing database or a missing approvals table (a database an older Fronde left, which serving
/// would migrate to an empty table) means no stored approval.
async fn approval_checks(config: &Config) -> anyhow::Result<fronde_wasm::Checks> {
    let file = fronde_db::path(&config.data.dir);
    let exists = file
        .try_exists()
        .with_context(|| format!("cannot look for the database {}", file.display()))?;
    if !exists {
        return Ok(fronde_wasm::Checks::of(config));
    }
    let db = fronde_db::open_read_only(&config.data.dir).await?;
    let has_approvals = fronde_db::approvals::table_exists(&db)
        .await
        .with_context(|| format!("cannot read the database {}", file.display()))?;
    let checks = if has_approvals {
        fronde_wasm::Checks::load(config, &db).await?
    } else {
        fronde_wasm::Checks::of(config)
    };
    db.close().await?;
    Ok(checks)
}
