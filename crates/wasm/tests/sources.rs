//! A component's [[sources]] entry against its manifest and approval: what `prepare` grants or
//! refuses, before anything is compiled.

use anyhow::Context;
use fronde_hub::config::ComponentConfig;
use fronde_wasm::{
    Approval, Approvals, Checks, ComponentHost, ComponentSpec, Credential, Hashes, LoadedComponent,
    NotApproved, NotApprovedReason, Shipped, Stored, prepare,
};
use secrecy::ExposeSecret;
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

/// The user's time zone, and no other source or shipped component.
fn checks(timezone: &str) -> Checks {
    Checks {
        timezone: timezone.into(),
        ..Checks::default()
    }
}

fn spec(config: &ComponentConfig, timezone: &str) -> ComponentSpec {
    prepare(config, &checks(timezone)).unwrap().spec().clone()
}

fn refused_by(config: &ComponentConfig, checks: &Checks) -> String {
    format!("{:#}", prepare(config, checks).unwrap_err())
}

fn refused(config: &ComponentConfig) -> String {
    refused_by(config, &checks("Europe/Paris"))
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
    let prepared = prepare(&config, &checks("Europe/Paris")).unwrap();
    assert_eq!(Some(prepared.sha256()), config.sha256.as_deref());
    let spec: ComponentSpec = prepared.spec().clone();
    assert_eq!(spec.name, "c");
    let allowed: Vec<(String, bool)> = spec
        .allow
        .iter()
        .map(|a| (a.origin.to_string(), a.local))
        .collect();
    assert_eq!(
        allowed,
        [
            ("https://cloud.example".to_string(), false),
            ("https://tasks.example:8443".to_string(), true)
        ]
    );
    let [(origin, Credential::Basic { username, password })] = &spec.credentials[..] else {
        panic!("{:?}", spec.credentials)
    };
    assert_eq!(origin.to_string(), "https://cloud.example");
    assert_eq!(username, "alix");
    assert_eq!(password.expose_secret(), std::env::var("PATH").unwrap());
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
    let spec = spec(&config, "Europe/Paris");
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
    let spec = spec(&entry(dir.path(), ""), "Europe/Paris");
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
    // The echo test component, approved by the hashes in its entry.
    let dist = Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../components/dist"
    ));
    let dir = component(&std::fs::read_to_string(dist.join("echo.toml")).unwrap());
    std::fs::copy(dist.join("echo.wasm"), dir.path().join("c.wasm")).unwrap();
    let prepared = prepare(&entry(dir.path(), ""), &checks("UTC")).unwrap();
    // Replaced between the check and the compilation.
    std::fs::copy(dist.join("calendar.wasm"), dir.path().join("c.wasm")).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let e = rt
        .block_on(LoadedComponent::load(
            &ComponentHost::new().unwrap(),
            prepared,
        ))
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
                && e.contains("https://cloud.example from events_url"),
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
        tool_prefixes = ["feeds_"]
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
        e.contains("asks for no bearer credential for https://cloud.example"),
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
        tool_prefixes = ["weather_"]
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
    spec(&ok, "UTC");
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
        tool_prefixes = ["weather_"]
        [permissions]
        network = ["https://*.weather.example"]
        "#,
    );
    let ok = entry(dir.path(), "allow = [\"https://eu.weather.example\"]\n");
    spec(&ok, "UTC");
    for origin in [
        "https://weather.example",
        "https://evilweather.example",
        "http://eu.weather.example",
    ] {
        let e = refused(&entry(dir.path(), &format!("allow = [{origin:?}]\n")));
        assert!(e.contains("https://*.weather.example:443"), "{origin}: {e}");
    }
}

