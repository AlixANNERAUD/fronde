//! Types shared by the conversation log, the server and the frontend.

pub mod request;
pub mod text;
pub mod usage;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use request::{
    CallBy, Card, ChoiceAnswer, ChoiceOption, Field, FieldValue, Invalid, Keep, KeyPlace, Outcome,
    PermissionChoice, PermissionKind, Remember, RequestId, Response, SecretAnswer, SecretValue,
    TextFormat, ToolKind,
};

/// Why sub-agents' results wait for the owner's next message ([`Event::WakesHeld`]).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum HoldReason {
    /// `[threads] wakes_without_owner` wake turns ran since the owner's last message, a
    /// result's thread could not be checked, or a message's prompt never went out.
    #[default]
    Budget,
    /// The owner stopped the turn while results waited.
    Stopped,
    /// The owner's reserve of a subscription window is reached (`[threads] reserve`): `window`,
    /// and its use in percent when the provider gave one.
    Reserve {
        window: usage::WindowKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        used: Option<u8>,
    },
}

impl HoldReason {
    pub fn is_budget(&self) -> bool {
        *self == Self::Budget
    }
}

/// State of a tool call shown in the conversation.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ToolState {
    Running,
    Done,
    Failed,
}

/// How a turn ended.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Done,
    Cancelled,
    Refused,
    Limit,
    Error,
    /// Only in [`Event::ChildEnded`]: the owner archived the sub-agent itself. Its parent is
    /// told, never woken.
    Archived,
}

/// One thing that happened in the conversation.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A message to the thread's agent. `by`: who wrote it, when it was not the page's user (a
    /// parent thread's title for a child's task). `None` means the owner, whose message resets
    /// the wake budget: an outside client's message must never be logged so.
    User {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
    },
    /// One entry per text chunk.
    Assistant {
        text: String,
    },
    Tool {
        id: String,
        title: String,
        state: ToolState,
    },
    /// A permission request from before permissions were requests: kept so that old logs
    /// replay, never written any more. `id` is the seq of this entry.
    Permission {
        id: u64,
        title: String,
    },
    /// How a [`Event::Permission`] was answered; never written any more either.
    PermissionAnswered {
        id: u64,
        allowed: bool,
    },
    TurnEnded {
        outcome: TurnOutcome,
    },
    Notice {
        text: String,
    },
    /// A request to the user, open until its [`Event::RequestClosed`], whatever the turns do.
    /// `from` names an asker other than Fronde's assistant: an outside MCP client, or with
    /// `server` one of Fronde's upstream MCP servers.
    Request {
        id: RequestId,
        card: Card,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        server: bool,
        /// The sub-agent whose result started the turn that asks: the card says so, since that
        /// turn follows text the owner did not write.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        woken_by: Option<WokenBy>,
        /// How many sub-agents' results, held for the owner's message, that message carried
        /// to the agent in the turn that asks: the card says so, since the turn follows their
        /// text too.
        #[serde(default, skip_serializing_if = "is_zero")]
        carries: u32,
    },
    /// `by`: the user who answered, when someone did.
    RequestClosed {
        id: RequestId,
        outcome: Outcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
    },
    /// A turn of sub-agent `thread` ended and no waiting call took its result: the parent is
    /// woken with it. Logged in the parent's stream. `summary`: the sub-agent's final answer, at
    /// most [`MAX_ANSWER`] characters with its hidden characters made visible ([`text::readable`]),
    /// `truncated` when it was cut. It is the sub-agent's own text: data, never instructions.
    ///
    /// Logged while no turn runs, it opens the parent's wake turn; logged during a turn, its wake
    /// waits, and each [`Event::TurnEnded`] opens the next wake waiting, in order; past the wake
    /// budget, or after the owner's Stop, it is held instead ([`Event::WakesHeld`]). With the
    /// outcome [`TurnOutcome::Archived`] it only tells, and wakes nothing.
    ChildEnded {
        thread: ThreadId,
        title: String,
        outcome: TurnOutcome,
        summary: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
    /// The wake turn of the [`Event::ChildEnded`] entry `child_ended` gave the parent's agent
    /// its prompt: the result reached it, and is never given again, a restart included. Pages
    /// show nothing of it.
    Woken {
        child_ended: u64,
    },
    /// Sub-agents' results wait for the owner's next message instead of waking the thread:
    /// `count` of them, since the owner stopped the turn, since
    /// `[threads] wakes_without_owner` turns were woken without the owner, or since the owner's
    /// reserve of the subscription was reached (`reason`). That message carries them to the
    /// agent. Logged in the parent's stream.
    WakesHeld {
        count: u32,
        /// The message logged last never reached the agent (its agent did not start): the
        /// results it carried are held again, first.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        returned: bool,
        /// Why they are held; an entry from before reasons were kept reads as
        /// [`HoldReason::Budget`].
        #[serde(default, skip_serializing_if = "HoldReason::is_budget")]
        reason: HoldReason,
    },
    /// The owner answered one of the thread's cards once nobody waited for it, while a turn
    /// ran: the answer waits for that turn's end, then goes to the agent as the owner's
    /// message. Kept in the stream so that a restart before then still gives it, once: the
    /// owner's next [`Event::User`] entry is that message. Pages show nothing of it.
    LateAnswer {
        request: RequestId,
        /// The message, as the thread's stream shows it once given.
        text: String,
        /// For the agent only: how long a `secret_ref` lasts.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// The thread was archived while `count` [`Event::LateAnswer`] entries waited: those are
    /// never given, a restart or bringing the thread back included. Pages show nothing of it:
    /// the notice logged with it says so.
    LateDropped {
        count: u32,
    },
}

fn is_zero(count: &u32) -> bool {
    *count == 0
}

/// The sub-agent whose result woke a thread, as a card asked in that wake's turn names it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct WokenBy {
    pub thread: ThreadId,
    pub title: String,
}

