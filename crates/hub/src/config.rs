//! Configuration: one strict TOML file, secrets referenced by file or environment variable.

use crate::origin::{Locality, Origin};
use anyhow::{Context, bail};
use secrecy::SecretString;
use secrecy::zeroize::Zeroize;
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
    /// The `code_run_python` tool; without it there is none.
    pub python: Option<PythonConfig>,
    /// Where the master key of the stored secrets is kept.
    #[serde(default)]
    pub secrets: SecretsConfig,
    /// The file this configuration was read from, absolute; [`load`] sets it.
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl Config {
    /// The file this configuration was read from, when [`load`] read it.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Every `{ env = "…" }` secret's variable: Fronde reads them from its own environment, and
    /// the agent's never holds them.
    pub fn secret_env_names(&self) -> Vec<String> {
        self.secrets()
            .filter_map(|secret| match secret {
                Secret::Env { env } => Some(env.clone()),
                _ => None,
            })
            .collect()
    }

    /// Every `{ file = "…" }` secret's file, the master key file included.
    pub fn secret_files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = self
            .secrets()
            .filter_map(|secret| match secret {
                Secret::File { file } => Some(file.clone()),
                _ => None,
            })
            .collect();
        files.extend(self.secrets.master_key_file.clone());
        files
    }

    fn secrets(&self) -> impl Iterator<Item = &Secret> {
        let sources = self.sources.iter().flat_map(|source| -> Vec<&Secret> {
            match source {
                SourceConfig::Mcp(m) => m.token.iter().collect(),
                SourceConfig::Component(c) => c
                    .credentials
                    .iter()
                    .flat_map(|credential| {
                        credential
                            .basic
                            .as_ref()
                            .map(|b| &b.password)
                            .into_iter()
                            .chain(credential.bearer.as_ref())
                    })
                    .collect(),
            }
        });
        std::iter::once(&self.server.token)
            .chain(self.web.as_ref().map(|web| &web.password))
            .chain(self.secrets.master_key.as_ref())
            .chain(self.agent.oauth_token.as_ref())
            .chain(sources)
    }

    /// The master key file, which [`load`] always sets: `[secrets] master_key`, or `master.key`
    /// beside the configuration file, out of the data directory, so that a backup of the data
    /// directory does not hold the key with the secrets it opens.
    /// A configuration parsed without [`load`] has only the file it names, as written.
    pub fn master_key_file(&self) -> anyhow::Result<&Path> {
        match (&self.secrets.master_key_file, &self.secrets.master_key) {
            (Some(file), _) | (None, Some(Secret::File { file })) => Ok(file),
            _ => bail!(
                "no master key file: this configuration was not read by config::load; set \
                 [secrets] master_key = {{ file = \"…\" }}"
            ),
        }
    }

    /// Where the master key may be kept, as `[secrets]` says: the backend, the key file, the
    /// service's systemd credentials and the platform's keychain for this data directory.
    pub fn master_key_backends(&self) -> anyhow::Result<fronde_db::master_key::Backends> {
        use fronde_db::master_key::{Backend, Backends, Choice, Keychain};
        let choice = match self.secrets.backend {
            KeyBackend::Auto => Choice::Auto,
            KeyBackend::Systemd => Choice::Only(Backend::Systemd),
            KeyBackend::Keychain => Choice::Only(Backend::Keychain),
            KeyBackend::File => Choice::Only(Backend::File),
        };
        let keychain = match &self.secrets.keychain {
            Some(keychain) => keychain.clone(),
            None => Keychain::platform(&self.data.dir),
        };
        Ok(Backends::new(
            choice,
            self.master_key_file()?.to_path_buf(),
            self.secrets.credentials_dir.clone(),
            keychain,
        ))
    }
}

/// `[secrets]`: where the master key that seals stored secrets is kept.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretsConfig {
    /// Which backend holds the key; `auto` by default.
    #[serde(default)]
    pub backend: KeyBackend,
    /// The file backend's key file. `{ file = "…" }` only: never `{ env }`, which every process
    /// Fronde starts outside the agent's helper inherits, nor `{ stored }`.
    master_key: Option<Secret>,
    /// The key file, read from the configuration's directory when relative; [`load`] sets it.
    #[serde(skip)]
    master_key_file: Option<PathBuf>,
    /// `$CREDENTIALS_DIRECTORY`, where systemd puts the service's credentials; [`load`] reads it.
    #[serde(skip)]
    credentials_dir: Option<PathBuf>,
    /// The keychain to use instead of the platform's: tests set a fake one.
    #[serde(skip)]
    keychain: Option<fronde_db::master_key::Keychain>,
}

