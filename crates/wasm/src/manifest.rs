//! A component's manifest, `fronde-component.toml` beside its `Cargo.toml`: who it is, which
//! `fronde:component` version and world it targets, what it asks to reach, and its settings.
//!
//! The manifest is a request, never a grant: the configuration decides what a component gets,
//! within what its manifest declares. Parsing is strict, so that a typo cannot widen a request.

use crate::api::{self, Api};
use crate::host::Limits;
use anyhow::{Context, bail, ensure};
use fronde_hub::origin::Origin;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

/// The manifest format this host reads.
const SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    /// Immutable: reverse-DNS (`org.example.calendar`) or `publisher/name`.
    pub id: String,
    pub version: semver::Version,
    pub world: World,
    /// The `fronde:component` version it targets.
    pub api: Api,
    pub description: String,
    pub license: String,
    pub authors: Vec<String>,
    pub repository: Option<String>,
    /// Every tool it exports starts with one of these (`calendar_`), which no other source may
    /// use: a component cannot shadow another source's tools.
    pub tool_prefixes: Vec<String>,
    /// The origins the component asks to reach.
    pub network: Vec<NetworkRule>,
    pub credentials: Vec<CredentialRequest>,
    pub limits: LimitRequest,
    /// JSON Schema of the settings, all strings and none secret.
    pub settings: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum World {
    Tool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Kind {
    Component,
}

/// One entry of `permissions.network`, in Spin's `allowed_outbound_hosts` syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkRule {
    /// `https://cal.example.com[:port]`.
    Origin(Origin),
    /// `https://*.example.com[:port]`: any subdomain of `domain`, not `domain` itself.
    Subdomains {
        https: bool,
        domain: String,
        port: u16,
    },
    /// `{{ name }}`: the origin of the URL the user gives in that setting.
    Setting(String),
}

/// A secret the host adds to requests for some of the component's origins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRequest {
    pub name: String,
    pub injection: Injection,
    pub origins: Vec<NetworkRule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Injection {
    /// `Authorization: Basic …` from a user name and password.
    Basic,
    /// `Authorization: Bearer …`.
    Bearer,
    /// The secret as the value of this header.
    Header(http::HeaderName),
}

/// Tighter limits than the host's, which are the maxima.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitRequest {
    pub memory_mb: Option<u64>,
    pub call_timeout_ms: Option<u64>,
    pub cpu_ms: Option<u64>,
    pub request_timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    schema: u32,
    id: String,
    version: String,
    kind: Kind,
    world: World,
    api: String,
    description: String,
    license: String,
    #[serde(default)]
    authors: Vec<String>,
    repository: Option<String>,
    tool_prefixes: Vec<String>,
    #[serde(default)]
    permissions: RawPermissions,
    #[serde(default)]
    credentials: Vec<RawCredential>,
    #[serde(default)]
    limits: LimitRequest,
    settings: Option<Value>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPermissions {
    #[serde(default)]
    network: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCredential {
    name: String,
    injection: String,
    origins: Vec<String>,
}

impl Manifest {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid manifest {}", path.display()))
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let raw: Raw = toml::from_str(text)?;
        let Kind::Component = raw.kind;
        ensure!(
            raw.schema == SCHEMA,
            "schema {} is not one this Fronde reads (it reads {SCHEMA})",
            raw.schema
        );
        ensure!(
            is_id(&raw.id),
            "id {:?} must be reverse-DNS (org.example.calendar) or publisher/name, in lower case",
            raw.id
        );
        let version = semver::Version::parse(&raw.version)
            .with_context(|| format!("version {:?} is not semver", raw.version))?;
        let Some(api) = Api::ALL.into_iter().find(|a| a.as_str() == raw.api) else {
            bail!(
                "api {:?} is not a fronde:component version this Fronde serves ({})",
                raw.api,
                api::served()
            );
        };
        ensure!(!raw.description.trim().is_empty(), "description is empty");
        ensure!(!raw.license.trim().is_empty(), "license is empty");
        check_tool_prefixes(&raw.tool_prefixes).context("tool_prefixes")?;
        let settings = raw.settings.unwrap_or_else(
            || serde_json::json!({"type": "object", "additionalProperties": false, "properties": {}}),
        );
        let properties = check_settings(&settings).context("settings")?;
        let network = raw
            .permissions
            .network
            .iter()
            .map(|s| NetworkRule::parse(s, properties))
            .collect::<anyhow::Result<Vec<_>>>()
            .context("permissions.network")?;
        let mut credentials: Vec<CredentialRequest> = Vec::new();
        for c in raw.credentials {
            let credential = CredentialRequest::parse(c, properties, &network)?;
            ensure!(
                !credentials.iter().any(|o| o.name == credential.name),
                "two credentials are named {:?}",
                credential.name
            );
            credentials.push(credential);
        }
        check_limits(&raw.limits).context("limits")?;
        Ok(Self {
            id: raw.id,
            version,
            world: raw.world,
            api,
            description: raw.description,
            license: raw.license,
            authors: raw.authors,
            repository: raw.repository,
            tool_prefixes: raw.tool_prefixes,
            network,
            credentials,
            limits: raw.limits,
            settings,
        })
    }
}

