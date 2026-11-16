use hub::config::{Secret, load};
use std::io::Write;

fn load_str(s: &str) -> anyhow::Result<hub::config::Config> {
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
    let c = load(std::path::Path::new("../../jarvis.example.toml")).unwrap();
    assert_eq!(c.data.timezone, chrono_tz::Europe::Paris);
    assert_eq!(c.mcp.len(), 1);
    assert_eq!(c.data.user, "alix");
}

#[test]
fn defaults_apply() {
    let c = load_str(MIN).unwrap();
    assert_eq!(c.server.listen.to_string(), "127.0.0.1:8484");
    assert_eq!(c.data.user, "me");
    assert_eq!((c.memory.memory_limit, c.memory.user_limit, c.memory.git), (4000, 2000, true));
    assert!(c.calendar.is_none());
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
    unsafe { std::env::set_var("HUB_TEST_SECRET", "abc") };
    assert_eq!(Secret::Env { env: "HUB_TEST_SECRET".into() }.resolve().unwrap(), "abc");
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(b"s3cret\n").unwrap();
    assert_eq!(Secret::File { file: f.path().into() }.resolve().unwrap(), "s3cret");
}

#[test]
fn missing_secret_explains() {
    let e = Secret::Env { env: "ABSENT_X".into() }.resolve().unwrap_err().to_string();
    assert!(e.contains("ABSENT_X") && e.contains("set"), "{e}");
}

#[test]
fn rejects_reserved_and_duplicate_upstream_names() {
    let s = format!("{MIN}\n[[mcp]]\nname = \"hub\"\nurl = \"http://x/mcp\"\n");
    assert!(format!("{:#}", load_str(&s).unwrap_err()).contains("reserved"));
    let s = format!("{MIN}\n[[mcp]]\nname = \"a\"\nurl = \"http://x/mcp\"\n[[mcp]]\nname = \"a\"\nurl = \"http://y/mcp\"\n");
    assert!(format!("{:#}", load_str(&s).unwrap_err()).contains("twice"));
}
