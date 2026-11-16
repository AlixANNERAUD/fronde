//! Configuration: one strict TOML file, secrets referenced by file or environment variable.

use anyhow::{Context, bail};
use serde::Deserialize;
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
}

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
        Self { memory_limit: 4000, user_limit: 2000, git: true }
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

impl Secret {
    pub fn resolve(&self) -> anyhow::Result<String> {
        match self {
            Secret::File { file } => {
                let s = std::fs::read_to_string(file)
                    .with_context(|| format!("cannot read secret file {}", file.display()))?;
                Ok(s.trim_end_matches(['\n', '\r']).to_string())
            }
            Secret::Env { env } => std::env::var(env)
                .with_context(|| format!("environment variable {env} is not set; set it or use {{ file = \"…\" }}")),
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
    let config: Config =
        toml::from_str(&text).with_context(|| format!("invalid config {}", path.display()))?;
    for m in &config.mcp {
        if !is_valid_name(&m.name) {
            bail!("mcp name {:?} must match ^[a-z][a-z0-9_]{{0,63}}$", m.name);
        }
        if m.url.is_some() == m.command.is_some() {
            bail!("mcp {:?}: set exactly one of url or command", m.name);
        }
    }
    Ok(config)
}
