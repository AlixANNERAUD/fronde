//! Configuration: one strict TOML file, secrets referenced by file or environment variable.

use anyhow::{Context, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use url::Url;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub data: DataConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    /// Where tools come from besides the hub's own: upstream MCP servers and components.
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
    /// The chat page; without it the server is only the hub.
    pub web: Option<WebConfig>,
    #[serde(default)]
    pub agent: AgentConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    pub password: Secret,
    /// Mark the session cookie `Secure`; turn on when the page is served over https.
    #[serde(default)]
    pub secure_cookie: bool,
}

/// The coding agent Fronde talks to over ACP.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgentConfig {
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Claude Code's own tools the agent gets besides the hub's; empty for none.
    pub builtin_tools: Vec<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            command: vec!["claude-agent-acp".to_string()],
            env: BTreeMap::new(),
            builtin_tools: SAFE_BUILTIN_TOOLS.map(str::to_string).to_vec(),
        }
    }
}

/// Built-in tools an assistant with no shell and no files may use: they read the web and touch
/// nothing on the machine. Widening this list is a deliberate decision.
pub const SAFE_BUILTIN_TOOLS: [&str; 2] = ["WebSearch", "WebFetch"];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    pub token: Secret,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataConfig {
    /// Where Fronde keeps its files. Left out, [`default_data_dir`]; `load` fills it in.
    #[serde(default)]
    pub dir: PathBuf,
    pub timezone: chrono_tz::Tz,
    #[serde(default = "default_user")]
    pub user: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MemoryConfig {
    pub memory_limit: usize,
    pub user_limit: usize,
    pub git: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            memory_limit: 4000,
            user_limit: 2000,
            git: true,
        }
    }
}

/// One `[[sources]]` entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SourceConfig {
    /// An upstream MCP server, whose tools become `{name}_{tool}`.
    Mcp(UpstreamConfig),
    /// A tool component (`.wasm`), run in the sandbox with what this entry grants it.
    Component(ComponentConfig),
}

impl SourceConfig {
    pub fn name(&self) -> &str {
        match self {
            Self::Mcp(m) => &m.name,
            Self::Component(c) => &c.name,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    pub name: String,
    pub url: Option<Url>,
    pub command: Option<Vec<String>>,
    pub token: Option<Secret>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentConfig {
    pub name: String,
    /// The component's `.wasm` file.
    pub path: PathBuf,
    /// Its manifest; by default the `.toml` file beside `path` with the same name.
    pub manifest: Option<PathBuf>,
    /// SHA-256 of the `.wasm` file, as `fronde check` prints it: approves this build.
    pub sha256: Option<String>,
    /// SHA-256 of the manifest: approves what the component asks for.
    pub manifest_sha256: Option<String>,
    /// Origins the component may reach, each one its manifest asks for.
    #[serde(default)]
    pub allow: Vec<Allow>,
    /// What the component reads through `settings.get`, checked against its manifest; never
    /// secrets.
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
    /// Secrets the host adds to requests for an allowed origin.
    #[serde(default)]
    pub credentials: Vec<CredentialConfig>,
}

impl ComponentConfig {
    /// The manifest's path: `manifest`, or `path` with a `.toml` extension.
    pub fn manifest_path(&self) -> PathBuf {
        self.manifest
            .clone()
            .unwrap_or_else(|| self.path.with_extension("toml"))
    }
}

/// An origin a component may reach: `"https://cloud.example"`, or
/// `{ origin = "http://192.168.1.10:5232", local = true }` for a server on the host or its
/// network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allow {
    /// `scheme://host[:port]`, with no path.
    pub origin: Url,
    /// Whether it may resolve to a local address (loopback, private, link-local).
    pub local: bool,
}

impl<'de> Deserialize<'de> for Allow {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::{self, MapAccess, Visitor, value::MapAccessDeserializer};

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Table {
            origin: String,
            #[serde(default)]
            local: bool,
        }

        struct AllowVisitor;

        impl<'de> Visitor<'de> for AllowVisitor {
            type Value = Allow;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(
                    "an origin such as \"https://cloud.example\", or { origin = \"…\", local = true }",
                )
            }

            fn visit_str<E: de::Error>(self, s: &str) -> Result<Allow, E> {
                Ok(Allow {
                    origin: parse_origin(s).map_err(E::custom)?,
                    local: false,
                })
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Allow, A::Error> {
                let t = Table::deserialize(MapAccessDeserializer::new(map))?;
                Ok(Allow {
                    origin: parse_origin(&t.origin).map_err(de::Error::custom)?,
                    local: t.local,
                })
            }
        }

        d.deserialize_any(AllowVisitor)
    }
}

/// A secret the host sends to one origin: exactly one of `basic` or `bearer`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialConfig {
    #[serde(deserialize_with = "origin")]
    pub origin: Url,
    pub basic: Option<BasicCredential>,
    pub bearer: Option<Secret>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BasicCredential {
    pub username: String,
    pub password: Secret,
}

/// `scheme://host[:port]` over http or https, with nothing else.
pub fn parse_origin(s: &str) -> anyhow::Result<Url> {
    let url = Url::parse(s).with_context(|| format!("origin {s:?} is not a URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("origin {s:?}: the scheme must be http or https");
    }
    if url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("origin {s:?} must be scheme://host[:port], with no path or user");
    }
    Ok(url)
}

fn origin<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Url, D::Error> {
    let s = String::deserialize(d)?;
    parse_origin(&s).map_err(serde::de::Error::custom)
}

/// A secret is never written in the TOML: it points to a file or an environment variable.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Secret {
    File { file: PathBuf },
    Env { env: String },
}

