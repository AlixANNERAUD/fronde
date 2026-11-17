//! A `[[sources]]` entry of kind `component`: checked against the component's manifest and its
//! approval, then loaded as a source of tools.
//!
//! The manifest asks; the configuration grants at most what it asks: each allowed origin and
//! each credential must match a request of the manifest, and the settings must fit its schema.
//! The approval is the SHA-256 of both files: either the hashes written in the entry, or, for an
//! entry without them, those of a component shipped with this build of Fronde or those the owner
//! approved for this source (rows of `fronde.db`). Hashes in the entry always win. A changed
//! manifest or build is not loaded until it is approved again. A path never approves anything.
//!
//! A credential `{ stored = "name" }` names a secret sealed in `fronde.db`. [`Checks`] carries
//! those rows still sealed, with the master key; [`prepare`] opens only the ones its entry's
//! credentials name, each only for the origin it was stored for.

use crate::egress::{Allowed, Credential, Grants};
use crate::manifest::{Injection, Manifest, NetworkRule, World};
use crate::{ComponentHost, ComponentSpec, LoadedComponent};
use anyhow::{Context, bail, ensure};
use async_trait::async_trait;
use fronde_db::master_key::MasterKey;
use fronde_db::secrets::Sealed;
use fronde_hub::config::{ComponentConfig, Config, RESERVED_TOOL_PREFIXES, Secret, SourceConfig};
use fronde_hub::origin::{Origin, is_local};
use fronde_hub::source::{Session, ToolSource, Waiting};
use secrecy::SecretString;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    /// No hashes in its entry, and an approval of these files for this source in the database.
    Stored,
}

impl fmt::Display for Approval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pinned => f.write_str("approved: pinned by the hashes in its entry"),
            Self::Shipped { name } => write!(f, "approved: shipped with this build ({name})"),
            Self::Stored => f.write_str("approved: by the owner, stored in the database"),
        }
    }
}

/// The owner's approval of a source's files, as stored in the database: it approves these files
/// for this source only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub source: String,
    pub hashes: Hashes,
}

/// The approvals of an entry without hashes: the components shipped with this build, and those
/// stored in the database.
#[derive(Debug, Clone, Default)]
pub struct Approvals {
    pub shipped: Vec<Shipped>,
    pub stored: Vec<Stored>,
}

impl Approvals {
    /// The components shipped with this build, and nothing stored.
    pub fn shipped() -> Self {
        Self {
            shipped: shipped(),
            stored: Vec::new(),
        }
    }

    /// What approves the files of `source` whose hashes are `current`: a shipped component with
    /// these files, whatever its entry's name, or else an approval stored for this source and
    /// both hashes.
    pub fn find(&self, source: &str, current: &Hashes) -> Option<Approval> {
        if let Some(s) = self
            .shipped
            .iter()
            .find(|s| s.sha256 == current.sha256 && s.manifest_sha256 == current.manifest_sha256)
        {
            return Some(Approval::Shipped {
                name: s.name.clone(),
            });
        }
        self.stored
            .iter()
            .any(|s| s.source == source && s.hashes == *current)
            .then_some(Approval::Stored)
    }
}

/// The secrets stored for components' credentials, still sealed, and the master key that opens
/// them. Nothing is opened until [`prepare`] needs one.
#[derive(Debug, Clone, Default)]
pub struct StoredSecrets {
    /// `None` when nothing is stored and there is no key file.
    pub key: Option<Arc<MasterKey>>,
    pub sealed: Vec<Sealed>,
}

impl StoredSecrets {
    /// The rows of `db` in the `sources` scope, and the key of `data_dir`, checked against `db`:
    /// a key that did not seal them, or no key file while secrets are stored, is an error. Only
    /// reads.
    pub async fn load(db: &fronde_db::DatabaseConnection, data_dir: &Path) -> anyhow::Result<Self> {
        let key = fronde_db::master_key::load(db, data_dir).await?;
        let sealed = fronde_db::secrets::load_sealed(db, fronde_db::secrets::SOURCES).await?;
        Ok(Self { key, sealed })
    }

    /// The value of the secret `name`, which must have been stored for `origin`. A secret not
    /// stored yet, or stored for another origin, is [`Waiting`]: `fronde secret set` fixes it
    /// while Fronde runs.
    fn open(&self, name: &str, origin: &Origin) -> anyhow::Result<SecretString> {
        let set = format!(
            "`fronde secret set {name} --origin {origin} --config <your fronde.toml>`, the value \
             on standard input"
        );
        let Some(sealed) = self.sealed.iter().find(|s| s.name == name) else {
            return Err(Waiting(format!(
                "no secret named {name} is stored; store it with {set}"
            ))
            .into());
        };
        let stored = Origin::parse(&sealed.origin).with_context(|| {
            format!("the stored secret {name} has an unreadable origin; store it again with {set}")
        })?;
        if stored != *origin {
            return Err(Waiting(format!(
                "the stored secret {name} is for {stored}, not {origin}, and a stored secret goes \
                 only to its own origin; store it again for {origin} with {set}, or name the \
                 secret stored for {origin}"
            ))
            .into());
        }
        let key = self.key.as_deref().with_context(|| {
            format!("the stored secret {name} cannot be opened: there is no master key")
        })?;
        sealed.open(key)
    }
}

