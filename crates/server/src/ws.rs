//! The chat WebSocket: the conversation log streamed to the page, its messages passed to Jarvis.

use crate::web::{Web, is_signed_in};
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use jarvis_agent::jarvis::Jarvis;
use jarvis_protocol::{ClientMsg, Entry, ServerMsg};
use std::sync::Arc;
use tokio::sync::broadcast::{self, error::RecvError};
use tower_sessions::Session;

pub async fn upgrade(
    State(web): State<Arc<Web>>,
    session: Session,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    // Browsers do not apply the same-origin policy to WebSockets: the page's origin must match.
    if !same_origin(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !is_signed_in(&session).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let jarvis = web.jarvis.clone();
    upgrade.on_upgrade(move |socket| chat(socket, jarvis))
}

/// True when `Origin` names the host the request was sent to.
fn same_origin(headers: &HeaderMap) -> bool {
    let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
    let origin_host = header(header::ORIGIN).and_then(|o| o.split_once("://").map(|(_, h)| h));
    match (origin_host, header(header::HOST)) {
        (Some(origin), Some(host)) => origin.eq_ignore_ascii_case(host),
        _ => false,
    }
}

/// Streams the log once the page said hello, and passes the page's messages to Jarvis.
async fn chat(mut socket: WebSocket, jarvis: Arc<Jarvis>) {
    let mut live: Option<broadcast::Receiver<Entry>> = None;
    let mut last_sent = 0;
    loop {
        tokio::select! {
            message = socket.recv() => {
                let text = match message {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                    Some(Ok(_)) => continue,
                };
                match serde_json::from_str::<ClientMsg>(&text) {
                    Ok(ClientMsg::Hello { last_seq }) => {
                        // Subscribed first, so nothing falls between the replay and the stream.
                        live = Some(jarvis.log().subscribe());
                        // A page ahead of the log saw a log since deleted: it gets the whole new
                        // one, whose first entry tells it to start over.
                        last_sent = if last_seq > jarvis.log().last_seq() { 0 } else { last_seq };
                        if replay(&mut socket, &jarvis, &mut last_sent).await.is_err() {
                            return;
                        }
                    }
                    Ok(ClientMsg::Send { text }) => jarvis.send(text).await,
                    Ok(ClientMsg::Cancel) => jarvis.cancel(),
                    Ok(ClientMsg::Answer { id, allow }) => jarvis.answer(id, allow),
                    Err(error) => tracing::debug!("ignoring a bad chat message: {error}"),
                }
            }
            entry = next_live(&mut live) => {
                let sent = match entry {
                    Ok(entry) if entry.seq > last_sent => {
                        last_sent = entry.seq;
                        send(&mut socket, entry).await
                    }
                    Ok(_) => Ok(()),
                    Err(RecvError::Lagged(_)) => replay(&mut socket, &jarvis, &mut last_sent).await,
                    Err(RecvError::Closed) => return,
                };
                if sent.is_err() {
                    return;
                }
            }
        }
    }
}

/// Waits for the next live entry; never resolves before the page said hello.
async fn next_live(live: &mut Option<broadcast::Receiver<Entry>>) -> Result<Entry, RecvError> {
    match live {
        Some(live) => live.recv().await,
        None => std::future::pending().await,
    }
}

/// Sends the entries after `last_sent`.
async fn replay(socket: &mut WebSocket, jarvis: &Jarvis, last_sent: &mut u64) -> Result<(), ()> {
    for entry in jarvis.log().since(*last_sent) {
        *last_sent = entry.seq;
        send(socket, entry).await?;
    }
    Ok(())
}

async fn send(socket: &mut WebSocket, entry: Entry) -> Result<(), ()> {
    let json = match serde_json::to_string(&ServerMsg::Entry(entry)) {
        Ok(json) => json,
        Err(error) => {
            tracing::error!("cannot encode a log entry: {error}");
            return Ok(());
        }
    };
    socket
        .send(Message::Text(json.into()))
        .await
        .map_err(|_| ())
}
