//! The chat WebSocket: the conversation log streamed to the page, its messages passed to Fronde.

use crate::web::{Web, is_signed_in, renew};
use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fronde_agent::assistant::Assistant;
use fronde_protocol::{CLOSE_SIGNED_OUT, ClientMsg, Entry, ServerMsg};
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
        return upgrade.on_upgrade(|mut socket| async move {
            let close = CloseFrame {
                code: CLOSE_SIGNED_OUT,
                reason: "".into(),
            };
            let _ = socket.send(Message::Close(Some(close))).await;
        });
    }
    renew(&session);
    let assistant = web.assistant.clone();
    upgrade.on_upgrade(move |socket| chat(socket, assistant))
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

/// Streams the log once the page said hello, and passes the page's messages to Fronde.
async fn chat(mut socket: WebSocket, assistant: Arc<Assistant>) {
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
                    Ok(ClientMsg::Hello { last_seq, log_id }) => {
                        // Subscribed first, so nothing falls between the replay and the stream.
                        live = Some(assistant.log().subscribe());
                        let log = assistant.log();
                        let newest = log.last_seq();
                        // A page that saw another log, or one ahead of this log, saw a log since
                        // deleted: it gets the whole new one.
                        let other_log = log_id.is_some_and(|id| id != log.log_id());
                        last_sent = if other_log || last_seq > newest { 0 } else { last_seq };
                        let hello = ServerMsg::Hello {
                            log_id: log.log_id().to_owned(),
                            last_seq: newest,
                        };
                        if send(&mut socket, &hello).await.is_err()
                            || replay(&mut socket, &assistant, &mut last_sent).await.is_err()
                        {
                            return;
                        }
                    }
                    Ok(ClientMsg::Send { text }) => assistant.send(text).await,
                    Ok(ClientMsg::Cancel) => assistant.cancel().await,
                    Ok(ClientMsg::Answer { id, allow }) => assistant.answer(id, allow).await,
                    Err(error) => tracing::debug!("ignoring a bad chat message: {error}"),
                }
            }
            entry = next_live(&mut live) => {
                let sent = match entry {
                    Ok(entry) if entry.seq > last_sent => {
                        last_sent = entry.seq;
                        send(&mut socket, &ServerMsg::Entry(entry)).await
                    }
                    Ok(_) => Ok(()),
                    Err(RecvError::Lagged(_)) => replay(&mut socket, &assistant, &mut last_sent).await,
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
async fn replay(
    socket: &mut WebSocket,
    assistant: &Assistant,
    last_sent: &mut u64,
) -> Result<(), ()> {
    for entry in assistant.log().since(*last_sent) {
        *last_sent = entry.seq;
        send(socket, &ServerMsg::Entry(entry)).await?;
    }
    Ok(())
}

async fn send(socket: &mut WebSocket, message: &ServerMsg) -> Result<(), ()> {
    let json = match serde_json::to_string(message) {
        Ok(json) => json,
        Err(error) => {
            tracing::error!("cannot encode a chat message: {error}");
            return Ok(());
        }
    };
    socket
        .send(Message::Text(json.into()))
        .await
        .map_err(|_| ())
}
