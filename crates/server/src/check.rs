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
    config.check_inherited()?;
    crate::check_token(&config.server.token.resolve().context("server.token")?)?;
    if config.web.is_some() {
        for user in config.users().iter() {
            let (key, password) = config
                .password_of(&user.name)
                .with_context(|| format!("{} has no password to sign in with", user.name))?;
            crate::check_password(&password.resolve().context(key.clone())?, &key)?;
        }
    }
    let checks = approval_checks(config).await?;
    // No two people's credentials hold the same value, as `serve` checks.
    let claude = fronde_hub::origin::Origin::parse(fronde_hub::config::CLAUDE_ORIGIN)?;
    crate::refuse_equal(&crate::readable_credentials(config, |name| {
        checks.secrets.open(name, &claude).ok()
    }))?;
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
        for line in usage_check_lines(config).await {
            writeln!(out, "usage: {line}")?;
        }
        writeln!(out, "users: {}", users_line(config))?;
        for account in config.accounts() {
            writeln!(out, "{}", account_line(config, &account, &checks.secrets)?)?;
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

/// Who signs in, whether an admin, and the accounts each owns: `alix (admin, account alix),
/// paul (no account)`.
fn users_line(config: &Config) -> String {
    let accounts = config.accounts();
    config
        .users()
        .iter()
        .map(|user| {
            let mut parts: Vec<String> = Vec::new();
            if user.admin {
                parts.push("admin".to_owned());
            }
            let owned: Vec<String> = accounts
                .iter()
                .filter(|account| account.owner == user.name)
                .map(|account| format!("account {}", account.name))
                .collect();
            if owned.is_empty() {
                parts.push("no account".to_owned());
            }
            parts.extend(owned);
            format!("{} ({})", user.name, parts.join(", "))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// One account's line: its provider and owner, where its token comes from and whether it reads,
/// never its value. A stored token is opened as an upstream server's is, for Claude's API. A
/// token that does not read fails the check, but for a stored one not stored yet of an account
/// that is not the owner's: a warning, since its owner's agents are not started yet.
fn account_line(
    config: &Config,
    account: &fronde_hub::config::Account,
    stored: &fronde_wasm::StoredSecrets,
) -> anyhow::Result<String> {
    use fronde_hub::config::{AccountForm, CLAUDE_ORIGIN};
    let name = &account.name;
    let head = format!(
        "account {name} ({}, owner {})",
        account.provider, account.owner
    );
    let place = || format!("account {name}: oauth_token");
    let legacy = match account.form {
        AccountForm::Declared => "",
        AccountForm::AgentToken => " ([agent] oauth_token, legacy form)",
        AccountForm::Inherited | AccountForm::Unsigned => " (legacy form)",
    };
    let token = match &account.token {
        None => {
            return Ok(format!(
                "{head}: no token: the agent starts unsigned{legacy}"
            ));
        }
        Some(token) => token,
    };
    let source = match token {
        Secret::Stored { stored: secret } => {
            let origin = fronde_hub::origin::Origin::parse(CLAUDE_ORIGIN)?;
            match stored.open(secret, &origin) {
                Ok(value) => {
                    crate::one_line(value).with_context(place)?;
                    return Ok(format!(
                        "{head}: stored secret {secret} opens, for {CLAUDE_ORIGIN}{legacy}"
                    ));
                }
                Err(_) if !stored.is_stored(secret) && account.owner != config.data.user => {
                    return Ok(format!(
                        "{head}: warning: stored secret {secret} not stored yet: store it with \
                         `fronde secret set {secret} --origin {CLAUDE_ORIGIN} --config <your \
                         fronde.toml>`, the token on standard input"
                    ));
                }
                Err(error) => return Err(error.context(place())),
            }
        }
        Secret::File { file } => format!("the file {}", file.display()),
        Secret::Env { env } if account.form == AccountForm::Inherited => {
            format!("Fronde's environment, {env}")
        }
        Secret::Env { env } => format!("the environment variable {env}"),
    };
    crate::one_line(token.resolve().with_context(place)?).with_context(place)?;
    Ok(format!("{head}: token from {source}, read{legacy}"))
}

/// [`crate::usage_line`] of each configured account (there is always one, the legacy form's
/// included), one line each, from the readings stored in the database, opened read-only: never created, migrated or written. A data directory
/// without a database has no reading. A database that cannot be read is said once,
/// `cannot read (…)`; a row that cannot be read is said on its account's line, and the check
/// goes on.
async fn usage_check_lines(config: &Config) -> Vec<String> {
    let file = fronde_db::path(&config.data.dir);
    let db = match file.try_exists() {
        Ok(true) => match fronde_db::open_read_only(&config.data.dir).await {
            Ok(db) => Some(db),
            Err(error) => return vec![format!("cannot read ({error:#})")],
        },
        Ok(false) => None,
        Err(error) => {
            return vec![format!(
                "cannot read (cannot look for the database {}: {error})",
                file.display()
            )];
        }
    };
    let mut lines = Vec::new();
    for account in config.accounts() {
        let reading = match &db {
            Some(db) => fronde_agent::usage::read_usage(db, &account.name).await,
            None => Ok(None),
        };
        lines.push(match reading {
            Ok(reading) => crate::usage_line(
                &[(account.name.as_str(), reading)],
                config.threads.reserve.0,
                chrono::Utc::now(),
                config.data.timezone,
            ),
            Err(error) => format!(
                "cannot read ({:#})",
                error.context(format!("the usage of {}", account.name))
            ),
        });
    }
    // Read only: a failed close loses nothing, and is no account's usage.
    if let Some(db) = db
        && let Err(error) = db.close().await
    {
        tracing::debug!("cannot close the database read for the usage: {error:#}");
    }
    lines
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
