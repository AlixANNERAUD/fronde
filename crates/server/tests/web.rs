use futures::{SinkExt, StreamExt};
use jarvis_agent::acp::{AgentClient, AgentSpec};
use jarvis_agent::fake::{FakeScript, fake_agent, text_chunk};
use jarvis_agent::jarvis::Connector;
use jarvis_protocol::{ClientMsg, Entry, Event, ServerMsg, TurnOutcome};
use reqwest::header::{
    ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_TYPE, COOKIE, LOCATION, SET_COOKIE,
};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "correct horse battery staple";
const WAIT: Duration = Duration::from_secs(10);
const INDEX: &str =
    "<!doctype html><title>Jarvis</title><script src=\"/assets/app-1a2b.js\"></script>";
const CSS: &str = "body{color:red}";
/// A well-formed session id the server never issued.
const FORGED: &str = "jarvis_session=AAAAAAAAAAAAAAAAAAAAAA";

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn config(dir: &Path, web: bool) -> jarvis_hub::config::Config {
    std::fs::write(dir.join("token"), TOKEN).unwrap();
    std::fs::write(dir.join("password"), format!("{PASSWORD}\n")).unwrap();
    let mut toml = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\ntoken = {{ file = {:?} }}\n[data]\ndir = {:?}\ntimezone = \"Europe/Paris\"\nuser = \"Alix\"\n[memory]\ngit = false\n[agent]\nenv = {{ JARVIS_TEST = \"1\" }}\n",
        dir.join("token").display().to_string(),
        dir.join("data").display().to_string()
    );
    if web {
        toml.push_str(&format!(
            "[web]\npassword = {{ file = {:?} }}\n",
            dir.join("password").display().to_string()
        ));
    }
    toml::from_str(&toml).unwrap()
}

/// A connector starting a fake agent that answers each prompt with two text chunks; it keeps
/// the last spec it was given.
fn fake_connector() -> (Connector, Arc<Mutex<Option<AgentSpec>>>) {
    fake_connector_with(FakeScript {
        updates: vec![text_chunk("Bonjour "), text_chunk("Alix !")],
        ..FakeScript::default()
    })
}

/// A connector starting a fake agent that plays `script`; it keeps the last spec it was given.
fn fake_connector_with(script: FakeScript) -> (Connector, Arc<Mutex<Option<AgentSpec>>>) {
    let seen = Arc::new(Mutex::new(None));
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
    (connector, seen)
}

/// A built interface as dx writes it: the page, hashed assets with their `.br` copies, and a
/// file at the root.
fn site(dir: &Path) -> PathBuf {
    let site = dir.join("site");
    std::fs::create_dir_all(site.join("assets")).unwrap();
    std::fs::write(site.join("index.html"), INDEX).unwrap();
    std::fs::write(site.join("assets/app-1a2b.css"), CSS).unwrap();
    std::fs::write(site.join("assets/app-1a2b.css.br"), "brotli bytes").unwrap();
    std::fs::write(site.join("favicon.ico"), "icon").unwrap();
    site
}

async fn start(dir: &Path) -> (SocketAddr, Arc<Mutex<Option<AgentSpec>>>) {
    let (connector, seen) = fake_connector();
    (
        start_with(config(dir, true), connector, &site(dir)).await,
        seen,
    )
}

async fn start_with(
    config: jarvis_hub::config::Config,
    connector: Connector,
    site_root: &Path,
) -> SocketAddr {
    let (addr, _server) = timeout(WAIT, jarvis::start_with(config, connector, site_root))
        .await
        .unwrap()
        .unwrap();
    addr
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

async fn post_password(addr: SocketAddr, password: &str) -> reqwest::Response {
    let body = format!("password={}", password.replace(' ', "+"));
    timeout(
        WAIT,
        client()
            .post(format!("http://{addr}/sign-in"))
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .send(),
    )
    .await
    .unwrap()
    .unwrap()
}

/// Signs in and returns the session cookie (`name=value`).
async fn sign_in(addr: SocketAddr) -> String {
    let response = post_password(addr, PASSWORD).await;
    let cookie = response.headers()[SET_COOKIE].to_str().unwrap();
    cookie.split(';').next().unwrap().to_owned()
}

async fn get(addr: SocketAddr, path: &str, cookie: Option<&str>) -> reqwest::Response {
    let mut request = client().get(format!("http://{addr}{path}"));
    if let Some(cookie) = cookie {
        request = request.header(COOKIE, cookie);
    }
    timeout(WAIT, request.send()).await.unwrap().unwrap()
}

async fn connect(
    addr: SocketAddr,
    cookie: Option<&str>,
    origin: Option<&str>,
) -> Result<Socket, tungstenite::Error> {
    let mut request = format!("ws://{addr}/ws").into_client_request().unwrap();
    if let Some(cookie) = cookie {
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
    }
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert("origin", origin.parse().unwrap());
    }
    timeout(WAIT, tokio_tungstenite::connect_async(request))
        .await
        .unwrap()
        .map(|(socket, _)| socket)
}