/// Lower-case letters, digits and inner hyphens.
fn is_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && !s.starts_with('-')
        && !s.ends_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn is_id(id: &str) -> bool {
    match id.split_once('/') {
        Some((publisher, name)) => is_label(publisher) && is_label(name),
        None => id.contains('.') && id.split('.').all(is_label),
    }
}

/// `^[a-z][a-z0-9]*_$`: a word and an underscore, so that two prefixes overlap only when equal.
pub(crate) fn is_tool_prefix(s: &str) -> bool {
    s.strip_suffix('_').is_some_and(|word| {
        word.starts_with(|c: char| c.is_ascii_lowercase())
            && word
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    })
}

fn check_tool_prefixes(prefixes: &[String]) -> anyhow::Result<()> {
    ensure!(
        !prefixes.is_empty(),
        "list the prefixes of the component's tool names, such as [\"weather_\"]"
    );
    for (i, prefix) in prefixes.iter().enumerate() {
        ensure!(
            is_tool_prefix(prefix),
            "{prefix:?} must be lower-case letters and digits followed by _, such as \"weather_\""
        );
        ensure!(
            !prefixes[..i].contains(prefix),
            "{prefix:?} is listed twice"
        );
    }
    Ok(())
}

/// `[a-z][a-z0-9_]*`, as setting keys are.
fn is_setting_name(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl NetworkRule {
    fn parse(s: &str, settings: &Map<String, Value>) -> anyhow::Result<Self> {
        if let Some(inner) = s.strip_prefix("{{").and_then(|r| r.strip_suffix("}}")) {
            let name = inner.trim();
            let Some(property) = settings.get(name) else {
                bail!("{s:?} names no setting: declare {name:?} in [settings.properties]");
            };
            ensure!(
                property.get("format") == Some(&Value::from("uri")),
                "{s:?}: the setting {name:?} must have format = \"uri\""
            );
            return Ok(Self::Setting(name.to_string()));
        }
        ensure!(
            !s.contains("{{") && !s.contains("}}"),
            "{s:?}: a setting stands alone, as \"{{{{ name }}}}\""
        );
        let Some(wildcard) = s.find('*') else {
            let origin: Origin = s.parse()?;
            public_name(s, &origin)?;
            return Ok(Self::Origin(origin));
        };
        // Only `scheme://*.domain[:port]`: never all hosts, a wildcard scheme or port, or a
        // wildcard inside a name.
        let scheme_end = s.find("://").map_or(0, |i| i + 3);
        ensure!(
            wildcard == scheme_end
                && s[scheme_end..].starts_with("*.")
                && s.matches('*').count() == 1,
            "{s:?}: `*` is allowed only as a whole subdomain at the start of the host, as in \
             https://*.example.com"
        );
        let origin: Origin = format!("{}{}", &s[..scheme_end], &s[scheme_end + 2..])
            .parse()
            .with_context(|| format!("{s:?} is not scheme://*.domain[:port]"))?;
        let domain = public_name(s, &origin)?;
        ensure!(
            domain.contains('.'),
            "{s:?}: a wildcard covers subdomains of a domain such as example.com, not of a \
             top-level domain"
        );
        Ok(Self::Subdomains {
            https: origin.is_https(),
            domain: domain.to_string(),
            port: origin.port(),
        })
    }
}

/// A manifest names public hosts: an address or a local name is the user's to allow, through a
/// URL setting and an origin the configuration declares local. Returns the name.
fn public_name<'a>(entry: &str, origin: &'a Origin) -> anyhow::Result<&'a str> {
    let Some(host) = origin.domain() else {
        bail!(
            "{entry:?}: name a host, not an address; for a server of the user's, use a URL \
             setting (\"{{{{ name }}}}\")"
        );
    };
    // A fully qualified name ("localhost.") is the same host.
    let bare = host.trim_end_matches('.');
    ensure!(
        bare != "localhost" && !bare.ends_with(".localhost"),
        "{entry:?}: localhost is the user's to allow; use a URL setting (\"{{{{ name }}}}\")"
    );
    Ok(host)
}