#[test]
fn check_refuses_what_the_sandbox_would() {
    // A CalDAV server on the local network over plain http: the password would travel in clear.
    let dir = calendar();
    let lan = SETTINGS
        .replace("https://cloud.example", "http://192.168.1.10:5232")
        .replace("https://tasks.example:8443", "http://192.168.1.10:5232");
    let config = entry(
        dir.path(),
        &format!(
            "allow = [{{ origin = \"http://192.168.1.10:5232\", local = true }}]\n{lan}\n\
             [[credentials]]\norigin = \"http://192.168.1.10:5232\"\n\
             basic = {{ username = \"alix\", password = {{ env = \"PATH\" }} }}\n"
        ),
    );
    let e = refused(&config);
    assert!(e.contains("would travel unencrypted"), "{e}");
}

#[test]
fn local_ip_literal_needs_local_true() {
    let dir = calendar();
    let lan = SETTINGS
        .replace("https://cloud.example", "http://192.168.1.10:5232")
        .replace("https://tasks.example:8443", "http://192.168.1.10:5232");
    let config = entry(
        dir.path(),
        &format!("allow = [\"http://192.168.1.10:5232\"]\n{lan}"),
    );
    let e = refused(&config);
    assert!(
        e.contains("{ origin = \"http://192.168.1.10:5232\", local = true }"),
        "{e}"
    );
    // With the flag it passes.
    let config = entry(
        dir.path(),
        &format!("allow = [{{ origin = \"http://192.168.1.10:5232\", local = true }}]\n{lan}"),
    );
    assert!(prepare(&config, &checks("Europe/Paris")).is_ok());
}

#[test]
fn url_settings_hold_no_credentials() {
    let dir = calendar();
    let config = entry(
        dir.path(),
        &SETTINGS.replace("https://cloud.example", "https://alix:s3cret@cloud.example"),
    );
    let e = refused(&config);
    assert!(
        e.contains("events_url has a user name or password")
            && e.contains("[[sources.credentials]]"),
        "{e}"
    );
    assert!(!e.contains("s3cret"), "the password is not echoed: {e}");
}

#[test]
fn tools_outside_the_declared_prefixes_are_refused() {
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
        tool_prefixes: vec!["other_".into()],
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let e = rt
        .block_on(LoadedComponent::load_unapproved(
            &ComponentHost::new().unwrap(),
            spec,
        ))
        .err()
        .unwrap();
    assert!(
        format!("{e:#}").contains("exports the tool echo_text, whose name starts with none of"),
        "{e:#}"
    );
}

/// A `[[sources]]` entry.
fn source(toml: &str) -> fronde_hub::config::SourceConfig {
    toml::from_str(toml).unwrap()
}

fn as_source(config: &ComponentConfig) -> fronde_hub::config::SourceConfig {
    fronde_hub::config::SourceConfig::Component(config.clone())
}

#[test]
fn tool_prefixes_stay_clear_of_other_sources() {
    let dir = calendar();
    let config = entry(dir.path(), SETTINGS);
    let among = |others: Vec<fronde_hub::config::SourceConfig>| {
        let mut sources = vec![as_source(&config)];
        sources.extend(others);
        Checks {
            timezone: "UTC".into(),
            approvals: Approvals::default(),
            sources,
            ..Checks::default()
        }
    };
    // Itself, and sources whose tools it cannot reach.
    let fine = among(vec![source(
        "kind = \"mcp\"\nname = \"mcphub\"\nurl = \"https://mcphub.example/mcp\"",
    )]);
    prepare(&config, &fine).unwrap();

    // An upstream whose tools are reminders_…, or reminders_x_….
    for name in ["reminders", "calendar_sync"] {
        let e = refused_by(
            &config,
            &among(vec![source(&format!(
                "kind = \"mcp\"\nname = \"{name}\"\nurl = \"https://mcphub.example/mcp\""
            ))]),
        );
        assert!(
            e.contains("would mix its tools with those of the source") && e.contains(name),
            "{name}: {e}"
        );
    }

    // Another component declaring calendar_ too.
    let other = calendar();
    let other = entry(other.path(), SETTINGS);
    let mut other_toml = as_source(&other);
    if let fronde_hub::config::SourceConfig::Component(c) = &mut other_toml {
        c.name = "agenda".into();
    }
    let e = refused_by(&config, &among(vec![other_toml]));
    assert!(
        e.contains("\"calendar_\" is also declared by the component \"agenda\""),
        "{e}"
    );
}