async fn open_chat(addr: SocketAddr, cookie: &str) -> Socket {
    connect(addr, Some(cookie), Some(&format!("http://{addr}")))
        .await
        .unwrap()
}

async fn say(socket: &mut Socket, message: ClientMsg) {
    let json = serde_json::to_string(&message).unwrap();
    timeout(WAIT, socket.send(Message::text(json)))
        .await
        .unwrap()
        .unwrap();
}

async fn next_message(socket: &mut Socket) -> ServerMsg {
    loop {
        let message = timeout(WAIT, socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Message::Text(text) = message {
            return serde_json::from_str(&text).unwrap();
        }
    }
}

/// The next entry, past the server's hello.
async fn next_entry(socket: &mut Socket) -> Entry {
    loop {
        if let ServerMsg::Entry(entry) = next_message(socket).await {
            return entry;
        }
    }
}

/// Says hello and returns the server's answer, which comes before any entry.
async fn hello(socket: &mut Socket, last_seq: u64, log_id: Option<&str>) -> (String, u64) {
    let log_id = log_id.map(str::to_owned);
    say(socket, ClientMsg::Hello { last_seq, log_id }).await;
    match next_message(socket).await {
        ServerMsg::Hello { log_id, last_seq } => (log_id, last_seq),
        other => panic!("expected the server's hello, got {other:?}"),
    }
}

/// Entries up to and including the next `turn_ended`.
async fn until_turn_ended(socket: &mut Socket) -> Vec<Entry> {
    let mut entries = Vec::new();
    loop {
        let entry = next_entry(socket).await;
        let ended = matches!(entry.event, Event::TurnEnded { .. });
        entries.push(entry);
        if ended {
            return entries;
        }
    }
}

/// The code of the close frame the server sends next.
async fn close_code(socket: &mut Socket) -> u16 {
    loop {
        match timeout(WAIT, socket.next()).await.unwrap() {
            Some(Ok(Message::Close(Some(frame)))) => return frame.code.into(),
            Some(Ok(Message::Close(None))) | None => panic!("closed without a code"),
            Some(Ok(_)) => {}
            Some(Err(error)) => panic!("socket error: {error}"),
        }
    }
}

fn status_of(error: tungstenite::Error) -> u16 {
    match error {
        tungstenite::Error::Http(response) => response.status().as_u16(),
        other => panic!("expected an HTTP refusal, got {other}"),
    }
}

#[tokio::test]
async fn sign_in_sets_session_and_wrong_password_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _) = start(dir.path()).await;

    let home = get(addr, "/", None).await;
    assert_eq!(home.status(), 303);
    assert_eq!(home.headers()[LOCATION], "/sign-in");
    assert_eq!(get(addr, "/sign-in", None).await.status(), 200);

    let wrong = post_password(addr, "nope").await;
    assert_eq!(wrong.status(), 303);
    assert_eq!(wrong.headers()[LOCATION], "/sign-in?error=1");
    assert!(wrong.headers().get(SET_COOKIE).is_none());

    let right = post_password(addr, PASSWORD).await;
    assert_eq!(right.status(), 303);
    assert_eq!(right.headers()[LOCATION], "/");
    let set_cookie = right.headers()[SET_COOKIE].to_str().unwrap().to_owned();
    assert!(set_cookie.starts_with("jarvis_session="), "{set_cookie}");
    assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
    assert!(set_cookie.contains("SameSite=Lax"), "{set_cookie}");
    assert!(!set_cookie.contains("Secure"), "{set_cookie}");
    assert!(
        set_cookie.contains(&format!("Max-Age={}", 30 * 24 * 3600)),
        "{set_cookie}"
    );
    let cookie = set_cookie.split(';').next().unwrap();
    assert_eq!(get(addr, "/", Some(cookie)).await.status(), 200);

    let out = timeout(
        WAIT,
        client()
            .post(format!("http://{addr}/sign-out"))
            .header(COOKIE, cookie)
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(out.status(), 303);
    assert_eq!(out.headers()[LOCATION], "/sign-in");
    assert_eq!(get(addr, "/", Some(cookie)).await.status(), 303);
}