/// Per-user data directory used when `[data] dir` is not set: `$XDG_DATA_HOME/fronde`, else
/// `~/.local/share/fronde` on Linux and other Unixes, the platform's equivalent on macOS and
/// Windows.
pub fn default_data_dir() -> Option<PathBuf> {
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        xdg_data_dir(
            std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
            etcetera::home_dir().ok(),
        )
    }
    #[cfg(any(target_os = "macos", windows))]
    {
        use etcetera::BaseStrategy as _;
        let dir = etcetera::base_strategy::choose_native_strategy()
            .ok()
            .filter(|d| d.home_dir().is_absolute())?
            .data_dir()
            .join("fronde");
        // Same layout as before the switch from `directories`: %APPDATA%\fronde\data on Windows.
        #[cfg(windows)]
        let dir = dir.join("data");
        Some(dir)
    }
}

/// The XDG rule: an absolute `XDG_DATA_HOME` wins, then `HOME/.local/share` (an absolute home only).
#[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
pub fn xdg_data_dir(xdg_data_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    let base = match xdg_data_home {
        Some(p) if p.is_absolute() => p,
        _ => home.filter(|h| h.is_absolute())?.join(".local/share"),
    };
    Some(base.join("fronde"))
}

impl Secret {
    /// Make a relative secret file path relative to `base`, the config file's directory, so it
    /// does not depend on the directory Fronde was started from.
    fn anchor(&mut self, base: &Path) {
        if let Secret::File { file } = self
            && file.is_relative()
        {
            *file = base.join(&*file);
        }
    }

    pub fn resolve(&self) -> anyhow::Result<String> {
        match self {
            Secret::File { file } => {
                crate::private::check_secret_file(file)?;
                let s = std::fs::read_to_string(file)
                    .with_context(|| format!("cannot read secret file {}", file.display()))?;
                Ok(s.trim_end_matches(['\n', '\r']).to_string())
            }
            Secret::Env { env } => std::env::var(env).with_context(|| {
                format!("environment variable {env} is not set; set it or use {{ file = \"…\" }}")
            }),
        }
    }
}

fn default_listen() -> SocketAddr {
    "127.0.0.1:8484".parse().expect("valid default address")
}

fn default_user() -> String {
    "me".to_string()
}

/// True for names usable as tool prefixes: `^[a-z][a-z0-9_]{0,63}$`.
pub fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z'))
        && name.len() <= 64
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_'))
}

