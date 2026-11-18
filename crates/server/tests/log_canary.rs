//! No secret reaches the log, whatever the level, the thread or the logging crate. Its own test
//! binary: it installs the process's global subscriber.

#[path = "support/asks.rs"]
mod asks;

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
use tracing_subscriber::fmt::format::FmtSpan;
use wiremock::matchers::{method, path as url_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CALDAV_CANARY: &str = "canary-caldav-secret-9f8e7d6c";
const MCP_CANARY: &str = "canary-mcp-token-0123456789abcdef0123456789";
const WEB_CANARY: &str = "canary-web-secret-4a5b6c7d";
const SETUP_CANARY: &str = "canary-setup-secret-3b9d1e";
const CONFIG_CANARY: &str = "canary-config-line-5e2f81";
const ACCOUNT_CANARY: &str = "canary-account-token-8c1d4e";
const USER_CANARY: &str = "canary-user-password-2d7f3a";

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
    /// Keeps every line, for a secret only known once the server runs; only for a run without
    /// components, whose log stays small.
    keep: bool,
    kept: Vec<String>,
}

#[derive(Clone, Default)]
struct Scanner(Arc<Mutex<Scan>>);

impl std::io::Write for Scanner {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let line = String::from_utf8_lossy(b);
        let mut scan = self.0.lock().unwrap();
        scan.lines += 1;
        if scan.keep {
            scan.kept.push(line.to_string());
        }
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

/// The process's global subscriber, through Fronde's own log filter at TRACE (the compiler's
/// targets off), with every span event, fed by `log` records too, so that every thread (tokio
/// workers, `spawn_blocking`) and every crate writes to the scanner.
fn capture() -> Scanner {
    use tracing_subscriber::layer::{Layer as _, SubscriberExt as _};
    static SCANNER: OnceLock<Scanner> = OnceLock::new();
    SCANNER
        .get_or_init(|| {
            let scanner = Scanner::default();
            let out = scanner.clone();
            let spec = std::iter::once("trace".to_owned())
                .chain(COMPILER.iter().map(|target| format!("{target}=off")))
                .collect::<Vec<_>>()
                .join(",");
            let subscriber = tracing_subscriber::registry().with(
                tracing_subscriber::fmt::layer()
                    .with_span_events(FmtSpan::FULL)
                    .with_writer(move || out.clone())
                    .with_ansi(false)
                    .with_filter(fronde::log_filter_from(Some(&spec))),
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

/// The page's WebSocket to `/ws`, signed in with `cookie`. Its client, tokio-websockets, logs
/// nothing: a canary in the log came from the server, never from the test's own client.
type Page =
    tokio_websockets::WebSocketStream<tokio_websockets::MaybeTlsStream<tokio::net::TcpStream>>;

async fn open_page(addr: std::net::SocketAddr, cookie: &str) -> Page {
    use axum::http::{HeaderValue, header};
    let (socket, _) = tokio_websockets::ClientBuilder::new()
        .uri(&format!("ws://{addr}/ws"))
        .unwrap()
        .add_header(header::COOKIE, HeaderValue::from_str(cookie).unwrap())
        .unwrap()
        .add_header(
            header::ORIGIN,
            HeaderValue::from_str(&format!("http://{addr}")).unwrap(),
        )
        .unwrap()
        .connect()
        .await
        .unwrap();
    socket
}

/// The page's next text message; `None` once the socket closed.
async fn next_text(page: &mut Page) -> Option<String> {
    use futures::StreamExt as _;
    while let Some(Ok(message)) = page.next().await {
        if let Some(text) = message.as_text() {
            return Some(text.to_owned());
        }
    }
    None
}

async fn send_text(page: &mut Page, text: String) {
    use futures::SinkExt as _;
    page.send(tokio_websockets::Message::text(text))
        .await
        .unwrap();
}

/// The tests share the process's one subscriber: they run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    let _serial = SERIAL.lock().await;
    let result = call_calendar(207).await;
    assert!(!result.is_error.unwrap_or(false), "{result:?}");
    let result = call_calendar(401).await;
    assert!(result.is_error.unwrap_or(false), "{result:?}");
}

/// The assistant's own `/mcp` token, made at each start and kept in memory, is never logged:
/// not at start, not when the agent gets it, not when the assistant calls the hub with it. Nor
/// is the owner's account token, read at the agent's start, nor the owner's password from
/// `[users]`, given to sign in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assistant_token_never_appears() {
    use fronde_agent::acp::{AgentClient, AgentSpec};
    use fronde_agent::assistant::Connector;
    use fronde_agent::fake::{FakeScript, fake_agent, text_chunk};
    use fronde_protocol::ClientMsg;
    use std::time::Duration;
    use tokio::time::timeout;

    const WAIT: Duration = Duration::from_secs(10);
    let _serial = SERIAL.lock().await;
    let scanner = capture();
    *scanner.0.lock().unwrap() = Scan {
        secrets: [MCP_CANARY, USER_CANARY, ACCOUNT_CANARY]
            .map(String::from)
            .to_vec(),
        wanted: vec!["memory_search".to_string(), "user_ask".to_string()],
        keep: true,
        ..Scan::default()
    };

    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("mcp_token");
    let user_file = dir.path().join("user_pass");
    let account_file = dir.path().join("account_token");
    fronde_hub::private::write(&token_file, MCP_CANARY).unwrap();
    fronde_hub::private::write(&user_file, USER_CANARY).unwrap();
    fronde_hub::private::write(&account_file, ACCOUNT_CANARY).unwrap();
    let config: Config = toml::from_str(&format!(
        "[server]\nlisten = \"127.0.0.1:0\"\ntoken = {{ file = {:?} }}\n\
         [data]\ndir = {:?}\ntimezone = \"Europe/Paris\"\n\
         [memory]\ngit = false\n\
         [secrets]\nmaster_key = {{ file = {:?} }}\n\
         [web]\n[users.me]\nadmin = true\npassword = {{ file = {:?} }}\n\
         [accounts.me]\nprovider = \"claude\"\nowner = \"me\"\noauth_token = {{ file = {:?} }}\n",
        token_file.display().to_string(),
        dir.path().join("data").display().to_string(),
        dir.path().join("master.key").display().to_string(),
        user_file.display().to_string(),
        account_file.display().to_string(),
    ))
    .unwrap();
    // The fake agent holds its turn open, so the assistant's calls belong to it.
    let gate = Arc::new(tokio::sync::Notify::new());
    let script = FakeScript {
        updates: vec![text_chunk("Je regarde."), text_chunk(" Voilà.")],
        pause: Some((1, gate.clone())),
        ..FakeScript::default()
    };
    let seen: Arc<Mutex<Option<AgentSpec>>> = Arc::default();
    let account_token: Arc<Mutex<Option<String>>> = Arc::default();
    let connector: Connector = Arc::new({
        let (seen, account_token) = (seen.clone(), account_token.clone());
        move |spec: &AgentSpec, resume, events| {
            *seen.lock().unwrap() = Some(spec.clone());
            let (spec, script) = (spec.clone(), script.clone());
            let account_token = account_token.clone();
            Box::pin(async move {
                // The account's token is read as a real start reads it.
                let read = spec
                    .oauth_token
                    .as_ref()
                    .expect("the owner's account token");
                let token = read().await?;
                *account_token.lock().unwrap() =
                    Some(secrecy::ExposeSecret::expose_secret(&token).to_owned());
                let (transport, _handle) = fake_agent(script);
                AgentClient::connect(transport, &spec, resume, events).await
            })
        }
    });
    let (addr, server) = fronde::start_with(config, connector, &dir.path().join("site"))
        .await
        .unwrap();

    // A message from the chat starts the agent, which gets its token.
    let signed_in = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(format!("http://{addr}/auth/sign-in"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("password={USER_CANARY}"))
        .send()
        .await
        .unwrap();
    let cookie = signed_in.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let mut socket = open_page(addr, &cookie).await;
    for message in [
        ClientMsg::Hello {
            last_seq: 0,
            log_id: None,
            thread: None,
        },
        ClientMsg::Send {
            text: "Salut".into(),
            thread: None,
        },
    ] {
        send_text(&mut socket, serde_json::to_string(&message).unwrap()).await;
    }
    let token = timeout(WAIT, async {
        loop {
            if let Some(spec) = seen.lock().unwrap().clone() {
                return secrecy::ExposeSecret::expose_secret(&spec.hub_token).to_owned();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    // The assistant calls the hub with it: a plain tool, and a question that expires.
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
            .auth_header(token.clone()),
    );
    let client = ().serve(transport).await.unwrap();
    let search = json!({"query": "chat"}).as_object().unwrap().clone();
    let result = client
        .call_tool(CallToolRequestParams::new("memory_search").with_arguments(search))
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false), "{result:?}");
    let ask = json!({"timeout_ms": 1000, "questions": [{
        "question": "Laquelle ?",
        "header": "Appli",
        "options": [
            {"label": "Mealie", "description": "Recettes"},
            {"label": "Seerr", "description": "Films"},
        ],
    }]});
    let result = timeout(
        WAIT,
        client.call_tool(
            CallToolRequestParams::new("user_ask").with_arguments(ask.as_object().unwrap().clone()),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!result.is_error.unwrap_or(false), "{result:?}");
    gate.notify_one();
    // The turn ends: its last entry reaches the page.
    timeout(WAIT, async {
        while let Some(text) = next_text(&mut socket).await {
            if text.contains("turn_ended") {
                return;
            }
        }
    })
    .await
    .unwrap();
    drop(client);
    server.abort();

    assert_eq!(
        account_token.lock().unwrap().as_deref(),
        Some(ACCOUNT_CANARY),
        "the agent's start read the owner's account token"
    );
    let scan = std::mem::take(&mut *scanner.0.lock().unwrap());
    assert!(scan.lines > 0, "nothing was captured");
    assert!(scan.leaked.is_empty(), "{:#?}", scan.leaked);
    for want in &scan.wanted {
        assert!(scan.seen.contains(want), "{want} is not in the log");
    }
    let leaks: Vec<&String> = scan
        .kept
        .iter()
        .filter(|line| line.contains(&token))
        .take(4)
        .collect();
    assert!(
        leaks.is_empty(),
        "the assistant's token is in the log: {leaks:#?}"
    );
}

/// Every thread's agent process gets its own `/mcp` token, and none is ever logged: not when it
/// is made, not when the agent gets it, not when it calls the hub, not when it is revoked and
/// refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_threads_token_never_appears() {
    use fronde_agent::acp::{AgentClient, AgentSpec};
    use fronde_agent::assistant::Connector;
    use fronde_agent::fake::{FakeScript, fake_agent};
    use fronde_agent::threads::{StartedBy, ThreadId, Threads, thread_dirs};
    use secrecy::{ExposeSecret as _, SecretString};
    use std::time::Duration;
    use tokio::time::timeout;

    const WAIT: Duration = Duration::from_secs(10);
    let _serial = SERIAL.lock().await;
    let scanner = capture();
    *scanner.0.lock().unwrap() = Scan {
        secrets: vec![MCP_CANARY.to_owned()],
        wanted: vec!["memory_search".to_string()],
        keep: true,
        ..Scan::default()
    };

    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let config: Config = toml::from_str(&format!(
        "[server]\ntoken = {{ env = \"UNUSED\" }}\n[data]\ndir = {:?}\ntimezone = \"Europe/Paris\"\n\
         [memory]\ngit = false\n[secrets]\nbackend = \"file\"\nmaster_key = {{ file = {:?} }}\n",
        data.display().to_string(),
        dir.path().join("master.key").display().to_string(),
    ))
    .unwrap();
    fronde_hub::private::prepare_data_dir(&data).unwrap();
    let db = fronde_db::open(&data).await.unwrap();
    let starts: Arc<Mutex<Vec<(ThreadId, String)>>> = Arc::default();
    let connector: Connector = Arc::new({
        let starts = starts.clone();
        move |spec: &AgentSpec, resume, events| {
            starts
                .lock()
                .unwrap()
                .push((spec.thread, spec.hub_token.expose_secret().to_owned()));
            let spec = spec.clone();
            Box::pin(async move {
                let (transport, _handle) = fake_agent(FakeScript::default());
                AgentClient::connect(transport, &spec, resume, events).await
            })
        }
    });
    let main = thread_dirs(&data, ThreadId::MAIN);
    let spec = AgentSpec {
        thread: ThreadId::MAIN,
        provider: &fronde_agent::provider::Provider::CLAUDE,
        command: vec!["claude-agent-acp".into()],
        env: Vec::new(),
        cwd: main.work,
        home: main.home,
        hub_url: "http://127.0.0.1:1/mcp".into(),
        hub_token: SecretString::from(String::new()),
        oauth_token: None,
        system_prompt: String::new(),
        builtin_tools: Vec::new(),
        usage: None,
        account: "claude".into(),
    };
    let owner = fronde_requests::User {
        name: "alix".into(),
        admin: true,
    };
    let threads = Threads::open(db.clone(), &data, spec, connector, owner)
        .await
        .unwrap();
    let requests = fronde_requests::Requests::open(db.clone(), threads.clone())
        .await
        .unwrap();
    threads.set_requests(&requests).await;
    let hub = fronde_hub::Hub::start(config, db, vec![], Some(requests), None, None)
        .await
        .unwrap();
    let app = fronde::router(
        Arc::new(hub),
        SecretString::from(MCP_CANARY),
        Some(threads.clone()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // Two threads' agents start, each with its token, and call the hub with it.
    let second = threads
        .create("alix", StartedBy::User, None, "Une autre")
        .await
        .unwrap()
        .id;
    for id in [ThreadId::MAIN, second] {
        let assistant = threads.assistant(id).await.unwrap().unwrap();
        assistant.send("Salut".into()).await;
    }
    let tokens = timeout(WAIT, async {
        loop {
            let tokens = starts.lock().unwrap().clone();
            if tokens.len() == 2 {
                return tokens;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    for (_, token) in &tokens {
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
                .auth_header(token.clone()),
        );
        let client = ().serve(transport).await.unwrap();
        let search = json!({"query": "chat"}).as_object().unwrap().clone();
        let result = client
            .call_tool(CallToolRequestParams::new("memory_search").with_arguments(search))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false), "{result:?}");
    }
    // Thread 2's agent stops: its token is revoked, and refused.
    let assistant = threads.assistant(second).await.unwrap().unwrap();
    timeout(WAIT, async {
        while !assistant.stop_agent().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let (_, revoked) = tokens.iter().find(|(id, _)| *id == second).unwrap();
    let refused = reqwest::Client::new()
        .post(format!("http://{addr}/mcp"))
        .header("authorization", format!("Bearer {revoked}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 401);
    server.abort();

    let scan = std::mem::take(&mut *scanner.0.lock().unwrap());
    assert!(scan.lines > 0, "nothing was captured");
    assert!(scan.leaked.is_empty(), "{:#?}", scan.leaked);
    for want in &scan.wanted {
        assert!(scan.seen.contains(want), "{want} is not in the log");
    }
    for (id, token) in &tokens {
        let leaks: Vec<&String> = scan
            .kept
            .iter()
            .filter(|line| line.contains(token.as_str()))
            .take(4)
            .collect();
        assert!(
            leaks.is_empty(),
            "thread {id}'s token is in the log: {leaks:#?}"
        );
    }
}

/// A secret typed in a card goes from the page to the store and nowhere else: not to the log
/// (at TRACE, every crate), not to the conversation stream or any message the pages get, not to
/// the request's row or any file of the data directory, not to the tool's result. A malformed
/// answer carrying one is not logged either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secret_answer_never_appears() {
    use fronde_agent::acp::{AgentClient, AgentSpec};
    use fronde_agent::assistant::Connector;
    use fronde_agent::fake::{FakeScript, fake_agent, text_chunk};
    use fronde_protocol::{Card, ClientMsg, Event, Keep, Response, SecretValue, ServerMsg};
    use secrecy::ExposeSecret as _;
    use std::time::Duration;
    use tokio::time::timeout;

    const WAIT: Duration = Duration::from_secs(10);
    // The file backend, named: the default would try the machine's real keychain.
    const KEPT: &str = "canary-card-conversation-5e6f7a8b";
    const STORED: &str = "canary-card-stored-1c2d3e4f";
    const MALFORMED: &str = "canary-card-malformed-9a8b7c6d";
    let canaries = [KEPT, STORED, MALFORMED];
    let _serial = SERIAL.lock().await;
    let scanner = capture();
    *scanner.0.lock().unwrap() = Scan {
        secrets: [MCP_CANARY, WEB_CANARY, KEPT, STORED, MALFORMED]
            .map(String::from)
            .to_vec(),
        wanted: vec!["user_secret".to_string()],
        ..Scan::default()
    };

    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let key_file = dir.path().join("master.key");
    let token_file = dir.path().join("mcp_token");
    let web_file = dir.path().join("web_pass");
    fronde_hub::private::write(&token_file, MCP_CANARY).unwrap();
    fronde_hub::private::write(&web_file, WEB_CANARY).unwrap();
    let config: Config = toml::from_str(&format!(
        "[server]\nlisten = \"127.0.0.1:0\"\ntoken = {{ file = {:?} }}\n\
         [data]\ndir = {:?}\ntimezone = \"Europe/Paris\"\n\
         [memory]\ngit = false\n\
         [secrets]\nbackend = \"file\"\nmaster_key = {{ file = {:?} }}\n\
         [web]\npassword = {{ file = {:?} }}\n",
        token_file.display().to_string(),
        data.display().to_string(),
        key_file.display().to_string(),
        web_file.display().to_string(),
    ))
    .unwrap();
    // The fake agent holds its turn open, so the assistant's calls belong to it.
    let gate = Arc::new(tokio::sync::Notify::new());
    let script = FakeScript {
        updates: vec![text_chunk("Je demande."), text_chunk(" Merci.")],
        pause: Some((1, gate.clone())),
        ..FakeScript::default()
    };
    let seen: Arc<Mutex<Option<AgentSpec>>> = Arc::default();
    let connector: Connector = Arc::new({
        let seen = seen.clone();
        move |spec: &AgentSpec, resume, events| {
            *seen.lock().unwrap() = Some(spec.clone());
            let (spec, script) = (spec.clone(), script.clone());
            Box::pin(async move {
                let (transport, _handle) = fake_agent(script);
                AgentClient::connect(transport, &spec, resume, events).await
            })
        }
    });
    let (addr, server) = fronde::start_with(config, connector, &dir.path().join("site"))
        .await
        .unwrap();

    let signed_in = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(format!("http://{addr}/auth/sign-in"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("password={WEB_CANARY}"))
        .send()
        .await
        .unwrap();
    let cookie = signed_in.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let mut socket = open_page(addr, &cookie).await;
    for message in [
        ClientMsg::Hello {
            last_seq: 0,
            log_id: None,
            thread: None,
        },
        ClientMsg::Send {
            text: "Salut".into(),
            thread: None,
        },
    ] {
        send_text(&mut socket, serde_json::to_string(&message).unwrap()).await;
    }
    let token = timeout(WAIT, async {
        loop {
            if let Some(spec) = seen.lock().unwrap().clone() {
                return spec.hub_token.expose_secret().to_owned();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
            .auth_header(token),
    );
    let client = ().serve(transport).await.unwrap();

    // Everything the page is sent, kept to be searched.
    let mut received: Vec<String> = Vec::new();
    let mut hello_admin = None;
    for (args, value, keep) in [
        (
            json!({"label": "Jeton", "reason": "Pour un essai."}),
            KEPT,
            Keep::Conversation,
        ),
        (
            json!({"label": "Mot de passe", "reason": "Pour l'agenda.", "remember": {"name": "nextcloud", "origin": "https://cloud.example"}}),
            STORED,
            Keep::Stored,
        ),
    ] {
        let peer = client.peer().clone();
        let call = tokio::spawn(async move {
            peer.call_tool(
                CallToolRequestParams::new("user_secret")
                    .with_arguments(args.as_object().unwrap().clone()),
            )
            .await
        });
        let id = timeout(WAIT, async {
            while let Some(text) = next_text(&mut socket).await {
                received.push(text.clone());
                match serde_json::from_str::<ServerMsg>(&text).unwrap() {
                    ServerMsg::Hello { admin, .. } => hello_admin = Some(admin),
                    ServerMsg::Entry(entry) => {
                        if let Event::Request {
                            id,
                            card: Card::Secret { .. },
                            ..
                        } = entry.event
                        {
                            return id;
                        }
                    }
                    ServerMsg::Refused { .. }
                    | ServerMsg::Threads { .. }
                    | ServerMsg::Created { .. }
                    | ServerMsg::ThreadRefused { .. }
                    | ServerMsg::Usage { .. } => {}
                }
            }
            panic!("the socket closed");
        })
        .await
        .unwrap();
        // A malformed answer, with a secret in it: ignored, and not logged.
        let malformed = format!(
            r#"{{"type":"answer","id":{id},"response":{{"type":"secret","value":"{MALFORMED}","keep":"forever"}}}}"#
        );
        send_text(&mut socket, malformed).await;
        let answer = ClientMsg::Answer {
            id,
            response: Response::Secret {
                value: SecretValue::new(value.into()),
                keep,
            },
        };
        send_text(&mut socket, serde_json::to_string(&answer).unwrap()).await;
        let result = timeout(WAIT, call).await.unwrap().unwrap().unwrap();
        assert!(!result.is_error.unwrap_or(false), "{result:?}");
        let text = serde_json::to_string(&result).unwrap();
        for canary in canaries {
            assert!(
                !text.contains(canary),
                "the tool result holds {canary}: {text}"
            );
        }
        assert!(text.contains("saved"), "{text}");
        let expected = match keep {
            Keep::Conversation => "sr_",
            Keep::Stored => "nextcloud",
        };
        assert!(text.contains(expected), "{text}");
    }
    assert_eq!(hello_admin, Some(true), "the owner may keep secrets");
    gate.notify_one();
    timeout(WAIT, async {
        while let Some(text) = next_text(&mut socket).await {
            let ended = text.contains("turn_ended");
            received.push(text);
            if ended {
                return;
            }
        }
    })
    .await
    .unwrap();
    drop(client);
    drop(socket);
    server.abort();

    for message in &received {
        for canary in canaries {
            assert!(!message.contains(canary), "a page got {canary}: {message}");
        }
    }
    assert!(
        received.iter().any(|m| m.contains(r#""kept":"stored""#)),
        "{received:#?}"
    );
    let scan = std::mem::take(&mut *scanner.0.lock().unwrap());
    assert!(scan.lines > 0, "nothing was captured");
    assert!(scan.leaked.is_empty(), "{:#?}", scan.leaked);
    for want in &scan.wanted {
        assert!(scan.seen.contains(want), "{want} is not in the log");
    }

    // The data directory's files, the database's WAL and shared memory among them.
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&data).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            files.push(path);
        }
    }
    for suffix in ["", "-wal", "-shm"] {
        let name = format!("{}{suffix}", fronde_db::FILE_NAME);
        assert!(
            files
                .iter()
                .any(|f| f.file_name().unwrap() == name.as_str()),
            "{name} is missing: {files:?}"
        );
    }
    for file in &files {
        let bytes = std::fs::read(file).unwrap();
        for canary in canaries {
            assert!(
                !bytes
                    .windows(canary.len())
                    .any(|window| window == canary.as_bytes()),
                "{} holds {canary}",
                file.display()
            );
        }
    }
    // Rows and entries say where it went, never what it was.
    let db = fronde_db::open(&data).await.unwrap();
    let rows: Vec<String> = <fronde_entity::requests::Entity as sea_orm::EntityTrait>::find()
        .all(&db)
        .await
        .unwrap()
        .into_iter()
        .map(|row| format!("{:?} {:?}", row.card, row.response))
        .collect();
    assert_eq!(rows.len(), 2);
    for row in &rows {
        for canary in canaries {
            assert!(!row.contains(canary), "{row}");
        }
    }
    // The kept one is sealed, for its origin, and opens with the key made for it.
    let sealed = fronde_db::secrets::load_sealed(&db, fronde_db::secrets::SOURCES)
        .await
        .unwrap();
    assert_eq!(sealed.len(), 1);
    assert_eq!(
        (sealed[0].name.as_str(), sealed[0].origin.as_str()),
        ("nextcloud", "https://cloud.example")
    );
    let key = fronde_db::master_key::Backends::file(&key_file)
        .load(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sealed[0].open(&key).unwrap().expose_secret(), STORED);
}

/// Hands each card shown to the test.
struct Cards(tokio::sync::mpsc::UnboundedSender<fronde_protocol::RequestId>);

#[async_trait::async_trait]
impl fronde_requests::Journal for Cards {
    async fn announce(
        &self,
        _topic: &str,
        id: fronde_protocol::RequestId,
        _card: &fronde_protocol::Card,
        _caller: &fronde_requests::Caller,
    ) -> anyhow::Result<u64> {
        let _ = self.0.send(id);
        Ok(id)
    }

    async fn close(
        &self,
        _topic: &str,
        _id: fronde_protocol::RequestId,
        _outcome: &fronde_protocol::Outcome,
        _by: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

/// A password typed on an integration's setup card never reaches the log: not as the card is
/// answered, nor as the admin confirms another origin, the secret is sealed, the entry checked
/// with it, written to `fronde.toml` and started.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_secret_never_appears() {
    use fronde_protocol::{FieldValue, Response, SecretAnswer, SecretValue};
    use std::time::Duration;
    use tokio::time::timeout;

    const WAIT: Duration = Duration::from_secs(30);
    const URL: &str = "https://dav.example.org/calendars/alix/";
    let _serial = SERIAL.lock().await;
    let scanner = capture();
    let basic = STANDARD.encode(format!("alix:{SETUP_CANARY}"));
    *scanner.0.lock().unwrap() = Scan {
        secrets: [SETUP_CANARY, &basic].map(String::from).to_vec(),
        // The capture sees the setup through to the entry written.
        wanted: vec!["calendar: added to".to_string()],
        ..Scan::default()
    };

    let (_dir, path, hub, requests, mut cards) = setup_bench().await;
    let context = setup_context();
    let call = tokio::spawn({
        let hub = hub.clone();
        async move {
            hub.registry
                .call_as(
                    &context,
                    "integrations_request_setup",
                    json!({
                        "integration": "calendar",
                        "prefill": {"events_url": "https://cloud.example/dav/", "caldav.username": "alix"},
                    }),
                )
                .await
        }
    });
    let admin = fronde_requests::User {
        name: "alix".into(),
        admin: true,
    };
    // Another server than the card showed: the admin confirms it on a second card.
    let card = timeout(WAIT, cards.recv()).await.unwrap().unwrap();
    let answer = Response::Form {
        values: [
            ("events_url".to_owned(), FieldValue::Text(URL.into())),
            ("reminders_url".to_owned(), FieldValue::Text(URL.into())),
            (
                "caldav.username".to_owned(),
                FieldValue::Text("alix".into()),
            ),
            (
                "caldav.secret".to_owned(),
                FieldValue::Secret(SecretAnswer {
                    secret: SecretValue::new(SETUP_CANARY.into()),
                }),
            ),
            ("allow.local".to_owned(), FieldValue::Toggle(false)),
        ]
        .into(),
    };
    requests.answer(card, answer, &admin).await.unwrap();
    let confirm = timeout(WAIT, cards.recv()).await.unwrap().unwrap();
    requests
        .answer(
            confirm,
            Response::Form {
                values: Default::default(),
            },
            &admin,
        )
        .await
        .unwrap();
    let out = timeout(WAIT, call).await.unwrap().unwrap();
    assert!(!out.is_error, "{}", out.to_text());
    assert!(!out.to_text().contains(SETUP_CANARY));
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains(r#"stored = "calendar_caldav""#)
    );

    let scan = std::mem::take(&mut *scanner.0.lock().unwrap());
    assert!(scan.lines > 0, "nothing was captured");
    assert!(scan.leaked.is_empty(), "{:#?}", scan.leaked);
    for want in &scan.wanted {
        assert!(scan.seen.contains(want), "{want} is not in the log");
    }
}

/// A Fronde whose chat can set up integrations, with its `fronde.toml` and the cards shown.
async fn setup_bench() -> (
    tempfile::TempDir,
    PathBuf,
    Arc<fronde_hub::Hub>,
    Arc<fronde_requests::Requests>,
    tokio::sync::mpsc::UnboundedReceiver<fronde_protocol::RequestId>,
) {
    let dir = tempfile::tempdir().unwrap();
    let etc = dir.path().join("etc");
    std::fs::create_dir(&etc).unwrap();
    let path = etc.join("fronde.toml");
    std::fs::write(
        &path,
        "[server]\ntoken = { env = \"UNUSED\" }\n[data]\ndir = \"../data\"\ntimezone = \"Europe/Paris\"\nuser = \"alix\"\n[memory]\ngit = false\n[secrets]\nbackend = \"file\"\nmaster_key = { file = \"../master.key\" }\n",
    )
    .unwrap();
    let config = fronde_hub::config::load(&path).unwrap();
    fronde_hub::private::prepare_data_dir(&config.data.dir).unwrap();
    let db = fronde_db::open(&config.data.dir).await.unwrap();
    let (send, cards) = tokio::sync::mpsc::unbounded_channel();
    let store = fronde_requests::SecretStore::new(
        fronde_db::master_key::Backends::file(dir.path().join("master.key")),
        Arc::new(fronde_db::ephemeral::EphemeralSecrets::new()),
    );
    let requests =
        fronde_requests::Requests::open_with(db.clone(), Arc::new(Cards(send)), Some(store))
            .await
            .unwrap();
    let integrations = Arc::new(fronde::integrations::Shipped::new(&config, db.clone()).unwrap());
    let hub = Arc::new(
        fronde_hub::Hub::start(
            config,
            db,
            vec![],
            Some(requests.clone()),
            Some(Arc::new(asks::Asks(requests.clone()))),
            Some(integrations),
        )
        .await
        .unwrap(),
    );
    (dir, path, hub, requests, cards)
}

/// The main thread's agent, during its turn.
fn setup_context() -> fronde_hub::tools::CallContext {
    fronde_hub::tools::CallContext {
        caller: fronde_requests::Caller::Thread(fronde_protocol::ThreadId::MAIN),
        user: "alix".into(),
        admin: true,
        topic: "conversation".into(),
        turn: Some(3),
        cancelled: tokio_util::sync::CancellationToken::new(),
    }
}

/// A line of `fronde.toml` that no longer parses, holding a secret written in plain, never
/// reaches the agent nor the log when a setup finds the file broken: only its path and line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broken_config_line_never_appears() {
    use std::time::Duration;
    use tokio::time::timeout;

    const WAIT: Duration = Duration::from_secs(30);
    let _serial = SERIAL.lock().await;
    let scanner = capture();
    *scanner.0.lock().unwrap() = Scan {
        secrets: vec![CONFIG_CANARY.to_owned()],
        wanted: vec!["no longer loads".to_string()],
        ..Scan::default()
    };
    let (_dir, path, hub, requests, mut cards) = setup_bench().await;
    let context = setup_context();
    let call = tokio::spawn({
        let hub = hub.clone();
        async move {
            hub.registry
                .call_as(
                    &context,
                    "integrations_request_setup",
                    json!({"integration": "echo"}),
                )
                .await
        }
    });
    let card = timeout(WAIT, cards.recv()).await.unwrap().unwrap();
    // The owner pastes a token without its quotes meanwhile: the file no longer parses.
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str(&format!("\n[agent]\noauth_token = {CONFIG_CANARY}\n"));
    std::fs::write(&path, &text).unwrap();
    let admin = fronde_requests::User {
        name: "alix".into(),
        admin: true,
    };
    requests
        .answer(
            card,
            fronde_protocol::Response::Form {
                values: Default::default(),
            },
            &admin,
        )
        .await
        .unwrap();
    let out = timeout(WAIT, call).await.unwrap().unwrap();
    assert!(out.is_error, "{}", out.to_text());
    assert!(out.to_text().contains("fronde check"), "{}", out.to_text());
    assert!(!out.to_text().contains(CONFIG_CANARY), "{}", out.to_text());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);

    let scan = std::mem::take(&mut *scanner.0.lock().unwrap());
    assert!(scan.lines > 0, "nothing was captured");
    assert!(scan.leaked.is_empty(), "{:#?}", scan.leaked);
    for want in &scan.wanted {
        assert!(scan.seen.contains(want), "{want} is not in the log");
    }
}