/// The longest final answer of a sub-agent its parent gets at once, in characters.
pub const MAX_ANSWER: usize = 8_000;

/// A numbered, timestamped event of the conversation log.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub seq: u64,
    pub at: DateTime<Utc>,
    pub event: Event,
}

/// A thread, one conversation over a stream of its own. Not a secret: every access is checked
/// against who asks, never against knowing an id.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[serde(transparent)]
pub struct ThreadId(pub i64);

impl ThreadId {
    /// The main thread: the chat as it was before threads, over the topic `conversation`.
    pub const MAIN: Self = Self(1);
}

impl std::fmt::Display for ThreadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The close code of a chat WebSocket opened without a session: the page must sign in again.
/// Browsers show a refused upgrade only as 1006, like a network failure.
pub const CLOSE_SIGNED_OUT: u16 = 4401;

/// What the chat page sends over the WebSocket.
///
/// `thread` fields are optional: none means the main thread, so that a page that knows nothing of
/// threads talks to the main thread as before. Every thread id is checked against the page's user:
/// an unknown thread and one the user may not see are refused alike ([`ServerMsg::ThreadRefused`]).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Start streaming `thread`: the entries after `last_seq`, then the live ones. A page that
    /// names another log than the server's (`log_id` of its last [`ServerMsg::Hello`]) gets the
    /// whole log. A new hello on the same socket switches the thread it streams. A page that
    /// names a thread also gets the thread list ([`ServerMsg::Threads`]), now and on each change.
    Hello {
        last_seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        log_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<ThreadId>,
    },
    /// A message to `thread`'s agent.
    Send {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<ThreadId>,
    },
    /// Stops `thread`'s running turn.
    Cancel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<ThreadId>,
    },
    /// Answers the request `id`; the first answer wins. Request ids are global: a card of any
    /// thread is answered from any page.
    Answer { id: RequestId, response: Response },
    /// Starts a thread with its first message, so that no thread is ever empty. `client_id`
    /// names the attempt, made by the page: a second `Create` with the same id (a retry after
    /// a lost reply) gets the thread the first one made, without sending `text` again.
    Create { client_id: String, text: String },
    /// Renames `thread`: at most [`MAX_TITLE`] characters, none of them hidden.
    Rename { thread: ThreadId, title: String },
    /// Archives `thread`, or brings it back. An archived thread is read-only; the main thread is
    /// never archived.
    Archive { thread: ThreadId, archived: bool },
}

/// The longest title a page may give a thread, in characters.
pub const MAX_TITLE: usize = 80;
/// The longest title a thread gets from its first message, in characters.
pub const FIRST_TITLE: usize = 60;
/// The longest `client_id` of a [`ClientMsg::Create`], in characters.
pub const MAX_CLIENT_ID: usize = 64;

