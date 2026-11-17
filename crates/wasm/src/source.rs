//! A `[[sources]]` entry of kind `component`: checked against the component's manifest and its
//! approval, then loaded as a source of tools.
//!
//! The manifest asks; the configuration grants at most what it asks: each allowed origin and
//! each credential must match a request of the manifest, and the settings must fit its schema.
//! The approval is the SHA-256 of both files: either the hashes written in the entry, or, for an
//! entry without them, those of a component shipped with this build of Fronde. A changed manifest
//! or build is not loaded until it is approved again. A path never approves anything.

use crate::egress::{Allowed, Credential, Grants};
use crate::manifest::{Injection, Manifest, NetworkRule, World};
use crate::{ComponentHost, ComponentSpec, LoadedComponent};
use anyhow::{Context, bail, ensure};
use async_trait::async_trait;
use fronde_hub::config::{ComponentConfig, Config, RESERVED_TOOL_PREFIXES, SourceConfig};
use fronde_hub::origin::{Origin, is_local};
use fronde_hub::source::{Session, ToolSource};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

/// `(name, sha256, manifest_sha256)` of each component in `components/dist` when this build was
/// compiled (`build.rs`); empty if there was none.
const SHIPPED: &[(&str, &str, &str)] = include!(concat!(env!("OUT_DIR"), "/shipped.rs"));

/// Lower-case hex SHA-256.
pub(crate) fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn sha256_of(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(sha256(&bytes))
}

/// The current SHA-256 of a component's files, which its entry's `sha256` and `manifest_sha256`
/// must match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hashes {
    pub sha256: String,
    pub manifest_sha256: String,
}

impl Hashes {
    pub fn of(config: &ComponentConfig) -> anyhow::Result<Self> {
        Ok(Self {
            sha256: sha256_of(&config.path)?,
            manifest_sha256: sha256_of(&config.manifest_path())?,
        })
    }
}

/// A component built from this repository and shipped with this build of Fronde: its files are
/// approved by their hashes alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shipped {
    pub name: String,
    pub sha256: String,
    pub manifest_sha256: String,
}

/// The components shipped with this build: those of `components/dist` when it was compiled.
pub fn shipped() -> Vec<Shipped> {
    SHIPPED
        .iter()
        .map(|(name, sha256, manifest_sha256)| Shipped {
            name: name.to_string(),
            sha256: sha256.to_string(),
            manifest_sha256: manifest_sha256.to_string(),
        })
        .collect()
}

/// What approved a component's files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    /// The `sha256` and `manifest_sha256` of its entry.
    Pinned,
    /// No hashes in its entry, and the files of this shipped component.
    Shipped { name: String },
}

impl fmt::Display for Approval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pinned => f.write_str("approved: pinned by the hashes in its entry"),
            Self::Shipped { name } => write!(f, "approved: shipped with this build ({name})"),
        }
    }
}

/// What a component entry is checked against besides its own files.
#[derive(Debug, Clone, Default)]
pub struct Checks {
    /// The user's time zone, given to a component that declares a `timezone` setting left unset.
    pub timezone: String,
    /// Components approved without hashes in their entry.
    pub shipped: Vec<Shipped>,
    /// Every `[[sources]]` entry: a component's tool prefixes must stay clear of the others.
    pub sources: Vec<SourceConfig>,
}

impl Checks {
    /// For the entries of `config`, with the components shipped with this build.
    pub fn of(config: &Config) -> Self {
        Self {
            timezone: config.data.timezone.name().to_string(),
            shipped: shipped(),
            sources: config.sources.clone(),
        }
    }
}

/// A component entry ready to load, and what approved it.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub spec: ComponentSpec,
    pub approval: Approval,
}

