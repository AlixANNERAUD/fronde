use fronde_hub::config::{Secret, SourceConfig, load};
use secrecy::ExposeSecret;
use std::io::Write;

fn load_str(s: &str) -> anyhow::Result<fronde_hub::config::Config> {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(s.as_bytes()).unwrap();
    load(f.path())
}

const MIN: &str = r#"
[server]
token = { env = "T" }
[data]
dir = "/tmp/x"
timezone = "Europe/Paris"
"#;

#[test]
fn loads_example() {
    let c = load(std::path::Path::new("../../fronde.example.toml")).unwrap();
    assert_eq!(c.data.timezone, chrono_tz::Europe::Paris);
    let kinds: Vec<_> = c
        .sources
        .iter()
        .map(|s| match s {
            SourceConfig::Mcp(m) => ("mcp", m.name.as_str()),
            SourceConfig::Component(c) => ("component", c.name.as_str()),
        })
        .collect();
    assert_eq!(kinds, [("component", "calendar"), ("mcp", "mcphub")]);
    assert_eq!(c.data.user, "alix");
    assert_eq!(
        c.master_key_file().unwrap(),
        std::path::Path::new("/keys/master.key")
    );
    assert!(c.web.is_some());
    assert_eq!(
        c.python.unwrap().dir,
        std::path::PathBuf::from("/app/python")
    );
}

#[test]
fn defaults_apply() {
    let c = load_str(MIN).unwrap();
    assert_eq!(c.server.listen.to_string(), "127.0.0.1:8484");
    assert_eq!(c.data.user, "me");
    assert_eq!(
        (c.memory.memory_limit, c.memory.user_limit, c.memory.git),
        (4000, 2000, true)
    );
    assert!(c.sources.is_empty());
    assert!(c.web.is_none());
    assert_eq!(c.agent.command, ["claude-agent-acp"]);
    assert!(c.agent.env.is_empty());
    assert_eq!(c.agent.builtin_tools, ["WebSearch", "WebFetch"]);
}

#[test]
fn builtin_tools_can_be_narrowed_or_emptied() {
    let c = load_str(&format!("{MIN}\n[agent]\nbuiltin_tools = [\"WebFetch\"]\n")).unwrap();
    assert_eq!(c.agent.builtin_tools, ["WebFetch"]);
    let c = load_str(&format!("{MIN}\n[agent]\nbuiltin_tools = []\n")).unwrap();
    assert!(c.agent.builtin_tools.is_empty());
}

#[test]
fn rejects_unsafe_builtin_tool() {
    let s = format!("{MIN}\n[agent]\nbuiltin_tools = [\"WebSearch\", \"Bash\"]\n");
    let e = format!("{:#}", load_str(&s).unwrap_err());
    assert!(e.contains("\"Bash\" is not allowed"), "{e}");
    assert!(e.contains("WebSearch"), "{e}");
}

#[test]
fn loads_web_and_agent() {
    let s = format!(
        "{MIN}\n[web]\npassword = {{ env = \"P\" }}\nsecure_cookie = true\n[agent]\ncommand = [\"npx\", \"claude-agent-acp\"]\nenv = {{ CLAUDE_CODE_OAUTH_TOKEN_FILE = \"/run/secrets/claude\" }}\n"
    );
    let c = load_str(&s).unwrap();
    let web = c.web.unwrap();
    assert!(matches!(web.password, Secret::Env { env } if env == "P"));
    assert!(web.secure_cookie);
    assert_eq!(c.agent.command, ["npx", "claude-agent-acp"]);
    assert_eq!(
        c.agent
            .env
            .get("CLAUDE_CODE_OAUTH_TOKEN_FILE")
            .map(String::as_str),
        Some("/run/secrets/claude")
    );
    let s = format!("{MIN}\n[web]\npassword = {{ env = \"P\" }}\n");
    assert!(!load_str(&s).unwrap().web.unwrap().secure_cookie);
}

#[test]
fn rejects_password_written_in_the_toml() {
    let s = format!("{MIN}\n[web]\npassword = \"hunter2\"\n");
    assert!(load_str(&s).is_err());
}

#[test]
fn rejects_unknown_key() {
    let e = load_str(&MIN.replace("[server]", "[server]\nlistn = \"x\"")).unwrap_err();
    assert!(format!("{e:#}").contains("listn"), "{e:#}");
}

#[test]
fn rejects_url_and_command() {
    let s = format!(
        "{MIN}\n[[sources]]\nkind = \"mcp\"\nname = \"a\"\nurl = \"http://x/mcp\"\ncommand = [\"x\"]\n"
    );
    let e = load_str(&s).unwrap_err();
    assert!(format!("{e:#}").contains("exactly one of"), "{e:#}");
    let s = format!("{MIN}\n[[sources]]\nkind = \"mcp\"\nname = \"a\"\n");
    assert!(format!("{:#}", load_str(&s).unwrap_err()).contains("exactly one of"));
}

#[test]
fn rejects_bad_timezone() {
    assert!(load_str(&MIN.replace("Europe/Paris", "Paris")).is_err());
}

