//! Adds a `[[sources]]` entry to `fronde.toml`, keeping everything the file holds: comments,
//! order, formatting. Only Fronde's own process calls it, once an admin answered a setup card.
//!
//! The file is replaced atomically (`fronde-fs`), keeps its mode (never wider), and is never
//! written through a link. An entry never holds a secret: its credentials name stored ones.

use anyhow::{Context, bail};
use fronde_hub::config::{ComponentConfig, Secret};
use std::path::Path;
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value, value};

/// The line written above an entry Fronde adds.
const ADDED: &str = "# Added by Fronde from a setup card in the chat.";

/// A secret as an entry names it: `{ stored = "…" }`, or a file or variable, never a value.
fn secret(secret: &Secret) -> InlineTable {
    let mut table = InlineTable::new();
    match secret {
        Secret::Stored { stored } => table.insert("stored", stored.as_str().into()),
        Secret::File { file } => table.insert("file", file.display().to_string().into()),
        Secret::Env { env } => table.insert("env", env.as_str().into()),
    };
    table
}

/// `entry` as a `[[sources]]` table.
fn table(entry: &ComponentConfig) -> anyhow::Result<Table> {
    let mut table = Table::new();
    table["kind"] = value("component");
    table["name"] = value(entry.name.as_str());
    let path = entry
        .path
        .to_str()
        .with_context(|| format!("{} is not UTF-8", entry.path.display()))?;
    table["path"] = value(path);
    if !entry.allow.is_empty() {
        let mut allow = Array::new();
        for a in &entry.allow {
            if a.local {
                let mut local = InlineTable::new();
                local.insert("origin", a.origin.to_string().into());
                local.insert("local", true.into());
                allow.push(Value::InlineTable(local));
            } else {
                allow.push(a.origin.to_string());
            }
        }
        table["allow"] = value(allow);
    }
    if !entry.settings.is_empty() {
        let mut settings = Table::new();
        for (key, setting) in &entry.settings {
            settings[key.as_str()] = value(setting.as_str());
        }
        table["settings"] = Item::Table(settings);
    }
    if !entry.credentials.is_empty() {
        let mut credentials = ArrayOfTables::new();
        for c in &entry.credentials {
            let mut credential = Table::new();
            credential["origin"] = value(c.origin.to_string());
            if let Some(basic) = &c.basic {
                let mut inline = InlineTable::new();
                inline.insert("username", basic.username.as_str().into());
                inline.insert("password", Value::InlineTable(secret(&basic.password)));
                credential["basic"] = value(inline);
            }
            if let Some(bearer) = &c.bearer {
                credential["bearer"] = value(secret(bearer));
            }
            credentials.push(credential);
        }
        table["credentials"] = Item::ArrayOfTables(credentials);
    }
    table.decor_mut().set_prefix(format!("\n{ADDED}\n"));
    Ok(table)
}

/// `entry` as the text to paste at the end of `fronde.toml`.
pub fn snippet(entry: &ComponentConfig) -> anyhow::Result<String> {
    let mut document = DocumentMut::new();
    let mut sources = ArrayOfTables::new();
    sources.push(table(entry)?);
    document["sources"] = Item::ArrayOfTables(sources);
    Ok(document.to_string().trim_start().to_owned())
}

/// The text of `fronde.toml`, `text`, with `entry` added as its last `[[sources]]` entry: the
/// rest kept as written. The result must still read as a configuration.
pub fn with_entry(text: &str, entry: &ComponentConfig) -> anyhow::Result<String> {
    let mut document: DocumentMut = text.parse().context("cannot read it as TOML")?;
    let sources = document
        .entry("sources")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    let Some(sources) = sources.as_array_of_tables_mut() else {
        bail!("its sources are not written as [[sources]] tables");
    };
    sources.push(table(entry)?);
    let text = document.to_string();
    toml::from_str::<fronde_hub::config::Config>(&text)
        .context("with the new entry, it would no longer read as a configuration")?;
    Ok(text)
}

/// Adds `entry` to the configuration file `path`: refused for a link, a file that does not read,
/// or one that cannot be written (a read-only mount); then the entry is pasted by hand. The file
/// keeps its mode.
pub fn add_component(path: &Path, entry: &ComponentConfig) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "{} is a link: Fronde does not replace it with a file",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!("{} is not a file", path.display());
    }
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o777
    };
    #[cfg(not(unix))]
    let mode = 0o600;
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let text = with_entry(&text, entry).with_context(|| format!("{}", path.display()))?;
    fronde_fs::write_atomic_mode(path, text, mode)
        .with_context(|| format!("cannot write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fronde_hub::config::{Allow, BasicCredential, CredentialConfig};
    use fronde_hub::origin::Origin;

    fn entry() -> ComponentConfig {
        let origin = Origin::parse("https://cloud.example").unwrap();
        ComponentConfig {
            name: "calendar".into(),
            path: "/app/components/calendar.wasm".into(),
            manifest: None,
            sha256: None,
            manifest_sha256: None,
            allow: vec![
                Allow {
                    origin: origin.clone(),
                    local: false,
                },
                Allow {
                    origin: Origin::parse("http://192.168.1.10:5232").unwrap(),
                    local: true,
                },
            ],
            settings: [(
                "events_url".to_owned(),
                "https://cloud.example/dav/".to_owned(),
            )]
            .into(),
            credentials: vec![CredentialConfig {
                origin,
                basic: Some(BasicCredential {
                    username: "alix".into(),
                    password: Secret::Stored {
                        stored: "calendar_caldav".into(),
                    },
                }),
                bearer: None,
            }],
        }
    }

    #[test]
    fn the_snippet_names_the_stored_secret() {
        let snippet = snippet(&entry()).unwrap();
        assert_eq!(
            snippet,
            "# Added by Fronde from a setup card in the chat.\n\
             [[sources]]\n\
             kind = \"component\"\n\
             name = \"calendar\"\n\
             path = \"/app/components/calendar.wasm\"\n\
             allow = [\"https://cloud.example\", { origin = \"http://192.168.1.10:5232\", local = true }]\n\
             \n\
             [sources.settings]\n\
             events_url = \"https://cloud.example/dav/\"\n\
             \n\
             [[sources.credentials]]\n\
             origin = \"https://cloud.example\"\n\
             basic = { username = \"alix\", password = { stored = \"calendar_caldav\" } }\n"
        );
    }

    #[test]
    fn an_inline_sources_array_is_left_alone() {
        let text = "sources = []\n";
        let error = with_entry(text, &entry()).unwrap_err();
        assert!(format!("{error:#}").contains("[[sources]]"), "{error:#}");
    }
}