/// Check `config` against the component's approval and manifest and against `checks`, resolve
/// its credentials, and return what to load.
pub fn prepare(config: &ComponentConfig, checks: &Checks) -> anyhow::Result<Prepared> {
    let name = &config.name;
    let manifest_path = config.manifest_path();
    // The manifest parsed is the text hashed; the .wasm is hashed again as it is compiled.
    let text = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("cannot read {}", manifest_path.display()))?;
    let current = Hashes {
        sha256: sha256_of(&config.path)?,
        manifest_sha256: sha256(text.as_bytes()),
    };
    let approval = approve(config, &current, &manifest_path, &checks.shipped)?;
    let manifest = Manifest::parse(&text)
        .with_context(|| format!("invalid manifest {}", manifest_path.display()))?;
    let World::Tool = manifest.world;
    check_namespace(config, &manifest.tool_prefixes, &checks.sources)?;
    let timezone = &checks.timezone;

    let mut settings = config.settings.clone();
    let properties = manifest
        .settings
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if properties.contains_key("timezone") {
        settings
            .entry("timezone".to_string())
            .or_insert_with(|| timezone.to_string());
    }
    check_settings(&manifest.settings, &properties, &settings).context("settings")?;

    let mut allow = Vec::new();
    for a in &config.allow {
        let origin = a.origin.clone();
        ensure!(
            manifest
                .network
                .iter()
                .any(|r| covers(r, &origin, &settings)),
            "allow has {origin}, which the manifest does not ask for (it asks for {})",
            asked(&manifest.network, &settings)
        );
        if !a.local && origin.ip().is_some_and(is_local) {
            bail!(
                "allow has {origin}, a local address, which is refused unless allowed as local: \
                 write it as {{ origin = \"{origin}\", local = true }}"
            );
        }
        allow.push(Allowed {
            origin,
            local: a.local,
        });
    }

    let mut credentials = Vec::new();
    for c in &config.credentials {
        let origin = c.origin.clone();
        let (injection, kind) = match (&c.basic, &c.bearer) {
            (Some(_), None) => (Injection::Basic, "basic"),
            (None, Some(_)) => (Injection::Bearer, "bearer"),
            _ => bail!("the credential for {origin} needs exactly one of basic or bearer"),
        };
        ensure!(
            manifest.credentials.iter().any(|r| r.injection == injection
                && r.origins.iter().any(|o| covers(o, &origin, &settings))),
            "the manifest asks for no {kind} credential for {origin}"
        );
        let credential = match (&c.basic, &c.bearer) {
            (Some(b), _) => Credential::Basic {
                username: b.username.clone(),
                password: b
                    .password
                    .resolve()
                    .with_context(|| format!("credential for {origin}"))?,
            },
            (_, Some(t)) => Credential::Bearer(
                t.resolve()
                    .with_context(|| format!("credential for {origin}"))?,
            ),
            _ => unreachable!("checked above"),
        };
        credentials.push((origin, credential));
    }
    // What the sandbox will refuse when the component loads (a credential over plain http to
    // another machine), refused here too, so that `fronde check` reports it.
    Grants::new(&allow, &credentials)?;

    Ok(Prepared {
        spec: ComponentSpec {
            name: name.clone(),
            path: config.path.clone(),
            allow,
            credentials,
            settings,
            sha256: Some(current.sha256),
            tool_prefixes: manifest.tool_prefixes,
        },
        approval,
    })
}

/// Hashes in the entry pin it to those files; without any, the files must be those of a component
/// shipped with this build.
fn approve(
    config: &ComponentConfig,
    current: &Hashes,
    manifest_path: &Path,
    shipped: &[Shipped],
) -> anyhow::Result<Approval> {
    let (sha256, manifest_sha256) = match (&config.sha256, &config.manifest_sha256) {
        (Some(sha256), Some(manifest_sha256)) => (sha256, manifest_sha256),
        (None, None) => {
            if let Some(s) = shipped.iter().find(|s| {
                s.sha256 == current.sha256 && s.manifest_sha256 == current.manifest_sha256
            }) {
                return Ok(Approval::Shipped {
                    name: s.name.clone(),
                });
            }
            bail!(
                "not approved yet: these files are not a component shipped with this build of \
                 Fronde. Read its manifest {}, then add sha256 = \"{}\" and manifest_sha256 = \
                 \"{}\" to its [[sources]] entry",
                manifest_path.display(),
                current.sha256,
                current.manifest_sha256
            );
        }
        _ => bail!(
            "not approved yet: set both sha256 and manifest_sha256, or neither for a component \
             shipped with this build. Read its manifest {}, then add sha256 = \"{}\" and \
             manifest_sha256 = \"{}\" to its [[sources]] entry",
            manifest_path.display(),
            current.sha256,
            current.manifest_sha256
        ),
    };
    for (file, approved, now) in [
        (config.path.as_path(), sha256, &current.sha256),
        (manifest_path, manifest_sha256, &current.manifest_sha256),
    ] {
        ensure!(
            approved == now,
            "{} changed since it was approved (its sha256 is now {now}); read the manifest {} \
             again, then update the hashes in its [[sources]] entry (`fronde check` prints them)",
            file.display(),
            manifest_path.display()
        );
    }
    Ok(Approval::Pinned)
}

/// The component's tool prefixes stay clear of the hub's own tools, of the tools of every other
/// source (an upstream named `github` has `github_…`) and of other components' prefixes.
fn check_namespace(
    config: &ComponentConfig,
    prefixes: &[String],
    sources: &[SourceConfig],
) -> anyhow::Result<()> {
    let overlap = |a: &str, b: &str| a.starts_with(b) || b.starts_with(a);
    for prefix in prefixes {
        if let Some(reserved) = RESERVED_TOOL_PREFIXES.iter().find(|r| overlap(prefix, r)) {
            bail!("tool prefix {prefix:?} is reserved for the hub's own tools ({reserved}…)");
        }
        for source in sources.iter().filter(|s| s.name() != config.name) {
            let other = source.name();
            let theirs = format!("{other}_");
            ensure!(
                !overlap(prefix, &theirs),
                "tool prefix {prefix:?} would mix its tools with those of the source {other:?} \
                 ({theirs}…); rename that source"
            );
            // Another component's manifest that cannot be read keeps it from loading anyway.
            if let SourceConfig::Component(c) = source
                && let Ok(manifest) = Manifest::load(&c.manifest_path())
                && let Some(p) = manifest.tool_prefixes.iter().find(|p| overlap(prefix, p))
            {
                bail!(
                    "tool prefix {prefix:?} is also declared by the component {other:?} ({p}); \
                     two components cannot share tool names, so keep only one of them"
                );
            }
        }
    }
    Ok(())
}

