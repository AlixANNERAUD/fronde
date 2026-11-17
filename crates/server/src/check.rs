//! `fronde check`: what can be validated without serving.

use anyhow::Context;
use fronde_hub::config::{Config, Secret, SourceConfig};
use std::io::Write;

/// Check the secrets and sources of a loaded configuration. For each component, write the hashes
/// that approve its files to `out` first, so that a component not approved yet shows them, then
/// what approved it, then each credential that names a stored secret (and each upstream server's
/// stored token): it exists, is for the
/// credential's origin and opens with the master key (its value is never shown). Approvals and
/// secrets stored in the database count as when serving; a data directory without a database has
/// none, and the database is never changed by this check. With `[python]`, check and compile the
/// runtime as `serve` would.
pub async fn check(config: &Config, out: &mut impl Write) -> anyhow::Result<()> {
    crate::check_token(&config.server.token.resolve().context("server.token")?)?;
    if let Some(web) = &config.web {
        crate::check_password(&web.password.resolve().context("web.password")?)?;
    }
    let checks = approval_checks(config).await?;
    if let Some(backend) = checks.secrets.key_backend() {
        writeln!(
            out,
            "secrets: master key from {} ({backend}), opens the stored secrets",
            config.master_key_backends()?.place(backend)
        )?;
    }
    for source in &config.sources {
        let name = source.name();
        match source {
            SourceConfig::Mcp(m) => {
                match (&m.token, &m.url) {
                    // A stored token is opened for the origin of the URL, as starting the server
                    // opens it; the value is dropped, never shown.
                    (Some(Secret::Stored { stored }), Some(url)) => {
                        let origin = fronde_hub::origin::Origin::of_url(url)
                            .with_context(|| format!("source {name}"))?;
                        checks
                            .secrets
                            .open(stored, &origin)
                            .with_context(|| format!("source {name}: token"))?;
                        writeln!(
                            out,
                            "source {name}: token: stored secret {stored} opens, stored for \
                             this origin"
                        )?;
                    }
                    (Some(t), _) => {
                        t.resolve()
                            .with_context(|| format!("source {name}: token"))?;
                    }
                    (None, _) => {}
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
                for credential in &c.credentials {
                    let secret = credential.basic.as_ref().map(|b| &b.password);
                    if let Some(Secret::Stored { stored }) = secret.or(credential.bearer.as_ref()) {
                        writeln!(
                            out,
                            "source {name}: credential for {}: stored secret {stored} opens, \
                             stored for this origin",
                            credential.origin
                        )?;
                    }
                }
            }
        }
    }
    if config.web.is_some() {
        let helper = std::env::current_exe()?;
        let confine = crate::agent_confine(config, &helper)?;
        let (line, starts) = crate::sandbox_line(&confine, config.server.listen.port());
        let warning = if starts {
            ""
        } else {
            "; the agent cannot start"
        };
        writeln!(out, "agent sandbox: {line}{warning}")?;
        let scope = crate::agent_scope(config, std::env::var_os("PATH").as_deref());
        let limits = crate::limits_check_line(config, scope.as_ref()).await;
        writeln!(out, "agent limits: {limits}")?;
        writeln!(out, "agents: {}", crate::pool_line(config, scope.as_ref()))?;
        writeln!(out, "sub-agents: {}", crate::children_line(config))?;
        writeln!(out, "usage: {}", usage_check_line(config).await?)?;
        // A stored token is opened at each start of the agent, with the master key.
        match &config.agent.oauth_token {
            Some(fronde_hub::config::Secret::Stored { stored }) => {
                writeln!(out, "agent token: the stored secret {stored}")?
            }
            Some(token) => {
                token.resolve().context("[agent] oauth_token")?;
                writeln!(out, "agent token: [agent] oauth_token, read")?
            }
            None => {}
        }
    }
    if let Some(python) = &config.python {
        let host = fronde_wasm::ComponentHost::new()?;
        fronde_wasm::Python::load(&host, &python.dir)
            .await
            .context("python")?;
        writeln!(
            out,
            "python: approved (CPython {})",
            fronde_wasm::PYTHON_VERSION
        )?;
    }
    Ok(())
}

/// [`crate::usage_line`] from the owner's readings stored in the database, opened read-only:
/// never created, migrated or written. A data directory without a database has no reading.
async fn usage_check_line(config: &Config) -> anyhow::Result<String> {
    let file = fronde_db::path(&config.data.dir);
    let exists = file
        .try_exists()
        .with_context(|| format!("cannot look for the database {}", file.display()))?;
    let db = match exists {
        true => Some(fronde_db::open_read_only(&config.data.dir).await?),
        false => None,
    };
    let mut readings = Vec::new();
    for provider in fronde_hub::config::PROVIDERS {
        let account = fronde_agent::usage::account_of(&config.data.user, provider);
        let reading = match &db {
            Some(db) => fronde_agent::usage::read_usage(db, &account)
                .await
                .with_context(|| format!("cannot read the usage of {account}"))?,
            None => None,
        };
        readings.push((provider, reading));
    }
    if let Some(db) = db {
        db.close().await?;
    }
    Ok(crate::usage_line(
        &readings,
        config.threads.reserve.0,
        chrono::Utc::now(),
        config.data.timezone,
    ))
}

/// The checks serving would make. The database is read, never created, migrated or written: a
/// missing database, or a missing approvals or secrets table (a database an older Fronde left,
/// which serving would migrate to an empty table), means no stored approval or secret. The master
/// key is checked as serving checks it.
async fn approval_checks(config: &Config) -> anyhow::Result<fronde_wasm::Checks> {
    let mut checks = fronde_wasm::Checks::of(config);
    let file = fronde_db::path(&config.data.dir);
    let exists = file
        .try_exists()
        .with_context(|| format!("cannot look for the database {}", file.display()))?;
    if !exists {
        return Ok(checks);
    }
    let db = fronde_db::open_read_only(&config.data.dir).await?;
    let unreadable = || format!("cannot read the database {}", file.display());
    if fronde_db::approvals::table_exists(&db)
        .await
        .with_context(unreadable)?
    {
        checks.approvals.stored = fronde_wasm::stored_approvals(&db).await?;
    }
    if fronde_db::secrets::table_exists(&db)
        .await
        .with_context(unreadable)?
    {
        // Where serving would read the key, without moving one an earlier version left in the
        // data directory: serving moves it.
        let key_file = fronde_db::master_key::locate(config.master_key_file()?, &config.data.dir)?;
        let backends = config.master_key_backends()?.with_file(key_file);
        checks.secrets = fronde_wasm::StoredSecrets::load(&db, &backends).await?;
    }
    db.close().await?;
    Ok(checks)
}