#[test]
fn tool_prefixes_of_the_hub_are_reserved() {
    for prefix in ["memory_", "search_", "use_", "hub_"] {
        let dir = component(&format!(
            r#"
            schema = 1
            id = "org.example.notes"
            version = "1.0.0"
            kind = "component"
            world = "tool"
            api = "0.1"
            description = "Notes."
            license = "MIT"
            tool_prefixes = ["notes_", "{prefix}"]
            "#
        ));
        let e = refused(&entry(dir.path(), ""));
        assert!(
            e.contains(&format!("tool prefix \"{prefix}\" is reserved")),
            "{prefix}: {e}"
        );
    }
}

/// The entry for the component in `dir`, followed by `rest`, with no hashes.
fn unpinned(dir: &Path, rest: &str) -> ComponentConfig {
    toml::from_str(&format!(
        "name = \"c\"\npath = {:?}\n{rest}",
        dir.join("c.wasm").display().to_string()
    ))
    .unwrap()
}

/// `shipped` as the only shipped components.
fn shipping(shipped: Vec<Shipped>) -> Checks {
    Checks {
        timezone: "UTC".into(),
        approvals: Approvals {
            shipped,
            stored: vec![],
        },
        sources: vec![],
        ..Checks::default()
    }
}

fn shipped_as(name: &str, hashes: &Hashes) -> Shipped {
    Shipped {
        name: name.into(),
        sha256: hashes.sha256.clone(),
        manifest_sha256: hashes.manifest_sha256.clone(),
    }
}

#[test]
fn a_shipped_component_needs_no_hashes() {
    let dir = calendar();
    let config = unpinned(dir.path(), SETTINGS);
    let hashes = Hashes::of(&config).unwrap();
    let other = Shipped {
        name: "echo".into(),
        sha256: "1".repeat(64),
        manifest_sha256: "2".repeat(64),
    };
    let prepared = prepare(
        &config,
        &shipping(vec![other, shipped_as("calendar", &hashes)]),
    )
    .unwrap();
    assert_eq!(
        *prepared.approval(),
        Approval::Shipped {
            name: "calendar".into()
        }
    );
    assert_eq!(
        prepared.approval().to_string(),
        "approved: shipped with this build (calendar)"
    );
    // The bytes compiled are still checked against the approved hash.
    assert_eq!(prepared.sha256(), hashes.sha256);
}

#[test]
fn files_that_differ_from_the_shipped_ones_are_refused() {
    let dir = calendar();
    let config = unpinned(dir.path(), SETTINGS);
    let hashes = Hashes::of(&config).unwrap();
    // A rebuilt .wasm, and a manifest that asks for more, beside the shipped one.
    for changed in [
        Hashes {
            sha256: "0".repeat(64),
            ..hashes.clone()
        },
        Hashes {
            manifest_sha256: "0".repeat(64),
            ..hashes.clone()
        },
    ] {
        let e = refused_by(&config, &shipping(vec![shipped_as("calendar", &changed)]));
        assert!(
            e.contains("not approved yet: these files are not a component shipped"),
            "{e}"
        );
        assert!(
            e.contains(&format!("sha256 = \"{}\"", hashes.sha256))
                && e.contains(&format!("manifest_sha256 = \"{}\"", hashes.manifest_sha256)),
            "{e}"
        );
    }
    // Half of one shipped pair and half of another approve nothing.
    let e = refused_by(
        &config,
        &shipping(vec![
            Shipped {
                manifest_sha256: "0".repeat(64),
                ..shipped_as("a", &hashes)
            },
            Shipped {
                sha256: "0".repeat(64),
                ..shipped_as("b", &hashes)
            },
        ]),
    );
    assert!(e.contains("not approved yet"), "{e}");
}