impl CredentialRequest {
    fn parse(
        raw: RawCredential,
        settings: &Map<String, Value>,
        network: &[NetworkRule],
    ) -> anyhow::Result<Self> {
        let name = raw.name;
        ensure!(
            is_setting_name(&name.replace('-', "_")),
            "credential name {name:?} must be lower case letters, digits, _ and -"
        );
        let injection = match raw.injection.as_str() {
            "basic" => Injection::Basic,
            "bearer" => Injection::Bearer,
            other => {
                let header = other.strip_prefix("header:").with_context(|| {
                    format!(
                        "credential {name:?}: injection {other:?} is not basic, bearer or \
                         header:<Name>"
                    )
                })?;
                let header =
                    http::HeaderName::from_bytes(header.as_bytes()).with_context(|| {
                        format!("credential {name:?}: {header:?} is not a header name")
                    })?;
                ensure!(
                    !wasmtime_wasi_http::DEFAULT_FORBIDDEN_HEADERS.contains(&header),
                    "credential {name:?}: the header {header} cannot carry a credential"
                );
                Injection::Header(header)
            }
        };
        ensure!(
            !raw.origins.is_empty(),
            "credential {name:?} has no origins"
        );
        let origins = raw
            .origins
            .iter()
            .map(|s| {
                let rule = NetworkRule::parse(s, settings)
                    .with_context(|| format!("credential {name:?}"))?;
                ensure!(
                    network.contains(&rule),
                    "credential {name:?}: {s:?} is not in permissions.network"
                );
                Ok(rule)
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self {
            name,
            injection,
            origins,
        })
    }
}

/// Words that name a secret, as whole parts of a setting name (`api_key`, `password`).
const SECRET_WORDS: [&str; 8] = [
    "password",
    "passwd",
    "passphrase",
    "secret",
    "token",
    "apikey",
    "credential",
    "credentials",
];
const SECRET_PAIRS: [(&str, &str); 3] = [("api", "key"), ("private", "key"), ("access", "key")];

fn looks_secret(name: &str) -> bool {
    let words: Vec<&str> = name.split('_').collect();
    words.iter().any(|w| SECRET_WORDS.contains(w))
        || words
            .windows(2)
            .any(|pair| SECRET_PAIRS.contains(&(pair[0], pair[1])))
}

const SCHEMA_KEYS: [&str; 7] = [
    "$schema",
    "title",
    "description",
    "type",
    "properties",
    "required",
    "additionalProperties",
];
const PROPERTY_KEYS: [&str; 8] = [
    "type",
    "title",
    "description",
    "format",
    "default",
    "enum",
    "pattern",
    "examples",
];

static NO_PROPERTIES: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);

