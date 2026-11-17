//! The components shipped with this build, as integrations a setup card can add: the hub's
//! [`Integrations`] over their manifests, `prepare` and the configuration file.

use crate::config_edit;
use anyhow::{Context, bail};
use async_trait::async_trait;
use fronde_db::DatabaseConnection;
use fronde_db::master_key::Backends;
use fronde_hub::config::{ComponentConfig, Config, SourceConfig, shipped_component};
use fronde_hub::integrations::{
    CredentialAsk, CredentialKind, Integration, Integrations, Reach, Setting, SettingKind, Written,
};
use fronde_hub::source::Supervisor;
use fronde_wasm::{
    Checks, ComponentHost, ComponentSource, Hashes, Injection, Manifest, NetworkRule,
    StoredSecrets, check_manifest_settings, prepare,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

/// The components shipped with this build, found beside the binary or in the checkout it was
/// built from, whose files are the ones the build approves.
pub struct Shipped {
    /// The configuration Fronde started with.
    config: Config,
    db: DatabaseConnection,
    backends: Backends,
    /// Each shipped component, with its manifest: read once, they do not change while Fronde
    /// runs.
    shipped: Vec<(Integration, Manifest)>,
    /// The `[[sources]]` entries: the configuration's, and those added since.
    sources: Mutex<Vec<SourceConfig>>,
    /// Held while an entry is added, so that two cards answered at once add it once.
    adding: tokio::sync::Mutex<()>,
    /// The engine new components run in, made at the first one.
    host: tokio::sync::OnceCell<ComponentHost>,
}

impl Shipped {
    /// The components shipped with this build, for `config`'s Fronde: those whose files are
    /// found and match the hashes the build approves.
    pub fn new(config: &Config, db: DatabaseConnection) -> anyhow::Result<Self> {
        let mut shipped = Vec::new();
        for component in fronde_wasm::shipped() {
            let Some(path) = shipped_component(&component.name) else {
                continue;
            };
            let probe = ComponentConfig {
                name: component.name.clone(),
                path: path.clone(),
                manifest: None,
                sha256: None,
                manifest_sha256: None,
                allow: Vec::new(),
                settings: BTreeMap::new(),
                credentials: Vec::new(),
            };
            // Other files than the build's would not be approved: never offered.
            match Hashes::of(&probe) {
                Ok(hashes)
                    if hashes.sha256 == component.sha256
                        && hashes.manifest_sha256 == component.manifest_sha256 => {}
                _ => continue,
            }
            let manifest = match Manifest::load(&probe.manifest_path()) {
                Ok(manifest) => manifest,
                Err(error) => {
                    tracing::warn!("{}: cannot offer it to set up: {error:#}", component.name);
                    continue;
                }
            };
            shipped.push((integration(&component.name, path, &manifest), manifest));
        }
        Ok(Self {
            config: config.clone(),
            db,
            backends: config.master_key_backends()?,
            shipped,
            sources: Mutex::new(config.sources.clone()),
            adding: tokio::sync::Mutex::default(),
            host: tokio::sync::OnceCell::new(),
        })
    }

    fn sources(&self) -> Vec<SourceConfig> {
        self.sources
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The checks a start of `entry` runs, with every other entry beside it.
    async fn checks(&self, entry: &ComponentConfig) -> anyhow::Result<Checks> {
        let mut config = self.config.clone();
        config.sources = self.sources();
        config.sources.retain(|source| source.name() != entry.name);
        config.sources.push(SourceConfig::Component(entry.clone()));
        Checks::load(&config, &self.db).await
    }
}

/// What the card shows of a shipped component, from its manifest.
fn integration(name: &str, path: std::path::PathBuf, manifest: &Manifest) -> Integration {
    let mut unsupported = None;
    let reach = |rule: &NetworkRule, unsupported: &mut Option<String>| match rule {
        NetworkRule::Origin(origin) => Some(Reach::Origin(origin.clone())),
        NetworkRule::Setting(key) => Some(Reach::Setting(key.clone())),
        NetworkRule::Subdomains { domain, .. } => {
            *unsupported = Some(format!("it asks for any subdomain of {domain}"));
            None
        }
    };
    let network = manifest
        .network
        .iter()
        .filter_map(|rule| reach(rule, &mut unsupported))
        .collect();
    let credentials = manifest
        .credentials
        .iter()
        .filter_map(|c| {
            let kind = match c.injection {
                Injection::Basic => CredentialKind::Basic,
                Injection::Bearer => CredentialKind::Bearer,
                Injection::Header(ref header) => {
                    unsupported = Some(format!(
                        "its credential {} goes in the header {header}, which an entry cannot give yet",
                        c.name
                    ));
                    return None;
                }
            };
            Some(CredentialAsk {
                name: c.name.clone(),
                kind,
                origins: c
                    .origins
                    .iter()
                    .filter_map(|rule| reach(rule, &mut unsupported))
                    .collect(),
            })
        })
        .collect();
    let required: Vec<&str> = manifest
        .settings
        .get("required")
        .and_then(Value::as_array)
        .map(|keys| keys.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let settings = manifest
        .settings
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| {
            properties
                .iter()
                .map(|(key, property)| Setting {
                    key: key.clone(),
                    description: property
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    required: required.contains(&key.as_str()),
                    kind: match (property.get("enum"), property.get("format")) {
                        (Some(Value::Array(values)), _) => SettingKind::Choice(
                            values
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect(),
                        ),
                        (_, Some(format)) if format == "uri" => SettingKind::Url,
                        _ => SettingKind::Text,
                    },
                })
                .collect()
        })
        .unwrap_or_default();
    Integration {
        name: name.to_owned(),
        description: manifest.description.clone(),
        path,
        tool_prefixes: manifest.tool_prefixes.clone(),
        settings,
        credentials,
        network,
        unsupported,
    }
}

#[async_trait]
impl Integrations for Shipped {
    fn available(&self) -> Vec<Integration> {
        let sources = self.sources();
        self.shipped
            .iter()
            .filter(|(integration, _)| {
                !sources
                    .iter()
                    .any(|source| source.name() == integration.name)
            })
            .map(|(integration, _)| integration.clone())
            .collect()
    }

    fn check_settings(
        &self,
        name: &str,
        settings: &BTreeMap<String, String>,
        complete: bool,
    ) -> Result<(), String> {
        let Some((_, manifest)) = self.shipped.iter().find(|(i, _)| i.name == name) else {
            return Err(format!("{name} is not shipped with this build"));
        };
        check_manifest_settings(manifest, settings, complete).map_err(|error| format!("{error:#}"))
    }

    async fn dry_run(&self, entry: &ComponentConfig) -> anyhow::Result<()> {
        let mut checks = self.checks(entry).await?;
        if !entry.credentials.is_empty() {
            checks.secrets = StoredSecrets::load(&self.db, &self.backends).await?;
        }
        prepare(entry, &checks).map(|_| ())
    }

    async fn add(&self, entry: &ComponentConfig, sources: &Supervisor) -> anyhow::Result<Written> {
        let _adding = self.adding.lock().await;
        if self
            .sources()
            .iter()
            .any(|source| source.name() == entry.name)
        {
            bail!("a source named {} is already configured", entry.name);
        }
        let checks = self.checks(entry).await?;
        let written = match self.config.path() {
            Some(path) => match config_edit::add_component(path, entry) {
                Ok(()) => {
                    tracing::info!("{}: added to {}", entry.name, path.display());
                    Written::File
                }
                Err(error) => {
                    tracing::warn!(
                        "{}: the entry goes to the owner to paste instead: {error:#}",
                        entry.name
                    );
                    Written::Snippet(config_edit::snippet(entry)?)
                }
            },
            None => Written::Snippet(config_edit::snippet(entry)?),
        };
        self.sources
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(SourceConfig::Component(entry.clone()));
        let host = self
            .host
            .get_or_try_init(|| async { ComponentHost::new() })
            .await?
            .clone();
        let source = ComponentSource::new(entry.clone(), checks, host)
            .reading_secrets_from(self.db.clone(), self.backends.clone());
        sources
            .start(Arc::new(source))
            .await
            .with_context(|| match &written {
                Written::File => "added to fronde.toml, but it did not start",
                Written::Snippet(_) => "it did not start",
            })?;
        Ok(written)
    }
}