#[test]
fn hashes_in_the_entry_win_over_the_shipped_ones() {
    let dir = calendar();
    let hashes = Hashes::of(&unpinned(dir.path(), SETTINGS)).unwrap();
    let checks = shipping(vec![shipped_as("calendar", &hashes)]);
    // Pinned to an older build: refused, although the files are the shipped ones.
    let config = entry(
        dir.path(),
        &format!("sha256 = \"{}\"\n{SETTINGS}", "0".repeat(64)),
    );
    let e = refused_by(&config, &checks);
    assert!(e.contains("c.wasm changed since it was approved"), "{e}");
    // Pinned to these files: approved by the pin.
    let config = entry(dir.path(), SETTINGS);
    assert_eq!(
        *prepare(&config, &checks).unwrap().approval(),
        Approval::Pinned
    );
    // Half a pin is no pin.
    let mut half = unpinned(dir.path(), SETTINGS);
    half.manifest_sha256 = Some(hashes.manifest_sha256.clone());
    let e = refused_by(&half, &checks);
    assert!(e.contains("set both sha256 and manifest_sha256"), "{e}");
}

#[test]
fn without_shipped_components_an_entry_needs_hashes() {
    let dir = calendar();
    let e = refused_by(&unpinned(dir.path(), SETTINGS), &shipping(vec![]));
    assert!(
        e.contains("not approved yet: these files are not a component shipped"),
        "{e}"
    );
}

/// `stored` as the only approvals.
fn storing(stored: Vec<Stored>) -> Checks {
    Checks {
        timezone: "UTC".into(),
        approvals: Approvals {
            shipped: vec![],
            stored,
        },
        sources: vec![],
        ..Checks::default()
    }
}

fn stored_for(source: &str, hashes: &Hashes) -> Stored {
    Stored {
        source: source.into(),
        hashes: hashes.clone(),
    }
}

/// The reason `prepare` refused `config`, with the error itself.
fn not_approved(config: &ComponentConfig, checks: &Checks) -> NotApproved {
    let e = prepare(config, checks).unwrap_err();
    e.downcast_ref::<NotApproved>()
        .unwrap_or_else(|| panic!("not a NotApproved: {e:#}"))
        .clone()
}

#[test]
fn a_stored_approval_approves_these_files_for_this_source() {
    let dir = calendar();
    let config = unpinned(dir.path(), SETTINGS);
    let hashes = Hashes::of(&config).unwrap();
    let prepared = prepare(&config, &storing(vec![stored_for("c", &hashes)])).unwrap();
    assert_eq!(*prepared.approval(), Approval::Stored);
    assert_eq!(
        prepared.approval().to_string(),
        "approved: by the owner, stored in the database"
    );
    // The bytes compiled are still checked against the approved hash.
    assert_eq!(prepared.sha256(), hashes.sha256);
}

#[test]
fn a_stored_approval_of_other_files_is_refused() {
    let dir = calendar();
    let config = unpinned(dir.path(), SETTINGS);
    let hashes = Hashes::of(&config).unwrap();
    // Approved for an earlier build, or an earlier manifest, or half of each.
    for stored in [
        vec![stored_for(
            "c",
            &Hashes {
                sha256: "0".repeat(64),
                ..hashes.clone()
            },
        )],
        vec![stored_for(
            "c",
            &Hashes {
                manifest_sha256: "0".repeat(64),
                ..hashes.clone()
            },
        )],
        vec![
            stored_for(
                "c",
                &Hashes {
                    sha256: "0".repeat(64),
                    ..hashes.clone()
                },
            ),
            stored_for(
                "c",
                &Hashes {
                    manifest_sha256: "0".repeat(64),
                    ..hashes.clone()
                },
            ),
        ],
    ] {
        let refusal = not_approved(&config, &storing(stored.clone()));
        assert_eq!(refusal.reason, NotApprovedReason::Unknown, "{stored:?}");
        assert_eq!(refusal.current, hashes);
        assert!(
            refusal
                .to_string()
                .starts_with("not approved yet: these files are not a component shipped"),
            "{refusal}"
        );
    }
}