/// The settings schema is an object of strings that refuses other properties and holds no
/// secret: credentials are `[[credentials]]`, added by the host. Returns its properties.
fn check_settings(schema: &Value) -> anyhow::Result<&Map<String, Value>> {
    let Some(schema) = schema.as_object() else {
        bail!("must be a JSON Schema table");
    };
    if let Some(key) = schema.keys().find(|k| !SCHEMA_KEYS.contains(&k.as_str())) {
        bail!("unknown key {key:?}; allowed: {}", SCHEMA_KEYS.join(", "));
    }
    ensure!(
        schema.get("type") == Some(&Value::from("object")),
        "type must be \"object\""
    );
    ensure!(
        schema.get("additionalProperties") == Some(&Value::Bool(false)),
        "additionalProperties must be false: a component reads only the settings it declares"
    );
    let properties = match schema.get("properties") {
        None => &*NO_PROPERTIES,
        Some(Value::Object(p)) => p,
        Some(_) => bail!("properties must be a table"),
    };
    for (name, property) in properties {
        ensure!(
            is_setting_name(name),
            "setting {name:?} must match [a-z][a-z0-9_]*"
        );
        let Some(property) = property.as_object() else {
            bail!("setting {name:?} must be a table");
        };
        let secret = looks_secret(name)
            || property.contains_key("x-secret")
            || property.get("writeOnly") == Some(&Value::Bool(true))
            || property.get("format") == Some(&Value::from("password"));
        ensure!(
            !secret,
            "setting {name:?} looks like a secret: settings reach the component, so a secret \
             belongs in [[credentials]], which the host adds to requests itself"
        );
        if let Some(key) = property
            .keys()
            .find(|k| !PROPERTY_KEYS.contains(&k.as_str()))
        {
            bail!(
                "setting {name:?}: unknown key {key:?}; allowed: {}",
                PROPERTY_KEYS.join(", ")
            );
        }
        ensure!(
            property.get("type") == Some(&Value::from("string")),
            "setting {name:?} must have type = \"string\": settings are strings"
        );
        if let Some(format) = property.get("format") {
            ensure!(
                format == "uri",
                "setting {name:?}: format {format} is not supported; the only format is \"uri\""
            );
        }
    }
    match schema.get("required") {
        None => {}
        Some(Value::Array(required)) => {
            for name in required {
                ensure!(
                    name.as_str().is_some_and(|n| properties.contains_key(n)),
                    "required names {name}, which is not in properties"
                );
            }
        }
        Some(_) => bail!("required must be a list of setting names"),
    }
    Ok(properties)
}