/// The title of a thread whose first message gives none.
pub const DEFAULT_TITLE: &str = "Nouvelle conversation";

/// A new thread's title, from its first message: on one line, without hidden characters, at
/// most [`FIRST_TITLE`] characters.
pub fn first_title(message: &str) -> String {
    let visible: String = message
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|&c| !text::hidden(c))
        .collect();
    let title = visible
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(FIRST_TITLE)
        .collect::<String>();
    let title = title.trim_end();
    if title.is_empty() {
        DEFAULT_TITLE.into()
    } else {
        title.into()
    }
}

/// `title` trimmed, if a page may give it to a thread: not empty, at most [`MAX_TITLE`]
/// characters, on one line, without hidden characters (controls, bidi controls, zero-width
/// characters…). Else why not, in French.
pub fn check_title(title: &str) -> Result<String, &'static str> {
    let title = title.trim();
    if title.is_empty() {
        return Err("Le titre est vide.");
    }
    if title.chars().count() > MAX_TITLE {
        return Err("Le titre est trop long (80 caractères au plus).");
    }
    if text::first_hidden(title, false).is_some() {
        return Err("Le titre contient un caractère invisible ou de contrôle.");
    }
    Ok(title.into())
}

/// Where a thread stands, computed live.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    /// No turn ran yet.
    Idle,
    /// A turn waits for a place among the agents running at once.
    Queued,
    Running,
    /// Its last turn ended well.
    Done,
    /// Its last turn ended in an error, a refusal or a limit.
    Failed,
    /// Its last turn was stopped.
    Cancelled,
}

/// A thread as the thread list shows it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct ThreadSummary {
    pub id: ThreadId,
    pub title: String,
    /// The thread that started it, for a sub-agent's thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ThreadId>,
    pub status: ThreadStatus,
    /// Requests waiting for an answer in its stream: « attend ta réponse ».
    #[serde(default)]
    pub open_requests: u32,
    /// Its last activity: its newest entry, or its last change.
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
}

