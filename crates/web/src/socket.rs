//! The chat's WebSocket to `/ws`, run by `fronde_client::Connection`: it says hello with what the
//! chat has, applies what the server sends, sends what the page asks while it is open, retries
//! after 1 s then up to every 30 s (at once when the page comes back to the foreground), and goes
//! to the sign-in page when the server closes with 4401.
//!
//! One coroutine owns the socket and the retry timer, so there is never more than one of each:
//! a retry drops the timer it replaces, and a closed socket is dropped before the next opens, so
//! no stale timer or socket can act on the chat.

use dioxus::prelude::*;
use fronde_client::Chat;
use fronde_protocol::ClientMsg;

/// What the page shows of the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The first connection, before it opened.
    Connecting,
    Open,
    /// Closed, or opening again after a failure.
    Reconnecting,
}

/// A message for the server, and what to do once it went out.
// Natively only tests read it: there is no socket.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub struct Outgoing {
    pub message: ClientMsg,
    /// Called only if the message was sent: a message the page sends while the socket is not
    /// open is dropped.
    pub sent: Option<Callback<()>>,
    /// Called if it was dropped instead, the socket closed before it went out.
    pub dropped: Option<Callback<()>>,
}

impl Outgoing {
    /// Says the message was dropped, to whoever asked.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    fn drop_it(self) {
        if let Some(dropped) = self.dropped {
            dropped.call(());
        }
    }
}

/// Connects the chat; messages sent to the returned coroutine go to the server.
pub fn use_chat_socket(chat: Signal<Chat>, status: Signal<Status>) -> Coroutine<Outgoing> {
    use_coroutine(move |commands: UnboundedReceiver<Outgoing>| imp::run(chat, status, commands))
}

#[cfg(target_arch = "wasm32")]
mod imp {
    use dioxus::prelude::*;
    use fronde_client::{Action, Chat, Connection};
    use fronde_protocol::{ClientMsg, ServerMsg};
    use futures::{FutureExt, SinkExt, StreamExt, future::poll_fn, select};
    use gloo_net::websocket::{Message, State, WebSocketError, futures::WebSocket};
    use gloo_timers::future::TimeoutFuture;
    use wasm_bindgen::{JsCast, closure::Closure};

    use super::{Outgoing, Status};

    /// What a socket that closed without saying why (or never opened) reports.
    const ABNORMAL_CLOSURE: u16 = 1006;

    pub async fn run(
        chat: Signal<Chat>,
        mut status: Signal<Status>,
        mut commands: UnboundedReceiver<Outgoing>,
    ) {
        let mut connection = Connection::default();
        let mut visible = visibility_changes();
        loop {
            let code = match ws_url().and_then(|url| WebSocket::open(&url).ok()) {
                Some(socket) => {
                    talk(
                        socket,
                        &mut connection,
                        chat,
                        status,
                        &mut commands,
                        &mut visible,
                    )
                    .await
                }
                None => ABNORMAL_CLOSURE,
            };
            match connection.on_close(code) {
                Action::GoToSignIn => {
                    // The session expired, or the server restarted and forgot it.
                    go_to("/sign-in");
                    return;
                }
                Action::Retry(delay) => {
                    status.set(Status::Reconnecting);
                    if !wait(delay, &mut connection, &mut commands, &mut visible).await {
                        return;
                    }
                }
                // A close never asks to retry at once.
                Action::RetryNow => {}
            }
        }
    }

