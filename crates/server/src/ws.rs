//! The chat WebSocket: the conversation log streamed to the page, its messages passed to Fronde.

use crate::web::{Web, is_signed_in, renew};
use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fronde_agent::assistant::Assistant;
use fronde_protocol::{CLOSE_SIGNED_OUT, ClientMsg, Entry, ServerMsg};
use fronde_requests::User;
use std::sync::Arc;
use tokio::sync::broadcast::{self, error::RecvError};
use tower_sessions::Session;
use zeroize::Zeroize as _;

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
    upgrade
        .max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| chat(socket, web))
}

/// The largest message a page may send: a message to Fronde or a card's answer is far smaller,
/// and anything bigger only costs the server memory. The socket closes on a bigger one.
const MAX_MESSAGE: usize = 64 * 1024;

/// The answer a page gives a permission entry of the log to allow it.
const ALLOW_ONCE: &str = "allow_once";

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
async fn chat(mut socket: WebSocket, web: Arc<Web>) {
    let assistant = &web.assistant;
    // Whoever signed in is the owner, the one user so far, and an admin.
    let user = User {
        name: web.user.clone(),
        admin: true,
    };
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
                let parsed = serde_json::from_str::<ClientMsg>(&text);
                // The frame may carry a secret typed in a card: wiped once read, whatever it was.
                wipe(text);
                match parsed {
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
                            user: Some(web.user.clone()),
                            admin: user.admin,
                        };
                        if send(&mut socket, &hello).await.is_err()
                            || replay(&mut socket, assistant, &mut last_sent).await.is_err()
                        {
                            return;
                        }
                    }
                    Ok(ClientMsg::Send { text }) => assistant.send(text).await,
                    Ok(ClientMsg::Cancel) => assistant.cancel().await,
                    // Until the agent's permissions are requests, they answer the log's
                    // permission entries; anything but an allow refuses.
                    Ok(ClientMsg::Answer { id, response: fronde_protocol::Response::Permission { choice } }) => {
                        assistant.answer(id, choice == ALLOW_ONCE).await;
                    }
                    Ok(ClientMsg::Answer { id, response }) => {
                        // A refusal is for this page alone: the request did not change.
                        if let Err(refusal) = web.requests.answer(id, response, &user).await {
                            let refused = ServerMsg::Refused { id, reason: refusal.to_string() };
                            if send(&mut socket, &refused).await.is_err() {
                                return;
                            }
                        }
                    }
                    // serde's message can quote the input, which may be an answer: only where
                    // and what kind of error.
                    Err(error) => tracing::debug!(
                        "ignoring a bad chat message: {:?} error at line {} column {}",
                        error.classify(),
                        error.line(),
                        error.column()
                    ),
                }
            }
            entry = next_live(&mut live) => {
                let sent = match entry {
                    Ok(entry) if entry.seq > last_sent => {
                        last_sent = entry.seq;
                        send(&mut socket, &ServerMsg::Entry(entry)).await
                    }
                    Ok(_) => Ok(()),
                    Err(RecvError::Lagged(_)) => replay(&mut socket, assistant, &mut last_sent).await,
                    Err(RecvError::Closed) => return,
                };
                if sent.is_err() {
                    return;
                }
            }
        }
    }
}

/// Overwrites a frame's text with zeros, when nothing else holds it. A secret answer then lives on
/// only in the `SecretString` the request service makes of it.
fn wipe(text: Utf8Bytes) {
    if let Ok(mut bytes) = bytes::Bytes::from(text).try_into_mut() {
        bytes.as_mut().zeroize();
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