/// What a component entry is checked against besides its own files.
#[derive(Debug, Clone, Default)]
pub struct Checks {
    /// The user's time zone, given to a component that declares a `timezone` setting left unset.
    pub timezone: String,
    /// What approves components without hashes in their entry.
    pub approvals: Approvals,
    /// Every `[[sources]]` entry: a component's tool prefixes must stay clear of the others.
    pub sources: Vec<SourceConfig>,
    /// What `{ stored = "…" }` credentials name.
    pub secrets: StoredSecrets,
}

impl Checks {
    /// For the entries of `config`, with the components shipped with this build and no stored
    /// approval.
    pub fn of(config: &Config) -> Self {
        Self {
            timezone: config.data.timezone.name().to_string(),
            approvals: Approvals::shipped(),
            sources: config.sources.clone(),
            secrets: StoredSecrets::default(),
        }
    }

    /// [`Checks::of`], with the approvals and the sealed secrets stored in `db` and the master
    /// key of the data directory, read now: [`prepare`] reads no database, so an approval stored
    /// later applies at the next start. A [`ComponentSource`] reads the secrets again each time
    /// it starts.
    pub async fn load(config: &Config, db: &fronde_db::DatabaseConnection) -> anyhow::Result<Self> {
        let mut checks = Self::of(config);
        checks.approvals.stored = stored_approvals(db).await?;
        checks.secrets = StoredSecrets::load(db, &config.data.dir).await?;
        Ok(checks)
    }
}

/// The approvals the owner stored in `db`.
pub async fn stored_approvals(db: &fronde_db::DatabaseConnection) -> anyhow::Result<Vec<Stored>> {
    Ok(fronde_db::approvals::all(db)
        .await
        .context("cannot read the component approvals in the database")?
        .into_iter()
        .map(|row| Stored {
            source: row.source,
            hashes: Hashes {
                sha256: row.sha256,
                manifest_sha256: row.manifest_sha256,
            },
        })
        .collect())
}

/// Why a component's files are not approved: [`prepare`]'s error, under any context its callers
/// add, with what a request to approve them needs to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotApproved {
    /// The `[[sources]]` entry's name.
    pub source: String,
    /// The manifest to read before approving.
    pub manifest: PathBuf,
    /// The hashes of the files as they are now, which would approve them.
    pub current: Hashes,
    pub reason: NotApprovedReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotApprovedReason {
    /// No hashes in the entry, and nothing approves these files.
    Unknown,
    /// Only one of `sha256` and `manifest_sha256` in the entry.
    HalfPinned,
    /// `file` no longer has the hash its entry pins; `sha256` is its hash now.
    Changed { file: PathBuf, sha256: String },
}

impl fmt::Display for NotApproved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let manifest = self.manifest.display();
        let Hashes {
            sha256,
            manifest_sha256,
        } = &self.current;
        match &self.reason {
            NotApprovedReason::Unknown => write!(
                f,
                "not approved yet: these files are not a component shipped with this build of \
                 Fronde. Read its manifest {manifest}, then add sha256 = \"{sha256}\" and \
                 manifest_sha256 = \"{manifest_sha256}\" to its [[sources]] entry"
            ),
            NotApprovedReason::HalfPinned => write!(
                f,
                "not approved yet: set both sha256 and manifest_sha256, or neither for a \
                 component shipped with this build. Read its manifest {manifest}, then add \
                 sha256 = \"{sha256}\" and manifest_sha256 = \"{manifest_sha256}\" to its \
                 [[sources]] entry"
            ),
            NotApprovedReason::Changed { file, sha256: now } => write!(
                f,
                "{} changed since it was approved (its sha256 is now {now}); read the manifest \
                 {manifest} again, then update the hashes in its [[sources]] entry (`fronde \
                 check` prints them)",
                file.display()
            ),
        }
    }
}

impl std::error::Error for NotApproved {}

/// A component entry ready to load, and what approved it. Only [`prepare`] makes one, so that
/// nothing loads a component without its approval or with more than its entry grants.
#[derive(Debug, Clone)]
pub struct Prepared {
    spec: ComponentSpec,
    approval: Approval,
    /// The approved build's SHA-256: the file is hashed again as it is compiled.
    sha256: String,
}

