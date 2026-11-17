use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use fronde_agent::acp::AgentSpec;
use fronde_hub::config::{ComponentConfig, Config};
use fronde_wasm::{Checks, Credential, Hashes, prepare};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path as url_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn dist(file: &str) -> PathBuf {
    let p = workspace_root().join("components/dist").join(file);
    assert!(
        p.exists(),
        "{} is missing: run `mise run components`",
        p.display()
    );
    p
}

/// Strip `#[cfg(test)]` / `#[cfg(all(test, ...))]` items from Rust source so unit tests can
/// assert on `.expose_secret()` without widening the production call-site allowlist.
fn strip_test_modules(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut pending_cfg_test = false;
    let mut skip_depth: usize = 0;

    for line in source.lines() {
        let trimmed = line.trim();
        if skip_depth > 0 {
            for ch in line.chars() {
                if ch == '{' {
                    skip_depth += 1;
                } else if ch == '}' {
                    skip_depth -= 1;
                    if skip_depth == 0 {
                        break;
                    }
                }
            }
            continue;
        }
        if trimmed.starts_with("#[cfg(") && trimmed.contains("test") {
            pending_cfg_test = true;
            continue;
        }
        if pending_cfg_test {
            if trimmed.starts_with("#[") || trimmed.is_empty() {
                continue;
            }
            let opens = line.chars().filter(|&c| c == '{').count();
            if opens > 0 {
                let closes = line.chars().filter(|&c| c == '}').count();
                skip_depth = opens.saturating_sub(closes);
                pending_cfg_test = false;
                continue;
            }
            if trimmed.ends_with(';') {
                pending_cfg_test = false;
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_rs_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn expose_secret_call_sites() {
    let root = workspace_root();
    let mut files = Vec::new();
    for parent in ["crates", "components"] {
        let Ok(entries) = std::fs::read_dir(root.join(parent)) else {
            continue;
        };
        for entry in entries.flatten() {
            let src = entry.path().join("src");
            if src.is_dir() {
                collect_rs_files(&src, &mut files);
            }
        }
    }
    files.sort();

    let mut found: BTreeMap<String, Vec<(usize, String)>> = BTreeMap::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let prod = strip_test_modules(&text);
        let hits: Vec<(usize, String)> = prod
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains("expose_secret("))
            .map(|(i, line)| (i + 1, line.trim().to_string()))
            .collect();
        if !hits.is_empty() {
            let rel = file
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            found.insert(rel, hits);
        }
    }

    let expected: BTreeSet<&str> = [
        "crates/agent/src/acp.rs",
        "crates/hub/src/upstream.rs",
        "crates/server/src/lib.rs",
        "crates/server/src/web.rs",
        "crates/wasm/src/egress.rs",
    ]
    .into_iter()
    .collect();
    let actual: BTreeSet<&str> = found.keys().map(String::as_str).collect();
    assert_eq!(
        actual, expected,
        "expose_secret( call sites changed: {found:#?}"
    );
}

#[test]
fn no_secret_in_debug() {
    let dir = tempfile::tempdir().unwrap();
    let mcp_canary = "canary-mcp-token-0123456789abcdef0123456789";
    let web_canary = "canary-web-password-9f8e7d6c";
    let upstream_canary = "canary-upstream-token-5a4b3c2d";
    let caldav_canary = "canary-caldav-password-1e2f3a4b";
    let bearer_canary = "canary-bearer-token-7c8d9e0f";
    let agent_canary = "canary-agent-hub-token-3b4c5d6e";

    let token_file = dir.path().join("mcp_token");
    let web_file = dir.path().join("web_pass");
    let upstream_file = dir.path().join("upstream_tok");
    let caldav_file = dir.path().join("caldav_pass");
    fronde_hub::private::write(&token_file, mcp_canary).unwrap();
    fronde_hub::private::write(&web_file, web_canary).unwrap();
    fronde_hub::private::write(&upstream_file, upstream_canary).unwrap();
    fronde_hub::private::write(&caldav_file, caldav_canary).unwrap();

    let wasm = dist("calendar.wasm");
    let manifest = dist("calendar.toml");
    let toml = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\ntoken = {{ file = {:?} }}\n\
         [data]\ndir = {:?}\ntimezone = \"Europe/Paris\"\n\
         [web]\npassword = {{ file = {:?} }}\n\
         [[sources]]\nkind = \"mcp\"\nname = \"mcphub\"\nurl = \"https://mcphub.example/mcp\"\ntoken = {{ file = {:?} }}\n\
         [[sources]]\nkind = \"component\"\nname = \"calendar\"\npath = {:?}\nmanifest = {:?}\n\
         allow = [\"https://cloud.example\"]\n\
         [sources.settings]\nevents_url = \"https://cloud.example/e/\"\nreminders_url = \"https://cloud.example/r/\"\n\
         [[sources.credentials]]\norigin = \"https://cloud.example\"\n\
         basic = {{ username = \"alix\", password = {{ file = {:?} }} }}\n",
        token_file.display().to_string(),
        dir.path().join("data").display().to_string(),
        web_file.display().to_string(),
        upstream_file.display().to_string(),
        wasm.display().to_string(),
        manifest.display().to_string(),
        caldav_file.display().to_string(),
    );
    let config_path = dir.path().join("fronde.toml");
    std::fs::write(&config_path, &toml).unwrap();
    let config: Config = fronde_hub::config::load(&config_path).unwrap();

    let component_cfg: ComponentConfig = toml::from_str(&format!(
        "name = \"calendar\"\npath = {:?}\nmanifest = {:?}\n\
         allow = [\"https://cloud.example\"]\n\
         [settings]\nevents_url = \"https://cloud.example/e/\"\nreminders_url = \"https://cloud.example/r/\"\n\
         [[credentials]]\norigin = \"https://cloud.example\"\n\
         basic = {{ username = \"alix\", password = {{ file = {:?} }} }}\n",
        wasm.display().to_string(),
        manifest.display().to_string(),
        caldav_file.display().to_string(),
    ))
    .unwrap();
    let prepared = prepare(&component_cfg, &Checks::of(&config)).unwrap();
    let spec = prepared.spec();

    let basic = Credential::Basic {
        username: "alix".into(),
        password: caldav_canary.into(),
    };
    let bearer = Credential::Bearer(bearer_canary.into());
    let agent_spec = AgentSpec {
        command: vec!["claude-agent-acp".into()],
        env: Vec::new(),
        cwd: dir.path().to_path_buf(),
        hub_url: "http://127.0.0.1:8484/mcp".into(),
        hub_token: agent_canary.into(),
        system_prompt: "You are Fronde.".into(),
        builtin_tools: Vec::new(),
    };

    let all_canaries = [
        mcp_canary,
        web_canary,
        upstream_canary,
        caldav_canary,
        bearer_canary,
        agent_canary,
    ];
    let formatted = [
        ("Config compact", format!("{config:?}")),
        ("Config pretty", format!("{config:#?}")),
        ("Prepared compact", format!("{prepared:?}")),
        ("Prepared pretty", format!("{prepared:#?}")),
        ("ComponentSpec compact", format!("{spec:?}")),
        ("ComponentSpec pretty", format!("{spec:#?}")),
        ("Credential::Basic compact", format!("{basic:?}")),
        ("Credential::Basic pretty", format!("{basic:#?}")),
        ("Credential::Bearer compact", format!("{bearer:?}")),
        ("Credential::Bearer pretty", format!("{bearer:#?}")),
        ("AgentSpec compact", format!("{agent_spec:?}")),
        ("AgentSpec pretty", format!("{agent_spec:#?}")),
    ];
    for (label, text) in &formatted {
        for canary in all_canaries {
            assert!(!text.contains(canary), "{label} leaked {canary}: {text}");
        }
    }
    assert!(format!("{agent_spec:?}").contains("[REDACTED]"));
    assert!(format!("{agent_spec:#?}").contains("[REDACTED]"));
}

#[tokio::test]
async fn log_canary_never_appears() {
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Buffer {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let caldav_canary = "canary-caldav-secret-9f8e7d6c";
    let mcp_canary = "canary-mcp-token-0123456789abcdef0123456789";
    let web_canary = "canary-web-secret-4a5b6c7d";
    let expected_basic_b64 = STANDARD.encode(format!("alix:{caldav_canary}"));
    let expected_auth = format!("Basic {expected_basic_b64}");

    let mock_server = MockServer::start().await;
    let body =
        r#"<multistatus xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"></multistatus>"#;
    Mock::given(method("REPORT"))
        .and(url_path("/cal/"))
        .respond_with(ResponseTemplate::new(207).set_body_string(body))
        .expect(1)
        .mount(&mock_server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("mcp_token");
    let web_file = dir.path().join("web_pass");
    let caldav_file = dir.path().join("caldav_pass");
    fronde_hub::private::write(&token_file, mcp_canary).unwrap();
    fronde_hub::private::write(&web_file, web_canary).unwrap();
    fronde_hub::private::write(&caldav_file, format!("{caldav_canary}\n")).unwrap();

    let mut component_cfg: ComponentConfig = toml::from_str(&format!(
        "name = \"calendar\"\npath = {:?}\nmanifest = {:?}\n",
        dist("calendar.wasm").display().to_string(),
        dist("calendar.toml").display().to_string(),
    ))
    .unwrap();
    let hashes = Hashes::of(&component_cfg).unwrap();
    component_cfg.sha256 = Some(hashes.sha256);
    component_cfg.manifest_sha256 = Some(hashes.manifest_sha256);

    let origin = mock_server.uri();
    let toml = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\ntoken = {{ file = {:?} }}\n\
         [data]\ndir = {:?}\ntimezone = \"Europe/Paris\"\n\
         [memory]\ngit = false\n\
         [web]\npassword = {{ file = {:?} }}\n\
         [[sources]]\nkind = \"component\"\nname = \"calendar\"\npath = {:?}\nmanifest = {:?}\n\
         sha256 = {:?}\nmanifest_sha256 = {:?}\n\
         allow = [{{ origin = {origin:?}, local = true }}]\n\
         [sources.settings]\nevents_url = \"{origin}/cal/\"\nreminders_url = \"{origin}/tasks/\"\n\
         [[sources.credentials]]\norigin = {origin:?}\n\
         basic = {{ username = \"alix\", password = {{ file = {:?} }} }}\n",
        token_file.display().to_string(),
        dir.path().join("data").display().to_string(),
        web_file.display().to_string(),
        component_cfg.path.display().to_string(),
        component_cfg.manifest_path().display().to_string(),
        component_cfg.sha256.unwrap(),
        component_cfg.manifest_sha256.unwrap(),
        caldav_file.display().to_string(),
    );
    let config: Config = toml::from_str(&toml).unwrap();

    let buffer = Buffer::default();
    let out = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || out.clone())
        .with_ansi(false)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);

    let (addr, server_handle) = fronde::start(config).await.unwrap();
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
            .auth_header(mcp_canary),
    );
    let client = ().serve(transport).await.unwrap();
    let args = json!({"from": "2026-11-16", "to": "2026-11-22"})
        .as_object()
        .unwrap()
        .clone();
    let result = client
        .call_tool(CallToolRequestParams::new("calendar_list").with_arguments(args))
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false), "{result:?}");

    drop(client);
    server_handle.abort();
    drop(guard);

    let requests = mock_server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent_auth = requests[0]
        .headers
        .get("authorization")
        .expect("missing Authorization header on wiremock request")
        .to_str()
        .unwrap();
    assert_eq!(sent_auth, expected_auth);

    let logs = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
    assert!(!logs.is_empty(), "expected TRACE logs to be captured");
    for secret in [
        caldav_canary,
        &expected_basic_b64,
        &expected_auth,
        mcp_canary,
        web_canary,
    ] {
        assert!(
            !logs.contains(secret),
            "captured TRACE logs leaked secret {secret:?}:\n{logs}"
        );
    }
}
