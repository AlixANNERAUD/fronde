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
    assert!(c.users().get("alix").unwrap().admin);
    assert_eq!(c.password_of("alix").unwrap().0, "[users.alix] password");
    let accounts = c.accounts();
    assert_eq!(accounts.len(), 1);
    assert_eq!(
        (accounts[0].name.as_str(), accounts[0].owner.as_str()),
        ("alix", "alix")
    );
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
        "{MIN}\n[web]\npassword = {{ env = \"P\" }}\nsecure_cookie = true\n[agent]\ncommand = [\"npx\", \"claude-agent-acp\"]\nenv = {{ CLAUDE_CODE_EXECUTABLE = \"/opt/claude/bin/claude\" }}\n"
    );
    let c = load_str(&s).unwrap();
    let web = c.web.unwrap();
    assert!(matches!(web.password, Some(Secret::Env { env }) if env == "P"));
    assert!(web.secure_cookie);
    assert_eq!(c.agent.command, ["npx", "claude-agent-acp"]);
    assert_eq!(
        c.agent
            .env
            .get("CLAUDE_CODE_EXECUTABLE")
            .map(String::as_str),
        Some("/opt/claude/bin/claude")
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
         [[sources.credentials]]\norigin = \"http://192.168.1.10:5232\"\nbearer = { env = \"CAL_TOKEN\" }\n",
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
         [[sources.credentials]]\norigin = \"http://127.0.0.1:5232/\"\nbearer = { env = \"CAL_TOKEN\" }\n",
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
        "allow = [\"https://a.example\"]\n[[sources.credentials]]\norigin = \"https://b.example\"\nbearer = { env = \"CAL_TOKEN\" }\n",
    ));
    assert!(
        e.contains("https://b.example") && e.contains("not in allow"),
        "{e}"
    );
}

