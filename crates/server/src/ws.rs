//! The chat WebSocket: a thread's log streamed to the page, the thread list kept up to date,
//! and the page's messages passed to Fronde. Every thread a page names is checked against its
//! user: a thread id is only a selector, never an identity.

use crate::web::{Web, is_signed_in, renew};
use anyhow::bail;
use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fronde_agent::assistant::Assistant;
use fronde_agent::threads::{Refusal, ThreadRow, Threads};
use fronde_protocol::{
    CLOSE_SIGNED_OUT, ClientMsg, Entry, MAX_CLIENT_ID, ServerMsg, ThreadId, ThreadSummary,
    check_title,
};
use fronde_requests::User;
use std::sync::Arc;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::watch;
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
    upgrade
        .max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| chat(socket, web))
}

/// The largest message a page may send: a message to Fronde or a card's answer is far smaller,
/// and anything bigger only costs the server memory. The socket closes on a bigger one.
const MAX_MESSAGE: usize = 64 * 1024;

/// True when `Origin` names the host the request was sent to.
fn same_origin(headers: &HeaderMap) -> bool {
    let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
    let origin_host = header(header::ORIGIN).and_then(|o| o.split_once("://").map(|(_, h)| h));
    match (origin_host, header(header::HOST)) {
        (Some(origin), Some(host)) => origin.eq_ignore_ascii_case(host),
        _ => false,
    }
}