impl SecretsConfig {
    /// `$CREDENTIALS_DIRECTORY`, when systemd set it.
    pub fn credentials_dir(&self) -> Option<&Path> {
        self.credentials_dir.as_deref()
    }
}

/// Test hooks, behind the `testing` feature that only dev-dependencies enable.
#[cfg(feature = "testing")]
impl SecretsConfig {
    /// Read systemd credentials from `dir` instead of `$CREDENTIALS_DIRECTORY`.
    pub fn set_credentials_dir(&mut self, dir: Option<PathBuf>) {
        self.credentials_dir = dir;
    }

    /// Use `keychain` instead of the platform's.
    pub fn set_keychain(&mut self, keychain: fronde_db::master_key::Keychain) {
        self.keychain = Some(keychain);
    }
}

/// `[secrets] backend`: where the master key is kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyBackend {
    /// For the first key, a systemd credential if the service has one, else the keychain if it
    /// answers, else a file; then always the one that got it.
    #[default]
    Auto,
    Systemd,
    Keychain,
    File,
}

/// `[python]`: where the Python runtime is, as `mise run python` lays it out.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PythonConfig {
    /// Holds `python.wasm` and `lib/`; the image has it at `/app/python`.
    pub dir: PathBuf,
}

/// The name of the source that serves `code_run_python`.
pub const PYTHON_SOURCE: &str = "python";

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
    /// Variables the agent gets besides the few it keeps from Fronde's environment.
    pub env: BTreeMap<String, String>,
    /// Claude Code's own tools the agent gets besides the hub's; empty for none.
    pub builtin_tools: Vec<String>,
    /// Confine the agent with Landlock; it does not start where the kernel cannot. `false` runs
    /// it unconfined, with a warning at each start.
    pub sandbox: bool,
    /// More directories the confined agent may read and run programs from.
    pub read: Vec<PathBuf>,
    /// The token `claude setup-token` printed, given to the agent as `CLAUDE_CODE_OAUTH_TOKEN`
    /// and read again at each start. A stored one must be stored for [`CLAUDE_ORIGIN`].
    pub oauth_token: Option<Secret>,
    /// The caps of the agent's systemd scope, when a systemd user manager runs it.
    pub limits: AgentLimits,
}

/// `[agent.limits]`: what the agent's systemd scope caps; `None` (`"none"`) for no cap of
/// Fronde's. Without a systemd user manager there is no scope, and these do not apply.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgentLimits {
    /// Bytes of memory (`MemoryMax`; `MemoryHigh` is three quarters of it).
    #[serde(deserialize_with = "memory_limit")]
    pub memory: Option<u64>,
    /// CPUs' worth of time (`CPUQuota`, 100 % per CPU).
    #[serde(deserialize_with = "cpus_limit")]
    pub cpus: Option<f64>,
    /// Processes and threads (`TasksMax`).
    #[serde(deserialize_with = "tasks_limit")]
    pub tasks: Option<u64>,
}

impl Default for AgentLimits {
    fn default() -> Self {
        Self {
            memory: Some(2 << 30),
            cpus: Some(2.0),
            tasks: Some(512),
        }
    }
}

/// A limit as TOML writes it: a number, or a string (a size, or `"none"`).
#[derive(Deserialize)]
#[serde(untagged)]
enum RawLimit {
    Integer(i64),
    Float(f64),
    Text(String),
}

fn memory_limit<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    let expected = "memory: expected a size such as \"2G\" or \"512M\", or \"none\"";
    match RawLimit::deserialize(d)? {
        RawLimit::Text(text) if text == "none" => Ok(None),
        RawLimit::Text(text) => parse_size(&text)
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom(format!("{expected}, not {text:?}"))),
        RawLimit::Integer(bytes) if bytes > 0 => Ok(Some(bytes.unsigned_abs())),
        _ => Err(serde::de::Error::custom(expected)),
    }
}