#[test]
fn a_stored_approval_approves_no_other_source() {
    let dir = calendar();
    let config = unpinned(dir.path(), SETTINGS);
    let hashes = Hashes::of(&config).unwrap();
    // The same files approved under another name, and no approval at all.
    for stored in [vec![stored_for("agenda", &hashes)], vec![]] {
        let refusal = not_approved(&config, &storing(stored));
        assert_eq!(refusal.source, "c");
        assert_eq!(refusal.reason, NotApprovedReason::Unknown);
    }
}

#[test]
fn hashes_in_the_entry_win_over_stored_approvals() {
    let dir = calendar();
    let hashes = Hashes::of(&unpinned(dir.path(), SETTINGS)).unwrap();
    let checks = storing(vec![stored_for("c", &hashes)]);
    // Pinned to an older build: refused, although these files are approved in the database.
    let config = entry(
        dir.path(),
        &format!("sha256 = \"{}\"\n{SETTINGS}", "0".repeat(64)),
    );
    let refusal = not_approved(&config, &checks);
    assert_eq!(
        refusal.reason,
        NotApprovedReason::Changed {
            file: dir.path().join("c.wasm"),
            sha256: hashes.sha256.clone()
        }
    );
    // Half a pin is no pin, whatever the database approves.
    let mut half = unpinned(dir.path(), SETTINGS);
    half.manifest_sha256 = Some(hashes.manifest_sha256.clone());
    assert_eq!(
        not_approved(&half, &checks).reason,
        NotApprovedReason::HalfPinned
    );
    // Pinned to these files: approved by the pin, not by the row.
    assert_eq!(
        *prepare(&entry(dir.path(), SETTINGS), &checks)
            .unwrap()
            .approval(),
        Approval::Pinned
    );
}

#[test]
fn shipped_files_are_approved_as_shipped_even_when_stored() {
    let dir = calendar();
    let config = unpinned(dir.path(), SETTINGS);
    let hashes = Hashes::of(&config).unwrap();
    let checks = Checks {
        timezone: "UTC".into(),
        approvals: Approvals {
            shipped: vec![shipped_as("calendar", &hashes)],
            stored: vec![stored_for("c", &hashes)],
        },
        sources: vec![],
        ..Checks::default()
    };
    assert_eq!(
        *prepare(&config, &checks).unwrap().approval(),
        Approval::Shipped {
            name: "calendar".into()
        }
    );
}

#[test]
fn not_approved_is_typed() {
    let dir = calendar();
    let hashes = Hashes::of(&unpinned(dir.path(), SETTINGS)).unwrap();
    let manifest = dir.path().join("c.toml");
    let expect = |reason| NotApproved {
        source: "c".into(),
        manifest: manifest.clone(),
        current: hashes.clone(),
        reason,
    };

    let unknown = not_approved(&unpinned(dir.path(), SETTINGS), &storing(vec![]));
    assert_eq!(unknown, expect(NotApprovedReason::Unknown));

    let mut half = unpinned(dir.path(), SETTINGS);
    half.sha256 = Some(hashes.sha256.clone());
    assert_eq!(
        not_approved(&half, &storing(vec![])),
        expect(NotApprovedReason::HalfPinned)
    );

    let wasm = entry(
        dir.path(),
        &format!("sha256 = \"{}\"\n{SETTINGS}", "0".repeat(64)),
    );
    assert_eq!(
        not_approved(&wasm, &storing(vec![])),
        expect(NotApprovedReason::Changed {
            file: dir.path().join("c.wasm"),
            sha256: hashes.sha256.clone()
        })
    );

    let toml = entry(
        dir.path(),
        &format!("manifest_sha256 = \"{}\"\n{SETTINGS}", "0".repeat(64)),
    );
    assert_eq!(
        not_approved(&toml, &storing(vec![])),
        expect(NotApprovedReason::Changed {
            file: manifest.clone(),
            sha256: hashes.manifest_sha256.clone()
        })
    );
}

