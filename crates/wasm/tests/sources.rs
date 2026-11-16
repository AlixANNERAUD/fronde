//! A component's [[sources]] entry against its manifest and approval: what `prepare` grants or
//! refuses, before anything is compiled.

use fronde_hub::config::ComponentConfig;
use fronde_wasm::{ComponentHost, ComponentSpec, Credential, Hashes, LoadedComponent, prepare};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const CALENDAR_MANIFEST: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../components/calendar/fronde-component.toml"
);

/// A directory with `c.wasm` (not compiled here) and its manifest `c.toml`.
fn component(manifest: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("c.wasm"), b"\0asm, or so").unwrap();
    std::fs::write(dir.path().join("c.toml"), manifest).unwrap();
    dir
}

fn calendar() -> tempfile::TempDir {
    component(&std::fs::read_to_string(CALENDAR_MANIFEST).unwrap())
}

/// The entry for the component in `dir`, followed by `rest`, approved unless `rest` sets the
/// hashes.
fn entry(dir: &Path, rest: &str) -> ComponentConfig {
    let mut config: ComponentConfig = toml::from_str(&format!(
        "name = \"c\"\npath = {:?}\n{rest}",
        dir.join("c.wasm").display().to_string()
    ))
    .unwrap();
    let hashes = Hashes::of(&config).unwrap();
    config.sha256.get_or_insert(hashes.sha256);
    config.manifest_sha256.get_or_insert(hashes.manifest_sha256);
    config
}

const SETTINGS: &str = r#"
[settings]
events_url = "https://cloud.example/dav/personal/"
reminders_url = "https://tasks.example:8443/dav/tasks/"
"#;

fn refused(config: &ComponentConfig) -> String {
    format!("{:#}", prepare(config, "Europe/Paris").unwrap_err())
}

#[test]
fn grants_what_the_manifest_asks() {
    let dir = calendar();
    let config = entry(
        dir.path(),
        &format!(
            "allow = [\"https://cloud.example\", {{ origin = \"https://tasks.example:8443\", local = true }}]\n\
             {SETTINGS}\n[[credentials]]\norigin = \"https://cloud.example\"\n\
             basic = {{ username = \"alix\", password = {{ env = \"PATH\" }} }}\n"
        ),
    );
    let spec: ComponentSpec = prepare(&config, "Europe/Paris").unwrap();
    assert_eq!(spec.name, "c");
    assert_eq!(spec.sha256, config.sha256);
    let allowed: Vec<(String, bool)> = spec
        .allow
        .iter()
        .map(|a| (a.origin.to_string(), a.local))
        .collect();
    assert_eq!(
        allowed,
        [
            ("https://cloud.example:443".to_string(), false),
            ("https://tasks.example:8443".to_string(), true)
        ]
    );
    let [(origin, Credential::Basic { username, password })] = &spec.credentials[..] else {
        panic!("{:?}", spec.credentials)
    };
    assert_eq!(origin.to_string(), "https://cloud.example:443");
    assert_eq!(username, "alix");
    assert_eq!(*password, std::env::var("PATH").unwrap());
    // The host gives the user's time zone to a component that declares the setting.
    assert_eq!(spec.settings["timezone"], "Europe/Paris");
}

#[test]
fn settings_timezone_set_by_the_user_wins() {
    let dir = calendar();
    let config = entry(
        dir.path(),
        &format!("{SETTINGS}timezone = \"America/New_York\"\n"),
    );
    let spec = prepare(&config, "Europe/Paris").unwrap();
    assert_eq!(spec.settings["timezone"], "America/New_York");
}

#[test]
fn timezone_is_given_only_when_declared() {
    let echo = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../components/echo/fronde-component.toml"
    ))
    .unwrap();
    let dir = component(&echo);
    let spec = prepare(&entry(dir.path(), ""), "Europe/Paris").unwrap();
    assert!(spec.settings.is_empty(), "{:?}", spec.settings);
}

#[test]
fn not_approved_shows_the_hashes() {
    let dir = calendar();
    let mut config = entry(dir.path(), SETTINGS);
    let hashes = Hashes::of(&config).unwrap();
    config.sha256 = None;
    let e = refused(&config);
    assert!(e.contains("not approved yet"), "{e}");
    assert!(
        e.contains(&format!("sha256 = \"{}\"", hashes.sha256))
            && e.contains(&format!("manifest_sha256 = \"{}\"", hashes.manifest_sha256)),
        "{e}"
    );
}

#[test]
fn sha256_mismatch_refused() {
    let dir = calendar();
    let config = entry(dir.path(), SETTINGS);
    // A new build of the component.
    std::fs::write(dir.path().join("c.wasm"), b"\0asm, rebuilt").unwrap();
    let e = refused(&config);
    assert!(e.contains("c.wasm changed since it was approved"), "{e}");
    let now = Hashes::of(&config).unwrap().sha256;
    assert!(e.contains(&now), "{e}");

    // A manifest that asks for more.
    let config = entry(dir.path(), SETTINGS);
    let wider = std::fs::read_to_string(CALENDAR_MANIFEST)
        .unwrap()
        .replace("network = [", "network = [\"https://evil.example\", ");
    std::fs::write(dir.path().join("c.toml"), wider).unwrap();
    let e = refused(&config);
    assert!(e.contains("c.toml changed since it was approved"), "{e}");
}