#[tokio::test]
async fn session_is_renewed_by_use() {
    // Sessions expire after 30 days without a request, not 30 days after sign-in: each
    // signed-in request sends the cookie again with a fresh lifetime.
    let dir = tempfile::tempdir().unwrap();
    let (addr, _) = start(dir.path()).await;
    let cookie = sign_in(addr).await;

    let page = get(addr, "/", Some(&cookie)).await;

    assert_eq!(page.status(), 200);
    let renewed = page
        .headers()
        .get(SET_COOKIE)
        .expect("the session cookie is not renewed")
        .to_str()
        .unwrap();
    assert!(renewed.starts_with(&format!("{cookie};")), "{renewed}");
    assert!(
        renewed.contains(&format!("Max-Age={}", 30 * 24 * 3600)),
        "{renewed}"
    );

    // The chat page stays open on its socket: opening it renews the session too.
    let mut request = format!("ws://{addr}/ws").into_client_request().unwrap();
    let headers = request.headers_mut();
    headers.insert("cookie", cookie.parse().unwrap());
    headers.insert("origin", format!("http://{addr}").parse().unwrap());
    let (_socket, response) = timeout(WAIT, tokio_tungstenite::connect_async(request))
        .await
        .unwrap()
        .unwrap();
    let renewed = response
        .headers()
        .get(SET_COOKIE)
        .expect("the socket does not renew the session")
        .to_str()
        .unwrap();
    assert!(renewed.starts_with(&format!("{cookie};")), "{renewed}");
}

#[tokio::test]
async fn ws_requires_session_and_same_origin() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _) = start(dir.path()).await;
    let origin = format!("http://{addr}");

    // Without a session the socket opens and closes at once with 4401, which a page can read
    // (a refused upgrade only shows as 1006) to go and sign in.
    for cookie in [None, Some("jarvis_session=forged")] {
        let mut socket = connect(addr, cookie, Some(&origin)).await.unwrap();
        assert_eq!(close_code(&mut socket).await, 4401);
    }

    let cookie = sign_in(addr).await;
    let cross_site = connect(addr, Some(&cookie), Some("https://evil.example"))
        .await
        .unwrap_err();
    assert_eq!(status_of(cross_site), 403);
    let no_origin = connect(addr, Some(&cookie), None).await.unwrap_err();
    assert_eq!(status_of(no_origin), 403);

    connect(addr, Some(&cookie), Some(&origin)).await.unwrap();
}

#[tokio::test]
async fn ws_send_runs_a_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, seen) = start(dir.path()).await;
    let cookie = sign_in(addr).await;
    let mut socket = open_chat(addr, &cookie).await;

    // A message that is not JSON is ignored and the connection stays open.
    timeout(WAIT, socket.send(Message::text("not json")))
        .await
        .unwrap()
        .unwrap();
    say(
        &mut socket,
        ClientMsg::Hello {
            last_seq: 0,
            log_id: None,
        },
    )
    .await;
    say(
        &mut socket,
        ClientMsg::Send {
            text: "Salut".into(),
        },
    )
    .await;
    let events: Vec<Event> = until_turn_ended(&mut socket)
        .await
        .into_iter()
        .map(|entry| entry.event)
        .collect();
    assert_eq!(
        events,
        [
            Event::User {
                text: "Salut".into()
            },
            Event::Assistant {
                text: "Bonjour ".into()
            },
            Event::Assistant {
                text: "Alix !".into()
            },
            Event::TurnEnded {
                outcome: TurnOutcome::Done
            },
        ]
    );

    say(
        &mut socket,
        ClientMsg::Answer {
            id: 99,
            allow: true,
        },
    )
    .await;
    assert_eq!(
        next_entry(&mut socket).await.event,
        Event::Notice {
            text: "Cette demande n'est plus active.".into()
        }
    );

    let spec = seen.lock().unwrap().clone().unwrap();
    assert_eq!(spec.command, ["claude-agent-acp"]);
    assert_eq!(spec.env, [("JARVIS_TEST".to_owned(), "1".to_owned())]);
    assert_eq!(
        spec.hub_url,
        format!("http://127.0.0.1:{}/mcp", addr.port())
    );
    assert_eq!(spec.hub_token, TOKEN);
    assert_eq!(spec.cwd, dir.path().join("data").join("agent"));
    assert!(spec.cwd.is_dir());
    assert_eq!(
        spec.system_prompt,
        "You are Jarvis, Alix's personal assistant. Answer in the user's language, briefly and warmly. Use your jarvis tools for memory, calendar and reminders; never invent data they did not return."
    );
}

