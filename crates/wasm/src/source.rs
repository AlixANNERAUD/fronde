//! A `[[sources]]` entry of kind `component`: checked against the component's manifest and the
//! approval in the configuration, then loaded as a source of tools.
//!
//! The manifest asks; the configuration grants at most what it asks: each allowed origin and
//! each credential must match a request of the manifest, and the settings must fit its schema.
//! The approval is the SHA-256 of both files, so a changed manifest or build is not loaded until
//! the configuration approves it again.

use crate::egress::{Allowed, Credential, Origin};
use crate::manifest::{Injection, Manifest, NetworkRule, World};
use crate::{ComponentHost, ComponentSpec, LoadedComponent};
use anyhow::{Context, bail, ensure};
use async_trait::async_trait;
use fronde_hub::config::ComponentConfig;
use fronde_hub::source::{Session, ToolSource};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

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

/// Check `config` against the component's approval and manifest, resolve its credentials, and
/// return what to load. `timezone` is the user's, given to a component that declares a
/// `timezone` setting left unset.
pub fn prepare(config: &ComponentConfig, timezone: &str) -> anyhow::Result<ComponentSpec> {
    let name = &config.name;
    let manifest_path = config.manifest_path();
    // The manifest parsed is the text hashed; the .wasm is hashed again as it is compiled.
    let text = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("cannot read {}", manifest_path.display()))?;
    let current = Hashes {
        sha256: sha256_of(&config.path)?,
        manifest_sha256: sha256(text.as_bytes()),
    };
    check_approval(config, &current, &manifest_path)?;
    let manifest = Manifest::parse(&text)
        .with_context(|| format!("invalid manifest {}", manifest_path.display()))?;
    let World::Tool = manifest.world;

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
        let origin: Origin = a.origin.as_str().parse()?;
        ensure!(
            manifest
                .network
                .iter()
                .any(|r| covers(r, &origin, &settings)),
            "allow has {origin}, which the manifest does not ask for (it asks for {})",
            asked(&manifest.network, &settings)
        );
        allow.push(Allowed {
            origin,
            local: a.local,
        });
    }

    let mut credentials = Vec::new();
    for c in &config.credentials {
        let origin: Origin = c.origin.as_str().parse()?;
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

    Ok(ComponentSpec {
        name: name.clone(),
        path: config.path.clone(),
        allow,
        credentials,
        settings,
        sha256: Some(current.sha256),
    })
}

fn check_approval(
    config: &ComponentConfig,
    current: &Hashes,
    manifest_path: &Path,
) -> anyhow::Result<()> {
    let (Some(sha256), Some(manifest_sha256)) = (&config.sha256, &config.manifest_sha256) else {
        bail!(
            "not approved yet: read its manifest {}, then add sha256 = \"{}\" and \
             manifest_sha256 = \"{}\" to its [[sources]] entry",
            manifest_path.display(),
            current.sha256,
            current.manifest_sha256
        );
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
    Ok(())
}

/// The origin of a URL setting.
fn origin_of(url: &str) -> Option<Origin> {
    let url = url::Url::parse(url).ok()?;
    url.origin().ascii_serialization().parse().ok()
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
            origin.https == *https
                && origin.port == *port
                && origin
                    .host
                    .strip_suffix(domain.as_str())
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
            url::Url::parse(value).with_context(|| format!("{key} = {value:?} is not a URL"))?;
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
    timezone: String,
    host: ComponentHost,
}

impl ComponentSource {
    pub fn new(config: ComponentConfig, timezone: impl Into<String>, host: ComponentHost) -> Self {
        Self {
            config,
            timezone: timezone.into(),
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
        let spec = prepare(&self.config, &self.timezone)?;
        let component = LoadedComponent::load(&self.host, spec).await?;
        Ok(Session::lasting(component.tools()))
    }
}