impl Prepared {
    pub fn spec(&self) -> &ComponentSpec {
        &self.spec
    }

    pub fn approval(&self) -> &Approval {
        &self.approval
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub(crate) fn into_parts(self) -> (ComponentSpec, String) {
        (self.spec, self.sha256)
    }
}

/// Check `config` against the component's approval and manifest and against `checks`, resolve
/// its credentials, opening the stored secrets they name, and return what to load.
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
    let approval = approve(
        config,
        current.clone(),
        manifest_path.clone(),
        &checks.approvals,
    )?;
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
        let resolve = |secret: &Secret| {
            match secret {
                Secret::Stored { stored } => checks.secrets.open(stored, &origin),
                other => other.resolve(),
            }
            .with_context(|| format!("credential for {origin}"))
        };
        let credential = match (&c.basic, &c.bearer) {
            (Some(b), _) => Credential::Basic {
                username: b.username.clone(),
                password: resolve(&b.password)?,
            },
            (_, Some(t)) => Credential::Bearer(resolve(t)?),
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
            tool_prefixes: manifest.tool_prefixes,
        },
        approval,
        sha256: current.sha256,
    })
}

/// The only function that decides whether a component's files are approved. Hashes in the entry
/// pin it to those files, whatever else approves them; without any, `approvals` must approve
/// these files for this source. A refusal is a [`NotApproved`], the error's root.
fn approve(
    config: &ComponentConfig,
    current: Hashes,
    manifest: PathBuf,
    approvals: &Approvals,
) -> anyhow::Result<Approval> {
    let refuse = |current: Hashes, reason| {
        anyhow::Error::new(NotApproved {
            source: config.name.clone(),
            manifest: manifest.clone(),
            current,
            reason,
        })
    };
    let (sha256, manifest_sha256) = match (&config.sha256, &config.manifest_sha256) {
        (Some(sha256), Some(manifest_sha256)) => (sha256, manifest_sha256),
        (None, None) => {
            return approvals
                .find(&config.name, &current)
                .ok_or_else(|| refuse(current, NotApprovedReason::Unknown));
        }
        _ => return Err(refuse(current, NotApprovedReason::HalfPinned)),
    };
    for (file, approved, now) in [
        (config.path.as_path(), sha256, &current.sha256),
        (
            manifest.as_path(),
            manifest_sha256,
            &current.manifest_sha256,
        ),
    ] {
        if approved != now {
            let reason = NotApprovedReason::Changed {
                file: file.to_path_buf(),
                sha256: now.clone(),
            };
            return Err(refuse(current.clone(), reason));
        }
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

/// Whether a credential of `config` names a stored secret.
fn uses_stored_secrets(config: &ComponentConfig) -> bool {
    config.credentials.iter().any(|c| {
        let secret = c.basic.as_ref().map(|b| &b.password).or(c.bearer.as_ref());
        matches!(secret, Some(Secret::Stored { .. }))
    })
}

/// A component as a source of tools: it is checked and loaded each time it starts.
pub struct ComponentSource {
    config: ComponentConfig,
    checks: Checks,
    host: ComponentHost,
    /// The database and data directory where the stored secrets and the master key are read
    /// again at each start, so that a secret stored or changed since the last one applies.
    /// Without them, those of `checks`.
    store: Option<(fronde_db::DatabaseConnection, PathBuf)>,
}

impl ComponentSource {
    pub fn new(config: ComponentConfig, checks: Checks, host: ComponentHost) -> Self {
        Self {
            config,
            checks,
            host,
            store: None,
        }
    }

    /// Read the stored secrets from `db`, and the master key from `data_dir`, each time the
    /// component starts.
    pub fn reading_secrets_from(
        mut self,
        db: fronde_db::DatabaseConnection,
        data_dir: PathBuf,
    ) -> Self {
        self.store = Some((db, data_dir));
        self
    }
}

#[async_trait]
impl ToolSource for ComponentSource {
    fn name(&self) -> &str {
        &self.config.name
    }

    async fn start(&self) -> anyhow::Result<Session> {
        let prepared = match &self.store {
            Some((db, data_dir)) if uses_stored_secrets(&self.config) => {
                let mut checks = self.checks.clone();
                checks.secrets = StoredSecrets::load(db, data_dir).await?;
                prepare(&self.config, &checks)?
            }
            _ => prepare(&self.config, &self.checks)?,
        };
        tracing::info!("{}: {}", self.config.name, prepared.approval());
        let component = LoadedComponent::load(&self.host, prepared).await?;
        Ok(Session::lasting(component.tools()))
    }
}
