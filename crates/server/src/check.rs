//! `fronde check`: what can be validated without serving.

use anyhow::Context;
use fronde_hub::config::{Config, SourceConfig};
use std::io::Write;

/// Check the secrets and sources of a loaded configuration. For each component, write the hashes
/// that approve its files to `out` first, so that a component not approved yet shows them, then
/// what approved it. Approvals stored in the database count as when serving; a data directory
/// without a database has none, and gets no database from this check.
pub async fn check(config: &Config, out: &mut impl Write) -> anyhow::Result<()> {
    crate::check_token(&config.server.token.resolve().context("server.token")?)?;
    if let Some(web) = &config.web {
        crate::check_password(&web.password.resolve().context("web.password")?)?;
    }
    let checks = if fronde_db::path(&config.data.dir).exists() {
        let db = fronde_db::open(&config.data.dir).await?;
        fronde_wasm::Checks::load(config, &db).await?
    } else {
        fronde_wasm::Checks::of(config)
    };
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