/// A size in bytes, as systemd writes it: a number with an optional `K`, `M`, `G` or `T`
/// (powers of 1024); `None` unless it is above zero.
pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let (number, unit) = match text.char_indices().last()? {
        (at, c) if c.is_ascii_alphabetic() => (&text[..at], c.to_ascii_uppercase()),
        _ => (text, 'B'),
    };
    let shift = match unit {
        'B' => 0,
        'K' => 10,
        'M' => 20,
        'G' => 30,
        'T' => 40,
        _ => return None,
    };
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let bytes = number.parse::<f64>().ok()? * 2f64.powi(shift);
    // Below 2^64, so the cast is exact enough and never saturates.
    (1.0..1.8e19).contains(&bytes).then_some(bytes as u64)
}

fn cpus_limit<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    let expected = "cpus: expected a number of CPUs above 0, such as 2 or 0.5, or \"none\"";
    let cpus = match RawLimit::deserialize(d)? {
        RawLimit::Text(text) if text == "none" => return Ok(None),
        RawLimit::Integer(cpus) => cpus as f64,
        RawLimit::Float(cpus) => cpus,
        RawLimit::Text(_) => return Err(serde::de::Error::custom(expected)),
    };
    // systemd counts in percent: at least 1 % of one CPU.
    if (0.01..=100_000.0).contains(&cpus) {
        Ok(Some(cpus))
    } else {
        Err(serde::de::Error::custom(expected))
    }
}

fn tasks_limit<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    let expected = "tasks: expected a number of processes and threads above 0, or \"none\"";
    match RawLimit::deserialize(d)? {
        RawLimit::Text(text) if text == "none" => Ok(None),
        RawLimit::Integer(tasks) if tasks > 0 => Ok(Some(tasks.unsigned_abs())),
        _ => Err(serde::de::Error::custom(expected)),
    }
}

/// The origin a stored `[agent] oauth_token` is stored for: Claude's API, which it opens.
pub const CLAUDE_ORIGIN: &str = "https://api.anthropic.com";

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            command: vec!["claude-agent-acp".to_string()],
            env: BTreeMap::new(),
            builtin_tools: SAFE_BUILTIN_TOOLS.map(str::to_string).to_vec(),
            sandbox: true,
            read: Vec::new(),
            oauth_token: None,
            limits: AgentLimits::default(),
        }
    }
}

/// Variables `[agent] env` may not set: Fronde sets the agent's home and temporary directory,
/// and the others point at the owner's session, its buses and the service's credentials.
fn reserved_agent_variable(name: &str) -> bool {
    matches!(name, "HOME" | "TMPDIR" | "CREDENTIALS_DIRECTORY")
        || name.starts_with("XDG_")
        || name.starts_with("DBUS_")
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
    pub origin: Origin,
    /// Whether it may resolve to a local address (loopback, private, link-local).
    pub local: bool,
}

