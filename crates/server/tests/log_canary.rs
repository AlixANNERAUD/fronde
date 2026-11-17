//! No secret reaches the log, whatever the level, the thread or the logging crate. Its own test
//! binary: it installs the process's global subscriber.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use fronde_hub::config::{ComponentConfig, Config};
use fronde_wasm::Hashes;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::fmt::format::FmtSpan;
use wiremock::matchers::{method, path as url_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CALDAV_CANARY: &str = "canary-caldav-secret-9f8e7d6c";
const MCP_CANARY: &str = "canary-mcp-token-0123456789abcdef0123456789";
const WEB_CANARY: &str = "canary-web-secret-4a5b6c7d";

fn dist(file: &str) -> PathBuf {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../components/dist")
        .join(file);
    assert!(
        p.exists(),
        "{} is missing: run `mise run components`",
        p.display()
    );
    p
}

/// What the capture looks for, line by line. Lines are scanned as they are written and dropped,
/// so memory stays flat whatever the volume.
#[derive(Default)]
struct Scan {
    secrets: Vec<String>,
    wanted: Vec<String>,
    leaked: Vec<(String, String)>,
    seen: Vec<String>,
    lines: usize,
}

#[derive(Clone, Default)]
struct Scanner(Arc<Mutex<Scan>>);

impl std::io::Write for Scanner {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let line = String::from_utf8_lossy(b);
        let mut scan = self.0.lock().unwrap();
        scan.lines += 1;
        let Scan {
            secrets,
            wanted,
            leaked,
            seen,
            ..
        } = &mut *scan;
        for secret in secrets.iter() {
            if line.contains(secret.as_str()) && leaked.len() < 8 {
                leaked.push((secret.clone(), line.chars().take(2000).collect()));
            }
        }
        for want in wanted.iter() {
            if line.contains(want.as_str()) && !seen.contains(want) {
                seen.push(want.clone());
            }
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Targets that only compile WebAssembly: they never see a secret, and at TRACE they write
/// gigabytes. Everything else (rmcp, hyper, reqwest, wasmtime-wasi-http, Fronde) stays at TRACE.
const COMPILER: [&str; 6] = [
    "cranelift",
    "regalloc2",
    "wasmtime_cranelift",
    "wasmtime_internal_cranelift",
    "wasmtime::compile",
    "wasmtime_internal_unwinder",
];

/// The process's global subscriber, at TRACE with every span event, fed by `log` records too, so
/// that every thread (tokio workers, `spawn_blocking`) and every crate writes to the scanner.
fn capture() -> Scanner {
    use tracing_subscriber::layer::SubscriberExt;
    static SCANNER: OnceLock<Scanner> = OnceLock::new();
    SCANNER
        .get_or_init(|| {
            let scanner = Scanner::default();
            let out = scanner.clone();
            let targets = COMPILER.iter().fold(
                Targets::new().with_default(LevelFilter::TRACE),
                |t, target| t.with_target(*target, LevelFilter::OFF),
            );
            let subscriber = tracing_subscriber::registry().with(targets).with(
                tracing_subscriber::fmt::layer()
                    .with_span_events(FmtSpan::FULL)
                    .with_writer(move || out.clone())
                    .with_ansi(false),
            );
            tracing::subscriber::set_global_default(subscriber).unwrap();
            tracing_log::LogTracer::builder()
                .with_max_level(tracing_log::log::LevelFilter::Trace)
                .ignore_all(["cranelift", "regalloc2", "wasmtime_cranelift"])
                .init()
                .unwrap();
            scanner
        })
        .clone()
}

/// Starts the server with the calendar component, whose origin answers `status`, calls
/// `calendar_list` and checks the log saw the call and no secret.
async fn call_calendar(status: u16) -> CallToolResult {
    let scanner = capture();
    let mock_server = MockServer::start().await;
    let response = if status == 207 {
        ResponseTemplate::new(207).set_body_string(
            r#"<multistatus xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"></multistatus>"#,
        )
    } else {
        ResponseTemplate::new(status).insert_header("WWW-Authenticate", "Basic realm=\"dav\"")
    };
    Mock::given(method("REPORT"))
        .and(url_path("/cal/"))
        .respond_with(response)
        .expect(1)
        .mount(&mock_server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("mcp_token");
    let web_file = dir.path().join("web_pass");
    let caldav_file = dir.path().join("caldav_pass");
    fronde_hub::private::write(&token_file, MCP_CANARY).unwrap();
    fronde_hub::private::write(&web_file, WEB_CANARY).unwrap();
    fronde_hub::private::write(&caldav_file, format!("{CALDAV_CANARY}\n")).unwrap();

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
    let basic = STANDARD.encode(format!("alix:{CALDAV_CANARY}"));
    *scanner.0.lock().unwrap() = Scan {
        secrets: [CALDAV_CANARY, &basic, MCP_CANARY, WEB_CANARY]
            .map(String::from)
            .to_vec(),
        // A leak on the request path would be seen: the capture sees the call and the origin.
        wanted: vec![
            "calendar_list".to_string(),
            origin.trim_start_matches("http://").to_string(),
        ],
        ..Scan::default()
    };
    let toml = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\ntoken = {{ file = {:?} }}\n\
         [data]\ndir = {:?}\ntimezone = \"Europe/Paris\"\n\
         [memory]\ngit = false\n\
         [secrets]\nmaster_key = {{ file = {:?} }}\n\
         [web]\npassword = {{ file = {:?} }}\n\
         [[sources]]\nkind = \"component\"\nname = \"calendar\"\npath = {:?}\nmanifest = {:?}\n\
         sha256 = {:?}\nmanifest_sha256 = {:?}\n\
         allow = [{{ origin = {origin:?}, local = true }}]\n\
         [sources.settings]\nevents_url = \"{origin}/cal/\"\nreminders_url = \"{origin}/tasks/\"\n\
         [[sources.credentials]]\norigin = {origin:?}\n\
         basic = {{ username = \"alix\", password = {{ file = {:?} }} }}\n",
        token_file.display().to_string(),
        dir.path().join("data").display().to_string(),
        dir.path().join("master.key").display().to_string(),
        web_file.display().to_string(),
        component_cfg.path.display().to_string(),
        component_cfg.manifest_path().display().to_string(),
        component_cfg.sha256.unwrap(),
        component_cfg.manifest_sha256.unwrap(),
        caldav_file.display().to_string(),
    );
    let config: Config = toml::from_str(&toml).unwrap();

    let (addr, server_handle) = fronde::start(config).await.unwrap();
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
            .auth_header(MCP_CANARY),
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
    drop(client);
    server_handle.abort();

    // The host added the credential: the canary did reach the origin.
    let requests = mock_server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent_auth = requests[0]
        .headers
        .get("authorization")
        .expect("missing Authorization header on wiremock request")
        .to_str()
        .unwrap();
    assert_eq!(sent_auth, expected_auth());

    let scan = std::mem::take(&mut *scanner.0.lock().unwrap());
    assert!(scan.lines > 0, "nothing was captured");
    assert!(
        scan.leaked.is_empty(),
        "captured TRACE logs leaked secrets: {:#?}",
        scan.leaked
    );
    for want in &scan.wanted {
        assert!(
            scan.seen.contains(want),
            "{want} is not in the {} captured lines",
            scan.lines
        );
    }
    result
}

fn expected_auth() -> String {
    format!("Basic {}", STANDARD.encode(format!("alix:{CALDAV_CANARY}")))
}

/// The two cases run one after the other, under one global subscriber: the call that succeeds,
/// then one whose origin answers 401, for the error paths.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log_canary_never_appears() {
    let result = call_calendar(207).await;
    assert!(!result.is_error.unwrap_or(false), "{result:?}");
    let result = call_calendar(401).await;
    assert!(result.is_error.unwrap_or(false), "{result:?}");
}
