//! A chat with the real Claude adapter, signed in with the owner's subscription. Ignored by
//! default; run it with the adapter's path:
//!
//! ```sh
//! JARVIS_LIVE_AGENT=$(which claude-agent-acp) \
//!   mise exec -- cargo nextest run -p jarvis --test live --run-ignored only --no-capture
//! ```
//!
//! `JARVIS_LIVE_CLAUDE` optionally names the Claude Code to run instead of the adapter's own.

use futures::{SinkExt, StreamExt};
use jarvis_protocol::{ClientMsg, Entry, Event, ServerMsg};
use reqwest::header::{CONTENT_TYPE, SET_COOKIE};
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "live";
const TURN: Duration = Duration::from_secs(180);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn config(dir: &Path, agent: &str) -> jarvis_hub::config::Config {
    std::fs::write(dir.join("token"), TOKEN).unwrap();
    std::fs::write(dir.join("password"), PASSWORD).unwrap();
    let env = match std::env::var("JARVIS_LIVE_CLAUDE") {
        Ok(claude) => format!("{{ CLAUDE_CODE_EXECUTABLE = {claude:?} }}"),
        Err(_) => "{}".to_owned(),
    };
    let toml = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\ntoken = {{ file = {:?} }}\n[data]\ndir = {:?}\ntimezone = \"Europe/Paris\"\nuser = \"Alix\"\n[memory]\ngit = false\n[web]\npassword = {{ file = {:?} }}\n[agent]\ncommand = [{agent:?}]\nenv = {env}\n",
        dir.join("token").display().to_string(),
        dir.join("data").display().to_string(),
        dir.join("password").display().to_string(),
    );
    toml::from_str(&toml).unwrap()
}

async fn sign_in(addr: SocketAddr) -> String {
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(format!("http://{addr}/auth/sign-in"))
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(format!("password={PASSWORD}"))
        .send()
        .await
        .unwrap();
    let cookie = response.headers()[SET_COOKIE].to_str().unwrap();
    cookie.split(';').next().unwrap().to_owned()
}

async fn open_chat(addr: SocketAddr, cookie: &str) -> Socket {
    let mut request = format!("ws://{addr}/ws").into_client_request().unwrap();
    let headers = request.headers_mut();
    headers.insert("cookie", cookie.parse().unwrap());
    headers.insert("origin", format!("http://{addr}").parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    say(
        &mut socket,
        ClientMsg::Hello {
            last_seq: 0,
            log_id: None,
        },
    )
    .await;
    socket
}

async fn say(socket: &mut Socket, message: ClientMsg) {
    let json = serde_json::to_string(&message).unwrap();
    socket.send(Message::text(json)).await.unwrap();
}

/// Sends `text` and returns the entries of its turn, printed as they come. A permission request
/// fails the test: Jarvis's tools are allowed up front.
async fn turn(socket: &mut Socket, text: &str) -> Vec<Event> {
    say(socket, ClientMsg::Send { text: text.into() }).await;
    let mut events = Vec::new();
    let wait = async {
        while let Some(message) = socket.next().await {
            let Message::Text(json) = message.unwrap() else {
                continue;
            };
            let ServerMsg::Entry(Entry { event, .. }) = serde_json::from_str(&json).unwrap() else {
                continue;
            };
            eprintln!("{}", serde_json::to_string(&event).unwrap());
            if let Event::Permission { title, .. } = &event {
                panic!("the agent asked for a permission: {title}");
            }
            let ended = matches!(event, Event::TurnEnded { .. });
            events.push(event);
            if ended {
                return;
            }
        }
        panic!("the chat closed during the turn");
    };
    timeout(TURN, wait)
        .await
        .unwrap_or_else(|_| panic!("no turn_ended within {TURN:?}"));
    events
}

#[tokio::test]
#[ignore = "talks to the real Claude; set JARVIS_LIVE_AGENT to the adapter's path"]
async fn live_chat_with_claude() {
    let Ok(agent) = std::env::var("JARVIS_LIVE_AGENT") else {
        eprintln!("JARVIS_LIVE_AGENT is not set: skipped");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (addr, _server) = jarvis::start(config(dir.path(), &agent)).await.unwrap();
    let cookie = sign_in(addr).await;
    let mut socket = open_chat(addr, &cookie).await;

    let hello = turn(&mut socket, "Bonjour ! Réponds en une seule phrase courte.").await;
    assert!(
        hello
            .iter()
            .any(|event| matches!(event, Event::Assistant { .. })),
        "no answer: {hello:?}"
    );

    turn(&mut socket, "Retiens que je préfère le thé au café.").await;
    // A preference fits the core memory or what Jarvis knows about the user; either will do.
    let memory = dir.path().join("data/memory");
    let files = ["MEMORY.md", "USER.md"].map(|file| {
        (
            file,
            std::fs::read_to_string(memory.join(file)).unwrap_or_default(),
        )
    });
    for (file, text) in &files {
        eprintln!("{file}: {text:?}");
    }
    assert!(
        files.iter().any(|(_, text)| text.contains("thé")),
        "the preference was not saved: {files:?}"
    );

    let web = turn(
        &mut socket,
        "Cherche sur le web la version stable actuelle de Rust et réponds en une phrase.",
    )
    .await;
    assert!(
        web.iter()
            .any(|event| matches!(event, Event::Tool { title, .. } if is_web(title))),
        "no web tool ran: {web:?}"
    );
    assert!(
        web.iter()
            .any(|event| matches!(event, Event::Assistant { .. })),
        "no answer: {web:?}"
    );

    // Besides the web, Jarvis has only the hub's tools: no shell, no file access, nothing from
    // the owner's Claude settings or plugins.
    let listing = turn(
        &mut socket,
        "Liste les fichiers du répertoire courant (par exemple avec ls) et dis-moi quels outils tu as.",
    )
    .await;
    for event in web.iter().chain(&listing) {
        if let Event::Tool { title, .. } = event {
            assert!(
                title.starts_with("mcp__jarvis__") || is_web(title),
                "a tool from outside the hub and the web ran: {title}"
            );
        }
    }
}

/// The adapter's titles for WebSearch (`Search "…"`, `Web search`) and WebFetch (`Fetch …`).
fn is_web(title: &str) -> bool {
    title.starts_with("Search \"") || title == "Web search" || title.starts_with("Fetch")
}