#[test]
fn rejects_bad_tool_prefix_name() {
    let s =
        format!("{MIN}\n[[sources]]\nkind = \"mcp\"\nname = \"MCP Hub\"\nurl = \"http://x/mcp\"\n");
    let e = load_str(&s).unwrap_err();
    assert!(format!("{e:#}").contains("^[a-z][a-z0-9_]"), "{e:#}");
}

#[test]
fn secret_from_env_and_file() {
    // Setting a variable is unsafe in edition 2024, so read one every process already has.
    assert_eq!(
        Secret::Env { env: "PATH".into() }
            .resolve()
            .unwrap()
            .expose_secret(),
        std::env::var("PATH").unwrap()
    );
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(b"s3cret\n").unwrap();
    assert_eq!(
        Secret::File {
            file: f.path().into()
        }
        .resolve()
        .unwrap()
        .expose_secret(),
        "s3cret"
    );
}

/// A value that is not UTF-8 is named, never shown. Setting a variable is unsafe in edition 2024,
/// so the test runs itself again in a child process that has it.
#[cfg(unix)]
#[test]
fn env_secret_that_is_not_utf8_never_prints() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    const VAR: &str = "FRONDE_TEST_NOT_UTF8_SECRET";
    const NAME: &str = "env_secret_that_is_not_utf8_never_prints";
    let value = OsString::from_vec(b"canary-\xff\xfe-s3cret".to_vec());

    if let Some(given) = std::env::var_os(VAR) {
        assert_eq!(given, value);
        let e = Secret::Env { env: VAR.into() }.resolve().unwrap_err();
        let shown = [format!("{e}"), format!("{e:#}"), format!("{e:?}")];
        let debug = format!("{value:?}");
        let lossy = value.to_string_lossy().into_owned();
        for text in &shown {
            assert!(text.contains(VAR) && text.contains("valid UTF-8"), "{text}");
            for leak in ["canary", "s3cret", debug.as_str(), lossy.as_str()] {
                assert!(!text.contains(leak), "{leak:?} in {text}");
            }
        }
        return;
    }

    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([NAME, "--exact", "--nocapture", "--test-threads=1"])
        .env(VAR, &value)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    // The child ran this very test, not an empty filter.
    assert!(stdout.contains("1 passed"), "{stdout}\n{stderr}");
}

#[test]
fn missing_secret_explains() {
    let e = Secret::Env {
        env: "ABSENT_X".into(),
    }
    .resolve()
    .unwrap_err()
    .to_string();
    assert!(e.contains("ABSENT_X") && e.contains("set"), "{e}");
}

#[test]
fn rejects_reserved_and_duplicate_upstream_names() {
    for name in [
        "hub",
        "hub_extra",
        "memory",
        "memory_extra",
        "search",
        "use",
        "fronde",
        "assistant",
    ] {
        let s = format!(
            "{MIN}\n[[sources]]\nkind = \"mcp\"\nname = {name:?}\nurl = \"http://x/mcp\"\n"
        );
        let err = format!("{:#}", load_str(&s).unwrap_err());
        assert!(
            err.contains("reserved") && err.contains("pick another name"),
            "{name}: {err}"
        );
    }
    // Non-overlapping names starting like a reserved prefix stay allowed.
    for name in ["mem", "memory2", "hub2"] {
        let s = format!(
            "{MIN}\n[[sources]]\nkind = \"mcp\"\nname = {name:?}\nurl = \"http://x/mcp\"\n"
        );
        load_str(&s).unwrap_or_else(|e| panic!("{name}: {e:#}"));
    }
    // Names are unique across kinds.
    let s = format!(
        "{MIN}\n[[sources]]\nkind = \"mcp\"\nname = \"a\"\nurl = \"http://x/mcp\"\n[[sources]]\nkind = \"component\"\nname = \"a\"\npath = \"a.wasm\"\n"
    );
    assert!(format!("{:#}", load_str(&s).unwrap_err()).contains("twice"));
}

#[test]
fn data_dir_is_optional() {
    let c = load_str("[server]\ntoken = { env = \"T\" }\n[data]\ntimezone = \"UTC\"\n").unwrap();
    assert!(c.data.dir.is_absolute() && c.data.dir.ends_with("fronde"));
}

#[test]
fn relative_secret_files_are_read_from_the_config_directory() {
    let dir = tempfile::tempdir().unwrap();
    fronde_hub::private::write(dir.path().join("tok"), "from-config-dir\n").unwrap();
    let path = dir.path().join("fronde.toml");
    std::fs::write(
        &path,
        "[server]\ntoken = { file = \"tok\" }\n[data]\ndir = \"/tmp/x\"\ntimezone = \"UTC\"\n",
    )
    .unwrap();
    let c = load(&path).unwrap();
    assert_eq!(
        c.server.token.resolve().unwrap().expose_secret(),
        "from-config-dir"
    );
}

#[test]
fn rejects_unparsable_listen() {
    let e = load_str(
        "[server]\nlisten = \"nope\"\ntoken = { env = \"T\" }\n[data]\ntimezone = \"UTC\"\n",
    )
    .unwrap_err();
    assert!(format!("{e:#}").contains("listen"), "{e:#}");
}