#[test]
fn compiling_checks_the_approved_hash_again() {
    let echo = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../components/dist/echo.wasm"
    ));
    let spec = ComponentSpec {
        name: "echo".into(),
        path: echo,
        allow: vec![],
        credentials: vec![],
        settings: BTreeMap::new(),
        sha256: Some("0".repeat(64)),
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let e = rt
        .block_on(LoadedComponent::load(&ComponentHost::new().unwrap(), spec))
        .err()
        .unwrap();
    assert!(
        format!("{e:#}").contains("changed since it was approved"),
        "{e:#}"
    );
}

#[test]
fn grant_outside_the_manifest_refused() {
    let dir = calendar();
    // Same host as events_url, another port; and a host no setting names.
    for origin in ["https://cloud.example:444", "https://evil.example"] {
        let config = entry(dir.path(), &format!("allow = [{origin:?}]\n{SETTINGS}"));
        let e = refused(&config);
        assert!(
            e.contains("which the manifest does not ask for")
                && e.contains("https://cloud.example:443 from events_url"),
            "{e}"
        );
    }
    // Without the setting, its origin is not asked for at all.
    let dir = component(
        r#"
        schema = 1
        id = "org.example.feeds"
        version = "1.0.0"
        kind = "component"
        world = "tool"
        api = "0.1"
        description = "Feeds."
        license = "MIT"
        [permissions]
        network = ["{{ server }}"]
        [settings]
        type = "object"
        additionalProperties = false
        [settings.properties.server]
        type = "string"
        format = "uri"
        "#,
    );
    let e = refused(&entry(dir.path(), "allow = [\"https://a.example\"]\n"));
    assert!(e.contains("the origin of server, which is not set"), "{e}");
}

#[test]
fn credential_the_manifest_does_not_ask_for_refused() {
    let dir = calendar();
    let config = entry(
        dir.path(),
        &format!(
            "allow = [\"https://cloud.example\"]\n{SETTINGS}\n[[credentials]]\n\
             origin = \"https://cloud.example\"\nbearer = {{ env = \"PATH\" }}\n"
        ),
    );
    let e = refused(&config);
    assert!(
        e.contains("asks for no bearer credential for https://cloud.example:443"),
        "{e}"
    );
}

#[test]
fn component_secrets_are_resolved() {
    let dir = calendar();
    let config = entry(
        dir.path(),
        &format!(
            "allow = [\"https://cloud.example\"]\n{SETTINGS}\n[[credentials]]\n\
             origin = \"https://cloud.example\"\n\
             basic = {{ username = \"alix\", password = {{ env = \"FRONDE_TEST_ABSENT_CALDAV\" }} }}\n"
        ),
    );
    let e = refused(&config);
    assert!(e.contains("FRONDE_TEST_ABSENT_CALDAV"), "{e}");
}

#[test]
fn settings_follow_the_manifest_schema() {
    let dir = calendar();
    let e = refused(&entry(dir.path(), &format!("{SETTINGS}colour = \"red\"\n")));
    assert!(
        e.contains("\"colour\" is not a setting") && e.contains("events_url"),
        "{e}"
    );
    let e = refused(&entry(
        dir.path(),
        "[settings]\nevents_url = \"https://cloud.example/\"\n",
    ));
    assert!(e.contains("reminders_url is required"), "{e}");
    let e = refused(&entry(
        dir.path(),
        &SETTINGS.replace("https://cloud.example/dav/personal/", "not a url"),
    ));
    assert!(e.contains("events_url = \"not a url\" is not a URL"), "{e}");
}

#[test]
fn enum_and_pattern_are_checked() {
    let dir = component(
        r#"
        schema = 1
        id = "org.example.weather"
        version = "1.0.0"
        kind = "component"
        world = "tool"
        api = "0.1"
        description = "Weather."
        license = "MIT"
        [settings]
        type = "object"
        additionalProperties = false
        [settings.properties.units]
        type = "string"
        enum = ["metric", "imperial"]
        [settings.properties.station]
        type = "string"
        pattern = "^[A-Z]{4}$"
        "#,
    );
    let ok = entry(
        dir.path(),
        "[settings]\nunits = \"metric\"\nstation = \"LFPG\"\n",
    );
    prepare(&ok, "UTC").unwrap();
    let e = refused(&entry(dir.path(), "[settings]\nunits = \"kelvin\"\n"));
    assert!(e.contains("is not one of"), "{e}");
    let e = refused(&entry(dir.path(), "[settings]\nstation = \"paris\"\n"));
    assert!(e.contains("does not match"), "{e}");
}

#[test]
fn subdomain_requests_cover_subdomains_only() {
    let dir = component(
        r#"
        schema = 1
        id = "org.example.weather"
        version = "1.0.0"
        kind = "component"
        world = "tool"
        api = "0.1"
        description = "Weather."
        license = "MIT"
        [permissions]
        network = ["https://*.weather.example"]
        "#,
    );
    let ok = entry(dir.path(), "allow = [\"https://eu.weather.example\"]\n");
    prepare(&ok, "UTC").unwrap();
    for origin in [
        "https://weather.example",
        "https://evilweather.example",
        "http://eu.weather.example",
    ] {
        let e = refused(&entry(dir.path(), &format!("allow = [{origin:?}]\n")));
        assert!(e.contains("https://*.weather.example:443"), "{origin}: {e}");
    }
}