/// What the server sends over the WebSocket.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// The answer to [`ClientMsg::Hello`], before any entry. `log_id` changes when the log is
    /// started over, so a page can tell that what it shows is gone even when seqs overlap;
    /// `last_seq` is the newest entry at that moment. `user`: whom the page is signed in as, so
    /// a closed card can say who answered when it was someone else. `admin`: that user may keep
    /// a secret for good, so its card offers it. `thread`: the thread streamed, when the page
    /// named one; the entries that follow are its.
    Hello {
        log_id: String,
        last_seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        admin: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<ThreadId>,
    },
    Entry(Entry),
    /// The answer to the request `id` was refused, for this page only: the request did not
    /// change. `reason` is French, for the page.
    Refused {
        id: RequestId,
        reason: String,
    },
    /// The threads the page's user may see, archived ones included, oldest first: after a hello
    /// that named a thread, and on every change (a thread made, renamed or archived, a turn
    /// started, queued or ended, a card opened or closed).
    Threads {
        threads: Vec<ThreadSummary>,
    },
    /// The thread [`ClientMsg::Create`] `client_id` made; the page says hello to it to stream it.
    Created {
        client_id: String,
        thread: ThreadId,
    },
    /// A thread message was refused, for this page only: the thread named does not exist or is
    /// not the user's (alike), is archived, or the title is not one. `thread` or `client_id`
    /// says which message. `reason` is French, for the page.
    ThreadRefused {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<ThreadId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
        reason: String,
    },
    /// How much of the page's user's own subscription accounts is used, one reading per
    /// account read so far, and the share of each window kept for each account's owner
    /// (`[threads] reserve`, in percent): after a hello, and when a reading the page is shown
    /// changes. Sent once there is a reading the page's user may see: their own accounts',
    /// and for an admin every other account's too, in `others`, each by its name.
    Usage {
        reserve: u8,
        usage: Vec<usage::AccountUsage>,
        /// Every other configured account's reading, for an admin only: empty for anyone
        /// else.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        others: Vec<usage::AccountUsage>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_entry_json_shape() {
        let at = "2026-11-16T10:00:00Z".parse().unwrap();
        let entry = Entry {
            seq: 1,
            at,
            event: Event::Assistant { text: "hi".into() },
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert_eq!(
            json,
            r#"{"seq":1,"at":"2026-11-16T10:00:00Z","event":{"type":"assistant","text":"hi"}}"#
        );
        assert_eq!(serde_json::from_str::<Entry>(&json).unwrap(), entry);
    }

    #[test]
    fn client_messages_json_shape() {
        let cases = [
            (
                r#"{"type":"hello","last_seq":3}"#,
                ClientMsg::Hello {
                    last_seq: 3,
                    log_id: None,
                    thread: None,
                },
            ),
            (
                r#"{"type":"send","text":"salut"}"#,
                ClientMsg::Send {
                    text: "salut".into(),
                    thread: None,
                },
            ),
            (r#"{"type":"cancel"}"#, ClientMsg::Cancel { thread: None }),
            (
                r#"{"type":"answer","id":7,"response":{"type":"decline"}}"#,
                ClientMsg::Answer {
                    id: 7,
                    response: Response::Decline,
                },
            ),
            (
                r#"{"type":"answer","id":8,"response":{"type":"form","values":{"q1":{"selected":["Mealie"],"other":"Paperless","note":"le soir"}}}}"#,
                ClientMsg::Answer {
                    id: 8,
                    response: Response::Form {
                        values: [(
                            "q1".to_owned(),
                            ChoiceAnswer {
                                selected: vec!["Mealie".into()],
                                other: Some("Paperless".into()),
                                note: Some("le soir".into()),
                            }
                            .into(),
                        )]
                        .into(),
                    },
                },
            ),
        ];
        for (json, message) in cases {
            assert_eq!(serde_json::from_str::<ClientMsg>(json).unwrap(), message);
            assert_eq!(serde_json::to_string(&message).unwrap(), json);
        }
    }

    #[test]
    fn server_entry_message_json_shape() {
        let entry = Entry {
            seq: 2,
            at: "2026-11-16T10:00:00Z".parse().unwrap(),
            event: Event::User {
                text: "hi".into(),
                by: None,
            },
        };
        let json = serde_json::to_string(&ServerMsg::Entry(entry.clone())).unwrap();
        assert_eq!(
            json,
            r#"{"type":"entry","seq":2,"at":"2026-11-16T10:00:00Z","event":{"type":"user","text":"hi"}}"#
        );
        assert_eq!(
            serde_json::from_str::<ServerMsg>(&json).unwrap(),
            ServerMsg::Entry(entry)
        );
    }

    #[test]
    fn permission_ids_are_numbers_both_ways() {
        let asked = Event::Permission {
            id: 7,
            title: "Lire l'agenda".into(),
        };
        let json = serde_json::to_string(&asked).unwrap();
        assert_eq!(
            json,
            r#"{"type":"permission","id":7,"title":"Lire l'agenda"}"#
        );
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), asked);
        // The page answers with the id it was shown.
        let Event::Permission { id, .. } = asked else {
            unreachable!()
        };
        let answer = ClientMsg::Answer {
            id,
            response: Response::Permission {
                choice: "allow_once".into(),
            },
        };
        let json = serde_json::to_string(&answer).unwrap();
        assert_eq!(
            json,
            r#"{"type":"answer","id":7,"response":{"type":"permission","choice":"allow_once"}}"#
        );
        assert_eq!(serde_json::from_str::<ClientMsg>(&json).unwrap(), answer);
    }

    #[test]
    fn request_ids_are_numbers_both_ways() {
        let card = Card::Form {
            message: String::new(),
            fields: Vec::new(),
            submit: None,
            truncated: false,
            replaced: false,
            during: Vec::new(),
        };
        let asked = Event::Request {
            id: 4,
            card: card.clone(),
            from: Some("Codex".into()),
            server: false,
            woken_by: None,
            carries: 0,
        };
        let json = serde_json::to_string(&asked).unwrap();
        assert_eq!(
            json,
            r#"{"type":"request","id":4,"card":{"kind":"form","fields":[]},"from":"Codex"}"#
        );
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), asked);
        // An upstream server says it is one.
        let served = Event::Request {
            id: 5,
            card,
            from: Some("github".into()),
            server: true,
            woken_by: None,
            carries: 0,
        };
        let json = serde_json::to_string(&served).unwrap();
        assert_eq!(
            json,
            r#"{"type":"request","id":5,"card":{"kind":"form","fields":[]},"from":"github","server":true}"#
        );
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), served);
        let closed = Event::RequestClosed {
            id: 4,
            outcome: Outcome::Declined,
            by: Some("alix".into()),
        };
        let json = serde_json::to_string(&closed).unwrap();
        assert_eq!(
            json,
            r#"{"type":"request_closed","id":4,"outcome":{"type":"declined"},"by":"alix"}"#
        );
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), closed);
        let refused = ServerMsg::Refused {
            id: 4,
            reason: "Cette demande est close.".into(),
        };
        let json = serde_json::to_string(&refused).unwrap();
        assert_eq!(
            json,
            r#"{"type":"refused","id":4,"reason":"Cette demande est close."}"#
        );
        assert_eq!(serde_json::from_str::<ServerMsg>(&json).unwrap(), refused);
    }

    #[test]
    fn hello_messages_json_shape() {
        let hello = ServerMsg::Hello {
            log_id: "0f3c".into(),
            last_seq: 12,
            user: None,
            admin: false,
            thread: None,
        };
        let json = serde_json::to_string(&hello).unwrap();
        assert_eq!(json, r#"{"type":"hello","log_id":"0f3c","last_seq":12}"#);
        assert_eq!(serde_json::from_str::<ServerMsg>(&json).unwrap(), hello);
        let hello = ServerMsg::Hello {
            log_id: "0f3c".into(),
            last_seq: 12,
            user: Some("alix".into()),
            admin: true,
            thread: None,
        };
        let json = serde_json::to_string(&hello).unwrap();
        assert_eq!(
            json,
            r#"{"type":"hello","log_id":"0f3c","last_seq":12,"user":"alix","admin":true}"#
        );
        assert_eq!(serde_json::from_str::<ServerMsg>(&json).unwrap(), hello);

        let hello = ClientMsg::Hello {
            last_seq: 12,
            log_id: Some("0f3c".into()),
            thread: None,
        };
        let json = serde_json::to_string(&hello).unwrap();
        assert_eq!(json, r#"{"type":"hello","last_seq":12,"log_id":"0f3c"}"#);
        assert_eq!(serde_json::from_str::<ClientMsg>(&json).unwrap(), hello);
    }

    #[test]
    fn enums_are_snake_case() {
        let event = Event::PermissionAnswered {
            id: 3,
            allowed: true,
        };
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"type":"permission_answered","id":3,"allowed":true}"#
        );
        let ended = Event::TurnEnded {
            outcome: TurnOutcome::Cancelled,
        };
        assert_eq!(
            serde_json::to_string(&ended).unwrap(),
            r#"{"type":"turn_ended","outcome":"cancelled"}"#
        );
        assert_eq!(
            serde_json::to_string(&ToolState::Running).unwrap(),
            r#""running""#
        );
    }

    #[test]
    fn thread_client_messages_json_shape() {
        let cases = [
            (
                r#"{"type":"hello","last_seq":3,"log_id":"0f3c","thread":2}"#,
                ClientMsg::Hello {
                    last_seq: 3,
                    log_id: Some("0f3c".into()),
                    thread: Some(ThreadId(2)),
                },
            ),
            (
                r#"{"type":"send","text":"salut","thread":2}"#,
                ClientMsg::Send {
                    text: "salut".into(),
                    thread: Some(ThreadId(2)),
                },
            ),
            (
                r#"{"type":"cancel","thread":2}"#,
                ClientMsg::Cancel {
                    thread: Some(ThreadId(2)),
                },
            ),
            (
                r#"{"type":"create","client_id":"c1","text":"Range le garage"}"#,
                ClientMsg::Create {
                    client_id: "c1".into(),
                    text: "Range le garage".into(),
                },
            ),
            (
                r#"{"type":"rename","thread":2,"title":"Garage"}"#,
                ClientMsg::Rename {
                    thread: ThreadId(2),
                    title: "Garage".into(),
                },
            ),
            (
                r#"{"type":"archive","thread":2,"archived":true}"#,
                ClientMsg::Archive {
                    thread: ThreadId(2),
                    archived: true,
                },
            ),
        ];
        for (json, message) in cases {
            assert_eq!(serde_json::from_str::<ClientMsg>(json).unwrap(), message);
            assert_eq!(serde_json::to_string(&message).unwrap(), json);
        }
    }

    #[test]
    fn thread_server_messages_json_shape() {
        let hello = ServerMsg::Hello {
            log_id: "0f3c".into(),
            last_seq: 12,
            user: Some("alix".into()),
            admin: true,
            thread: Some(ThreadId(2)),
        };
        let json = serde_json::to_string(&hello).unwrap();
        assert_eq!(
            json,
            r#"{"type":"hello","log_id":"0f3c","last_seq":12,"user":"alix","admin":true,"thread":2}"#
        );
        assert_eq!(serde_json::from_str::<ServerMsg>(&json).unwrap(), hello);

        let at = "2026-11-17T10:00:00Z".parse().unwrap();
        let list = ServerMsg::Threads {
            threads: vec![
                ThreadSummary {
                    id: ThreadId::MAIN,
                    title: "Fronde".into(),
                    parent: None,
                    status: ThreadStatus::Idle,
                    open_requests: 0,
                    updated_at: at,
                    archived: false,
                },
                ThreadSummary {
                    id: ThreadId(3),
                    title: "Recherche".into(),
                    parent: Some(ThreadId(2)),
                    status: ThreadStatus::Queued,
                    open_requests: 1,
                    updated_at: at,
                    archived: true,
                },
            ],
        };
        let json = serde_json::to_string(&list).unwrap();
        assert_eq!(
            json,
            r#"{"type":"threads","threads":[{"id":1,"title":"Fronde","status":"idle","open_requests":0,"updated_at":"2026-11-17T10:00:00Z"},{"id":3,"title":"Recherche","parent":2,"status":"queued","open_requests":1,"updated_at":"2026-11-17T10:00:00Z","archived":true}]}"#
        );
        assert_eq!(serde_json::from_str::<ServerMsg>(&json).unwrap(), list);

        let created = ServerMsg::Created {
            client_id: "c1".into(),
            thread: ThreadId(4),
        };
        let json = serde_json::to_string(&created).unwrap();
        assert_eq!(json, r#"{"type":"created","client_id":"c1","thread":4}"#);
        assert_eq!(serde_json::from_str::<ServerMsg>(&json).unwrap(), created);

        let refused = ServerMsg::ThreadRefused {
            thread: Some(ThreadId(9)),
            client_id: None,
            reason: "Conversation introuvable.".into(),
        };
        let json = serde_json::to_string(&refused).unwrap();
        assert_eq!(
            json,
            r#"{"type":"thread_refused","thread":9,"reason":"Conversation introuvable."}"#
        );
        assert_eq!(serde_json::from_str::<ServerMsg>(&json).unwrap(), refused);

        for (status, json) in [
            (ThreadStatus::Idle, r#""idle""#),
            (ThreadStatus::Queued, r#""queued""#),
            (ThreadStatus::Running, r#""running""#),
            (ThreadStatus::Done, r#""done""#),
            (ThreadStatus::Failed, r#""failed""#),
            (ThreadStatus::Cancelled, r#""cancelled""#),
        ] {
            assert_eq!(serde_json::to_string(&status).unwrap(), json);
        }
    }

    #[test]
    fn user_entry_names_another_writer_only() {
        let mine = Event::User {
            text: "salut".into(),
            by: None,
        };
        assert_eq!(
            serde_json::to_string(&mine).unwrap(),
            r#"{"type":"user","text":"salut"}"#
        );
        let task = Event::User {
            text: "Cherche".into(),
            by: Some("Vacances".into()),
        };
        let json = serde_json::to_string(&task).unwrap();
        assert_eq!(json, r#"{"type":"user","text":"Cherche","by":"Vacances"}"#);
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), task);
    }

    #[test]
    fn child_ended_entry_round_trips() {
        let ended = Event::ChildEnded {
            thread: ThreadId(7),
            title: "Vols".into(),
            outcome: TurnOutcome::Done,
            summary: "Trois vols.".into(),
            truncated: false,
        };
        let json = serde_json::to_string(&ended).unwrap();
        assert_eq!(
            json,
            r#"{"type":"child_ended","thread":7,"title":"Vols","outcome":"done","summary":"Trois vols."}"#
        );
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), ended);
        let woken = Event::Woken { child_ended: 4 };
        let json = serde_json::to_string(&woken).unwrap();
        assert_eq!(json, r#"{"type":"woken","child_ended":4}"#);
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), woken);
        let held = Event::WakesHeld {
            count: 2,
            returned: false,
            reason: HoldReason::Budget,
        };
        let json = serde_json::to_string(&held).unwrap();
        assert_eq!(json, r#"{"type":"wakes_held","count":2}"#);
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), held);
        let again = Event::WakesHeld {
            count: 2,
            returned: true,
            reason: HoldReason::Budget,
        };
        let json = serde_json::to_string(&again).unwrap();
        assert_eq!(json, r#"{"type":"wakes_held","count":2,"returned":true}"#);
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), again);
        let stopped = Event::WakesHeld {
            count: 1,
            returned: false,
            reason: HoldReason::Stopped,
        };
        let json = serde_json::to_string(&stopped).unwrap();
        assert_eq!(
            json,
            r#"{"type":"wakes_held","count":1,"reason":"stopped"}"#
        );
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), stopped);
        let reserve = Event::WakesHeld {
            count: 1,
            returned: false,
            reason: HoldReason::Reserve {
                window: usage::WindowKind::Session,
                used: Some(72),
            },
        };
        let json = serde_json::to_string(&reserve).unwrap();
        assert_eq!(
            json,
            r#"{"type":"wakes_held","count":1,"reason":{"reserve":{"window":"session","used":72}}}"#
        );
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), reserve);
        let late = Event::LateAnswer {
            request: 3,
            text: "Réponse".into(),
            note: None,
        };
        let json = serde_json::to_string(&late).unwrap();
        assert_eq!(
            json,
            r#"{"type":"late_answer","request":3,"text":"Réponse"}"#
        );
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), late);
        let dropped = Event::LateDropped { count: 2 };
        let json = serde_json::to_string(&dropped).unwrap();
        assert_eq!(json, r#"{"type":"late_dropped","count":2}"#);
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), dropped);
        let archived = serde_json::to_string(&TurnOutcome::Archived).unwrap();
        assert_eq!(archived, r#""archived""#);
    }

    /// An entry logged before reasons were kept reads as held for the wake budget.
    #[test]
    fn older_wakes_held_entries_still_read() {
        let older: Event = serde_json::from_str(r#"{"type":"wakes_held","count":3}"#).unwrap();
        assert_eq!(
            older,
            Event::WakesHeld {
                count: 3,
                returned: false,
                reason: HoldReason::Budget,
            }
        );
    }

    #[test]
    fn a_card_asked_in_a_wake_names_its_sub_agent() {
        let asked = Event::Request {
            id: 4,
            card: Card::Form {
                message: String::new(),
                fields: Vec::new(),
                submit: None,
                truncated: false,
                replaced: false,
                during: Vec::new(),
            },
            from: None,
            server: false,
            woken_by: Some(WokenBy {
                thread: ThreadId(7),
                title: "Vols".into(),
            }),
            carries: 0,
        };
        let json = serde_json::to_string(&asked).unwrap();
        assert!(
            json.ends_with(r#""woken_by":{"thread":7,"title":"Vols"}}"#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), asked);
        // A card of a turn whose message carried held results says how many.
        let carried = Event::Request {
            id: 5,
            card: Card::Form {
                message: String::new(),
                fields: Vec::new(),
                submit: None,
                truncated: false,
                replaced: false,
                during: Vec::new(),
            },
            from: None,
            server: false,
            woken_by: None,
            carries: 2,
        };
        let json = serde_json::to_string(&carried).unwrap();
        assert!(json.ends_with(r#""carries":2}"#), "{json}");
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), carried);
    }

    #[test]
    fn first_message_makes_a_short_visible_title() {
        assert_eq!(first_title("  Range\nle   garage  "), "Range le garage");
        assert_eq!(first_title("a\u{202E}b\u{200B}c"), "abc");
        assert_eq!(first_title(&"é".repeat(100)).chars().count(), FIRST_TITLE);
        assert_eq!(first_title(" \u{200B} "), DEFAULT_TITLE);
    }

    #[test]
    fn titles_are_checked() {
        assert_eq!(check_title("  Garage "), Ok("Garage".into()));
        assert!(check_title("").is_err());
        assert!(check_title("   ").is_err());
        assert!(check_title(&"x".repeat(MAX_TITLE + 1)).is_err());
        assert_eq!(
            check_title(&"x".repeat(MAX_TITLE)),
            Ok("x".repeat(MAX_TITLE))
        );
        for hidden in [
            "a\u{202E}b",
            "a\nb",
            "a\u{200B}b",
            "a\u{0007}b",
            "a\u{2066}b",
        ] {
            assert!(check_title(hidden).is_err(), "{hidden:?}");
        }
    }
}
