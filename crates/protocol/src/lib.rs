//! Types shared by the conversation log, the server and the frontend.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
    Permission {
        id: String,
        title: String,
    },
    PermissionAnswered {
        id: String,
        allowed: bool,
    },
    TurnEnded {
        outcome: TurnOutcome,
    },
    Notice {
        text: String,
    },
}

/// A numbered, timestamped event of the conversation log.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub seq: u64,
    pub at: DateTime<Utc>,
    pub event: Event,
}

/// The close code of a chat WebSocket opened without a session: the page must sign in again.
/// Browsers show a refused upgrade only as 1006, like a network failure.
pub const CLOSE_SIGNED_OUT: u16 = 4401;

/// What the chat page sends over the WebSocket.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Start streaming: the entries after `last_seq`, then the live ones.
    Hello {
        last_seq: u64,
    },
    Send {
        text: String,
    },
    Cancel,
    Answer {
        id: u64,
        allow: bool,
    },
}

/// What the server sends over the WebSocket.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Entry(Entry),
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
                ClientMsg::Hello { last_seq: 3 },
            ),
            (
                r#"{"type":"send","text":"salut"}"#,
                ClientMsg::Send {
                    text: "salut".into(),
                },
            ),
            (r#"{"type":"cancel"}"#, ClientMsg::Cancel),
            (
                r#"{"type":"answer","id":7,"allow":false}"#,
                ClientMsg::Answer {
                    id: 7,
                    allow: false,
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
    fn enums_are_snake_case() {
        let event = Event::PermissionAnswered {
            id: "p".into(),
            allowed: true,
        };
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"type":"permission_answered","id":"p","allowed":true}"#
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
