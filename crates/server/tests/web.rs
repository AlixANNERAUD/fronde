use futures::{SinkExt, StreamExt};
use jarvis_agent::acp::{AgentClient, AgentSpec};
use jarvis_agent::fake::{FakeScript, fake_agent, text_chunk};
use jarvis_agent::jarvis::Connector;
use jarvis_protocol::{ClientMsg, Entry, Event, ServerMsg, TurnOutcome};
use reqwest::header::{CONTENT_TYPE, COOKIE, LOCATION, SET_COOKIE};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "correct horse battery staple";
const WAIT: Duration = Duration::from_secs(10);

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
    let seen = Arc::new(Mutex::new(None));
    let connector: Connector = Arc::new({
        let seen = seen.clone();
        move |spec: &AgentSpec, resume, events| {
            *seen.lock().unwrap() = Some(spec.clone());
            let spec = spec.clone();
            Box::pin(async move {
                let (transport, _handle) = fake_agent(FakeScript {
                    updates: vec![text_chunk("Bonjour "), text_chunk("Alix !")],
                    ..FakeScript::default()
                });
                AgentClient::connect(transport, &spec, resume, events).await
            })
        }
    });
    (connector, seen)
}

async fn start(dir: &Path) -> (SocketAddr, Arc<Mutex<Option<AgentSpec>>>) {
    let (connector, seen) = fake_connector();
    let (addr, _server) = timeout(WAIT, jarvis::start_with(config(dir, true), connector))
        .await
        .unwrap()
        .unwrap();
    (addr, seen)
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

async fn next_entry(socket: &mut Socket) -> Entry {
    loop {
        let message = timeout(WAIT, socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Message::Text(text) = message {
            let ServerMsg::Entry(entry) = serde_json::from_str(&text).unwrap();
            return entry;
        }
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
    say(&mut socket, ClientMsg::Hello { last_seq: 0 }).await;
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
    say(&mut first, ClientMsg::Hello { last_seq: 0 }).await;
    say(&mut first, ClientMsg::Send { text: "Un".into() }).await;
    let turn = until_turn_ended(&mut first).await;
    let seqs: Vec<u64> = turn.iter().map(|entry| entry.seq).collect();
    assert_eq!(seqs, [1, 2, 3, 4]);

    // A client that saw entry 2 gets the rest replayed, then the live entries, each once.
    let mut second = open_chat(addr, &cookie).await;
    say(&mut second, ClientMsg::Hello { last_seq: 2 }).await;
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
async fn secure_cookie_marks_the_session_cookie_secure() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path(), true);
    config.web.as_mut().unwrap().secure_cookie = true;
    let (connector, _) = fake_connector();
    let (addr, _server) = timeout(WAIT, jarvis::start_with(config, connector))
        .await
        .unwrap()
        .unwrap();
    let response = post_password(addr, PASSWORD).await;
    let set_cookie = response.headers()[SET_COOKIE].to_str().unwrap();
    assert!(set_cookie.contains("Secure"), "{set_cookie}");
}

#[tokio::test]
async fn without_web_section_no_chat_routes() {
    let dir = tempfile::tempdir().unwrap();
    let (connector, seen) = fake_connector();
    let (addr, _server) = timeout(
        WAIT,
        jarvis::start_with(config(dir.path(), false), connector),
    )
    .await
    .unwrap()
    .unwrap();
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

#[tokio::test]
async fn sign_in_page_renders() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _) = start(dir.path()).await;

    let page = get(addr, "/sign-in", None).await;
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(html.contains("<title>Connexion · Jarvis</title>"), "{html}");
    assert!(html.contains("href=\"/pkg/jarvis.css\""), "{html}");
    assert!(html.contains("action=\"/sign-in\""), "{html}");
    assert!(html.contains("type=\"password\""), "{html}");
    assert!(html.contains("Mot de passe"), "{html}");
    assert!(html.contains("Se connecter"), "{html}");
    assert!(!html.contains("role=\"alert\""), "{html}");
    for (name, value) in [
        ("x-content-type-options", "nosniff"),
        ("content-security-policy", "frame-ancestors 'none'"),
        ("referrer-policy", "same-origin"),
    ] {
        assert_eq!(get(addr, "/sign-in", None).await.headers()[name], value);
    }

    let wrong = get(addr, "/sign-in?error=1", None)
        .await
        .text()
        .await
        .unwrap();
    assert!(wrong.contains("Mot de passe incorrect."), "{wrong}");
    let locked = get(addr, "/sign-in?error=2", None)
        .await
        .text()
        .await
        .unwrap();
    assert!(
        locked.contains("Trop d'essais, réessaie dans une minute."),
        "{locked}"
    );
}

#[tokio::test]
async fn chat_page_redirects_without_session() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _) = start(dir.path()).await;

    for cookie in [None, Some("jarvis_session=forged")] {
        let page = get(addr, "/", cookie).await;
        assert_eq!(page.status(), 303);
        assert_eq!(page.headers()[LOCATION], "/sign-in");
    }
}

#[tokio::test]
async fn chat_page_renders_island_when_signed_in() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _) = start(dir.path()).await;
    let cookie = sign_in(addr).await;

    let page = get(addr, "/", Some(&cookie)).await;
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(html.contains("<title>Jarvis</title>"), "{html}");
    assert!(html.contains("<leptos-island"), "{html}");
    assert!(html.contains("ChatPanel"), "{html}");
    assert!(html.contains("/pkg/jarvis.js"), "{html}");
    assert!(html.contains("action=\"/sign-out\""), "{html}");
    assert!(html.contains("Se déconnecter"), "{html}");
    assert!(html.contains("<textarea"), "{html}");
}

#[tokio::test]
async fn hello_after_a_log_reset_replays_everything() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _) = start(dir.path()).await;
    let cookie = sign_in(addr).await;
    let mut socket = open_chat(addr, &cookie).await;

    // A page that saw entries the log no longer has (it was deleted) gets the whole log again.
    say(&mut socket, ClientMsg::Hello { last_seq: 0 }).await;
    say(&mut socket, ClientMsg::Send { text: "Un".into() }).await;
    let turn = until_turn_ended(&mut socket).await;
    let mut stale = open_chat(addr, &cookie).await;
    say(&mut stale, ClientMsg::Hello { last_seq: 100 }).await;
    for entry in &turn {
        assert_eq!(&next_entry(&mut stale).await, entry);
    }
}