pub fn load(path: &Path) -> anyhow::Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read config {}", path.display()))?;
    replaced_sections(&text).with_context(|| format!("invalid config {}", path.display()))?;
    let mut config: Config =
        toml::from_str(&text).with_context(|| format!("invalid config {}", path.display()))?;
    if config.data.dir.as_os_str().is_empty() {
        config.data.dir = default_data_dir().context(
            "cannot work out the default data directory (no home directory); set [data] dir",
        )?;
    }
    // Relative paths in the file are read from its directory, made absolute so that every path
    // derived from them (agent cwd, memory, database) is absolute too.
    let abs = std::path::absolute(path)
        .with_context(|| format!("cannot resolve config path {}", path.display()))?;
    let base: PathBuf = abs
        .parent()
        .unwrap_or(Path::new("/"))
        .components()
        .collect();
    let base = base.as_path();
    if config.data.dir.is_relative() {
        config.data.dir = base.join(&config.data.dir).components().collect();
    }
    config.server.token.anchor(base);
    if let Some(web) = &mut config.web {
        web.password.anchor(base);
    }
    for source in &mut config.sources {
        match source {
            SourceConfig::Mcp(m) => {
                if let Some(t) = &mut m.token {
                    t.anchor(base);
                }
            }
            SourceConfig::Component(c) => {
                c.path = base.join(&c.path);
                if let Some(m) = &mut c.manifest {
                    *m = base.join(&*m);
                }
                for credential in &mut c.credentials {
                    if let Some(b) = &mut credential.basic {
                        b.password.anchor(base);
                    }
                    if let Some(t) = &mut credential.bearer {
                        t.anchor(base);
                    }
                }
            }
        }
    }
    check_sources(&config.sources)?;
    for tool in &config.agent.builtin_tools {
        if !SAFE_BUILTIN_TOOLS.contains(&tool.as_str()) {
            bail!(
                "agent.builtin_tools: {tool:?} is not allowed; use only {SAFE_BUILTIN_TOOLS:?}, or [] for none"
            );
        }
    }
    Ok(config)
}

/// `[calendar]` and `[[mcp]]` became `[[sources]]` entries: say how, rather than "unknown field".
fn replaced_sections(text: &str) -> anyhow::Result<()> {
    let Ok(table) = text.parse::<toml::Table>() else {
        return Ok(()); // the full parse reports it
    };
    if table.contains_key("calendar") {
        bail!(
            "[calendar] is gone: the calendar is now a component. Replace it with a [[sources]] \
             entry with kind = \"component\" and path = \"…/calendar.wasm\", its URLs under \
             [sources.settings] and the account under [[sources.credentials]]; see \
             fronde.example.toml and the \"Configure the calendar\" guide"
        );
    }
    if table.contains_key("mcp") {
        bail!(
            "[[mcp]] is gone: write each server as a [[sources]] entry with kind = \"mcp\" and \
             the same name, url or command, and token"
        );
    }
    Ok(())
}

fn check_sources(sources: &[SourceConfig]) -> anyhow::Result<()> {
    let mut seen = std::collections::HashSet::new();
    for source in sources {
        let name = source.name();
        if name == "hub" {
            bail!("source name \"hub\" is reserved for the hub's own tools; pick another name");
        }
        if !seen.insert(name) {
            bail!("source name {name:?} is used twice; names must be unique");
        }
        if !is_valid_name(name) {
            bail!("source name {name:?} must match ^[a-z][a-z0-9_]{{0,63}}$");
        }
        match source {
            SourceConfig::Mcp(m) => {
                if m.url.is_some() == m.command.is_some() {
                    bail!("source {name:?}: set exactly one of url or command");
                }
            }
            SourceConfig::Component(c) => check_component(c)?,
        }
    }
    Ok(())
}

fn check_component(c: &ComponentConfig) -> anyhow::Result<()> {
    let name = &c.name;
    for (key, hash) in [
        ("sha256", &c.sha256),
        ("manifest_sha256", &c.manifest_sha256),
    ] {
        if let Some(h) = hash
            && !(h.len() == 64
                && h.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        {
            bail!(
                "source {name:?}: {key} must be 64 lower-case hex digits, as `fronde check` prints it"
            );
        }
    }
    let mut origins = Vec::new();
    for a in &c.allow {
        let origin = a.origin.origin();
        if origins.contains(&origin) {
            bail!(
                "source {name:?}: {} is in allow twice; list each origin once",
                origin.ascii_serialization()
            );
        }
        origins.push(origin);
    }
    let mut credited = Vec::new();
    for credential in &c.credentials {
        let origin = credential.origin.origin();
        let shown = origin.ascii_serialization();
        if !origins.contains(&origin) {
            bail!(
                "source {name:?}: the credential for {shown} is for an origin not in allow; add it to allow or remove the credential"
            );
        }
        if credited.contains(&origin) {
            bail!("source {name:?}: two credentials for {shown}; keep one");
        }
        if credential.basic.is_some() == credential.bearer.is_some() {
            bail!(
                "source {name:?}: the credential for {shown} needs exactly one of basic or bearer"
            );
        }
        credited.push(origin);
    }
    Ok(())
}