/// The origin of a URL setting.
fn origin_of(url: &str) -> Option<Origin> {
    Origin::of_url(&url::Url::parse(url).ok()?).ok()
}

/// Whether the manifest's `rule` asks for `origin`.
fn covers(rule: &NetworkRule, origin: &Origin, settings: &BTreeMap<String, String>) -> bool {
    match rule {
        NetworkRule::Origin(o) => o == origin,
        NetworkRule::Subdomains {
            https,
            domain,
            port,
        } => {
            origin.is_https() == *https
                && origin.port() == *port
                && origin
                    .domain()
                    .and_then(|host| host.strip_suffix(domain.as_str()))
                    .is_some_and(|sub| sub.len() > 1 && sub.ends_with('.'))
        }
        NetworkRule::Setting(name) => settings
            .get(name)
            .and_then(|url| origin_of(url))
            .is_some_and(|o| o == *origin),
    }
}

/// The manifest's network requests, for an error message.
fn asked(rules: &[NetworkRule], settings: &BTreeMap<String, String>) -> String {
    if rules.is_empty() {
        return "no origin".to_string();
    }
    rules
        .iter()
        .map(|r| match r {
            NetworkRule::Origin(o) => o.to_string(),
            NetworkRule::Subdomains {
                https,
                domain,
                port,
            } => {
                let scheme = if *https { "https" } else { "http" };
                format!("{scheme}://*.{domain}:{port}")
            }
            NetworkRule::Setting(name) => match settings.get(name).and_then(|u| origin_of(u)) {
                Some(o) => format!("{o} from {name}"),
                None => format!("the origin of {name}, which is not set"),
            },
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The settings fit the manifest's schema, which only has string properties (checked when the
/// manifest was parsed).
fn check_settings(
    schema: &Value,
    properties: &Map<String, Value>,
    settings: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    for (key, value) in settings {
        let Some(property) = properties.get(key) else {
            let known: Vec<&str> = properties.keys().map(String::as_str).collect();
            bail!(
                "{key:?} is not a setting of this component (it has: {})",
                known.join(", ")
            );
        };
        if let Some(Value::Array(choices)) = property.get("enum") {
            ensure!(
                choices.iter().any(|c| c.as_str() == Some(value)),
                "{key} = {value:?} is not one of {}",
                Value::Array(choices.clone())
            );
        }
        if property.get("format").and_then(Value::as_str) == Some("uri") {
            let url = url::Url::parse(value)
                .with_context(|| format!("{key} = {value:?} is not a URL"))?;
            // Not echoed: the value holds a secret.
            ensure!(
                url.username().is_empty() && url.password().is_none(),
                "{key} has a user name or password in its URL: credentials go in the \
                 [[sources.credentials]] entry for its origin, never in settings"
            );
        }
        if let Some(pattern) = property.get("pattern").and_then(Value::as_str) {
            let re = regex::Regex::new(pattern)
                .with_context(|| format!("the manifest's pattern for {key} is invalid"))?;
            ensure!(
                re.is_match(value),
                "{key} = {value:?} does not match {pattern:?}"
            );
        }
    }
    if let Some(Value::Array(required)) = schema.get("required") {
        for key in required.iter().filter_map(Value::as_str) {
            ensure!(
                settings.contains_key(key),
                "{key} is required: add it under [sources.settings]"
            );
        }
    }
    Ok(())
}

/// A component as a source of tools: it is checked and loaded when the hub starts.
pub struct ComponentSource {
    config: ComponentConfig,
    checks: Checks,
    host: ComponentHost,
}

impl ComponentSource {
    pub fn new(config: ComponentConfig, checks: Checks, host: ComponentHost) -> Self {
        Self {
            config,
            checks,
            host,
        }
    }
}

#[async_trait]
impl ToolSource for ComponentSource {
    fn name(&self) -> &str {
        &self.config.name
    }

    async fn start(&self) -> anyhow::Result<Session> {
        let Prepared { spec, approval } = prepare(&self.config, &self.checks)?;
        tracing::info!("{}: {approval}", self.config.name);
        let component = LoadedComponent::load(&self.host, spec).await?;
        Ok(Session::lasting(component.tools()))
    }
}