    /// Runs one socket until it closes; returns its close code.
    async fn talk(
        mut socket: WebSocket,
        connection: &mut Connection,
        mut chat: Signal<Chat>,
        mut status: Signal<Status>,
        commands: &mut UnboundedReceiver<Outgoing>,
        visible: &mut UnboundedReceiver<()>,
    ) -> u16 {
        // The socket is ready to send once it opened, or once it failed to.
        loop {
            select! {
                _ = poll_fn(|cx| socket.poll_ready_unpin(cx)).fuse() => break,
                outgoing = commands.next() => outgoing.map_or((), Outgoing::drop_it),
                _ = visible.next() => {
                    connection.on_visible();
                }
            }
        }
        if !matches!(socket.state(), State::Open) {
            return close_code(socket).await;
        }
        let hello = connection.on_open(&chat.peek());
        status.set(Status::Open);
        send(&mut socket, &hello).await;
        loop {
            select! {
                incoming = socket.next().fuse() => match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(message) = serde_json::from_str::<ServerMsg>(&text) {
                            if matches!(message, ServerMsg::Hello { .. }) {
                                connection.on_hello();
                            }
                            chat.write().apply(&message);
                        }
                    }
                    Some(Err(WebSocketError::ConnectionClose(event))) => return event.code,
                    // An error event comes before the close.
                    Some(Ok(Message::Bytes(_))) | Some(Err(_)) => {}
                    None => return ABNORMAL_CLOSURE,
                },
                outgoing = commands.next() => {
                    if let Some(outgoing) = outgoing {
                        if send(&mut socket, &outgoing.message).await {
                            if let Some(sent) = outgoing.sent {
                                sent.call(());
                            }
                        } else {
                            outgoing.drop_it();
                        }
                    }
                }
                _ = visible.next() => {
                    connection.on_visible();
                }
            }
        }
    }

    /// Waits before the next attempt: until the delay ends or the page becomes visible. False when
    /// no attempt should follow.
    async fn wait(
        delay: u32,
        connection: &mut Connection,
        commands: &mut UnboundedReceiver<Outgoing>,
        visible: &mut UnboundedReceiver<()>,
    ) -> bool {
        let mut timer = TimeoutFuture::new(delay).fuse();
        loop {
            select! {
                () = timer => return connection.on_retry(),
                _ = visible.next() => {
                    if connection.on_visible() == Some(Action::RetryNow) {
                        return true;
                    }
                }
                // Nothing is sent while the socket is closed.
                outgoing = commands.next() => outgoing.map_or((), Outgoing::drop_it),
            }
        }
    }

    async fn close_code(mut socket: WebSocket) -> u16 {
        while let Some(event) = socket.next().await {
            if let Err(WebSocketError::ConnectionClose(event)) = event {
                return event.code;
            }
        }
        ABNORMAL_CLOSURE
    }

    /// False when the message did not go out; a failed send also shows as the socket closing.
    async fn send(socket: &mut WebSocket, message: &ClientMsg) -> bool {
        let Ok(json) = serde_json::to_string(message) else {
            return false;
        };
        // Browsers drop, without an error, what is sent to a closing socket.
        matches!(socket.state(), State::Open) && socket.send(Message::Text(json)).await.is_ok()
    }

    /// `ws://` or `wss://` to this site's `/ws`.
    fn ws_url() -> Option<String> {
        let location = web_sys::window()?.location();
        let scheme = if location.protocol().ok()? == "https:" {
            "wss"
        } else {
            "ws"
        };
        Some(format!("{scheme}://{}/ws", location.host().ok()?))
    }

    fn go_to(path: &str) {
        if let Some(window) = web_sys::window() {
            let _ = window.location().set_href(path);
        }
    }

    /// One message each time the page becomes visible again.
    fn visibility_changes() -> UnboundedReceiver<()> {
        let (sender, receiver) = futures::channel::mpsc::unbounded();
        let Some(document) = web_sys::window().and_then(|window| window.document()) else {
            return receiver;
        };
        let listener = Closure::<dyn Fn()>::new({
            let document = document.clone();
            move || {
                if document.visibility_state() == web_sys::VisibilityState::Visible {
                    let _ = sender.unbounded_send(());
                }
            }
        });
        let _ = document.add_event_listener_with_callback(
            "visibilitychange",
            listener.as_ref().unchecked_ref(),
        );
        // Listens for as long as the page is open.
        listener.forget();
        receiver
    }
}

/// Natively there is no socket: the page is only rendered, in tests.
#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use dioxus::prelude::*;
    use fronde_client::Chat;

    use super::{Outgoing, Status};

    pub async fn run(
        chat: Signal<Chat>,
        status: Signal<Status>,
        commands: UnboundedReceiver<Outgoing>,
    ) {
        let _ = (chat, status, commands);
    }
}
