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