#[cfg(unix)]
#[test]
fn prepare_data_dir_creates_it_private_and_reports_unwritable() {
    use fronde_hub::private::prepare_data_dir;
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let ok = t.path().join("a/b");
    prepare_data_dir(&ok).unwrap();
    assert_eq!(
        std::fs::metadata(&ok).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(std::fs::read_dir(&ok).unwrap().count(), 0, "probe removed");

    let locked = t.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    if std::fs::File::create(locked.join("x")).is_ok() {
        return; // running as root: permissions do not apply
    }
    let e = prepare_data_dir(&locked.join("data"))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains(&locked.display().to_string()) && e.contains("set [data] dir"),
        "{e}"
    );
    let e = prepare_data_dir(&locked).unwrap_err().to_string();
    assert!(
        e.contains("not writable") && e.contains("~/.local/share/fronde"),
        "{e}"
    );
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn xdg_data_dir_follows_the_spec() {
    use fronde_hub::config::xdg_data_dir;
    use std::path::PathBuf;
    let p = |s: &str| Some(PathBuf::from(s));
    assert_eq!(
        xdg_data_dir(p("/x/data"), p("/home/a")),
        p("/x/data/fronde")
    );
    assert_eq!(
        xdg_data_dir(None, p("/home/a")),
        p("/home/a/.local/share/fronde")
    );
    // A relative XDG_DATA_HOME is ignored, as the spec says.
    assert_eq!(
        xdg_data_dir(p("rel"), p("/home/a")),
        p("/home/a/.local/share/fronde")
    );
    // A relative or missing home gives no default rather than a relative directory.
    assert_eq!(xdg_data_dir(None, p("home")), None);
    assert_eq!(xdg_data_dir(None, None), None);
}

/// A component entry for `allow`, `settings` and `credentials` to follow.
fn component(rest: &str) -> String {
    format!(
        "{MIN}\n[[sources]]\nkind = \"component\"\nname = \"calendar\"\npath = \"calendar.wasm\"\n{rest}"
    )
}

fn refused(s: &str) -> String {
    format!("{:#}", load_str(s).unwrap_err())
}

#[test]
fn loads_a_component_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fronde.toml");
    let text = component(
        "sha256 = \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"\n\
         allow = [\"https://cloud.example\", { origin = \"http://192.168.1.10:5232\", local = true }]\n\
         [sources.settings]\nevents_url = \"https://cloud.example/e/\"\n\
         [[sources.credentials]]\norigin = \"https://cloud.example\"\n\
         basic = { username = \"alix\", password = { file = \"caldav\" } }\n\
         [[sources.credentials]]\norigin = \"http://192.168.1.10:5232\"\nbearer = { env = \"T\" }\n",
    );
    std::fs::write(&path, text).unwrap();
    let c = load(&path).unwrap();
    let [SourceConfig::Component(c)] = &c.sources[..] else {
        panic!("{:?}", c.sources)
    };
    // Paths are read from the config file's directory.
    assert_eq!(c.path, dir.path().join("calendar.wasm"));
    assert_eq!(c.manifest_path(), dir.path().join("calendar.toml"));
    assert_eq!(
        (c.allow[0].origin.to_string(), c.allow[0].local),
        ("https://cloud.example".to_string(), false)
    );
    assert_eq!(
        (c.allow[1].origin.to_string(), c.allow[1].local),
        ("http://192.168.1.10:5232".to_string(), true)
    );
    assert_eq!(c.credentials[1].origin, c.allow[1].origin);
    assert_eq!(c.settings["events_url"], "https://cloud.example/e/");
    let basic = c.credentials[0].basic.as_ref().unwrap();
    assert_eq!(basic.username, "alix");
    assert!(matches!(&basic.password, Secret::File { file } if *file == dir.path().join("caldav")));
    assert!(c.manifest_sha256.is_none());
}

#[test]
fn component_rejects_unknown_key() {
    let e = refused(&component("alow = []\n"));
    assert!(e.contains("alow"), "{e}");
    let e = refused(&component(
        "allow = [{ origin = \"https://a.example\", locale = true }]\n",
    ));
    assert!(e.contains("locale"), "{e}");
    let e = refused(&component(
        "allow = [\"https://a.example\"]\n[[sources.credentials]]\norigin = \"https://a.example\"\nbasic = { username = \"a\", pasword = { env = \"P\" } }\n",
    ));
    assert!(e.contains("pasword"), "{e}");
}

#[test]
fn component_origins_are_bare() {
    for bad in [
        "https://a.example/dav/",
        "ftp://a.example",
        "https://u@a.example",
    ] {
        let e = refused(&component(&format!("allow = [{bad:?}]\n")));
        assert!(e.contains("origin"), "{bad}: {e}");
    }
}

#[test]
fn rejects_duplicate_allowed_origin() {
    // The same origin spelled two ways.
    let e = refused(&component(
        "allow = [\"https://Cloud.example\", { origin = \"https://cloud.example:443\", local = true }]\n",
    ));
    assert!(e.contains("https://cloud.example is in allow twice"), "{e}");
    // Addresses are normalised as requests' hosts are: these are one origin too.
    for (a, b) in [
        ("http://0x7f.1:5232", "http://127.0.0.1:5232"),
        ("http://[0:0:0:0:0:0:0:1]", "http://[::1]:80"),
    ] {
        let e = refused(&component(&format!(
            "allow = [{{ origin = {a:?}, local = true }}, {{ origin = {b:?}, local = true }}]\n"
        )));
        assert!(e.contains("is in allow twice"), "{a} and {b}: {e}");
    }
}

#[test]
fn credential_origin_matches_its_allowed_spelling() {
    let c = load_str(&component(
        "allow = [{ origin = \"http://0x7f.1:5232\", local = true }]\n\
         [[sources.credentials]]\norigin = \"http://127.0.0.1:5232/\"\nbearer = { env = \"T\" }\n",
    ))
    .unwrap();
    let [SourceConfig::Component(c)] = &c.sources[..] else {
        panic!("{:?}", c.sources)
    };
    assert_eq!(c.allow[0].origin.to_string(), "http://127.0.0.1:5232");
    assert_eq!(c.credentials[0].origin, c.allow[0].origin);
}

#[test]
fn credential_origin_must_be_allowed() {
    let e = refused(&component(
        "allow = [\"https://a.example\"]\n[[sources.credentials]]\norigin = \"https://b.example\"\nbearer = { env = \"T\" }\n",
    ));
    assert!(
        e.contains("https://b.example") && e.contains("not in allow"),
        "{e}"
    );
}

#[test]
fn credential_needs_exactly_one_kind_and_no_plain_secret() {
    let both = "allow = [\"https://a.example\"]\n[[sources.credentials]]\norigin = \"https://a.example\"\nbearer = { env = \"T\" }\nbasic = { username = \"a\", password = { env = \"P\" } }\n";
    assert!(refused(&component(both)).contains("exactly one of basic or bearer"));
    let none = "allow = [\"https://a.example\"]\n[[sources.credentials]]\norigin = \"https://a.example\"\n";
    assert!(refused(&component(none)).contains("exactly one of basic or bearer"));
    let plain = "allow = [\"https://a.example\"]\n[[sources.credentials]]\norigin = \"https://a.example\"\nbearer = \"tok\"\n";
    assert!(load_str(&component(plain)).is_err());
    let twice = "allow = [\"https://a.example\"]\n[[sources.credentials]]\norigin = \"https://a.example\"\nbearer = { env = \"T\" }\n[[sources.credentials]]\norigin = \"https://a.example\"\nbearer = { env = \"U\" }\n";
    assert!(refused(&component(twice)).contains("two credentials"));
}

#[test]
fn a_credential_can_name_a_stored_secret() {
    let c = load_str(&component(
        "allow = [\"https://a.example\", \"https://b.example\"]\n\
         [[sources.credentials]]\norigin = \"https://a.example\"\n\
         basic = { username = \"alix\", password = { stored = \"caldav\" } }\n\
         [[sources.credentials]]\norigin = \"https://b.example\"\nbearer = { stored = \"api.token-2\" }\n",
    ))
    .unwrap();
    let [SourceConfig::Component(c)] = &c.sources[..] else {
        panic!("{:?}", c.sources)
    };
    let password = &c.credentials[0].basic.as_ref().unwrap().password;
    assert!(matches!(password, Secret::Stored { stored } if stored == "caldav"));
    let bearer = c.credentials[1].bearer.as_ref().unwrap();
    assert!(matches!(bearer, Secret::Stored { stored } if stored == "api.token-2"));
    // Only the component's preparation opens it.
    let e = format!("{:#}", bearer.resolve().unwrap_err());
    assert!(e.contains("opened only when the component"), "{e}");
}

#[test]
fn stored_secret_refused_outside_credentials() {
    let stored = "{ stored = \"tok\" }";
    let cases = [
        (
            format!("[server]\ntoken = {stored}\n[data]\ndir = \"/tmp/x\"\ntimezone = \"UTC\"\n"),
            "server.token",
        ),
        (
            format!("{MIN}\n[web]\npassword = {stored}\n"),
            "web.password",
        ),
        (
            format!(
                "{MIN}\n[[sources]]\nkind = \"mcp\"\nname = \"up\"\nurl = \"https://up.example/mcp\"\ntoken = {stored}\n"
            ),
            "source \"up\": token",
        ),
    ];
    for (text, place) in cases {
        let e = refused(&text);
        assert!(
            e.contains(&format!("{place} = {{ stored = \"tok\" }}"))
                && e.contains("[[sources.credentials]]")
                && e.contains("{ file = \"…\" }"),
            "{place}: {e}"
        );
    }
}

#[test]
fn stored_secret_names_are_checked() {
    for bad in ["", "a b", "caldav/x", &"x".repeat(65)] {
        let e = refused(&component(&format!(
            "allow = [\"https://a.example\"]\n[[sources.credentials]]\norigin = \"https://a.example\"\nbearer = {{ stored = {bad:?} }}\n"
        )));
        assert!(
            e.contains("the credential for https://a.example") && e.contains("is not allowed"),
            "{bad:?}: {e}"
        );
    }
}

#[test]
fn component_hash_must_be_hex() {
    let e = refused(&component("sha256 = \"abc\"\n"));
    assert!(e.contains("64 lower-case hex digits"), "{e}");
}

/// The `[[sources]]` entry the error for an old `[calendar]` section gives.
fn calendar_replacement(calendar: &str) -> String {
    let e = refused(&format!("{MIN}\n[calendar]\n{calendar}"));
    let start = e.find("Replace the section with:\n\n").expect(&e) + 27;
    let end = e.find("\nThe calendar shipped").expect(&e);
    e[start..end].to_string()
}

#[test]
fn the_calendar_migration_gives_a_working_entry() {
    let entry = calendar_replacement(
        "events_url = \"https://cloud.example/dav/alix/personal/\"\n\
         reminders_url = \"https://cloud.example/dav/alix/tasks/\"\n\
         username = \"alix\"\npassword = { env = \"CALDAV_PASSWORD\" }\n",
    );
    assert!(
        entry.contains("allow = [\"https://cloud.example\"]"),
        "{entry}"
    );
    assert!(!entry.contains("sha256"), "shipped: no hashes: {entry}");
    // Followed literally, it loads.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fronde.toml");
    std::fs::write(&path, format!("{MIN}\n{entry}")).unwrap();
    let config = fronde_hub::config::load(&path).unwrap();
    let [fronde_hub::config::SourceConfig::Component(c)] = &config.sources[..] else {
        panic!("{:?}", config.sources)
    };
    assert_eq!(c.name, "calendar");
    assert!(c.path.ends_with("calendar.wasm"), "{}", c.path.display());
    assert_eq!(
        c.settings["events_url"],
        "https://cloud.example/dav/alix/personal/"
    );
    let [credential] = &c.credentials[..] else {
        panic!("{:?}", c.credentials)
    };
    let basic = credential.basic.as_ref().unwrap();
    assert_eq!(basic.username, "alix");
    assert!(
        matches!(&basic.password, fronde_hub::config::Secret::Env { env } if env == "CALDAV_PASSWORD")
    );

    // A server on the network, over plain http, with reminders elsewhere.
    let entry = calendar_replacement(
        "events_url = \"http://192.168.1.10:5232/alix/cal/\"\n\
         reminders_url = \"https://tasks.example/alix/\"\n\
         username = \"alix\"\npassword = { file = \"caldav\" }\n",
    );
    assert!(
        entry.contains(
            "allow = [{ origin = \"http://192.168.1.10:5232\", local = true }, \"https://tasks.example\"]"
        ),
        "{entry}"
    );
    assert!(entry.contains("Serve CalDAV over https"), "{entry}");
    assert_eq!(
        entry.matches("[[sources.credentials]]").count(),
        2,
        "{entry}"
    );
    std::fs::write(&path, format!("{MIN}\n{entry}")).unwrap();
    fronde_hub::config::load(&path).unwrap();
}

#[test]
fn the_calendar_migration_marks_local_servers() {
    for (url, local) in [
        // Shared address space (carrier-grade NAT, Tailscale).
        ("http://100.64.0.1:5232/cal/", true),
        ("http://100.127.255.254/cal/", true),
        ("http://[::ffff:192.168.1.10]:5232/cal/", true),
        ("http://[::ffff:100.64.0.1]/cal/", true),
        ("http://[fd12::1]/cal/", true),
        ("http://nas.lan:5232/cal/", true),
        ("http://localhost:5232/cal/", true),
        ("https://100.128.0.1/cal/", false),
        ("https://cloud.example/cal/", false),
    ] {
        let entry = calendar_replacement(&format!(
            "events_url = {url:?}\nreminders_url = {url:?}\nusername = \"a\"\n"
        ));
        assert_eq!(entry.contains("local = true }]"), local, "{url}: {entry}");
    }
    // Plain http to a mapped IPv6 loopback stays on this machine: no https warning.
    let entry = calendar_replacement(
        "events_url = \"http://[::ffff:127.0.0.1]:5232/c/\"\n\
         reminders_url = \"http://[::ffff:127.0.0.1]:5232/r/\"\n",
    );
    assert!(
        entry.contains("allow = [{ origin = \"http://[::ffff:7f00:1]:5232\", local = true }]"),
        "{entry}"
    );
    assert!(!entry.contains("Serve CalDAV over https"), "{entry}");
}

/// A host the URL standard accepts may hold characters TOML must escape: the entry still reads
/// back with the same origin.
#[test]
fn the_calendar_migration_escapes_origins() {
    let entry = calendar_replacement(
        "events_url = 'https://we\"ird.example/c/'\n\
         reminders_url = 'http://lo\"cal:5232/r/'\nusername = \"a\"\n",
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fronde.toml");
    std::fs::write(&path, format!("{MIN}\n{entry}")).unwrap();
    let config = fronde_hub::config::load(&path).expect(&entry);
    let [fronde_hub::config::SourceConfig::Component(c)] = &config.sources[..] else {
        panic!("{:?}", config.sources)
    };
    let allowed: Vec<String> = c.allow.iter().map(|a| a.origin.to_string()).collect();
    assert_eq!(
        allowed,
        ["https://we\"ird.example", "http://lo\"cal:5232"],
        "{entry}"
    );
    let credited: Vec<String> = c.credentials.iter().map(|c| c.origin.to_string()).collect();
    assert_eq!(credited, allowed, "{entry}");
}

#[test]
fn old_sections_point_to_sources() {
    let e = refused(&format!(
        "{MIN}\n[calendar]\nevents_url = \"https://a.example/\"\n"
    ));
    assert!(
        e.contains("[[sources]]") && e.contains("kind = \"component\""),
        "{e}"
    );
    let e = refused(&format!(
        "{MIN}\n[[mcp]]\nname = \"a\"\nurl = \"http://x/mcp\"\n"
    ));
    assert!(
        e.contains("[[sources]]") && e.contains("kind = \"mcp\""),
        "{e}"
    );
}

#[test]
fn python_is_off_unless_configured() {
    assert!(load_str(MIN).unwrap().python.is_none());
}

#[test]
fn python_dir_is_read_from_the_config_directory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fronde.toml");
    std::fs::write(
        &path,
        format!("{MIN}\n[python]\ndir = \"runtime/python\"\n"),
    )
    .unwrap();
    let c = load(&path).unwrap();
    assert_eq!(c.python.unwrap().dir, dir.path().join("runtime/python"));
}

#[test]
fn python_refuses_unknown_keys() {
    let e = load_str(&format!(
        "{MIN}\n[python]\ndir = \"/app/python\"\nmemory_mb = 1024\n"
    ))
    .unwrap_err();
    assert!(format!("{e:#}").contains("memory_mb"), "{e:#}");
    let e = load_str(&format!("{MIN}\n[python]\n")).unwrap_err();
    assert!(format!("{e:#}").contains("dir"), "{e:#}");
}

#[test]
fn code_prefix_is_reserved() {
    // An upstream server named `code` would bring `code_*` tools.
    let e = load_str(&format!(
        "{MIN}\n[[sources]]\nkind = \"mcp\"\nname = \"code\"\nurl = \"http://127.0.0.1:1/mcp\"\n"
    ))
    .unwrap_err();
    assert!(format!("{e:#}").contains("reserved"), "{e:#}");
    assert!(fronde_hub::config::RESERVED_TOOL_PREFIXES.contains(&"code_"));
}

#[test]
fn python_source_name_is_taken_when_python_is_on() {
    let entry =
        "[[sources]]\nkind = \"mcp\"\nname = \"python\"\nurl = \"http://127.0.0.1:1/mcp\"\n";
    assert!(load_str(&format!("{MIN}\n{entry}")).is_ok());
    let e = load_str(&format!("{MIN}\n[python]\ndir = \"/app/python\"\n{entry}")).unwrap_err();
    assert!(format!("{e:#}").contains("taken by [python]"), "{e:#}");
}

/// The master key lives beside the configuration file unless `[secrets] master_key` names a
/// file, read from the configuration's directory when relative; never in the data directory by
/// default, so that a backup of the data directory does not hold it.
#[test]
fn master_key_is_beside_the_configuration_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fronde.toml");
    std::fs::write(&path, MIN).unwrap();
    let c = load(&path).unwrap();
    assert_eq!(c.master_key_file().unwrap(), dir.path().join("master.key"));
    assert!(!c.master_key_file().unwrap().starts_with(&c.data.dir));

    for (written, expected) in [
        ("keys/master.key", dir.path().join("keys/master.key")),
        ("../keys/fronde.key", dir.path().join("../keys/fronde.key")),
        (
            "/keys/master.key",
            std::path::PathBuf::from("/keys/master.key"),
        ),
    ] {
        std::fs::write(
            &path,
            format!("{MIN}[secrets]\nmaster_key = {{ file = {written:?} }}\n"),
        )
        .unwrap();
        let c = load(&path).unwrap();
        let expected: std::path::PathBuf = expected.components().collect();
        assert_eq!(c.master_key_file().unwrap(), expected, "{written}");
    }
}

