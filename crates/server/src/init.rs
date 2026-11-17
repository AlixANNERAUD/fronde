//! `fronde init`: a minimal native configuration and its two secrets, owned by the user.

use anyhow::{Context, bail};
use std::path::{Path, PathBuf};

/// Passphrase alphabet: 32 characters, none easy to misread.
const ALPHABET: &[u8; 32] = b"abcdefghijkmnpqrstuvwxyz23456789";

pub struct Init<'a> {
    pub config: &'a Path,
    pub force: bool,
    pub user: &'a str,
    pub timezone: &'a str,
    /// The adapter installed by `mise run agent`, when found: written as `[agent] command`.
    pub adapter: Option<&'a Path>,
}

#[derive(Debug)]
pub struct Created {
    pub config: PathBuf,
    pub token_file: PathBuf,
    pub password_file: PathBuf,
    pub password: String,
}

/// The system's IANA time zone when it is one Fronde accepts, else `UTC`.
pub fn system_timezone() -> String {
    iana_time_zone::get_timezone()
        .ok()
        .filter(|z| z.parse::<chrono_tz::Tz>().is_ok())
        .unwrap_or_else(|| "UTC".to_string())
}

/// 32 random bytes as 64 hex characters.
pub fn random_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness available: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Four groups of five characters (100 bits), such as `kx4m2-9qrtv-…`.
pub fn random_passphrase() -> anyhow::Result<String> {
    let mut bytes = [0u8; 20];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness available: {e}"))?;
    let chars: Vec<char> = bytes
        .iter()
        .map(|b| ALPHABET[usize::from(b & 31)] as char)
        .collect();
    Ok(chars
        .chunks(5)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-"))
}

fn quoted(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// The configuration text; the secret files sit next to it, so their paths are relative.
pub fn render_config(user: &str, timezone: &str, adapter: Option<&Path>) -> String {
    let command = quoted(
        &adapter
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "claude-agent-acp".to_string()),
    );
    let user = quoted(user);
    let timezone = quoted(timezone);
    let calendar = quoted(&fronde_hub::config::shipped_calendar().map_or_else(
        || "/path/to/fronde/components/dist/calendar.wasm".to_string(),
        |p| p.display().to_string(),
    ));
    format!(
        r#"# Fronde configuration, written by `fronde init`. Secrets are never written here:
# use {{ file = "…" }} (relative to this file's directory) or {{ env = "…" }}.

[server]
listen = "127.0.0.1:8484"
# Bearer token required on /mcp (at least 32 characters).
token = {{ file = "fronde_token" }}

[data]
# dir is left out: Fronde keeps its files in $XDG_DATA_HOME/fronde, or ~/.local/share/fronde.
# Set it to use another directory you own.
# dir = "/home/you/fronde-data"
timezone = {timezone}
user = {user}

# The chat at /, behind a password.
[web]
password = {{ file = "fronde_password" }}
# Set to true when the page is served over https.
secure_cookie = false

# The agent Fronde runs (ACP over stdio), started on the first message.
[agent]
command = [{command}]
env = {{}}
builtin_tools = ["WebSearch", "WebFetch"]

# Optional: where tools come from besides Fronde's own, 0..n. A calendar component (CalDAV
# events and reminders): the one shipped with Fronde needs no hashes (`fronde check` says
# "approved: shipped with this build"); another needs the sha256 and manifest_sha256 it prints.
# [[sources]]
# kind = "component"
# name = "calendar"
# path = {calendar}
# allow = ["https://cloud.example"]
# [sources.settings]
# events_url = "https://cloud.example/remote.php/dav/calendars/you/personal/"
# reminders_url = "https://cloud.example/remote.php/dav/calendars/you/tasks/"
# [[sources.credentials]]
# origin = "https://cloud.example"
# basic = {{ username = "you", password = {{ env = "FRONDE_CALDAV_PASSWORD" }} }}

# An upstream MCP server: exactly one of url or command.
# [[sources]]
# kind = "mcp"
# name = "mcphub"
# url = "https://mcphub.example/mcp"
# token = {{ env = "MCPHUB_TOKEN" }}
"#
    )
}

/// Write the configuration, `fronde_token` and `fronde_password` (all 0600) in the config's
/// directory. Refuses to replace an existing file unless `force`.
pub fn init(opts: &Init) -> anyhow::Result<Created> {
    let dir = match opts.config.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let token_file = dir.join("fronde_token");
    let password_file = dir.join("fronde_password");
    if !opts.force {
        for f in [opts.config, &token_file, &password_file] {
            if f.exists() {
                bail!("{} already exists; pass --force to replace it", f.display());
            }
        }
    }
    fronde_hub::private::create_dir_all(dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;
    let password = random_passphrase()?;
    let write = |path: &Path, content: &str| {
        fronde_hub::private::write(path, content)
            .with_context(|| format!("cannot write {}", path.display()))
    };
    write(&token_file, &random_token()?)?;
    write(&password_file, &password)?;
    write(
        opts.config,
        &render_config(opts.user, opts.timezone, opts.adapter),
    )?;
    Ok(Created {
        config: opts.config.to_path_buf(),
        token_file,
        password_file,
        password,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn run(dir: &Path, force: bool) -> anyhow::Result<Created> {
        init(&Init {
            config: &dir.join("fronde.toml"),
            force,
            user: "al\"ex",
            timezone: "Europe/Paris",
            adapter: None,
        })
    }

    #[test]
    fn writes_private_secrets_and_a_loadable_config() {
        use secrecy::{ExposeSecret, SecretString};
        let t = tempfile::tempdir().unwrap();
        let c = run(t.path(), false).unwrap();
        for f in [&c.config, &c.token_file, &c.password_file] {
            assert_eq!(mode(f), 0o600, "{}", f.display());
        }
        let token = std::fs::read_to_string(&c.token_file).unwrap();
        assert_eq!(token.len(), 64);
        crate::check_token(&SecretString::from(token.clone())).unwrap();
        assert_eq!(
            std::fs::read_to_string(&c.password_file).unwrap(),
            c.password
        );
        assert_eq!(c.password.len(), 23);

        let config = fronde_hub::config::load(&c.config).unwrap();
        assert_eq!(config.data.user, "al\"ex");
        assert_eq!(config.server.listen.to_string(), "127.0.0.1:8484");
        assert_eq!(
            config.server.token.resolve().unwrap().expose_secret(),
            token
        );
        assert_eq!(
            config
                .web
                .unwrap()
                .password
                .resolve()
                .unwrap()
                .expose_secret(),
            c.password
        );
        assert!(config.data.dir.ends_with("fronde"));
    }

    #[test]
    fn refuses_to_overwrite_unless_forced() {
        let t = tempfile::tempdir().unwrap();
        let first = run(t.path(), false).unwrap();
        let e = run(t.path(), false).unwrap_err().to_string();
        assert!(e.contains("already exists") && e.contains("--force"), "{e}");
        assert_eq!(
            std::fs::read_to_string(&first.password_file).unwrap(),
            first.password
        );
        let second = run(t.path(), true).unwrap();
        assert_ne!(first.password, second.password);
    }

    #[test]
    fn writes_the_adapter_path_when_given() {
        let text = render_config(
            "a",
            "UTC",
            Some(Path::new("/r/agent/node_modules/.bin/claude-agent-acp")),
        );
        assert!(text.contains(r#"command = ["/r/agent/node_modules/.bin/claude-agent-acp"]"#));
        assert!(render_config("a", "UTC", None).contains(r#"command = ["claude-agent-acp"]"#));
    }

    #[test]
    fn the_calendar_entry_needs_no_hashes() {
        let text = render_config("a", "UTC", None);
        let start = text.find("# [[sources]]\n# kind = \"component\"").unwrap();
        let entry: Vec<&str> = text[start..]
            .lines()
            .take_while(|l| l.starts_with("# "))
            .map(|l| &l[2..])
            .collect();
        let entry = entry.join("\n");
        assert!(!entry.contains("sha256"), "{entry}");
        let table: toml::Table = toml::from_str(&entry).unwrap();
        let source = &table["sources"].as_array().unwrap()[0];
        assert!(
            source["path"].as_str().unwrap().ends_with("calendar.wasm"),
            "{entry}"
        );
    }

    #[test]
    fn system_timezone_is_valid() {
        assert!(system_timezone().parse::<chrono_tz::Tz>().is_ok());
    }
}
