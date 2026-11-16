//! `fronde check`: what can be validated without serving.

use anyhow::Context;
use fronde_hub::config::{Config, SourceConfig};
use std::io::Write;

/// Check the secrets and sources of a loaded configuration. For each component, write the hashes
/// that approve its files to `out` first, so that a component not approved yet shows them.
pub fn check(config: &Config, out: &mut impl Write) -> anyhow::Result<()> {
    crate::check_token(&config.server.token.resolve().context("server.token")?)?;
    if let Some(web) = &config.web {
        crate::check_password(&web.password.resolve().context("web.password")?)?;
    }
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
                fronde_wasm::prepare(c, config.data.timezone.name())
                    .with_context(|| format!("source {name}"))?;
            }
        }
    }
    Ok(())
}