#[test]
fn downcast_through_context() {
    let dir = calendar();
    let config = unpinned(dir.path(), SETTINGS);
    let e = prepare(&config, &storing(vec![]))
        .context("source c")
        .context("starting the hub")
        .unwrap_err();
    let refusal = e.downcast_ref::<NotApproved>().expect("NotApproved");
    assert_eq!(refusal.reason, NotApprovedReason::Unknown);
    assert_eq!(
        format!("{e:#}"),
        format!("starting the hub: source c: {refusal}")
    );
}

/// The messages written out in full, as they were before the refusal had a type, so that the
/// docs and the tests matching parts of them stay true.
#[test]
fn not_approved_displays_as_before() {
    let refusal = |reason| NotApproved {
        source: "calendar".into(),
        manifest: "/c/calendar.toml".into(),
        current: Hashes {
            sha256: "aa".into(),
            manifest_sha256: "bb".into(),
        },
        reason,
    };
    assert_eq!(
        refusal(NotApprovedReason::Unknown).to_string(),
        "not approved yet: these files are not a component shipped with this build of Fronde. \
         Read its manifest /c/calendar.toml, then add sha256 = \"aa\" and manifest_sha256 = \
         \"bb\" to its [[sources]] entry"
    );
    assert_eq!(
        refusal(NotApprovedReason::HalfPinned).to_string(),
        "not approved yet: set both sha256 and manifest_sha256, or neither for a component \
         shipped with this build. Read its manifest /c/calendar.toml, then add sha256 = \"aa\" \
         and manifest_sha256 = \"bb\" to its [[sources]] entry"
    );
    assert_eq!(
        refusal(NotApprovedReason::Changed {
            file: "/c/calendar.wasm".into(),
            sha256: "aa".into()
        })
        .to_string(),
        "/c/calendar.wasm changed since it was approved (its sha256 is now aa); read the \
         manifest /c/calendar.toml again, then update the hashes in its [[sources]] entry \
         (`fronde check` prints them)"
    );
}

/// `Checks::load` reads the approvals stored in the database, bound to their source and hashes.
#[tokio::test]
async fn checks_load_reads_stored_approvals() {
    let data = tempfile::tempdir().unwrap();
    let db = fronde_db::open(data.path()).await.unwrap();
    let dir = calendar();
    let component = unpinned(dir.path(), SETTINGS);
    let hashes = Hashes::of(&component).unwrap();
    let config: fronde_hub::config::Config = toml::from_str(&format!(
        "[server]\ntoken = {{ env = \"FRONDE_UNUSED\" }}\n[data]\ndir = {:?}\ntimezone = \"UTC\"\n\
         [secrets]\nmaster_key = {{ file = {:?} }}\n",
        data.path().display().to_string(),
        data.path().join("master.key").display().to_string()
    ))
    .unwrap();

    let before = Checks::load(&config, &db).await.unwrap();
    assert!(before.approvals.stored.is_empty());
    assert_eq!(
        not_approved(&component, &before).reason,
        NotApprovedReason::Unknown
    );

    fronde_db::approvals::add(&db, "c", &hashes.sha256, &hashes.manifest_sha256)
        .await
        .unwrap();
    fronde_db::approvals::add(&db, "agenda", &"0".repeat(64), &hashes.manifest_sha256)
        .await
        .unwrap();
    // Approving the same files again changes nothing.
    fronde_db::approvals::add(&db, "c", &hashes.sha256, &hashes.manifest_sha256)
        .await
        .unwrap();
    let after = Checks::load(&config, &db).await.unwrap();
    assert_eq!(after.approvals.stored.len(), 2);
    assert_eq!(after.timezone, "UTC");
    assert_eq!(
        *prepare(&component, &after).unwrap().approval(),
        Approval::Stored
    );
}
