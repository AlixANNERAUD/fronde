use fronde_hub::config::{Secret, load};
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
    assert_eq!(c.mcp.len(), 1);
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
    assert!(c.calendar.is_none());
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
    let s = format!("{MIN}\n[[mcp]]\nname = \"a\"\nurl = \"http://x/mcp\"\ncommand = [\"x\"]\n");
    let e = load_str(&s).unwrap_err();
    assert!(format!("{e:#}").contains("exactly one of"), "{e:#}");
    let s = format!("{MIN}\n[[mcp]]\nname = \"a\"\n");
    assert!(format!("{:#}", load_str(&s).unwrap_err()).contains("exactly one of"));
}

#[test]
fn rejects_bad_timezone() {
    assert!(load_str(&MIN.replace("Europe/Paris", "Paris")).is_err());
}

#[test]
fn rejects_bad_tool_prefix_name() {
    let s = format!("{MIN}\n[[mcp]]\nname = \"MCP Hub\"\nurl = \"http://x/mcp\"\n");
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
    let s = format!("{MIN}\n[[mcp]]\nname = \"hub\"\nurl = \"http://x/mcp\"\n");
    assert!(format!("{:#}", load_str(&s).unwrap_err()).contains("reserved"));
    let s = format!(
        "{MIN}\n[[mcp]]\nname = \"a\"\nurl = \"http://x/mcp\"\n[[mcp]]\nname = \"a\"\nurl = \"http://y/mcp\"\n"
    );
    assert!(format!("{:#}", load_str(&s).unwrap_err()).contains("twice"));
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
    assert_eq!(xdg_data_dir(None, None), None);
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