impl<'de> Deserialize<'de> for Allow {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::{self, MapAccess, Visitor, value::MapAccessDeserializer};

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Table {
            origin: Origin,
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
                    origin: Origin::parse(s).map_err(E::custom)?,
                    local: false,
                })
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Allow, A::Error> {
                let t = Table::deserialize(MapAccessDeserializer::new(map))?;
                Ok(Allow {
                    origin: t.origin,
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
    pub origin: Origin,
    pub basic: Option<BasicCredential>,
    pub bearer: Option<Secret>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BasicCredential {
    pub username: String,
    pub password: Secret,
}

/// A secret is never written in the TOML: it points to a file, an environment variable, or, in a
/// component's credentials only, a secret stored encrypted in `fronde.db`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Secret {
    File {
        file: PathBuf,
    },
    Env {
        env: String,
    },
    /// The secret `fronde secret set` stored under this name. Only the preparation of the
    /// component whose credential names it opens it (`fronde_wasm::prepare`), so
    /// [`Secret::resolve`] refuses it.
    Stored {
        stored: String,
    },
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

    /// Refuse a stored secret where only a file or an environment variable may be, saying so
    /// for `place`.
    fn not_stored(&self, place: &str) -> anyhow::Result<()> {
        if let Secret::Stored { stored } = self {
            bail!(
                "{place} = {{ stored = {stored:?} }}: a stored secret can only be a component's \
                 credential, in [[sources.credentials]]; use {{ file = \"…\" }} or \
                 {{ env = \"…\" }} here"
            );
        }
        Ok(())
    }

    /// The value of a file or environment secret. A stored secret is refused: only the
    /// preparation of the component whose credential names it opens it.
    pub fn resolve(&self) -> anyhow::Result<SecretString> {
        match self {
            Secret::Stored { stored } => bail!(
                "the stored secret {stored} is opened only when the component whose credential \
                 names it is prepared"
            ),
            Secret::File { file } => {
                crate::private::check_secret_file(file)?;
                let mut s = std::fs::read_to_string(file)
                    .with_context(|| format!("cannot read secret file {}", file.display()))?;
                let trimmed = s.trim_end_matches(['\n', '\r']).to_owned();
                s.zeroize();
                Ok(SecretString::from(trimmed))
            }
            Secret::Env { env } => {
                // `std::env::var` would fail with `NotUnicode`, which prints the value.
                let Some(value) = std::env::var_os(env) else {
                    bail!(
                        "environment variable {env} is not set; set it or use {{ file = \"…\" }}"
                    );
                };
                // The value is left out of the message: it stays in the environment anyway.
                let value = value.into_string().map_err(|_| {
                    anyhow::anyhow!(
                        "environment variable {env} must be valid UTF-8; fix its value or use \
                         {{ file = \"…\" }}"
                    )
                })?;
                Ok(SecretString::from(value))
            }
        }
    }
}

fn default_listen() -> SocketAddr {
    "127.0.0.1:8484".parse().expect("valid default address")
}

fn default_user() -> String {
    "me".to_string()
}

/// Prefixes of the hub's own tools (`search_tools`, `use_tool`, `memory_*`, `code_run_python`,
/// `user_*`) and of its source name: no component may declare them.
pub const RESERVED_TOOL_PREFIXES: [&str; 6] =
    ["code_", "hub_", "memory_", "search_", "use_", "user_"];

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
    config.server.token.not_stored("server.token")?;
    if let Some(web) = &mut config.web {
        web.password.anchor(base);
        web.password.not_stored("web.password")?;
    }
    for source in &mut config.sources {
        match source {
            SourceConfig::Mcp(m) => {
                if let Some(t) = &mut m.token {
                    t.anchor(base);
                    t.not_stored(&format!("source {:?}: token", m.name))?;
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
    config.secrets.master_key_file = Some(match &config.secrets.master_key {
        None => base.join(fronde_db::master_key::FILE_NAME),
        Some(Secret::File { file }) => base.join(file).components().collect(),
        Some(_) => bail!(
            "[secrets] master_key must be {{ file = \"…\" }}: the master key never comes from \
             the environment, which the processes Fronde starts may inherit, nor from the store it \
             opens"
        ),
    });
    // Set by systemd, for this process only, when the unit loads credentials.
    config.secrets.credentials_dir = std::env::var_os("CREDENTIALS_DIRECTORY")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from);
    if let Some(python) = &mut config.python {
        python.dir = base.join(&python.dir).components().collect();
        if config.sources.iter().any(|s| s.name() == PYTHON_SOURCE) {
            bail!(
                "source name \"{PYTHON_SOURCE}\" is taken by [python]'s code_run_python; pick another name"
            );
        }
    }
    check_sources(&config.sources)?;
    for name in config.agent.env.keys() {
        if reserved_agent_variable(name) {
            bail!(
                "[agent] env: {name} cannot be set here: Fronde gives the agent its own HOME and \
                 TMPDIR in the data directory, and never passes XDG_*, DBUS_* or \
                 CREDENTIALS_DIRECTORY, which point at your session; remove it"
            );
        }
    }
    for dir in &mut config.agent.read {
        *dir = base.join(&*dir).components().collect();
    }
    if let Some(token) = &mut config.agent.oauth_token {
        token.anchor(base);
        if let Secret::Stored { stored } = token {
            fronde_db::secrets::check_name(stored).context("[agent] oauth_token")?;
        }
        if config.agent.env.contains_key("CLAUDE_CODE_OAUTH_TOKEN") {
            bail!(
                "[agent] env sets CLAUDE_CODE_OAUTH_TOKEN and [agent] oauth_token gives it too; \
                 keep oauth_token, and never write the token in fronde.toml"
            );
        }
    }
    config.path = Some(base.join(abs.file_name().unwrap_or_default()));
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
    if let Some(old) = table.get("calendar") {
        bail!(
            "[calendar] is gone: the calendar is now a component. Replace the section with:\n\n\
             {}\n\
             The calendar shipped with this Fronde needs no hashes: `fronde check` says \
             \"approved: shipped with this build\". A calendar.wasm from elsewhere needs the \
             sha256 and manifest_sha256 that `fronde check` prints, added to the entry once you \
             have read its manifest. See the \"Configure the calendar\" guide.",
            calendar_source(old)
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

/// The `[[sources]]` entry that replaces an old `[calendar]` section, with its values.
fn calendar_source(old: &toml::Value) -> String {
    let text = |key: &str, default: &str| {
        old.get(key)
            .and_then(toml::Value::as_str)
            .unwrap_or(default)
            .to_string()
    };
    let quoted = |s: &str| toml::Value::String(s.to_string()).to_string();
    let events = text(
        "events_url",
        "https://cloud.example/remote.php/dav/calendars/you/personal/",
    );
    let reminders = text(
        "reminders_url",
        "https://cloud.example/remote.php/dav/calendars/you/tasks/",
    );
    let username = text("username", "you");
    let password = old.get("password").map_or_else(
        || "{ env = \"FRONDE_CALDAV_PASSWORD\" }".to_string(),
        toml::Value::to_string,
    );
    let mut origins: Vec<Origin> = Vec::new();
    for url in [&events, &reminders] {
        if let Ok(url) = Url::parse(url)
            && let Ok(origin) = Origin::of_url(&url)
            && !origins.contains(&origin)
        {
            origins.push(origin);
        }
    }
    let allow: Vec<String> = origins
        .iter()
        .map(|o| match o.locality() {
            Locality::Public => quoted(&o.to_string()),
            Locality::Loopback | Locality::Local => {
                format!("{{ origin = {}, local = true }}", quoted(&o.to_string()))
            }
        })
        .collect();
    let mut entry = format!(
        "[[sources]]\n\
         kind = \"component\"\n\
         name = \"calendar\"\n\
         path = {}\n\
         # The origins of both URLs; a server on this machine or its network: {{ origin = \"…\", local = true }}\n\
         allow = [{}]\n\
         [sources.settings]\n\
         events_url = {}\n\
         reminders_url = {}\n",
        quoted(&shipped_calendar().map_or_else(
            || "/app/components/calendar.wasm".to_string(),
            |p| p.display().to_string()
        )),
        allow.join(", "),
        quoted(&events),
        quoted(&reminders),
    );
    for origin in &origins {
        if !origin.carries_credentials() {
            entry.push_str(
                "# Plain http: Fronde sends the password only over https or to this machine (in \
                 Docker, the container). Serve CalDAV over https.\n",
            );
        }
        entry.push_str(&format!(
            "[[sources.credentials]]\n\
             origin = {}\n\
             basic = {{ username = {}, password = {password} }}\n",
            quoted(&origin.to_string()),
            quoted(&username),
        ));
    }
    entry
}

/// Where the shipped calendar component is: beside the binary as in the image
/// (`/app/components`), or in `components/dist` of the checkout this binary was built from.
pub fn shipped_calendar() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok();
    let candidates = [
        exe.as_deref()
            .and_then(Path::parent)
            .map(|d| d.join("components/calendar.wasm")),
        Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../components/dist/calendar.wasm")),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|p| p.is_file())
        .and_then(|p| p.canonicalize().ok())
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
                let prefix = format!("{name}_");
                if let Some(reserved) = RESERVED_TOOL_PREFIXES
                    .iter()
                    .find(|r| prefix.starts_with(**r) || r.starts_with(&prefix))
                {
                    bail!(
                        "source name {name:?} is reserved for the hub's own tools ({reserved}…); pick another name"
                    );
                }
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
        let origin = &a.origin;
        if origins.contains(&origin) {
            bail!("source {name:?}: {origin} is in allow twice; list each origin once");
        }
        origins.push(origin);
    }
    let mut credited = Vec::new();
    for credential in &c.credentials {
        let origin = &credential.origin;
        if !origins.contains(&origin) {
            bail!(
                "source {name:?}: the credential for {origin} is for an origin not in allow; add it to allow or remove the credential"
            );
        }
        if credited.contains(&origin) {
            bail!("source {name:?}: two credentials for {origin}; keep one");
        }
        if credential.basic.is_some() == credential.bearer.is_some() {
            bail!(
                "source {name:?}: the credential for {origin} needs exactly one of basic or bearer"
            );
        }
        let secret = match (&credential.basic, &credential.bearer) {
            (Some(b), _) => &b.password,
            (_, Some(t)) => t,
            (None, None) => unreachable!("checked above"),
        };
        if let Secret::Stored { stored } = secret {
            fronde_db::secrets::check_name(stored)
                .with_context(|| format!("source {name:?}: the credential for {origin}"))?;
        }
        credited.push(origin);
    }
    Ok(())
}