#[tokio::test]
async fn ws_hello_replays_then_streams() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _) = start(dir.path()).await;
    let cookie = sign_in(addr).await;

    let mut first = open_chat(addr, &cookie).await;
    say(
        &mut first,
        ClientMsg::Hello {
            last_seq: 0,
            log_id: None,
        },
    )
    .await;
    say(&mut first, ClientMsg::Send { text: "Un".into() }).await;
    let turn = until_turn_ended(&mut first).await;
    let seqs: Vec<u64> = turn.iter().map(|entry| entry.seq).collect();
    assert_eq!(seqs, [1, 2, 3, 4]);

    // A client that saw entry 2 gets the rest replayed, then the live entries, each once.
    let mut second = open_chat(addr, &cookie).await;
    say(
        &mut second,
        ClientMsg::Hello {
            last_seq: 2,
            log_id: None,
        },
    )
    .await;
    assert_eq!(next_entry(&mut second).await, turn[2]);
    assert_eq!(next_entry(&mut second).await, turn[3]);
    say(
        &mut second,
        ClientMsg::Send {
            text: "Deux".into(),
        },
    )
    .await;
    let live = until_turn_ended(&mut second).await;
    let seqs: Vec<u64> = live.iter().map(|entry| entry.seq).collect();
    assert_eq!(seqs, [5, 6, 7, 8]);
    assert_eq!(
        live[0].event,
        Event::User {
            text: "Deux".into()
        }
    );

    // The first client streams the same live entries.
    assert_eq!(until_turn_ended(&mut first).await, live);
}

#[tokio::test]
async fn hub_url_follows_the_listen_address() {
    // A server listening on one address only, as on a Tailscale IP, is not on 127.0.0.1.
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path(), true);
    config.server.listen = "127.0.0.2:0".parse().unwrap();
    let (connector, seen) = fake_connector();
    let addr = start_with(config, connector, &site(dir.path())).await;
    let cookie = sign_in(addr).await;
    let mut socket = open_chat(addr, &cookie).await;
    say(
        &mut socket,
        ClientMsg::Hello {
            last_seq: 0,
            log_id: None,
        },
    )
    .await;
    say(&mut socket, ClientMsg::Send { text: "Un".into() }).await;
    until_turn_ended(&mut socket).await;

    let spec = seen.lock().unwrap().clone().unwrap();
    assert_eq!(
        spec.hub_url,
        format!("http://127.0.0.2:{}/mcp", addr.port())
    );
}

#[tokio::test]
async fn secure_cookie_marks_the_session_cookie_secure() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path(), true);
    config.web.as_mut().unwrap().secure_cookie = true;
    let (connector, _) = fake_connector();
    let addr = start_with(config, connector, &site(dir.path())).await;
    let response = post_password(addr, PASSWORD).await;
    let set_cookie = response.headers()[SET_COOKIE].to_str().unwrap();
    assert!(set_cookie.contains("Secure"), "{set_cookie}");
}

#[tokio::test]
async fn without_web_section_no_chat_routes() {
    let dir = tempfile::tempdir().unwrap();
    let (connector, seen) = fake_connector();
    let addr = start_with(config(dir.path(), false), connector, &site(dir.path())).await;
    assert_eq!(get(addr, "/sign-in", None).await.status(), 404);
    assert_eq!(get(addr, "/health", None).await.status(), 200);
    assert!(seen.lock().unwrap().is_none());
    assert!(!dir.path().join("data").join("agent").exists());
}

