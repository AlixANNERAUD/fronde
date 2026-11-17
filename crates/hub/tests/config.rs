use fronde_hub::config::{Secret, SourceConfig, load};
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
    assert!(c.web.is_some());
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
        Secret::Env { env: "PATH".into() }.resolve().unwrap(),
        std::env::var("PATH").unwrap()
    );
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(b"s3cret\n").unwrap();
    assert_eq!(
        Secret::File {
            file: f.path().into()
        }
        .resolve()
        .unwrap(),
        "s3cret"
    );
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
    let s = format!("{MIN}\n[[sources]]\nkind = \"mcp\"\nname = \"hub\"\nurl = \"http://x/mcp\"\n");
    assert!(format!("{:#}", load_str(&s).unwrap_err()).contains("reserved"));
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
    assert_eq!(c.server.token.resolve().unwrap(), "from-config-dir");
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
