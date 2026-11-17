//! Adds a `[[sources]]` entry to `fronde.toml`, keeping everything the file holds: comments,
//! order, formatting. Only Fronde's own process calls it, once an admin answered a setup card.
//!
//! The file is replaced atomically (`fronde-fs`), keeps its mode (never wider), and is never
//! written through a link; a hard link to it is detached by the rename. An entry never holds a
//! secret: its credentials name stored ones.

use anyhow::{Context, bail};
use fronde_hub::config::{ComponentConfig, Secret, SourceConfig};
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
/// rest kept as written. Whether the result still loads is for [`add_component`] to check.
pub fn with_entry(text: &str, entry: &ComponentConfig) -> anyhow::Result<String> {
    let mut document: DocumentMut = text.parse().context("cannot read it as TOML")?;
    let sources = document
        .entry("sources")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    let Some(sources) = sources.as_array_of_tables_mut() else {
        bail!("its sources are not written as [[sources]] tables");
    };
    sources.push(table(entry)?);
    Ok(document.to_string())
}

/// How the file is replaced: [`fronde_fs::write_atomic_mode`], unless a test makes it fail.
pub type Write = fn(&Path, &str, u32) -> std::io::Result<()>;

/// Replaces `path` with `text`, mode `mode`, atomically.
pub fn write_atomic(path: &Path, text: &str, mode: u32) -> std::io::Result<()> {
    fronde_fs::write_atomic_mode(path, text, mode)
}

/// Why an entry was not added to the file.
#[derive(Debug)]
pub enum NotAdded {
    /// The file cannot be written as it is (a read-only mount, a link, a file that does not
    /// read): the owner pastes the entry by hand.
    Unwritable(anyhow::Error),
    /// Adding it would leave a configuration Fronde cannot start with (a source of that name
    /// added by hand meanwhile, a check of `config::load` that fails), or the file kept changing:
    /// nothing is written, and pasting the entry would not do either. `error` never quotes the
    /// file, which may hold a secret written in plain. `sources` are the file's entries, when it
    /// could be read.
    Refused {
        error: anyhow::Error,
        sources: Option<Vec<SourceConfig>>,
    },
}

/// What identifies the file's content as read: replaced or edited, it changes. An edit in
/// place that keeps the size, within the granularity of the file system's timestamps, keeps
/// it too: that one goes unnoticed.
#[derive(Debug, PartialEq, Eq)]
struct Stamp {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    modified: Option<std::time::SystemTime>,
    len: u64,
}

/// The file at `path` as it is now, refused for a link or anything but a file, and its mode.
fn stamp(path: &Path) -> anyhow::Result<(Stamp, u32)> {
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
    let (dev, ino, mode) = {
        use std::os::unix::fs::MetadataExt as _;
        (metadata.dev(), metadata.ino(), metadata.mode() & 0o777)
    };
    #[cfg(not(unix))]
    let mode = 0o600;
    Ok((
        Stamp {
            #[cfg(unix)]
            dev,
            #[cfg(unix)]
            ino,
            modified: metadata.modified().ok(),
            len: metadata.len(),
        },
        mode,
    ))
}

/// A refusal for a text of `path` that `config::load` would refuse: logged with the path and,
/// for TOML that does not parse, the line, never the error itself, which quotes the file;
/// `with_entry` when only the text with the new entry fails.
fn not_loading(
    path: &Path,
    text: &str,
    error: &anyhow::Error,
    with_entry: bool,
    sources: Option<Vec<SourceConfig>>,
) -> NotAdded {
    let line = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<toml::de::Error>())
        .and_then(toml::de::Error::span)
        .map(|span| text[..span.start.min(text.len())].matches('\n').count() + 1);
    let at = line
        .map(|line| format!(" at line {line}"))
        .unwrap_or_default();
    let what = if with_entry {
        "would no longer load with the new entry"
    } else {
        "no longer loads"
    };
    tracing::warn!(
        "{}{at} {what}: nothing was written; `fronde check` says why",
        path.display()
    );
    let message = if with_entry {
        format!(
            "with the new entry, {} would no longer load; nothing was written; the owner can run `fronde check`",
            path.display()
        )
    } else {
        format!(
            "{} no longer loads; nothing was written; the owner can run `fronde check`",
            path.display()
        )
    };
    NotAdded::Refused {
        error: anyhow::anyhow!(message),
        sources,
    }
}

/// `fronde.toml` read and checked for `entry`, not written yet.
struct Checked {
    before: Stamp,
    mode: u32,
    /// The text with the entry.
    new: String,
    /// The file's entries, as it is.
    current: Vec<SourceConfig>,
    /// The file's entries, with the new one.
    added: Vec<SourceConfig>,
}