#[test]
fn check_requires_web_password_secret() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("token"), TOKEN).unwrap();
    let path = dir.path().join("jarvis.toml");
    std::fs::write(
        &path,
        format!(
            "[server]\ntoken = {{ file = {:?} }}\n[data]\ndir = \"/tmp/x\"\ntimezone = \"Europe/Paris\"\n[web]\npassword = {{ file = {:?} }}\n",
            dir.path().join("token").display().to_string(),
            dir.path().join("missing").display().to_string()
        ),
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_jarvis"))
        .args(["check", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("web.password"), "{stderr}");
}

fn assert_security_headers(response: &reqwest::Response) {
    for (name, value) in [
        ("x-content-type-options", "nosniff"),
        ("content-security-policy", "frame-ancestors 'none'"),
        ("referrer-policy", "same-origin"),
    ] {
        assert_eq!(response.headers()[name], value, "{name}");
    }
}

/// Checks that `response` is the interface's page, which browsers must check again each time.
async fn assert_index(response: reqwest::Response) {
    assert_eq!(response.status(), 200);
    assert_security_headers(&response);
    assert_eq!(response.headers()[CACHE_CONTROL], "no-cache");
    assert!(
        response.headers()[CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html"),
    );
    assert_eq!(response.text().await.unwrap(), INDEX);
}

#[tokio::test]
async fn sign_in_serves_index() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _) = start(dir.path()).await;

    // The page reads its error from the query.
    for path in ["/sign-in", "/sign-in?error=1"] {
        let page = get(addr, path, None).await;
        assert!(page.headers().get(SET_COOKIE).is_none());
        assert_index(page).await;
    }
}

#[tokio::test]
async fn root_redirects_without_session() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _) = start(dir.path()).await;

    for cookie in [None, Some("jarvis_session=forged"), Some(FORGED)] {
        let page = get(addr, "/", cookie).await;
        assert_eq!(page.status(), 303);
        assert_eq!(page.headers()[LOCATION], "/sign-in");
        assert_security_headers(&page);
    }
}

#[tokio::test]
async fn root_serves_index_when_signed_in() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _) = start(dir.path()).await;
    let cookie = sign_in(addr).await;

    assert_index(get(addr, "/", Some(&cookie)).await).await;
}

#[tokio::test]
async fn assets_are_cached_long() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _) = start(dir.path()).await;

    // Asset names change with their content, so browsers may keep them for good, without a session.
    let css = get(addr, "/assets/app-1a2b.css", None).await;
    assert_eq!(css.status(), 200);
    assert_security_headers(&css);
    assert_eq!(
        css.headers()[CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    assert!(
        css.headers()[CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/css")
    );
    assert_eq!(css.text().await.unwrap(), CSS);

    // The compressed copy dx wrote, for browsers that take it.
    let brotli = timeout(
        WAIT,
        client()
            .get(format!("http://{addr}/assets/app-1a2b.css"))
            .header(ACCEPT_ENCODING, "br")
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(brotli.status(), 200);
    assert_eq!(brotli.headers()[CONTENT_ENCODING], "br");
    assert_eq!(brotli.text().await.unwrap(), "brotli bytes");

    // A missing asset is not cached for a year.
    let missing = get(addr, "/assets/gone-9z9z.js", None).await;
    assert_eq!(missing.status(), 404);
    assert!(missing.headers().get(CACHE_CONTROL).is_none());

    // Other files at the site root are served as they are.
    let icon = get(addr, "/favicon.ico", None).await;
    assert_eq!(icon.status(), 200);
    assert_eq!(icon.text().await.unwrap(), "icon");
}

#[tokio::test]
async fn unknown_path_is_404() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _) = start(dir.path()).await;
    let cookie = sign_in(addr).await;

    for cookie in [None, Some(cookie.as_str())] {
        let page = get(addr, "/nope", cookie).await;
        assert_eq!(page.status(), 404);
        assert_security_headers(&page);
    }
}

#[tokio::test]
async fn forged_cookie_creates_no_session() {
    // Only signed-in sessions are saved again; an unknown id must not become a session.
    let dir = tempfile::tempdir().unwrap();
    let (addr, _) = start(dir.path()).await;

    for path in [
        "/assets/app-1a2b.css",
        "/assets/x",
        "/nope",
        "/sign-in",
        "/health",
    ] {
        let response = get(addr, path, Some(FORGED)).await;
        assert!(
            response.headers().get(SET_COOKIE).is_none(),
            "{path}: {:?}",
            response.headers().get(SET_COOKIE)
        );
    }
    let mut socket = connect(addr, Some(FORGED), Some(&format!("http://{addr}")))
        .await
        .unwrap();
    assert_eq!(close_code(&mut socket).await, 4401);
}

#[tokio::test]
async fn missing_build_answers_503() {
    // The hub works before the interface is built; the pages say how to build it.
    let dir = tempfile::tempdir().unwrap();
    let (connector, _) = fake_connector();
    let empty = dir.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    let addr = start_with(config(dir.path(), true), connector, &empty).await;
    let cookie = sign_in(addr).await;

    for (path, cookie) in [("/sign-in", None), ("/", Some(cookie.as_str()))] {
        let page = get(addr, path, cookie).await;
        assert_eq!(page.status(), 503, "{path}");
        assert_eq!(
            page.text().await.unwrap(),
            "Interface non construite : lance `mise run web-build`."
        );
    }
    assert_eq!(get(addr, "/health", None).await.status(), 200);
}

#[tokio::test]
async fn hello_names_the_log_before_the_replay() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _) = start(dir.path()).await;
    let cookie = sign_in(addr).await;

    let mut socket = open_chat(addr, &cookie).await;
    let (log_id, last_seq) = hello(&mut socket, 0, None).await;
    assert!(!log_id.is_empty());
    assert_eq!(last_seq, 0);
    say(&mut socket, ClientMsg::Send { text: "Un".into() }).await;
    let turn = until_turn_ended(&mut socket).await;

    // The same log, the next time: only the newer entries.
    let mut again = open_chat(addr, &cookie).await;
    assert_eq!(
        hello(&mut again, 2, Some(&log_id)).await,
        (log_id.clone(), 4)
    );
    assert_eq!(next_entry(&mut again).await, turn[2]);

    // A page that saw another log gets this one from the start, even though its seq is in range.
    let mut other = open_chat(addr, &cookie).await;
    assert_eq!(hello(&mut other, 2, Some("another-log")).await, (log_id, 4));
    for entry in &turn {
        assert_eq!(&next_entry(&mut other).await, entry);
    }
}