/// Each requested limit is positive and within the host's.
fn check_limits(limits: &LimitRequest) -> anyhow::Result<()> {
    let host = Limits::default();
    let ms = |d: Duration| d.as_millis() as u64;
    for (name, value, max) in [
        (
            "memory_mb",
            limits.memory_mb,
            host.memory_bytes as u64 >> 20,
        ),
        (
            "call_timeout_ms",
            limits.call_timeout_ms,
            ms(host.call_timeout),
        ),
        (
            "cpu_ms",
            limits.cpu_ms,
            host.cpu_ticks * ms(crate::host::TICK),
        ),
        (
            "request_timeout_ms",
            limits.request_timeout_ms,
            ms(host.request_timeout),
        ),
    ] {
        if let Some(value) = value {
            ensure!(
                (1..=max).contains(&value),
                "{name} = {value} is outside 1..={max}, the host's limit"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        schema = 1
        id = "org.example.weather"
        version = "1.2.0"
        kind = "component"
        world = "tool"
        api = "0.1"
        description = "Weather forecasts."
        license = "MIT"
        tool_prefixes = ["weather_"]
    "#;

    fn with(extra: &str) -> String {
        format!("{MINIMAL}\n{extra}")
    }

    fn refused(text: &str) -> String {
        format!("{:#}", Manifest::parse(text).unwrap_err())
    }

    fn manifest_of(component: &str) -> Manifest {
        let path = format!(
            "{}/../../components/{component}/fronde-component.toml",
            env!("CARGO_MANIFEST_DIR")
        );
        Manifest::load(Path::new(&path)).unwrap()
    }

    #[test]
    fn minimal_manifest_parses() {
        let m = Manifest::parse(MINIMAL).unwrap();
        assert_eq!(m.id, "org.example.weather");
        assert_eq!(m.version, semver::Version::new(1, 2, 0));
        assert_eq!(m.world, World::Tool);
        assert_eq!(m.tool_prefixes, ["weather_"]);
        assert!(m.network.is_empty() && m.credentials.is_empty());
        assert_eq!(m.limits, LimitRequest::default());
    }

    #[test]
    fn calendar_manifest_declares_its_collections_and_credential() {
        let m = manifest_of("calendar");
        assert_eq!(m.id, "fronde/calendar");
        assert_eq!(m.tool_prefixes, ["calendar_", "reminders_"]);
        assert_eq!(m.api, Api::V0_1);
        let collections = vec![
            NetworkRule::Setting("events_url".into()),
            NetworkRule::Setting("reminders_url".into()),
        ];
        assert_eq!(m.network, collections);
        assert_eq!(
            m.credentials,
            [CredentialRequest {
                name: "caldav".into(),
                injection: Injection::Basic,
                origins: collections,
            }]
        );
        assert_eq!(
            m.settings["required"],
            serde_json::json!(["events_url", "reminders_url"])
        );
    }

    #[test]
    fn echo_manifest_parses() {
        let m = manifest_of("echo");
        assert_eq!(m.id, "fronde/echo");
        assert_eq!(m.tool_prefixes, ["echo_"]);
    }

    #[test]
    fn full_manifest_parses() {
        let m = Manifest::parse(&with(
            r#"
            authors = ["Someone"]
            repository = "https://example.com/weather"

            [permissions]
            network = ["https://api.example.com", "https://*.example.org:8443", "{{ base_url }}"]

            [[credentials]]
            name = "weather-key"
            injection = "header:X-Api-Key"
            origins = ["https://api.example.com"]

            [limits]
            memory_mb = 32
            call_timeout_ms = 20000

            [settings]
            type = "object"
            additionalProperties = false
            [settings.properties.base_url]
            type = "string"
            format = "uri"
            "#,
        ))
        .unwrap();
        assert_eq!(
            m.network,
            [
                NetworkRule::Origin("https://api.example.com".parse().unwrap()),
                NetworkRule::Subdomains {
                    https: true,
                    domain: "example.org".into(),
                    port: 8443
                },
                NetworkRule::Setting("base_url".into()),
            ]
        );
        assert_eq!(
            m.credentials[0].injection,
            Injection::Header(http::HeaderName::from_static("x-api-key"))
        );
        assert_eq!(m.limits.memory_mb, Some(32));
    }

    #[test]
    fn unknown_keys_are_refused() {
        for (extra, key) in [
            ("homepage = \"https://example.com\"", "homepage"),
            ("[permissions]\nfilesystem = [\"/\"]", "filesystem"),
            ("[limits]\nfuel = 1", "fuel"),
            (
                "[[credentials]]\nname = \"a\"\ninjection = \"basic\"\norigins = []\nscope = \"x\"",
                "scope",
            ),
        ] {
            let e = refused(&with(extra));
            assert!(e.contains("unknown field") && e.contains(key), "{e}");
        }
    }

    #[test]
    fn tool_prefixes_are_required_and_checked() {
        let e = refused(&MINIMAL.replace("tool_prefixes = [\"weather_\"]", ""));
        assert!(e.contains("missing field `tool_prefixes`"), "{e}");
        for (prefixes, expected) in [
            ("[]", "list the prefixes"),
            ("[\"weather\"]", "followed by _"),
            ("[\"Weather_\"]", "followed by _"),
            ("[\"_\"]", "followed by _"),
            ("[\"1st_\"]", "followed by _"),
            ("[\"weather_now_\"]", "followed by _"),
            ("[\"weather_\", \"weather_\"]", "listed twice"),
        ] {
            let e = refused(&MINIMAL.replace("[\"weather_\"]", prefixes));
            assert!(
                e.contains("tool_prefixes") && e.contains(expected),
                "{prefixes}: {e}"
            );
        }
        let m = Manifest::parse(&MINIMAL.replace("[\"weather_\"]", "[\"weather_\", \"w2_\"]"));
        assert_eq!(m.unwrap().tool_prefixes, ["weather_", "w2_"]);
    }

    #[test]
    fn identity_is_checked() {
        for (from, to, expected) in [
            ("schema = 1", "schema = 2", "schema 2"),
            ("org.example.weather", "Weather", "reverse-DNS"),
            ("org.example.weather", "weather", "reverse-DNS"),
            ("org.example.weather", "a/b/c", "reverse-DNS"),
            ("1.2.0", "1.2", "semver"),
            ("\"component\"", "\"skill\"", "unknown variant"),
            ("\"tool\"", "\"channel\"", "unknown variant"),
            ("api = \"0.1\"", "api = \"0.2\"", "serves (0.1)"),
            ("Weather forecasts.", " ", "description"),
        ] {
            let e = refused(&MINIMAL.replacen(from, to, 1));
            assert!(e.contains(expected), "{to}: {e}");
        }
        assert!(Manifest::parse(&MINIMAL.replace("org.example.weather", "acme/weather")).is_ok());
    }

    #[test]
    fn network_wildcards_are_only_subdomain_prefixes() {
        for bad in [
            "*",
            "https://*",
            "*://example.com",
            "https://example.com:*",
            "https://api.*.example.com",
            "https://*example.com",
            "https://a*.example.com",
            "https://*.*.example.com",
            "https://*.com",
            "https://*.10.0.0.1",
            "https://example.com/path",
            "example.com",
        ] {
            let e = refused(&with(&format!("[permissions]\nnetwork = [{bad:?}]")));
            assert!(e.contains("permissions.network"), "{bad}: {e}");
        }
    }

    #[test]
    fn network_names_no_address_or_local_host() {
        for bad in [
            "http://127.0.0.1:8080",
            "https://10.0.0.2",
            "https://[::1]",
            "https://[fd00::1]:8443",
            "http://localhost:5232",
            "https://cal.localhost",
            "https://*.dev.localhost",
            "http://localhost.:5232",
            "https://cal.localhost.",
        ] {
            let e = refused(&with(&format!("[permissions]\nnetwork = [{bad:?}]")));
            assert!(e.contains("permissions.network"), "{bad}: {e}");
            assert!(
                e.contains("not an address") || e.contains("localhost is the user's"),
                "{bad}: {e}"
            );
        }
    }

    #[test]
    fn network_settings_must_be_declared_urls() {
        let settings = "[settings]\ntype = \"object\"\nadditionalProperties = false\n\
                        [settings.properties.host]\ntype = \"string\"";
        for (entry, expected) in [
            ("{{ base_url }}", "names no setting"),
            ("{{ host }}", r#"format = "uri""#),
            ("https://{{ host }}", "stands alone"),
        ] {
            let e = refused(&with(&format!(
                "[permissions]\nnetwork = [{entry:?}]\n{settings}"
            )));
            assert!(e.contains(expected), "{entry}: {e}");
        }
    }

    #[test]
    fn credentials_are_checked() {
        let network = "[permissions]\nnetwork = [\"https://api.example.com\"]\n";
        for (credential, expected) in [
            (
                "name = \"k\"\ninjection = \"basic\"\norigins = [\"https://other.example.com\"]",
                "not in permissions.network",
            ),
            (
                "name = \"k\"\ninjection = \"basic\"\norigins = []",
                "no origins",
            ),
            (
                "name = \"k\"\ninjection = \"oauth\"\norigins = [\"https://api.example.com\"]",
                "not basic, bearer",
            ),
            (
                "name = \"k\"\ninjection = \"header:Host\"\norigins = [\"https://api.example.com\"]",
                "cannot carry",
            ),
            (
                "name = \"K y\"\ninjection = \"basic\"\norigins = [\"https://api.example.com\"]",
                "lower case",
            ),
        ] {
            let e = refused(&with(&format!("{network}[[credentials]]\n{credential}")));
            assert!(e.contains(expected), "{credential}: {e}");
        }
        let twice = "[[credentials]]\nname = \"k\"\ninjection = \"bearer\"\norigins = [\"https://api.example.com\"]\n";
        let e = refused(&with(&format!("{network}{twice}{twice}")));
        assert!(e.contains("two credentials"), "{e}");
    }

    #[test]
    fn secrets_in_settings_are_refused() {
        let schema = |property: &str| {
            with(&format!(
                "[settings]\ntype = \"object\"\nadditionalProperties = false\n\
                 [settings.properties.{property}]\ntype = \"string\""
            ))
        };
        for name in [
            "password",
            "api_key",
            "access_token",
            "client_secret",
            "apikey",
        ] {
            let e = refused(&schema(name));
            assert!(e.contains("looks like a secret"), "{name}: {e}");
        }
        for extra in [
            "writeOnly = true",
            "format = \"password\"",
            "x-secret = true",
        ] {
            let e = refused(&format!("{}\n{extra}", schema("pin")));
            assert!(e.contains("looks like a secret"), "{extra}: {e}");
        }
        for fine in ["max_tokens", "keyboard", "username"] {
            assert!(Manifest::parse(&schema(fine)).is_ok(), "{fine}");
        }
    }

    #[test]
    fn settings_schema_is_strict() {
        for (schema, expected) in [
            ("type = \"object\"", "additionalProperties must be false"),
            (
                "type = \"object\"\nadditionalProperties = true",
                "additionalProperties must be false",
            ),
            (
                "type = \"array\"\nadditionalProperties = false",
                "type must be",
            ),
            (
                "type = \"object\"\nadditionalProperties = false\nrequired = [\"url\"]",
                "not in properties",
            ),
            (
                "type = \"object\"\nadditionalProperties = false\n[settings.properties.port]\ntype = \"integer\"",
                "settings are strings",
            ),
            (
                "type = \"object\"\nadditionalProperties = false\nif = true",
                "unknown key",
            ),
            (
                "type = \"object\"\nadditionalProperties = false\n[settings.properties.day]\ntype = \"string\"\nformat = \"date\"",
                "the only format is \"uri\"",
            ),
        ] {
            let e = refused(&with(&format!("[settings]\n{schema}")));
            assert!(e.contains(expected), "{schema}: {e}");
        }
    }

    #[test]
    fn limits_stay_within_the_host() {
        for limit in [
            "memory_mb = 65",
            "memory_mb = 0",
            "call_timeout_ms = 60001",
            "cpu_ms = 10001",
            "request_timeout_ms = 30001",
        ] {
            let e = refused(&with(&format!("[limits]\n{limit}")));
            assert!(e.contains("the host's limit"), "{limit}: {e}");
        }
        assert!(Manifest::parse(&with("[limits]\nmemory_mb = 64\ncpu_ms = 10000")).is_ok());
    }
}