/// Reads `path` and checks that `entry` can be added to it: no entry of that name, and the
/// file, as it is and with the entry, passes every check of `config::load`.
fn read_checked(path: &Path, base: &Path, entry: &ComponentConfig) -> Result<Checked, NotAdded> {
    let unwritable = |error: anyhow::Error| NotAdded::Unwritable(error);
    let (before, mode) = stamp(path).map_err(unwritable)?;
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read {}", path.display()))
        .map_err(unwritable)?;
    let current = fronde_hub::config::load_str(&text, base)
        .map_err(|error| not_loading(path, &text, &error, false, None))?
        .sources;
    if current.iter().any(|s| s.name() == entry.name) {
        return Err(NotAdded::Refused {
            error: anyhow::anyhow!(
                "{} already has a [[sources]] entry named {}, added since Fronde started; nothing was written",
                path.display(),
                entry.name
            ),
            sources: Some(current),
        });
    }
    let new = with_entry(&text, entry)
        .with_context(|| format!("{}", path.display()))
        .map_err(unwritable)?;
    let added = fronde_hub::config::load_str(&new, base)
        .map_err(|error| not_loading(path, &new, &error, true, Some(current.clone())))?
        .sources;
    Ok(Checked {
        before,
        mode,
        new,
        current,
        added,
    })
}

/// Whether `entry` could be added to the configuration file `path` now, as [`add_component`]
/// would check it, without writing anything: the file's entries, when it reads.
pub fn check_component(
    path: &Path,
    base: &Path,
    entry: &ComponentConfig,
) -> Result<Vec<SourceConfig>, NotAdded> {
    read_checked(path, base, entry).map(|checked| checked.current)
}

/// Adds `entry` to the configuration file `path`, whose relative paths are read from `base`,
/// and returns the file's `[[sources]]` entries with it.
///
/// The file is read again: an entry of the same name added by hand since Fronde started is
/// refused, and the new text must pass every check `config::load` makes, so that Fronde still
/// starts. The file is replaced atomically by a rename, keeping its mode: a hard link to it
/// keeps the old content, detached. Just before the rename, the file's inode, modification time
/// and size are compared with those read: a change (an edit meanwhile) reads it again, once,
/// then refuses. An edit in the instant between that check and the rename is lost, and so is
/// one that keeps the size within the granularity of the file system's timestamps.
pub fn add_component(
    path: &Path,
    base: &Path,
    entry: &ComponentConfig,
    write: Write,
) -> Result<Vec<SourceConfig>, NotAdded> {
    for _ in 0..2 {
        let checked = read_checked(path, base, entry)?;
        let (now, _) = stamp(path).map_err(NotAdded::Unwritable)?;
        if now != checked.before {
            continue;
        }
        write(path, &checked.new, checked.mode)
            .with_context(|| format!("cannot write {}", path.display()))
            .map_err(NotAdded::Unwritable)?;
        return Ok(checked.added);
    }
    Err(NotAdded::Refused {
        error: anyhow::anyhow!(
            "{} kept changing while Fronde added the entry; nothing was written",
            path.display()
        ),
        sources: None,
    })
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

    const CONFIG: &str = "# Mine.\n[server]\ntoken = { env = \"UNUSED\" }\n[data]\ndir = \"data\"\ntimezone = \"Europe/Paris\"\n[memory]\ngit = false\n";

    fn never(_: &Path, _: &str, _: u32) -> std::io::Result<()> {
        panic!("nothing is written")
    }

    /// `fronde.toml` holding `text` in a new directory.
    fn file(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fronde.toml");
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    #[test]
    fn the_entry_is_added_and_the_file_still_loads() {
        let (dir, path) = file(CONFIG);
        let sources = add_component(&path, dir.path(), &entry(), write_atomic).unwrap();
        assert_eq!(sources.len(), 1);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(CONFIG), "{text}");
        assert_eq!(
            fronde_hub::config::load(&path).unwrap().sources[0].name(),
            "calendar"
        );
    }

    #[test]
    fn a_name_the_file_has_already_is_refused() {
        let by_hand = format!(
            "{CONFIG}[[sources]]\nkind = \"mcp\"\nname = \"calendar\"\nurl = \"https://mcp.example\"\n"
        );
        let (dir, path) = file(&by_hand);
        let Err(NotAdded::Refused { error, sources }) =
            add_component(&path, dir.path(), &entry(), never)
        else {
            panic!("refused");
        };
        assert!(
            format!("{error:#}").contains("already has a [[sources]] entry named calendar"),
            "{error:#}"
        );
        assert_eq!(sources.unwrap()[0].name(), "calendar");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), by_hand);
    }

    #[test]
    fn an_entry_load_would_refuse_is_not_written() {
        let (dir, path) = file(CONFIG);
        let mut reserved = entry();
        reserved.name = "fronde".into();
        let Err(NotAdded::Refused { error, .. }) =
            add_component(&path, dir.path(), &reserved, never)
        else {
            panic!("refused");
        };
        assert!(
            format!("{error:#}").contains("would no longer load"),
            "{error:#}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), CONFIG);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_never_written_through() {
        let (dir, path) = file(CONFIG);
        let link = dir.path().join("link.toml");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let Err(NotAdded::Unwritable(error)) = add_component(&link, dir.path(), &entry(), never)
        else {
            panic!("unwritable");
        };
        assert!(format!("{error:#}").contains("is a link"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), CONFIG);
    }

    #[test]
    fn an_inline_sources_array_is_left_alone() {
        let text = "sources = []\n";
        let error = with_entry(text, &entry()).unwrap_err();
        assert!(format!("{error:#}").contains("[[sources]]"), "{error:#}");
    }
}