#[test]
fn credential_needs_exactly_one_kind_and_no_plain_secret() {
    let both = "allow = [\"https://a.example\"]\n[[sources.credentials]]\norigin = \"https://a.example\"\nbearer = { env = \"CAL_TOKEN\" }\nbasic = { username = \"a\", password = { env = \"P\" } }\n";
    assert!(refused(&component(both)).contains("exactly one of basic or bearer"));
    let none = "allow = [\"https://a.example\"]\n[[sources.credentials]]\norigin = \"https://a.example\"\n";
    assert!(refused(&component(none)).contains("exactly one of basic or bearer"));
    let plain = "allow = [\"https://a.example\"]\n[[sources.credentials]]\norigin = \"https://a.example\"\nbearer = \"tok\"\n";
    assert!(load_str(&component(plain)).is_err());
    let twice = "allow = [\"https://a.example\"]\n[[sources.credentials]]\norigin = \"https://a.example\"\nbearer = { env = \"CAL_TOKEN\" }\n[[sources.credentials]]\norigin = \"https://a.example\"\nbearer = { env = \"U\" }\n";
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
fn threads_prefix_is_reserved() {
    // An upstream server named `threads` would bring `threads_*` tools, which start sub-agents.
    let e = load_str(&format!(
        "{MIN}\n[[sources]]\nkind = \"mcp\"\nname = \"threads\"\nurl = \"http://127.0.0.1:1/mcp\"\n"
    ))
    .unwrap_err();
    assert!(format!("{e:#}").contains("reserved"), "{e:#}");
    assert!(fronde_hub::config::RESERVED_TOOL_PREFIXES.contains(&"threads_"));
}

#[test]
fn integrations_prefix_is_reserved() {
    // An upstream server named `integrations` would bring `integrations_*` tools, which set up
    // components from a card.
    let e = load_str(&format!(
        "{MIN}\n[[sources]]\nkind = \"mcp\"\nname = \"integrations\"\nurl = \"http://127.0.0.1:1/mcp\"\n"
    ))
    .unwrap_err();
    assert!(format!("{e:#}").contains("reserved"), "{e:#}");
    assert!(fronde_hub::config::RESERVED_TOOL_PREFIXES.contains(&"integrations_"));
}

#[test]
fn python_source_name_is_taken_when_python_is_on() {
    let entry =
        "[[sources]]\nkind = \"mcp\"\nname = \"python\"\nurl = \"http://127.0.0.1:1/mcp\"\n";
    // Reserved even without [python]: see `python_source_name_is_always_reserved`.
    let e = load_str(&format!("{MIN}\n{entry}")).unwrap_err();
    assert!(format!("{e:#}").contains("reserved"), "{e:#}");
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
        "CLAUDE_CONFIG_DIR",
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
            max_depth: 1,
            children_running: 3,
            children_per_day: 30,
            children_kept: 200,
            child_turns_per_day: 100,
            child_turn_timeout: Duration::from_secs(30 * 60),
            wakes_without_owner: 10,
            reserve: fronde_hub::config::Percent(30),
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
    // The limits on sub-agents.
    let t = threads(
        "max_depth = 3\nchildren_running = 1\nchildren_per_day = 100\nchildren_kept = 5\nchild_turn_timeout = \"2h\"",
    );
    assert_eq!(
        (
            t.max_depth,
            t.children_running,
            t.children_per_day,
            t.children_kept,
            t.child_turn_timeout
        ),
        (3, 1, 100, 5, Duration::from_secs(2 * 3600))
    );
    let t = threads(
        "child_turns_per_day = 1000\nwakes_without_owner = 1\nchild_turn_timeout = \"24h\"",
    );
    assert_eq!(
        (
            t.child_turns_per_day,
            t.wakes_without_owner,
            t.child_turn_timeout
        ),
        (1000, 1, Duration::from_secs(24 * 3600))
    );
    assert_eq!(threads("max_depth = 0").max_depth, 0);
    assert_eq!(
        threads("children_running = 2").child_turn_timeout,
        Duration::from_secs(30 * 60),
        "the others keep their defaults"
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
fn reserve_parses_and_refuses_bad_values() {
    use fronde_hub::config::Percent;
    let reserve = |value: &str| {
        load_str(&format!("{MIN}\n[threads]\nreserve = {value}\n")).map(|c| c.threads.reserve)
    };
    for (value, expected) in [
        ("\"30%\"", 30),
        ("\"0%\"", 0),
        ("\"100%\"", 100),
        ("\"5 %\"", 5),
    ] {
        assert_eq!(reserve(value).unwrap(), Percent(expected), "{value}");
    }
    assert_eq!(Percent(30).to_string(), "30%");
    for bad in [
        "\"30\"",
        "\"-1%\"",
        "\"101%\"",
        "\"abc\"",
        "\"%\"",
        "\"30.5%\"",
        "\"1000%\"",
        "30",
        "0.3",
    ] {
        let error = format!("{:#}", reserve(bad).unwrap_err());
        assert!(
            error.contains("reserve: expected a percentage from \"0%\" to \"100%\""),
            "{bad}: {error}"
        );
    }
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
        ("max_depth = 4", "max_depth: expected a depth from 0 to 3"),
        ("max_depth = -1", "max_depth: expected a depth from 0 to 3"),
        (
            "max_depth = \"1\"",
            "max_depth: expected a depth from 0 to 3",
        ),
        ("children_running = 0", "children_running: expected"),
        ("children_running = 2.5", "children_running: expected"),
        ("children_per_day = 0", "children_per_day: expected"),
        ("children_per_day = \"30\"", "children_per_day: expected"),
        ("children_kept = 0", "children_kept: expected"),
        ("children_kept = -3", "children_kept: expected"),
        (
            "child_turn_timeout = \"0s\"",
            "child_turn_timeout: expected a duration",
        ),
        (
            "child_turn_timeout = 1800",
            "child_turn_timeout: expected a duration",
        ),
        (
            "child_turn_timeout = \"long\"",
            "child_turn_timeout: expected a duration",
        ),
        // Bounded above too, the bound named.
        (
            "child_turn_timeout = \"25h\"",
            "child_turn_timeout: at most 24h, not 25h",
        ),
        (
            "children_per_day = 1001",
            "children_per_day: expected a number of sub-agents from 1 to 1000",
        ),
        ("max_running = 5000", "max_running: expected"),
        ("children_kept = 100000", "children_kept: expected"),
        (
            "child_turns_per_day = 0",
            "child_turns_per_day: expected a number of turns from 1 to 1000",
        ),
        (
            "child_turns_per_day = 1001",
            "child_turns_per_day: expected",
        ),
        (
            "wakes_without_owner = 0",
            "wakes_without_owner: expected a number of turns from 1 to 1000",
        ),
        (
            "wakes_without_owner = \"10\"",
            "wakes_without_owner: expected",
        ),
        // Unknown keys are refused, not ignored.
        ("max_children = 3", "unknown field"),
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

/// `[agent]` is Claude's settings and Claude the only provider; the planned ones are named as
/// such, and `[providers.<id>]` is not read yet.
#[test]
fn only_claude_is_a_configured_provider() {
    use fronde_hub::config::{PROVIDERS, check_provider};
    assert_eq!(PROVIDERS, ["claude"]);
    check_provider("claude").unwrap();
    for planned in ["codex", "gemini", "antigravity", "Gemini"] {
        let e = check_provider(planned).unwrap_err().to_string();
        assert_eq!(
            e, "only claude is configured; Codex and Gemini/Antigravity are planned",
            "{planned}"
        );
    }
    let e = check_provider("mistral").unwrap_err().to_string();
    assert!(e.contains("unknown provider \"mistral\""), "{e}");
    assert!(e.contains("only claude is configured"), "{e}");
    let e = refused(&format!(
        "{MIN}\n[providers.codex]\ncommand = [\"codex-acp\"]\n"
    ));
    assert!(e.contains("unknown field"), "{e}");
}

/// A URL server's token may be stored; a command has no origin to bind it to.
#[test]
fn stored_upstream_token_is_for_url_servers_only() {
    let url = format!(
        "{MIN}\n[[sources]]\nkind = \"mcp\"\nname = \"up\"\nurl = \"https://up.example/mcp\"\ntoken = {{ stored = \"tok\" }}\n"
    );
    let c = load_str(&url).unwrap();
    let SourceConfig::Mcp(m) = &c.sources[0] else {
        panic!("not an mcp source")
    };
    assert!(matches!(&m.token, Some(Secret::Stored { stored }) if stored == "tok"));

    let e = refused(&url.replace("\"tok\"", "\"bad name\""));
    assert!(
        e.contains("source \"up\": token") && e.contains("is not allowed"),
        "{e}"
    );

    let stdio = format!(
        "{MIN}\n[[sources]]\nkind = \"mcp\"\nname = \"up\"\ncommand = [\"x\"]\ntoken = {{ stored = \"tok\" }}\n"
    );
    let e = refused(&stdio);
    assert!(
        e.contains("a command has none") && e.contains("use { file = \"…\" }"),
        "{e}"
    );
}

/// `[accounts.<name>]`: a provider, an owner and a token, each token anchored and listed with
/// the other secrets, so that its file is checked private and its variable kept from the agent.
#[test]
fn accounts_parse() {
    use fronde_hub::config::AccountForm;
    let c = load_str(&format!(
        "{MIN}user = \"alix\"\n\
         [accounts.alix]\nprovider = \"claude\"\nowner = \"alix\"\noauth_token = {{ stored = \"claude_alix\" }}\n"
    ))
    .unwrap();
    let accounts = c.accounts();
    assert_eq!(accounts.len(), 1);
    let alix = &accounts[0];
    assert_eq!(
        (
            alix.name.as_str(),
            alix.provider.as_str(),
            alix.owner.as_str()
        ),
        ("alix", "claude", "alix")
    );
    assert!(matches!(&alix.token, Some(Secret::Stored { stored }) if stored == "claude_alix"));
    assert_eq!(alix.form, AccountForm::Declared);

    let c = load_str(&format!(
        "{MIN}user = \"alix\"\n\
         [accounts.alix]\nprovider = \"claude\"\nowner = \"alix\"\noauth_token = {{ file = \"alix.token\" }}\n"
    ))
    .unwrap();
    let base = c.path().unwrap().parent().unwrap().to_path_buf();
    assert!(
        matches!(&c.accounts()[0].token, Some(Secret::File { file }) if file == &base.join("alix.token"))
    );
    assert!(c.secret_files().contains(&base.join("alix.token")));
    let c = load_str(&format!(
        "{MIN}user = \"alix\"\n\
         [accounts.alix]\nprovider = \"claude\"\nowner = \"alix\"\noauth_token = {{ env = \"ALIX_TOKEN\" }}\n"
    ))
    .unwrap();
    assert!(c.secret_env_names().contains(&"ALIX_TOKEN".to_owned()));
    // Never the token itself, a stored name checked, and no key but these.
    for bad in [
        "oauth_token = \"sk-ant-oat\"",
        "oauth_token = { stored = \"no spaces\" }",
        "oauth_token = { file = \"t\" }\nshared_with = [\"marie\"]",
    ] {
        let text = format!(
            "{MIN}user = \"alix\"\n[accounts.alix]\nprovider = \"claude\"\nowner = \"alix\"\n{bad}\n"
        );
        assert!(load_str(&text).is_err(), "{bad}");
    }
    let e = refused(&format!(
        "{MIN}user = \"alix\"\n[accounts.alix]\nprovider = \"claude\"\nowner = \"alix\"\n\
         oauth_token = {{ file = \"t\" }}\nshared_with = [\"marie\"]\n"
    ));
    assert!(e.contains("shared_with"), "{e}");
}

/// An account's name is a plain identifier, as a tool's prefix is.
#[test]
fn account_name_is_checked() {
    let e = refused(&format!(
        "{MIN}[accounts.\"Alix\"]\nprovider = \"claude\"\nowner = \"me\"\noauth_token = {{ file = \"t\" }}\n"
    ));
    assert!(
        e.contains("[accounts.\"Alix\"]") && e.contains("^[a-z][a-z0-9_]{0,63}$"),
        "{e}"
    );
}

/// Without `[accounts]`, `[agent] oauth_token` is the owner's one account, named `claude`.
#[test]
fn legacy_agent_token_is_the_owner_account() {
    use fronde_hub::config::{AccountForm, LEGACY_ACCOUNT};
    let mut c = load_str(&format!(
        "{MIN}user = \"alix\"\n[agent]\noauth_token = {{ stored = \"claude\" }}\n"
    ))
    .unwrap();
    // The file's token wins over an inherited variable.
    c.set_inherited_sign_in(true);
    let accounts = c.accounts();
    assert_eq!(accounts.len(), 1);
    let account = &accounts[0];
    assert_eq!(LEGACY_ACCOUNT, "claude");
    assert_eq!(
        (
            account.name.as_str(),
            account.provider.as_str(),
            account.owner.as_str()
        ),
        ("claude", "claude", "alix")
    );
    assert!(matches!(&account.token, Some(Secret::Stored { stored }) if stored == "claude"));
    assert_eq!(account.form, AccountForm::AgentToken);

    // With neither a token nor the variable, the owner's agent starts unsigned, as before.
    let mut c = load_str(&format!("{MIN}user = \"alix\"\n")).unwrap();
    c.set_inherited_sign_in(false);
    let accounts = c.accounts();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].owner, "alix");
    assert!(accounts[0].token.is_none());
    assert_eq!(accounts[0].form, AccountForm::Unsigned);
}

/// Without `[accounts]` nor `[agent] oauth_token`, `CLAUDE_CODE_OAUTH_TOKEN` in Fronde's
/// environment (the Docker how-to) is the owner's account's token; it is not listed as a secret
/// of the file, so the agent's environment is what it was.
#[test]
fn legacy_env_token_is_the_owner_account() {
    use fronde_hub::config::AccountForm;
    let mut c = load_str(&format!("{MIN}user = \"alix\"\n")).unwrap();
    c.set_inherited_sign_in(true);
    let accounts = c.accounts();
    assert_eq!(accounts.len(), 1);
    let account = &accounts[0];
    assert_eq!(
        (account.name.as_str(), account.owner.as_str()),
        ("claude", "alix")
    );
    assert!(
        matches!(&account.token, Some(Secret::Env { env }) if env == "CLAUDE_CODE_OAUTH_TOKEN")
    );
    assert_eq!(account.form, AccountForm::Inherited);
    assert!(
        !c.secret_env_names()
            .contains(&"CLAUDE_CODE_OAUTH_TOKEN".to_owned())
    );
}

#[test]
fn agent_token_and_accounts_are_refused_together() {
    let e = refused(&format!(
        "{MIN}user = \"alix\"\n[agent]\noauth_token = {{ file = \"a\" }}\n\
         [accounts.alix]\nprovider = \"claude\"\nowner = \"alix\"\noauth_token = {{ file = \"b\" }}\n"
    ));
    assert!(
        e.contains("[agent] oauth_token and [accounts] are both set")
            && e.contains("[accounts.<name>] oauth_token"),
        "{e}"
    );
}

#[test]
fn unknown_provider_refused() {
    let account = |provider: &str| {
        refused(&format!(
            "{MIN}[accounts.me]\nprovider = {provider:?}\nowner = \"me\"\noauth_token = {{ file = \"t\" }}\n"
        ))
    };
    let e = account("mistral");
    assert!(
        e.contains("[accounts.me] provider = \"mistral\"") && e.contains("unknown provider"),
        "{e}"
    );
    let e = account("codex");
    assert!(
        e.contains("Codex and Gemini/Antigravity are planned"),
        "{e}"
    );
}

#[test]
fn owner_must_be_a_user() {
    let e = refused(&format!(
        "{MIN}user = \"alix\"\n\
         [accounts.marie]\nprovider = \"claude\"\nowner = \"marie\"\noauth_token = {{ file = \"t\" }}\n"
    ));
    assert!(
        e.contains("[accounts.marie] owner = \"marie\" is not a user") && e.contains("alix"),
        "{e}"
    );
}

/// One person may own two accounts of one provider, once the file names which one is their
/// default; without it, the default rule refuses the file rather than pick one.
#[test]
fn one_person_may_own_two_accounts() {
    let two = |alix: &str, alix_pro: &str| {
        format!(
            "{MIN}user = \"alix\"\n\
             [accounts.alix]\nprovider = \"claude\"\nowner = \"alix\"\n{alix}oauth_token = {{ file = \"a\" }}\n\
             [accounts.alix_pro]\nprovider = \"claude\"\nowner = \"alix\"\n{alix_pro}oauth_token = {{ file = \"b\" }}\n"
        )
    };
    let c = load_str(&two("", "default_for = [\"alix\"]\n")).unwrap();
    let owners: Vec<(String, String)> = c
        .accounts()
        .into_iter()
        .map(|account| (account.name, account.owner))
        .collect();
    assert_eq!(
        owners,
        [
            ("alix".to_owned(), "alix".to_owned()),
            ("alix_pro".to_owned(), "alix".to_owned())
        ]
    );
    assert!(c.may_use("alix", "alix") && c.may_use("alix", "alix_pro"));
    assert_eq!(
        c.default_account("alix", "claude")
            .map(|account| account.name),
        Some("alix_pro".to_owned())
    );
    let e = refused(&two("", ""));
    assert!(
        e.contains("[accounts.alix] and [accounts.alix_pro]")
            && e.contains("default_for = [\"alix\"]"),
        "{e}"
    );
}

/// Two accounts reading one token would sign two people in with it: refused, whether the
/// token is stored, in a file (however its path is written) or in a variable.
#[test]
fn two_accounts_cannot_name_one_secret() {
    // A real file, reached through a link and through `..`: the same file however it is named.
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    fronde_hub::private::write(&token, "token-of-alix").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let link = dir.path().join("link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&token, &link).unwrap();
    let file = |path: &std::path::Path| format!("{{ file = {:?} }}", path.display().to_string());
    let mut cases = vec![
        (
            "{ stored = \"claude\" }".to_owned(),
            "{ stored = \"claude\" }".to_owned(),
            "the stored secret claude",
        ),
        (
            "{ file = \"token\" }".to_owned(),
            "{ file = \"./token\" }".to_owned(),
            "the file ",
        ),
        (
            "{ file = \"token\" }".to_owned(),
            "{ file = \"sub/../token\" }".to_owned(),
            "the file ",
        ),
        (
            file(&token),
            file(&dir.path().join("sub/../token")),
            "the file ",
        ),
        (
            "{ env = \"CLAUDE\" }".to_owned(),
            "{ env = \"CLAUDE\" }".to_owned(),
            "the environment variable CLAUDE",
        ),
    ];
    #[cfg(unix)]
    cases.push((file(&token), file(&link), "the file "));
    for (a, b, place) in cases {
        // Two people, or one person twice: the shared token is refused first.
        for owner in ["marie", "alix"] {
            let e = refused(&owned_by_alix(&format!(
                "{USERS}\
                 [accounts.alix]\nprovider = \"claude\"\nowner = \"alix\"\noauth_token = {a}\n\
                 [accounts.other]\nprovider = \"claude\"\nowner = \"{owner}\"\noauth_token = {b}\n"
            )));
            assert!(
                e.contains(
                    "[accounts.alix] oauth_token and [accounts.other] oauth_token both read their \
                     secret from"
                ) && e.contains(place)
                    && e.contains("never shared"),
                "{a} {b}: {e}"
            );
        }
    }
}

/// A person's credentials (`server.token`, each password, each account's token, the master key)
/// share nothing with each other: not a variable, not a file however it is named.
#[test]
fn people_never_share_a_secret() {
    let account = |token: &str| {
        format!(
            "[accounts.marie]\nprovider = \"claude\"\nowner = \"marie\"\noauth_token = {token}\n"
        )
    };
    for (rest, keys) in [
        // Two passwords.
        (
            USERS
                .replace("MARIE_PASSWORD", "SHARED")
                .replace("{ file = \"alix.password\" }", "{ env = \"SHARED\" }"),
            ["[users.alix] password", "[users.marie] password"],
        ),
        (
            USERS.replace(
                "{ env = \"MARIE_PASSWORD\" }",
                "{ file = \"./alix.password\" }",
            ),
            ["[users.alix] password", "[users.marie] password"],
        ),
        // A token and a password.
        (
            format!("{USERS}{}", account("{ env = \"MARIE_PASSWORD\" }")),
            ["[users.marie] password", "[accounts.marie] oauth_token"],
        ),
        (
            format!("{USERS}{}", account("{ file = \"alix.password\" }")),
            ["[users.alix] password", "[accounts.marie] oauth_token"],
        ),
        // The server's token and a password; the master key and a token.
        (
            USERS.replace("MARIE_PASSWORD", "T"),
            ["server.token", "[users.marie] password"],
        ),
        (
            format!("{USERS}{}", account("{ file = \"master.key\" }")),
            ["[accounts.marie] oauth_token", "[secrets] master_key"],
        ),
    ] {
        let e = refused(&owned_by_alix(&rest));
        assert!(
            e.contains(&format!(
                "{} and {} both read their secret from",
                keys[0], keys[1]
            )) && e.contains("never shared"),
            "{rest}: {e}"
        );
    }
}

/// What an account's token or a password is read from is named by nothing else: no source's
/// credential or token, no variable `[agent] env` sets.
#[test]
fn account_secrets_are_named_nowhere_else() {
    let account = "[accounts.alix]\nprovider = \"claude\"\nowner = \"alix\"\n";
    let upstream = |token: &str| {
        format!(
            "[[sources]]\nkind = \"mcp\"\nname = \"up\"\nurl = \"https://up.example/mcp\"\ntoken = {token}\n"
        )
    };
    let component = |secret: &str| {
        format!(
            "[[sources]]\nkind = \"component\"\nname = \"calendar\"\npath = \"calendar.wasm\"\n\
             allow = [\"https://cloud.example\"]\n[[sources.credentials]]\n\
             origin = \"https://cloud.example\"\nbearer = {secret}\n"
        )
    };
    for (token, other, key) in [
        (
            "{ stored = \"claude_alix\" }",
            upstream("{ stored = \"claude_alix\" }"),
            "source \"up\": token",
        ),
        (
            "{ env = \"ALIX_TOKEN\" }",
            component("{ env = \"ALIX_TOKEN\" }"),
            "source \"calendar\": the credential for https://cloud.example",
        ),
        (
            "{ file = \"alix.token\" }",
            component("{ file = \"alix.token\" }"),
            "source \"calendar\": the credential for https://cloud.example",
        ),
    ] {
        let e = refused(&owned_by_alix(&format!(
            "{account}oauth_token = {token}\n{other}"
        )));
        assert!(
            e.contains(&format!(
                "[accounts.alix] oauth_token and {key} both read their secret from"
            )),
            "{token}: {e}"
        );
    }
    // A password's variable, set by [agent] env.
    let e = refused(&owned_by_alix(&format!(
        "[agent]\nenv = {{ MARIE_PASSWORD = \"x\" }}\n{USERS}"
    )));
    assert!(
        e.contains(
            "[agent] env sets MARIE_PASSWORD, the variable [users.marie] password is read from"
        ),
        "{e}"
    );
    // Two sources may still share one.
    load_str(&format!(
        "{MIN}{}{}",
        upstream("{ env = \"SHARED\" }"),
        component("{ env = \"SHARED\" }")
    ))
    .unwrap();
}

/// `[accounts.claude]`, the legacy form's name, is only ever the owner's account.
#[test]
fn the_legacy_account_name_is_the_owners() {
    let e = refused(&owned_by_alix(&format!(
        "{USERS}[accounts.claude]\nprovider = \"claude\"\nowner = \"marie\"\noauth_token = {{ file = \"t\" }}\n"
    )));
    assert!(
        e.contains("[accounts.claude] belongs to marie") && e.contains("another name"),
        "{e}"
    );
    load_str(&owned_by_alix(&format!(
        "{USERS}[accounts.claude]\nprovider = \"claude\"\nowner = \"alix\"\noauth_token = {{ file = \"t\" }}\n"
    )))
    .unwrap();
}

/// Two names that could be taken one for the other are never two people: the same letters in
/// another case, composed otherwise (NFD), or in another script that looks the same.
#[test]
fn user_names_never_look_alike() {
    for (a, b) in [
        ("marie", "Marie"),
        ("Andr\u{e9}", "Andre\u{301}"),
        ("marie", "m\u{430}rie"),
        ("paul", "\u{440}aul"),
        ("ﬁona", "fiona"),
    ] {
        let e = refused(&format!(
            "{MIN}user = {}\n[users.{}]\nadmin = true\npassword = {{ file = \"p\" }}\n\
             [users.{}]\npassword = {{ file = \"q\" }}\n",
            toml::Value::String(a.to_owned()),
            toml::Value::String(a.to_owned()),
            toml::Value::String(b.to_owned()),
        ));
        assert!(e.contains("look alike"), "{a:?} {b:?}: {e}");
    }
}

/// A name never holds a path's separators: it names a session, and may name a directory. The
/// legacy form's `[data] user` follows the same rules.
#[test]
fn user_names_are_no_paths() {
    for name in ["ma/rie", "ma\\rie", "ma:rie", "..", "a..b"] {
        let quoted = toml::Value::String(name.to_owned());
        let e = refused(&format!(
            "{MIN}user = \"alix\"\n[users.alix]\nadmin = true\npassword = {{ file = \"p\" }}\n\
             [users.{quoted}]\npassword = {{ file = \"q\" }}\n"
        ));
        assert!(e.contains("a user's name may not hold"), "{name:?}: {e}");
        let e = refused(&format!("{MIN}user = {quoted}\n"));
        assert!(
            e.contains("[data] user: a user's name may not hold"),
            "{name:?}: {e}"
        );
    }
    for hidden in ["al\u{200b}ix", " alix"] {
        let e = refused(&format!(
            "{MIN}user = {}\n",
            toml::Value::String(hidden.to_owned())
        ));
        assert!(e.contains("[data] user: a user's name"), "{hidden:?}: {e}");
    }
}

/// `[agent] env` never sets a variable an agent signs in with, accounts or not.
#[test]
fn agent_env_cannot_set_a_sign_in_variable() {
    for name in [
        "CLAUDE_CODE_OAUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "AWS_BEARER_TOKEN_BEDROCK",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_OAUTH_TOKEN_FILE",
        "CLAUDE_CODE_API_KEY_HELPER_TTL_MS",
        "CLAUDE_CODE_CLIENT_CERT",
    ] {
        for rest in ["", "oauth_token = { file = \"t\" }\n"] {
            let e = refused(&format!("{MIN}[agent]\n{rest}env = {{ {name} = \"x\" }}\n"));
            assert!(
                e.contains(&format!("[agent] env sets {name}"))
                    && e.contains("oauth_token")
                    && e.contains(&format!("remove {name} from env")),
                "{e}"
            );
        }
        let e = refused(&format!(
            "{MIN}[agent]\nenv = {{ {name} = \"x\" }}\n\
             [accounts.me]\nprovider = \"claude\"\nowner = \"me\"\noauth_token = {{ file = \"t\" }}\n"
        ));
        assert!(e.contains(&format!("[agent] env sets {name}")), "{e}");
    }
}

/// Only the legacy form's account may lack a token.
#[test]
fn declared_account_needs_a_token() {
    let e = refused(&format!(
        "{MIN}[accounts.me]\nprovider = \"claude\"\nowner = \"me\"\n"
    ));
    assert!(
        e.contains("[accounts.me] has no oauth_token") && e.contains("claude setup-token"),
        "{e}"
    );
}

/// Two people: the owner, an admin, and a member, each with a password.
const USERS: &str = "[users.alix]\nadmin = true\npassword = { file = \"alix.password\" }\n\
                     [users.marie]\npassword = { env = \"MARIE_PASSWORD\" }\n";

/// `MIN` with `[data] user = "alix"` and `rest` after it.
fn owned_by_alix(rest: &str) -> String {
    format!("{MIN}user = \"alix\"\n{rest}")
}

/// `[users.<name>]`: an admin flag and a password, each password anchored and listed with the
/// other secrets.
#[test]
fn users_parse() {
    let c = load_str(&owned_by_alix(&format!(
        "[web]\nsecure_cookie = true\n{USERS}"
    )))
    .unwrap();
    let users = c.users();
    assert_eq!(users.len(), 2);
    let alix = users.get("alix").unwrap();
    assert!(alix.admin);
    assert!(!users.get("marie").unwrap().admin);
    // Names are compared exactly.
    assert!(users.get("Marie").is_none());
    let base = c.path().unwrap().parent().unwrap().to_path_buf();
    let (key, password) = c.password_of("alix").unwrap();
    assert_eq!(key, "[users.alix] password");
    assert!(matches!(password, Secret::File { file } if file == &base.join("alix.password")));
    assert!(c.secret_files().contains(&base.join("alix.password")));
    assert!(c.secret_env_names().contains(&"MARIE_PASSWORD".to_owned()));
    assert!(c.password_of("paul").is_none());
    assert!(c.web.unwrap().secure_cookie);
    // A name with spaces and accents is a name.
    let c = load_str(&format!(
        "{MIN}user = \"Anne-Sophie Dupré\"\n\
         [users.\"Anne-Sophie Dupré\"]\nadmin = true\npassword = {{ file = \"p\" }}\n"
    ))
    .unwrap();
    assert_eq!(
        c.password_of("Anne-Sophie Dupré").unwrap().0,
        "[users.\"Anne-Sophie Dupré\"] password"
    );
    // No key but these, never a stored password, never one written in the file.
    for bad in [
        "[users.alix]\nadmin = true\npassword = { file = \"p\" }\nrole = \"x\"\n",
        "[users.alix]\nadmin = true\npassword = { stored = \"p\" }\n",
        "[users.alix]\nadmin = true\npassword = \"hunter2\"\n",
    ] {
        assert!(load_str(&owned_by_alix(bad)).is_err(), "{bad}");
    }
    let e = refused(&owned_by_alix(
        "[users.alix]\nadmin = true\npassword = { stored = \"p\" }\n",
    ));
    assert!(
        e.contains("[users.alix] password = { stored = \"p\" }"),
        "{e}"
    );
}

/// Without `[users]`, `[data] user` is the one user, an admin signing in with `[web] password`.
#[test]
fn legacy_password_is_the_owner() {
    let c = load_str(&owned_by_alix("[web]\npassword = { env = \"P\" }\n")).unwrap();
    let users = c.users();
    assert_eq!(users.len(), 1);
    assert_eq!(
        users.get("alix"),
        Some(&fronde_requests::User {
            name: "alix".into(),
            admin: true,
        })
    );
    let (key, password) = c.password_of("alix").unwrap();
    assert_eq!(key, "web.password");
    assert!(matches!(password, Secret::Env { env } if env == "P"));
    assert!(c.password_of("me").is_none());
    // Without [web], the one user has no password: nobody signs in to a page.
    let c = load_str(&owned_by_alix("")).unwrap();
    assert_eq!(c.user_names(), ["alix"]);
    assert!(c.password_of("alix").is_none());
    // [web] needs a password when no [users] gives one.
    let e = refused(&owned_by_alix("[web]\nsecure_cookie = true\n"));
    assert!(
        e.contains("[web] has no password") && e.contains("[users.<name>]"),
        "{e}"
    );
}

#[test]
fn web_password_and_users_are_refused_together() {
    let e = refused(&owned_by_alix(&format!(
        "[web]\npassword = {{ file = \"p\" }}\n{USERS}"
    )));
    assert!(
        e.contains("[web] password and [users] are both set")
            && e.contains("move the password to [users.alix] password"),
        "{e}"
    );
}

/// `[data] user` is the owner: listed in `[users]`, an admin.
#[test]
fn owner_must_be_an_admin_user() {
    let e = refused(&format!("{MIN}user = \"paul\"\n{USERS}"));
    assert!(
        e.contains("[data] user = \"paul\" is not in [users]")
            && e.contains("add [users.paul] with admin = true")
            && e.contains("alix, marie"),
        "{e}"
    );
    let e = refused(&format!("{MIN}user = \"marie\"\n{USERS}"));
    assert!(
        e.contains("[data] user = \"marie\" is the owner, an admin")
            && e.contains("set admin = true in [users.marie]"),
        "{e}"
    );
    // The default [data] user, too.
    let e = refused(&format!("{MIN}{USERS}"));
    assert!(e.contains("[data] user = \"me\" is not in [users]"), "{e}");
}

#[test]
fn user_needs_a_sign_in_method() {
    let e = refused(&owned_by_alix("[users.alix]\nadmin = true\n"));
    assert!(
        e.contains("[users.alix] has no way to sign in")
            && e.contains("password = { file = \"…\" }"),
        "{e}"
    );
}

/// A name is 1 to 64 visible characters, without spaces around it: two names that look alike
/// are never two people.
#[test]
fn user_name_refuses_hidden_characters() {
    let user = |name: &str| {
        refused(&format!(
            "{MIN}user = \"alix\"\n[users.alix]\nadmin = true\npassword = {{ file = \"p\" }}\n\
             [users.{}]\npassword = {{ file = \"q\" }}\n",
            toml::Value::String(name.to_owned())
        ))
    };
    for hidden in ["mar\u{200b}ie", "marie\u{202e}", "ma\trie", "ma\u{ad}rie"] {
        let e = user(hidden);
        assert!(
            e.contains("may not hold a hidden character"),
            "{hidden:?}: {e}"
        );
    }
    for spaced in [" marie", "marie "] {
        let e = user(spaced);
        assert!(
            e.contains("may not start or end with a space"),
            "{spaced:?}: {e}"
        );
    }
    let e = user("");
    assert!(e.contains("1 to 64 characters"), "{e}");
    let e = user(&"é".repeat(65));
    assert!(e.contains("1 to 64 characters"), "{e}");
    load_str(&format!(
        "{MIN}user = \"{}\"\n[users.\"{}\"]\nadmin = true\npassword = {{ file = \"p\" }}\n",
        "é".repeat(64),
        "é".repeat(64)
    ))
    .unwrap();
}

/// With `[users]`, an account's owner is one of them, and each person has their own token.
#[test]
fn accounts_belong_to_users() {
    let accounts = "[accounts.alix]\nprovider = \"claude\"\nowner = \"alix\"\n\
                    oauth_token = { stored = \"claude_alix\" }\n\
                    [accounts.marie]\nprovider = \"claude\"\nowner = \"marie\"\n\
                    oauth_token = { stored = \"claude_marie\" }\n";
    let c = load_str(&owned_by_alix(&format!("{USERS}{accounts}"))).unwrap();
    let owners: Vec<(String, String)> = c
        .accounts()
        .into_iter()
        .map(|account| (account.name, account.owner))
        .collect();
    assert_eq!(
        owners,
        [
            ("alix".to_owned(), "alix".to_owned()),
            ("marie".to_owned(), "marie".to_owned())
        ]
    );
    let e = refused(&owned_by_alix(&format!(
        "{USERS}{}",
        accounts.replace("owner = \"marie\"", "owner = \"paul\"")
    )));
    assert!(
        e.contains("[accounts.marie] owner = \"paul\" is not a user") && e.contains("alix, marie"),
        "{e}"
    );
    // Two people, one token: refused, the token being named once per account.
    let e = refused(&owned_by_alix(&format!(
        "{USERS}{}",
        accounts.replace("claude_marie", "claude_alix")
    )));
    assert!(
        e.contains(
            "[accounts.alix] oauth_token and [accounts.marie] oauth_token both read their \
             secret from the stored secret claude_alix"
        ),
        "{e}"
    );
}

/// `python` names `[python]`'s source, whose one tool, `code_run_python`, anyone may use: no
/// other source may take that name, with `[python]` or without.
#[test]
fn python_source_name_is_always_reserved() {
    let e = refused(&format!(
        "{MIN}\n[[sources]]\nkind = \"mcp\"\nname = \"python\"\ncommand = [\"x\"]\n"
    ));
    assert!(
        e.contains("source name \"python\" is reserved") && e.contains("pick another name"),
        "{e}"
    );
}

/// An unconfined agent reads everything Fronde can: never once someone who is not an admin
/// signs in.
#[test]
fn no_unconfined_agent_for_members() {
    let e = refused(&owned_by_alix(&format!(
        "[agent]\nsandbox = false\n{USERS}"
    )));
    assert!(
        e.contains("[agent] sandbox = false with [users.marie], who is not an admin"),
        "{e}"
    );
    // Alone, the owner may still choose it.
    load_str(&owned_by_alix(
        "[agent]\nsandbox = false\n[users.alix]\nadmin = true\npassword = { file = \"alix.password\" }\n",
    ))
    .unwrap();
    load_str(&format!("{MIN}[agent]\nsandbox = false\n")).unwrap();
}

/// A second person, an admin too, or a second account is someone whose files and token an
/// unconfined agent of the other could read: refused, the key named.
#[test]
fn no_unconfined_agent_for_a_second_person_or_account() {
    let admins = USERS.replace("[users.marie]\n", "[users.marie]\nadmin = true\n");
    let e = refused(&owned_by_alix(&format!(
        "[agent]\nsandbox = false\n{admins}"
    )));
    assert!(
        e.contains("[agent] sandbox = false")
            && e.contains("[users.alix] and [users.marie]")
            && e.contains("keep the sandbox on"),
        "{e}"
    );
    let accounts = "[accounts.alix]\nprovider = \"claude\"\nowner = \"alix\"\n\
                    oauth_token = { file = \"alix.token\" }\n\
                    [accounts.marie]\nprovider = \"claude\"\nowner = \"marie\"\n\
                    oauth_token = { file = \"marie.token\" }\n";
    let e = refused(&owned_by_alix(&format!(
        "[agent]\nsandbox = false\n{admins}{accounts}"
    )));
    assert!(e.contains("[agent] sandbox = false"), "{e}");
    // One person with one account keeps the choice.
    load_str(&owned_by_alix(
        "[agent]\nsandbox = false\n[users.alix]\nadmin = true\npassword = { file = \"alix.password\" }\n\
         [accounts.alix]\nprovider = \"claude\"\nowner = \"alix\"\noauth_token = { file = \"alix.token\" }\n",
    ))
    .unwrap();
}

/// Full case folding: names that differ only by a letter's folded form look alike.
#[test]
fn user_names_fold_fully() {
    for (a, b) in [("Straße", "STRASSE"), ("ǅana", "džana")] {
        let e = refused(&format!(
            "{MIN}user = {}\n[users.{}]\nadmin = true\npassword = {{ file = \"p\" }}\n\
             [users.{}]\npassword = {{ file = \"q\" }}\n",
            toml::Value::String(a.to_owned()),
            toml::Value::String(a.to_owned()),
            toml::Value::String(b.to_owned()),
        ));
        assert!(e.contains("look alike"), "{a:?} {b:?}: {e}");
    }
}

/// Three people: Alix, the owner and an admin, Marie and Paul.
const FAMILY: &str = "[users.alix]\nadmin = true\npassword = { file = \"alix.password\" }\n\
                      [users.marie]\npassword = { file = \"marie.password\" }\n\
                      [users.paul]\npassword = { file = \"paul.password\" }\n";

/// `[accounts.<name>]` on Claude, owned by `owner`, with `rest` (its `users` and `default_for`)
/// and its own stored token, `claude_<name>`.
fn account(name: &str, owner: &str, rest: &str) -> String {
    format!(
        "[accounts.{name}]\nprovider = \"claude\"\nowner = \"{owner}\"\n{rest}\
         oauth_token = {{ stored = \"claude_{name}\" }}\n"
    )
}

/// The design's family: Alix uses her own account and the family one (hers by default), Marie
/// the family one (by default) and her own, Paul the family one only.
fn family() -> String {
    owned_by_alix(&format!(
        "{FAMILY}{}{}{}",
        account("alix", "alix", "default_for = [\"alix\"]\n"),
        account(
            "famille",
            "alix",
            "users = [\"marie\", \"paul\"]\ndefault_for = [\"marie\", \"paul\"]\n"
        ),
        account("marie", "marie", ""),
    ))
}

/// An account's `users` and `default_for` both default to empty.
#[test]
fn account_users_parse() {
    let c = load_str(&family()).unwrap();
    let accounts = c.accounts();
    let seen: Vec<(&str, &[String], &[String])> = accounts
        .iter()
        .map(|account| {
            (
                account.name.as_str(),
                account.users.as_slice(),
                account.default_for.as_slice(),
            )
        })
        .collect();
    let names = |names: &[&str]| {
        names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        seen,
        [
            ("alix", &[][..], &names(&["alix"])[..]),
            (
                "famille",
                &names(&["marie", "paul"])[..],
                &names(&["marie", "paul"])[..]
            ),
            ("marie", &[][..], &[][..]),
        ]
    );
    // Who may use which: the owner and those listed, no one else.
    for (user, account, may) in [
        ("alix", "alix", true),
        ("alix", "famille", true),
        ("alix", "marie", false),
        ("marie", "famille", true),
        ("marie", "marie", true),
        ("marie", "alix", false),
        ("paul", "famille", true),
        ("paul", "alix", false),
        ("paul", "marie", false),
        ("paul", "nobody", false),
        ("lea", "famille", false),
    ] {
        assert_eq!(c.may_use(user, account), may, "{user} on {account}");
    }
    let default = |user: &str| {
        c.default_account(user, "claude")
            .map(|account| account.name)
    };
    assert_eq!(default("alix").as_deref(), Some("alix"));
    assert_eq!(default("marie").as_deref(), Some("famille"));
    assert_eq!(default("paul").as_deref(), Some("famille"));
    assert_eq!(default("lea"), None);
    assert_eq!(c.default_account("alix", "codex").map(|a| a.name), None);
}

/// `users` names people of `[users]`, and nobody else.
#[test]
fn account_users_must_be_users() {
    let e = refused(&owned_by_alix(&format!(
        "{FAMILY}{}",
        account("famille", "alix", "users = [\"marie\", \"lea\"]\n")
    )));
    assert!(
        e.contains("[accounts.famille] users names \"lea\", who is not a user")
            && e.contains("alix, marie, paul"),
        "{e}"
    );
}

/// The owner may always use their account, so `users` never names them, nor anyone twice.
#[test]
fn account_users_refuse_the_owner_and_duplicates() {
    let e = refused(&owned_by_alix(&format!(
        "{FAMILY}{}",
        account("famille", "alix", "users = [\"alix\", \"marie\"]\n")
    )));
    assert!(
        e.contains("[accounts.famille] users names \"alix\", its owner"),
        "{e}"
    );
    let e = refused(&owned_by_alix(&format!(
        "{FAMILY}{}",
        account(
            "famille",
            "alix",
            "users = [\"marie\", \"paul\", \"marie\"]\n"
        )
    )));
    assert!(
        e.contains("[accounts.famille] users names \"marie\" twice"),
        "{e}"
    );
    let e = refused(&owned_by_alix(&format!(
        "{FAMILY}{}",
        account(
            "famille",
            "alix",
            "users = [\"marie\"]\ndefault_for = [\"marie\", \"marie\"]\n"
        )
    )));
    assert!(
        e.contains("[accounts.famille] default_for names \"marie\" twice"),
        "{e}"
    );
}

/// An account is nobody's default unless they may use it.
#[test]
fn default_for_must_be_able_to_use() {
    let e = refused(&owned_by_alix(&format!(
        "{FAMILY}{}",
        account(
            "famille",
            "alix",
            "users = [\"marie\"]\ndefault_for = [\"marie\", \"paul\"]\n"
        )
    )));
    assert!(
        e.contains("[accounts.famille] default_for names \"paul\", who may not use it")
            && e.contains("users"),
        "{e}"
    );
    // Not even someone who is no user at all.
    let e = refused(&owned_by_alix(&format!(
        "{FAMILY}{}",
        account("famille", "alix", "default_for = [\"lea\"]\n")
    )));
    assert!(
        e.contains("[accounts.famille] default_for names \"lea\", who may not use it"),
        "{e}"
    );
}

/// A person has at most one default account per provider.
#[test]
fn one_default_per_person_and_provider() {
    let e = refused(&owned_by_alix(&format!(
        "{FAMILY}{}{}",
        account(
            "famille",
            "alix",
            "users = [\"marie\"]\ndefault_for = [\"marie\"]\n"
        ),
        account("marie", "marie", "default_for = [\"marie\"]\n"),
    )));
    assert!(
        e.contains(
            "[accounts.famille] default_for and [accounts.marie] default_for both name \"marie\" \
             on claude"
        ),
        "{e}"
    );
}

/// Two accounts one person may use on one provider, and neither names them in `default_for`:
/// refused, naming both and the key, so that adding someone to an account never changes their
/// default silently.
#[test]
fn two_usable_accounts_need_a_default() {
    let e = refused(&owned_by_alix(&format!(
        "{FAMILY}{}{}",
        account("famille", "alix", "users = [\"marie\"]\n"),
        account("marie", "marie", ""),
    )));
    assert!(
        e.contains("\"marie\" may use [accounts.famille] and [accounts.marie] on claude")
            && e.contains("default_for = [\"marie\"]"),
        "{e}"
    );
    // Alix owns one and is listed nowhere else: she uses one, hers, and needs no default.
    let e = refused(&owned_by_alix(&format!(
        "{FAMILY}{}{}{}",
        account("alix", "alix", ""),
        account(
            "famille",
            "alix",
            "users = [\"marie\"]\ndefault_for = [\"marie\"]\n"
        ),
        account("marie", "marie", ""),
    )));
    assert!(
        e.contains("\"alix\" may use [accounts.alix] and [accounts.famille] on claude"),
        "{e}"
    );
}

/// With one account a person may use on a provider, it is their default without naming it.
#[test]
fn default_is_the_only_usable_account() {
    let c = load_str(&owned_by_alix(&format!(
        "{FAMILY}{}{}",
        account("famille", "alix", "users = [\"marie\"]\n"),
        account("paul", "paul", ""),
    )))
    .unwrap();
    let default = |user: &str| {
        c.default_account(user, "claude")
            .map(|account| account.name)
    };
    assert_eq!(default("alix").as_deref(), Some("famille"));
    assert_eq!(default("marie").as_deref(), Some("famille"));
    assert_eq!(default("paul").as_deref(), Some("paul"));
    // An account of no one else's is nobody's default but its owner's.
    assert!(!c.may_use("marie", "paul"));
}

/// The legacy form's one account is its owner's alone, and their default.
#[test]
fn legacy_account_has_no_other_users() {
    let c = load_str(&format!(
        "{MIN}user = \"alix\"\n[agent]\noauth_token = {{ stored = \"claude\" }}\n"
    ))
    .unwrap();
    let accounts = c.accounts();
    assert_eq!(accounts.len(), 1);
    assert!(accounts[0].users.is_empty() && accounts[0].default_for.is_empty());
    assert!(c.may_use("alix", "claude"));
    assert!(!c.may_use("marie", "claude"));
    assert_eq!(
        c.default_account("alix", "claude")
            .map(|account| account.name),
        Some("claude".to_owned())
    );
    assert_eq!(c.default_account("marie", "claude").map(|a| a.name), None);
}

/// Sharing goes through `users`, never through one token: two accounts still cannot read one
/// secret, whoever may use them.
#[test]
fn shared_account_still_names_its_own_secret() {
    let e = refused(&owned_by_alix(&format!(
        "{FAMILY}{}{}",
        account(
            "famille",
            "alix",
            "users = [\"marie\"]\ndefault_for = [\"marie\"]\n"
        ),
        account("marie", "marie", "").replace("claude_marie", "claude_famille"),
    )));
    assert!(
        e.contains(
            "[accounts.famille] oauth_token and [accounts.marie] oauth_token both read their \
             secret from the stored secret claude_famille"
        ),
        "{e}"
    );
}

/// No upstream server is handed a sign-in variable: not as its token, nor set by its command
/// (`env CLAUDE_CODE_OAUTH_TOKEN=… server`). The same names `[agent] env` refuses.
#[test]
fn upstream_servers_get_no_sign_in_variable() {
    for name in [
        "CLAUDE_CODE_OAUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CONFIG_DIR",
        "CLAUDE_CODE_USE_BEDROCK",
    ] {
        let e = refused(&format!(
            "{MIN}\n[[sources]]\nkind = \"mcp\"\nname = \"shop\"\nurl = \"https://x/mcp\"\n\
             token = {{ env = \"{name}\" }}\n"
        ));
        assert!(
            e.contains(&format!("source \"shop\": token = {{ env = \"{name}\" }}"))
                && e.contains("signs an agent in"),
            "{name}: {e}"
        );
        let e = refused(&format!(
            "{MIN}\n[[sources]]\nkind = \"mcp\"\nname = \"shop\"\n\
             command = [\"env\", \"{name}=x\", \"shop-server\"]\n"
        ));
        assert!(
            e.contains(&format!("source \"shop\": command sets {name}"))
                && e.contains("signs an agent in"),
            "{name}: {e}"
        );
    }
    // Its own token, and its own variables, stay allowed.
    load_str(&format!(
        "{MIN}\n[[sources]]\nkind = \"mcp\"\nname = \"shop\"\nurl = \"https://x/mcp\"\n\
         token = {{ env = \"SHOP_TOKEN\" }}\n[[sources]]\nkind = \"mcp\"\nname = \"notes\"\n\
         command = [\"env\", \"NOTES_DIR=/srv/notes\", \"notes-server\"]\n"
    ))
    .unwrap();
}
