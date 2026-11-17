//! `fronde secret`: store, list and remove the secrets kept encrypted in `fronde.db`.

use anyhow::{Context as _, bail};
use fronde_db::master_key::Backend;
use fronde_db::secrets;
use fronde_hub::config::Config;
use fronde_hub::origin::Origin;
use secrecy::SecretString;
use std::io::{Read, Write};
use zeroize::Zeroizing;

/// The value of a secret from `input`, which must not be a terminal: a terminal would echo it.
/// One final line break is dropped.
pub fn read_value(input: impl Read, is_terminal: bool) -> anyhow::Result<SecretString> {
    if is_terminal {
        bail!(
            "standard input is a terminal, which would show the secret as you type it; pass it \
             on standard input instead, from a file (`< file`) or a pipe"
        );
    }
    let limit = secrets::MAX_VALUE_LEN;
    // Room for the longest value and its line break up front: a buffer that grows would leave
    // copies of the secret behind.
    let mut bytes = Zeroizing::new(Vec::with_capacity(limit + 3));
    input
        .take((limit + 3) as u64)
        .read_to_end(&mut bytes)
        .context("cannot read the secret from standard input")?;
    let mut value: &[u8] = &bytes;
    if let Some(rest) = value.strip_suffix(b"\n") {
        value = rest.strip_suffix(b"\r").unwrap_or(rest);
    }
    if value.is_empty() {
        bail!("standard input is empty: pass the secret on it");
    }
    if value.len() > limit {
        bail!("the secret is longer than {limit} bytes");
    }
    let text = std::str::from_utf8(value).context("the secret is not UTF-8 text")?;
    let mut owned = String::with_capacity(text.len());
    owned.push_str(text);
    Ok(SecretString::from(owned))
}

/// The database, after moving a master key an earlier version kept in the data directory to
/// where the configuration says.
async fn open(config: &Config) -> anyhow::Result<fronde_db::DatabaseConnection> {
    fronde_hub::private::prepare_data_dir(&config.data.dir)?;
    fronde_db::master_key::migrate(config.master_key_file()?, &config.data.dir)?;
    fronde_db::open(&config.data.dir).await
}

/// Store `value` under `name` for `origin`, which is stored canonical. Writes what it did to
/// `out`, with the master key's path when this made it.
pub async fn set(
    config: &Config,
    name: &str,
    origin: &str,
    value: &SecretString,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    secrets::check_name(name)?;
    let origin = Origin::parse(origin)?.to_string();
    let db = open(config).await?;
    let backends = config.master_key_backends()?;
    let (key, created) = backends.load_or_create(&db).await?;
    secrets::set(&db, &key, secrets::SOURCES, name, &origin, value).await?;
    db.close().await?;
    if created {
        let place = backends.place(key.backend());
        match key.backend() {
            Backend::File => writeln!(
                out,
                "created the master key {place}: back it up apart from the data directory, \
                 without it the stored secrets are lost"
            )?,
            _ => writeln!(
                out,
                "created the master key in {place}, where it stays: without it the stored \
                 secrets are lost"
            )?,
        }
    }
    writeln!(out, "stored secret {name} for {origin}")?;
    Ok(())
}

/// Write each stored secret's name, origin and last change to `out`, never its value.
pub async fn list(config: &Config, out: &mut impl Write) -> anyhow::Result<()> {
    let db = open(config).await?;
    let listed = secrets::list(&db, secrets::SOURCES).await?;
    db.close().await?;
    if listed.is_empty() {
        writeln!(out, "no stored secret")?;
    }
    for secret in listed {
        writeln!(
            out,
            "{}\t{}\t{}",
            secret.name,
            secret.origin,
            secret.updated_at.format("%Y-%m-%d %H:%M:%S UTC")
        )?;
    }
    Ok(())
}

/// Remove the secret `name`; an error if there is none.
pub async fn remove(config: &Config, name: &str, out: &mut impl Write) -> anyhow::Result<()> {
    let db = open(config).await?;
    let removed = secrets::remove(&db, secrets::SOURCES, name).await?;
    db.close().await?;
    if !removed {
        bail!("no stored secret is named {name}; `fronde secret list` shows them");
    }
    writeln!(out, "removed secret {name}")?;
    Ok(())
}
