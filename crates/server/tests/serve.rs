use rmcp::ServiceExt;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn config(dir: &std::path::Path, token: &str) -> fronde_hub::config::Config {
    let token_file = dir.join("token");
    fronde_hub::private::write(&token_file, token).unwrap();
    let toml = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\ntoken = {{ file = {:?} }}\n[data]\ndir = {:?}\ntimezone = \"Europe/Paris\"\n[memory]\ngit = false\n",
        token_file.display().to_string(),
        dir.join("data").display().to_string()
    );
    toml::from_str(&toml).unwrap()
}

#[tokio::test]
async fn health_without_token() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _h) = fronde::start(config(dir.path(), TOKEN)).await.unwrap();
    let r = reqwest::get(format!("http://{addr}/health")).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn mcp_rejects_missing_or_wrong_token() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _h) = fronde::start(config(dir.path(), TOKEN)).await.unwrap();
    let c = reqwest::Client::new();
    let url = format!("http://{addr}/mcp");
    assert_eq!(c.post(&url).send().await.unwrap().status(), 401);
    let wrong = c
        .post(&url)
        .bearer_auth("0123456789abcdef0123456789abcdeX")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
}

#[tokio::test]
async fn mcp_initialize_and_list_tools() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _h) = fronde::start(config(dir.path(), TOKEN)).await.unwrap();
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
            .auth_header(TOKEN),
    );
    let client = ().serve(transport).await.unwrap();
    let names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    assert!(
        names.contains(&"memory_add".to_string()) && names.contains(&"search_tools".to_string()),
        "{names:?}"
    );
}

#[tokio::test]
async fn non_localhost_host_header_is_accepted_with_token() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _h) = fronde::start(config(dir.path(), TOKEN)).await.unwrap();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    let r = reqwest::Client::new()
        .post(format!("http://{addr}/mcp"))
        .header("Host", "fronde.example")
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .bearer_auth(TOKEN)
        .body(body)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.status());
}

#[tokio::test]
async fn short_token_refuses_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let e = fronde::start(config(dir.path(), "short"))
        .await
        .unwrap_err();
    assert!(
        format!("{e:#}").contains("server.token must be at least 32 characters"),
        "{e:#}"
    );
}

#[test]
fn check_reports_unknown_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fronde.toml");
    std::fs::write(&path, "[server]\nlistn = 1\n").unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_fronde"))
        .args(["check", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("listn"));
}

#[test]
fn check_accepts_valid_config() {
    let dir = tempfile::tempdir().unwrap();
    fronde_hub::private::write(dir.path().join("token"), TOKEN).unwrap();
    let path = dir.path().join("fronde.toml");
    std::fs::write(&path, format!("[server]\ntoken = {{ file = {:?} }}\n[data]\ndir = \"/tmp/x\"\ntimezone = \"Europe/Paris\"\n", dir.path().join("token").display().to_string())).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_fronde"))
        .args(["check", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("config OK"));
}

#[cfg(unix)]
#[test]
fn check_refuses_shared_secret_file_unless_allowed() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, TOKEN).unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
    let path = dir.path().join("fronde.toml");
    std::fs::write(&path, format!("[server]\ntoken = {{ file = {:?} }}\n[data]\ndir = \"/tmp/x\"\ntimezone = \"Europe/Paris\"\n", token.display().to_string())).unwrap();
    let run = |allow: bool| {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_fronde"));
        cmd.args(["check", "--config"]).arg(&path);
        if allow {
            cmd.env("FRONDE_ALLOW_SHARED_SECRET_FILES", "1");
        }
        cmd.output().unwrap()
    };
    let out = run(false);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("readable by other users; run chmod 600"),
        "{stderr}"
    );
    assert_eq!(run(true).status.code(), Some(0));
}

#[tokio::test]
async fn healthcheck_reaches_a_running_server_only() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _h) = fronde::start(config(dir.path(), TOKEN)).await.unwrap();
    fronde::healthcheck(addr).await.unwrap();
    // A server listening on every interface is checked over loopback.
    let unspecified = std::net::SocketAddr::from(([0, 0, 0, 0], addr.port()));
    fronde::healthcheck(unspecified).await.unwrap();

    let free = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    assert!(fronde::healthcheck(free).await.is_err());
}

#[test]
fn healthcheck_command_fails_without_a_server() {
    let dir = tempfile::tempdir().unwrap();
    let free = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let path = dir.path().join("fronde.toml");
    std::fs::write(&path, format!("[server]\nlisten = \"{free}\"\ntoken = {{ env = \"UNSET\" }}\n[data]\ndir = \"/tmp/x\"\ntimezone = \"Europe/Paris\"\n")).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_fronde"))
        .args(["healthcheck", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains(&format!("cannot reach http://{free}/health"))
    );
}