#[tokio::test]
async fn reconnect_in_the_middle_of_an_answer_gets_the_rest_once() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(Notify::new());
    let (connector, _) = fake_connector_with(FakeScript {
        updates: vec![
            text_chunk("Un, "),
            text_chunk("deux, "),
            text_chunk("trois."),
        ],
        pause: Some((1, gate.clone())),
        ..FakeScript::default()
    });
    let addr = start_with(config(dir.path(), true), connector, &site(dir.path())).await;
    let cookie = sign_in(addr).await;

    let mut first = open_chat(addr, &cookie).await;
    let (log_id, _) = hello(&mut first, 0, None).await;
    say(
        &mut first,
        ClientMsg::Send {
            text: "Compte".into(),
        },
    )
    .await;
    assert_eq!(
        next_entry(&mut first).await.event,
        Event::User {
            text: "Compte".into()
        }
    );
    let seen = next_entry(&mut first).await;
    assert_eq!(
        seen.event,
        Event::Assistant {
            text: "Un, ".into()
        }
    );
    // The phone sleeps: the socket drops in the middle of the answer.
    drop(first);

    // It comes back with the last entry it saw, while the agent goes on.
    let mut second = open_chat(addr, &cookie).await;
    say(
        &mut second,
        ClientMsg::Hello {
            last_seq: seen.seq,
            log_id: Some(log_id),
        },
    )
    .await;
    gate.notify_one();
    let rest = until_turn_ended(&mut second).await;

    let rest: Vec<(u64, Event)> = rest.into_iter().map(|e| (e.seq, e.event)).collect();
    assert_eq!(
        rest,
        [
            (
                3,
                Event::Assistant {
                    text: "deux, ".into()
                }
            ),
            (
                4,
                Event::Assistant {
                    text: "trois.".into()
                }
            ),
            (
                5,
                Event::TurnEnded {
                    outcome: TurnOutcome::Done
                }
            ),
        ]
    );
    // Nothing comes twice.
    assert!(
        timeout(Duration::from_millis(200), second.next())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn hello_after_a_log_reset_replays_everything() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _) = start(dir.path()).await;
    let cookie = sign_in(addr).await;
    let mut socket = open_chat(addr, &cookie).await;

    // A page that saw entries the log no longer has (it was deleted) gets the whole log again.
    say(
        &mut socket,
        ClientMsg::Hello {
            last_seq: 0,
            log_id: None,
        },
    )
    .await;
    say(&mut socket, ClientMsg::Send { text: "Un".into() }).await;
    let turn = until_turn_ended(&mut socket).await;
    let mut stale = open_chat(addr, &cookie).await;
    say(
        &mut stale,
        ClientMsg::Hello {
            last_seq: 100,
            log_id: None,
        },
    )
    .await;
    for entry in &turn {
        assert_eq!(&next_entry(&mut stale).await, entry);
    }
}
