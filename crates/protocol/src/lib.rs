//! Types shared by the conversation log, the server and the frontend.

pub mod request;
pub mod text;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use request::{
    CallBy, Card, ChoiceAnswer, ChoiceOption, Field, FieldValue, Invalid, Keep, KeyPlace, Outcome,
    PermissionChoice, PermissionKind, Remember, RequestId, Response, SecretValue, TextFormat,
    ToolKind,
};

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
}

/// One thing that happened in the conversation.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    User {
        text: String,
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
    },
    /// `by`: the user who answered, when someone did.
    RequestClosed {
        id: RequestId,
        outcome: Outcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
    },
}

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
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Start streaming: the entries after `last_seq`, then the live ones. A page that names
    /// another log than the server's (`log_id` of its last [`ServerMsg::Hello`]) gets the whole
    /// log.
    Hello {
        last_seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        log_id: Option<String>,
    },
    Send {
        text: String,
    },
    Cancel,
    /// Answers the request `id`; the first answer wins.
    Answer {
        id: RequestId,
        response: Response,
    },
}

/// What the server sends over the WebSocket.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// The answer to [`ClientMsg::Hello`], before any entry. `log_id` changes when the log is
    /// started over, so a page can tell that what it shows is gone even when seqs overlap;
    /// `last_seq` is the newest entry at that moment. `user`: whom the page is signed in as, so
    /// a closed card can say who answered when it was someone else. `admin`: that user may keep
    /// a secret for good, so its card offers it.
    Hello {
        log_id: String,
        last_seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        admin: bool,
    },
    Entry(Entry),
    /// The answer to the request `id` was refused, for this page only: the request did not
    /// change. `reason` is French, for the page.
    Refused {
        id: RequestId,
        reason: String,
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
                },
            ),
            (
                r#"{"type":"send","text":"salut"}"#,
                ClientMsg::Send {
                    text: "salut".into(),
                },
            ),
            (r#"{"type":"cancel"}"#, ClientMsg::Cancel),
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
            event: Event::User { text: "hi".into() },
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
        };
        let json = serde_json::to_string(&hello).unwrap();
        assert_eq!(json, r#"{"type":"hello","log_id":"0f3c","last_seq":12}"#);
        assert_eq!(serde_json::from_str::<ServerMsg>(&json).unwrap(), hello);
        let hello = ServerMsg::Hello {
            log_id: "0f3c".into(),
            last_seq: 12,
            user: Some("alix".into()),
            admin: true,
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
}