#[test]
fn master_key_is_only_a_file() {
    for written in ["{ env = \"FRONDE_KEY\" }", "{ stored = \"key\" }"] {
        let err = load_str(&format!("{MIN}[secrets]\nmaster_key = {written}\n"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("[secrets] master_key must be { file = \"…\" }"),
            "{written}: {err}"
        );
    }
    // A backend this Fronde does not have is refused, naming those it has.
    let err = format!(
        "{:#}",
        load_str(&format!("{MIN}[secrets]\nbackend = \"keyring\"\n")).unwrap_err()
    );
    assert!(err.contains("unknown variant `keyring`"), "{err}");
    assert!(
        err.contains("`auto`, `systemd`, `keychain`, `file`"),
        "{err}"
    );
}

/// `[secrets]` takes only the keys it knows: a misspelt one is refused, not ignored.
#[test]
fn unknown_secrets_key_is_refused() {
    for written in [
        "backends = \"file\"",
        "keychain = \"fronde\"",
        "master_key_file = \"k\"",
    ] {
        let err = format!(
            "{:#}",
            load_str(&format!("{MIN}[secrets]\n{written}\n")).unwrap_err()
        );
        assert!(err.contains("unknown field"), "{written}: {err}");
    }
}

/// `[secrets] backend` is `auto` by default, and names one backend otherwise; the key file is
/// kept for the file backend whatever the choice.
#[test]
fn secrets_backend_is_auto_by_default() {
    use fronde_hub::config::KeyBackend;
    assert_eq!(load_str(MIN).unwrap().secrets.backend, KeyBackend::Auto);
    for (written, expected) in [
        ("auto", KeyBackend::Auto),
        ("systemd", KeyBackend::Systemd),
        ("keychain", KeyBackend::Keychain),
        ("file", KeyBackend::File),
    ] {
        let c = load_str(&format!(
            "{MIN}[secrets]\nbackend = {written:?}\nmaster_key = {{ file = \"/k/master.key\" }}\n"
        ))
        .unwrap();
        assert_eq!(c.secrets.backend, expected, "{written}");
        let backends = c.master_key_backends().unwrap();
        assert_eq!(backends.key_file(), std::path::Path::new("/k/master.key"));
    }
}

/// A configuration that `load` did not read has no master key location, rather than a guess.
#[test]
fn master_key_needs_a_loaded_configuration() {
    let c: fronde_hub::config::Config = toml::from_str(MIN).unwrap();
    let err = c.master_key_file().unwrap_err().to_string();
    assert!(err.contains("[secrets] master_key"), "{err}");
    let c: fronde_hub::config::Config = toml::from_str(&format!(
        "{MIN}[secrets]\nmaster_key = {{ file = \"/k/master.key\" }}\n"
    ))
    .unwrap();
    assert_eq!(
        c.master_key_file().unwrap(),
        std::path::Path::new("/k/master.key")
    );
}

#[test]
fn the_agent_is_confined_by_default() {
    let c = load_str(MIN).unwrap();
    assert!(c.agent.sandbox);
    assert!(c.agent.read.is_empty());
    let c = load_str(&format!(
        "{MIN}[agent]\nsandbox = false\nread = [\"tools\", \"/opt/x\"]\n"
    ))
    .unwrap();
    assert!(!c.agent.sandbox);
    let base = c.path().unwrap().parent().unwrap();
    assert_eq!(
        c.agent.read,
        [base.join("tools"), std::path::PathBuf::from("/opt/x")]
    );
}

#[test]
fn agent_env_cannot_point_the_agent_at_the_session() {
    for name in [
        "HOME",
        "TMPDIR",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "CREDENTIALS_DIRECTORY",
    ] {
        let error = load_str(&format!("{MIN}[agent]\nenv = {{ {name} = \"x\" }}\n"))
            .unwrap_err()
            .to_string();
        assert!(error.contains(name), "{error}");
        assert!(error.contains("remove it"), "{error}");
    }
    load_str(&format!("{MIN}[agent]\nenv = {{ FOO = \"x\" }}\n")).unwrap();
}

#[test]
fn secret_variables_are_listed() {
    let c = load_str(&format!(
        "{MIN}[web]\npassword = {{ file = \"pw\" }}\n\
         [[sources]]\nkind = \"mcp\"\nname = \"up\"\nurl = \"https://mcp.example\"\n\
         token = {{ env = \"UP_TOKEN\" }}\n"
    ))
    .unwrap();
    assert_eq!(c.secret_env_names(), ["T", "UP_TOKEN"]);
    let base = c.path().unwrap().parent().unwrap();
    assert_eq!(c.secret_files(), [base.join("pw"), base.join("master.key")]);
}

#[test]
fn the_agent_token_is_a_secret() {
    use fronde_hub::config::Secret;
    let c = load_str(&format!(
        "{MIN}[agent]\noauth_token = {{ file = \"claude-token\" }}\n"
    ))
    .unwrap();
    let base = c.path().unwrap().parent().unwrap().to_path_buf();
    match &c.agent.oauth_token {
        Some(Secret::File { file }) => assert_eq!(file, &base.join("claude-token")),
        other => panic!("{other:?}"),
    }
    // Its file is protected like any secret's, and its variable never reaches the agent as is.
    assert!(c.secret_files().contains(&base.join("claude-token")));
    let c = load_str(&format!(
        "{MIN}[agent]\noauth_token = {{ env = \"CLAUDE_TOKEN\" }}\n"
    ))
    .unwrap();
    assert!(c.secret_env_names().contains(&"CLAUDE_TOKEN".to_owned()));
    let c = load_str(&format!(
        "{MIN}[agent]\noauth_token = {{ stored = \"claude\" }}\n"
    ))
    .unwrap();
    assert!(matches!(&c.agent.oauth_token, Some(Secret::Stored { stored }) if stored == "claude"));
    assert!(
        load_str(&format!(
            "{MIN}[agent]\noauth_token = {{ stored = \"no spaces\" }}\n"
        ))
        .is_err()
    );
    // Never the token itself in the configuration, nor the token twice.
    assert!(load_str(&format!("{MIN}[agent]\noauth_token = \"sk-ant-oat\"\n")).is_err());
    let error = load_str(&format!(
        "{MIN}[agent]\noauth_token = {{ env = \"T\" }}\nenv = {{ CLAUDE_CODE_OAUTH_TOKEN = \"x\" }}\n"
    ))
    .unwrap_err()
    .to_string();
    assert!(error.contains("oauth_token"), "{error}");
}

#[test]
fn agent_limits_default_and_parse() {
    use fronde_hub::config::AgentLimits;
    let c = load_str(MIN).unwrap();
    assert_eq!(
        c.agent.limits,
        AgentLimits {
            memory: Some(2 * 1024 * 1024 * 1024),
            total_memory: Some(4 * 1024 * 1024 * 1024),
            cpus: Some(2.0),
            tasks: Some(512),
        }
    );
    let limits = |table: &str| {
        load_str(&format!("{MIN}\n[agent.limits]\n{table}\n"))
            .unwrap()
            .agent
            .limits
    };
    let l = limits("memory = \"512M\"\ncpus = 0.5\ntasks = 64");
    assert_eq!(
        (l.memory, l.cpus, l.tasks),
        (Some(512 << 20), Some(0.5), Some(64))
    );
    assert_eq!(limits("memory = \"1.5g\"").memory, Some(3 << 29));
    assert_eq!(limits("memory = \"4096K\"").memory, Some(4 << 20));
    assert_eq!(limits("memory = 1048576").memory, Some(1 << 20));
    assert_eq!(limits("cpus = 3").cpus, Some(3.0));
    // Each limit is turned off on its own; the others keep their defaults.
    let l = limits("memory = \"none\"");
    assert_eq!((l.memory, l.cpus, l.tasks), (None, Some(2.0), Some(512)));
    let l = limits("memory = \"none\"\ncpus = \"none\"\ntasks = \"none\"");
    assert_eq!((l.memory, l.cpus, l.tasks), (None, None, None));
    assert_eq!(l.total_memory, Some(4 << 30));
    let l = limits("total_memory = \"6G\"");
    assert_eq!((l.total_memory, l.memory), (Some(6 << 30), Some(2 << 30)));
    assert_eq!(limits("total_memory = \"none\"").total_memory, None);
}

#[test]
fn threads_default_and_parse() {
    use fronde_hub::config::ThreadsConfig;
    use std::time::Duration;
    let c = load_str(MIN).unwrap();
    assert_eq!(
        c.threads,
        ThreadsConfig {
            max_running: 3,
            idle_stop: Duration::from_secs(600),
        }
    );
    let threads = |table: &str| {
        load_str(&format!("{MIN}\n[threads]\n{table}\n"))
            .unwrap()
            .threads
    };
    let t = threads("max_running = 1\nidle_stop = \"1h 30m\"");
    assert_eq!(
        (t.max_running, t.idle_stop),
        (1, Duration::from_secs(90 * 60))
    );
    assert_eq!(
        threads("idle_stop = \"90s\"").idle_stop,
        Duration::from_secs(90)
    );
    assert_eq!(
        threads("max_running = 5").idle_stop,
        Duration::from_secs(600)
    );
    assert_eq!(
        fronde_hub::config::duration_text(Duration::from_secs(600)),
        "10m"
    );
    // The example says the defaults.
    let example = load(std::path::Path::new("../../fronde.example.toml")).unwrap();
    assert_eq!(example.threads, ThreadsConfig::default());
    assert_eq!(
        example.agent.limits,
        fronde_hub::config::AgentLimits::default()
    );
}

#[test]
fn threads_refuse_bad_values() {
    for (table, expected) in [
        ("max_running = 0", "max_running: expected"),
        ("max_running = -1", "max_running: expected"),
        ("max_running = 1.5", "max_running: expected"),
        ("max_running = \"3\"", "max_running: expected"),
        ("idle_stop = \"0s\"", "idle_stop: expected a duration"),
        ("idle_stop = \"soon\"", "idle_stop: expected a duration"),
        ("idle_stop = 600", "idle_stop: expected a duration"),
        ("max_depth = 1", "unknown field"),
    ] {
        let e = refused(&format!("{MIN}\n[threads]\n{table}\n"));
        assert!(e.contains(expected), "{table}: {e}");
    }
}

#[test]
fn agent_limits_refuse_bad_values() {
    for (table, expected) in [
        ("memory = \"2X\"", "memory: expected a size"),
        ("memory = \"G\"", "memory: expected a size"),
        ("memory = \"0\"", "memory: expected a size"),
        ("memory = 0", "memory: expected a size"),
        ("memory = \"-1G\"", "memory: expected a size"),
        ("memory = \"unlimited\"", "memory: expected a size"),
        ("total_memory = \"lots\"", "total_memory: expected a size"),
        ("total_memory = 0", "total_memory: expected a size"),
        ("cpus = 0", "cpus: expected"),
        ("cpus = -1.5", "cpus: expected"),
        ("cpus = \"fast\"", "cpus: expected"),
        ("cpus = nan", "cpus: expected"),
        ("tasks = 0", "tasks: expected"),
        ("tasks = 1.5", "tasks: expected"),
        ("tasks = \"many\"", "tasks: expected"),
        ("swap = \"1G\"", "unknown field"),
    ] {
        let e = refused(&format!("{MIN}\n[agent.limits]\n{table}\n"));
        assert!(e.contains(expected), "{table}: {e}");
    }
}
