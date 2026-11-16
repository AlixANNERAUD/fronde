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
    pub calendar: Option<CalendarConfig>,
    #[serde(default)]
    pub mcp: Vec<UpstreamConfig>,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalendarConfig {
    pub events_url: Url,
    pub reminders_url: Url,
    pub username: String,
    pub password: Secret,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    pub name: String,
    pub url: Option<Url>,
    pub command: Option<Vec<String>>,
    pub token: Option<Secret>,
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
    if let Some(c) = &mut config.calendar {
        c.password.anchor(base);
    }
    for m in &mut config.mcp {
        if let Some(t) = &mut m.token {
            t.anchor(base);
        }
    }
    let mut seen = std::collections::HashSet::new();
    for m in &config.mcp {
        if m.name == "hub" {
            bail!("mcp name \"hub\" is reserved for the hub's own tools; pick another name");
        }
        if !seen.insert(&m.name) {
            bail!("mcp name {:?} is used twice; names must be unique", m.name);
        }
        if !is_valid_name(&m.name) {
            bail!("mcp name {:?} must match ^[a-z][a-z0-9_]{{0,63}}$", m.name);
        }
        if m.url.is_some() == m.command.is_some() {
            bail!("mcp {:?}: set exactly one of url or command", m.name);
        }
    }
    for tool in &config.agent.builtin_tools {
        if !SAFE_BUILTIN_TOOLS.contains(&tool.as_str()) {
            bail!(
                "agent.builtin_tools: {tool:?} is not allowed; use only {SAFE_BUILTIN_TOOLS:?}, or [] for none"
            );
        }
    }
    Ok(config)
}