/// Streams a thread's log once the page said hello, and passes the page's messages to Fronde.
async fn chat(mut socket: WebSocket, web: Arc<Web>) {
    // Whoever signed in is the owner, the one user so far, and an admin.
    let user = User {
        name: web.user.clone(),
        admin: web.admin,
    };
    let mut talk = Talk {
        web: &web,
        user: &user,
        current: None,
        live: None,
        last_sent: 0,
        list: None,
        shown: None,
        usage: None,
    };
    loop {
        tokio::select! {
            message = socket.recv() => {
                let text = match message {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                    Some(Ok(_)) => continue,
                };
                // The frame may carry a secret typed in a card, and cannot be wiped: its bytes
                // share the WebSocket library's read buffer, and serde_json copies a string with
                // escapes into a scratch buffer of its own, both freed without being zeroed: they
                // stay in memory until it is reused. The `SecretString` the request service makes
                // of the value is the copy Fronde holds, and zeroes. Dropped at once, so that the
                // read buffer may reuse that memory sooner.
                let parsed = serde_json::from_str::<ClientMsg>(&text);
                drop(text);
                match parsed {
                    Ok(message) => {
                        if talk.handle(&mut socket, message).await.is_err() {
                            return;
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
            entry = next_live(&mut talk.live) => {
                let sent = match entry {
                    Ok(entry) if entry.seq > talk.last_sent => {
                        talk.last_sent = entry.seq;
                        send(&mut socket, &ServerMsg::Entry(entry)).await
                    }
                    Ok(_) => Ok(()),
                    Err(RecvError::Lagged(_)) => talk.replay(&mut socket).await,
                    Err(RecvError::Closed) => return,
                };
                if sent.is_err() {
                    return;
                }
            }
            changed = next_change(&mut talk.list) => {
                if !changed {
                    talk.list = None;
                } else if talk.send_list(&mut socket).await.is_err() {
                    return;
                }
            }
            changed = next_change(&mut talk.usage) => {
                if !changed {
                    talk.usage = None;
                } else if talk.send_usage(&mut socket).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// One page's socket: the user it is signed in as, the thread it streams, and the thread list
/// it was last sent.
struct Talk<'a> {
    web: &'a Web,
    user: &'a User,
    /// The thread streamed, once the page said hello.
    current: Option<Arc<Assistant>>,
    live: Option<broadcast::Receiver<Entry>>,
    last_sent: u64,
    /// The thread list's changes, once a hello named a thread: a page that names none knows
    /// nothing of threads, and gets the main thread's log only, as before threads.
    list: Option<watch::Receiver<u64>>,
    /// The thread list last sent.
    shown: Option<Vec<ThreadSummary>>,
    /// The usage book's changes, once a hello came, for a user who sees the meter.
    usage: Option<watch::Receiver<u64>>,
}

/// The usage `user` is sent for the meter: their own accounts' readings and the reserve, to an
/// admin only for now (the subscription is the owner's), and only once there is a reading.
pub fn usage_message(threads: &Threads, user: &User) -> Option<ServerMsg> {
    if !user.admin {
        return None;
    }
    let usage = threads.usage_of(&user.name);
    (!usage.is_empty()).then(|| ServerMsg::Usage {
        reserve: threads.limits().reserve,
        usage,
    })
}

/// Why a thread message failed, when it is not the page's fault.
const UNAVAILABLE: &str = "Impossible pour le moment ; réessaie plus tard.";

impl Talk<'_> {
    /// Does what the page asked; an error when the socket failed.
    async fn handle(&mut self, socket: &mut WebSocket, message: ClientMsg) -> Result<(), ()> {
        match message {
            ClientMsg::Hello {
                last_seq,
                log_id,
                thread,
            } => self.hello(socket, last_seq, log_id, thread).await,
            ClientMsg::Send { text, thread } => {
                let id = thread.unwrap_or(ThreadId::MAIN);
                match self.writable(id).await {
                    Ok(assistant) => {
                        assistant.send(text).await;
                        Ok(())
                    }
                    Err(error) => refuse(socket, Some(id), None, &error).await,
                }
            }
            ClientMsg::Cancel { thread } => {
                let id = thread.unwrap_or(ThreadId::MAIN);
                match self.assistant(id).await {
                    Ok((_, assistant)) => {
                        assistant.cancel().await;
                        Ok(())
                    }
                    Err(error) => refuse(socket, Some(id), None, &error).await,
                }
            }
            ClientMsg::Answer { id, response } => {
                // A refusal is for this page alone: the request did not change.
                if let Err(refusal) = self.web.requests.answer(id, response, self.user).await {
                    let refused = ServerMsg::Refused {
                        id,
                        reason: refusal.to_string(),
                    };
                    return send(socket, &refused).await;
                }
                Ok(())
            }
            ClientMsg::Create { client_id, text } => self.create(socket, client_id, text).await,
            ClientMsg::Rename { thread, title } => {
                let renamed = async {
                    let row = self.web.threads.visible(thread, self.user).await?;
                    if row.archived_at.is_some() {
                        bail!(Refusal::Archived);
                    }
                    let title = check_title(&title).map_err(Invalid)?;
                    self.web.threads.rename(thread, &title).await
                };
                match renamed.await {
                    Ok(()) => Ok(()),
                    Err(error) => refuse(socket, Some(thread), None, &error).await,
                }
            }
            ClientMsg::Archive { thread, archived } => {
                let done = async {
                    self.web.threads.visible(thread, self.user).await?;
                    if thread == ThreadId::MAIN {
                        bail!(Refusal::Main);
                    }
                    self.web.threads.set_archived(thread, archived).await
                };
                match done.await {
                    Ok(()) => Ok(()),
                    Err(error) => refuse(socket, Some(thread), None, &error).await,
                }
            }
        }
    }

    /// Streams thread `thread` (the main one when none): the entries after `last_seq`, then the
    /// live ones; and, when the page named a thread, the thread list.
    async fn hello(
        &mut self,
        socket: &mut WebSocket,
        last_seq: u64,
        log_id: Option<String>,
        thread: Option<ThreadId>,
    ) -> Result<(), ()> {
        let id = thread.unwrap_or(ThreadId::MAIN);
        let assistant = match self.assistant(id).await {
            Ok((_, assistant)) => assistant,
            Err(error) => return refuse(socket, Some(id), None, &error).await,
        };
        // Subscribed first, so nothing falls between the replay and the stream.
        self.live = Some(assistant.log().subscribe());
        let log = assistant.log();
        let newest = log.last_seq();
        // A page that saw another log, or one ahead of this log, saw a log since deleted: it
        // gets the whole new one.
        let other_log = log_id.is_some_and(|id| id != log.log_id());
        self.last_sent = if other_log || last_seq > newest {
            0
        } else {
            last_seq
        };
        let hello = ServerMsg::Hello {
            log_id: log.log_id().to_owned(),
            last_seq: newest,
            user: Some(self.user.name.clone()),
            admin: self.user.admin,
            thread,
        };
        self.current = Some(assistant);
        send(socket, &hello).await?;
        if thread.is_some() {
            if self.list.is_none() {
                self.list = Some(self.web.threads.changes());
            }
            // Sent again at each hello: the page may have missed changes while away.
            self.shown = None;
            self.send_list(socket).await?;
        }
        self.replay(socket).await?;
        // After the log: the meter, again at each hello, and on each new reading from then on.
        if self.user.admin && self.usage.is_none() {
            self.usage = self.web.threads.usage_changes();
        }
        self.send_usage(socket).await
    }

    /// Sends the meter's usage, if the page's user gets one.
    async fn send_usage(&mut self, socket: &mut WebSocket) -> Result<(), ()> {
        match usage_message(&self.web.threads, self.user) {
            Some(message) => send(socket, &message).await,
            None => Ok(()),
        }
    }

    /// Makes the thread the page asked for with its first message, says which, and sends the
    /// message to it.
    async fn create(
        &mut self,
        socket: &mut WebSocket,
        client_id: String,
        text: String,
    ) -> Result<(), ()> {
        let created = async {
            if client_id.is_empty()
                || client_id.chars().count() > MAX_CLIENT_ID
                || !client_id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                bail!(Invalid("Identifiant de conversation invalide."));
            }
            if text.trim().is_empty() {
                bail!(Invalid("Le premier message est vide."));
            }
            let (row, fresh) = self
                .web
                .threads
                .create_for(self.user, &client_id, &text)
                .await?;
            let assistant = self
                .web
                .threads
                .assistant(row.id)
                .await?
                .ok_or(Refusal::NotFound)?;
            Ok((row.id, fresh.then_some(assistant)))
        };
        match created.await {
            Ok((thread, first)) => {
                let created = ServerMsg::Created { client_id, thread };
                send(socket, &created).await?;
                if let Some(assistant) = first {
                    assistant.send(text).await;
                }
                Ok(())
            }
            Err(error) => refuse(socket, None, Some(client_id), &error).await,
        }
    }

    /// Thread `id`'s row and assistant, if the page's user may see it.
    async fn assistant(&self, id: ThreadId) -> anyhow::Result<(ThreadRow, Arc<Assistant>)> {
        let row = self.web.threads.visible(id, self.user).await?;
        let assistant = self
            .web
            .threads
            .assistant(id)
            .await?
            .ok_or(Refusal::NotFound)?;
        Ok((row, assistant))
    }

    /// Thread `id`'s assistant, if the page's user may write to it: theirs, and not archived.
    async fn writable(&self, id: ThreadId) -> anyhow::Result<Arc<Assistant>> {
        let (row, assistant) = self.assistant(id).await?;
        if row.archived_at.is_some() {
            bail!(Refusal::Archived);
        }
        Ok(assistant)
    }

    /// Sends the entries of the thread streamed after the last one sent.
    async fn replay(&mut self, socket: &mut WebSocket) -> Result<(), ()> {
        let Some(assistant) = &self.current else {
            return Ok(());
        };
        for entry in assistant.log().since(self.last_sent) {
            self.last_sent = entry.seq;
            send(socket, &ServerMsg::Entry(entry)).await?;
        }
        Ok(())
    }

    /// Sends the thread list, unless the page has it as it is.
    async fn send_list(&mut self, socket: &mut WebSocket) -> Result<(), ()> {
        let threads = match self.web.threads.summaries(self.user).await {
            Ok(threads) => threads,
            Err(error) => {
                tracing::error!("cannot list the threads: {error:#}");
                return Ok(());
            }
        };
        if self.shown.as_ref() == Some(&threads) {
            return Ok(());
        }
        self.shown = Some(threads.clone());
        send(socket, &ServerMsg::Threads { threads }).await
    }
}

/// A message the page sent that cannot be done as it is; its text is French, for the page.
#[derive(Debug)]
struct Invalid(&'static str);

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Invalid {}

/// Tells the page its thread message was refused, and why when the page can do something about
/// it; other errors are logged and the page only told to try again.
async fn refuse(
    socket: &mut WebSocket,
    thread: Option<ThreadId>,
    client_id: Option<String>,
    error: &anyhow::Error,
) -> Result<(), ()> {
    let reason = if let Some(refusal) = error.downcast_ref::<Refusal>() {
        refusal.to_string()
    } else if let Some(invalid) = error.downcast_ref::<Invalid>() {
        invalid.to_string()
    } else {
        tracing::error!("a thread message failed: {error:#}");
        UNAVAILABLE.into()
    };
    let refused = ServerMsg::ThreadRefused {
        thread,
        client_id,
        reason,
    };
    send(socket, &refused).await
}

/// Waits for the thread list to change: true when it did, false when it never will again;
/// never resolves before a hello named a thread.
async fn next_change(list: &mut Option<watch::Receiver<u64>>) -> bool {
    match list {
        Some(list) => list.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

/// Waits for the next live entry; never resolves before the page said hello.
async fn next_live(live: &mut Option<broadcast::Receiver<Entry>>) -> Result<Entry, RecvError> {
    match live {
        Some(live) => live.recv().await,
        None => std::future::pending().await,
    }
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
