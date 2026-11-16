//! What the chat shows, built from the conversation log's entries. Pure logic, without browser
//! APIs, so it is tested natively.

use jarvis_protocol::{Entry, Event, ToolState, TurnOutcome};

/// How a tool call ended, as the chat shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Done,
    Failed,
    /// The turn ended before the tool did.
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionStatus {
    /// Waiting for an answer: only while its turn runs.
    Open,
    Answered {
        allowed: bool,
    },
    /// Its turn ended without an answer.
    Expired,
}

/// One line or bubble of the chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    User {
        text: String,
    },
    /// Consecutive text chunks of a turn, merged.
    Assistant {
        text: String,
    },
    Tool {
        id: String,
        title: String,
        status: ToolStatus,
    },
    Permission {
        id: u64,
        title: String,
        status: PermissionStatus,
    },
    Notice {
        text: String,
    },
}

/// An item with the seq of its first entry and a version that changes each time the item does,
/// so the page re-renders only changed rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Which log the row comes from: seqs start again after a reset.
    pub generation: u32,
    pub seq: u64,
    pub version: u32,
    pub item: Item,
}

impl Row {
    /// Identifies the row as drawn: a new key means a new element.
    pub fn key(&self) -> (u32, u64, u32) {
        (self.generation, self.seq, self.version)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Chat {
    rows: Vec<Row>,
    last_seq: u64,
    /// A turn runs: from a user message to its `turn_ended`.
    running: bool,
    /// The next entry is the first the server sends after a hello.
    replaying: bool,
    /// Counts log resets.
    generation: u32,
}

impl Chat {
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    pub fn running(&self) -> bool {
        self.running
    }

    /// The seq to send in a hello; the entries that follow are the server's replay.
    pub fn hello(&mut self) -> u64 {
        self.replaying = true;
        self.last_seq
    }

    /// Adds an entry the server sent; one seen already is ignored.
    pub fn apply(&mut self, entry: Entry) {
        // After a hello the server only sends newer entries, unless its log restarted from 1
        // (it was deleted): then what the page shows is gone too.
        if std::mem::take(&mut self.replaying) && entry.seq <= self.last_seq {
            *self = Self {
                generation: self.generation + 1,
                ..Self::default()
            };
        }
        if entry.seq <= self.last_seq {
            return;
        }
        self.last_seq = entry.seq;
        let seq = entry.seq;
        match entry.event {
            Event::User { text } => {
                self.running = true;
                self.push(seq, Item::User { text });
            }
            Event::Assistant { text } => {
                let running = self.running;
                match self.rows.last_mut() {
                    Some(Row {
                        version,
                        item: Item::Assistant { text: bubble },
                        ..
                    }) if running => {
                        bubble.push_str(&text);
                        *version += 1;
                    }
                    _ => self.push(seq, Item::Assistant { text }),
                }
            }
            Event::Tool { id, title, state } => {
                let status = match state {
                    ToolState::Running => ToolStatus::Running,
                    ToolState::Done => ToolStatus::Done,
                    ToolState::Failed => ToolStatus::Failed,
                };
                let call = self.current_turn().find(|row| {
                    matches!(&row.item, Item::Tool { id: call, status: ToolStatus::Running, .. } if *call == id)
                });
                match call {
                    Some(row) if status != ToolStatus::Running => {
                        set_tool_status(row, status);
                    }
                    _ => self.push(seq, Item::Tool { id, title, status }),
                }
            }
            Event::Permission { id, title } => {
                let status = if self.running {
                    PermissionStatus::Open
                } else {
                    PermissionStatus::Expired
                };
                self.push(seq, Item::Permission { id, title, status });
            }
            Event::PermissionAnswered { id, allowed } => {
                let request = self.current_turn().find(|row| {
                    matches!(&row.item, Item::Permission { id: request, status: PermissionStatus::Open, .. } if *request == id)
                });
                if let Some(row) = request {
                    set_permission_status(row, PermissionStatus::Answered { allowed });
                }
            }
            Event::TurnEnded { outcome } => {
                self.running = false;
                for row in self.current_turn() {
                    set_tool_status(row, ToolStatus::Interrupted);
                    set_permission_status(row, PermissionStatus::Expired);
                }
                let notice = match outcome {
                    TurnOutcome::Cancelled => Some("Réponse interrompue."),
                    TurnOutcome::Refused => Some("Jarvis a refusé de répondre."),
                    TurnOutcome::Limit => Some("Limite d'utilisation atteinte."),
                    // An error is already explained by the notice logged before.
                    TurnOutcome::Done | TurnOutcome::Error => None,
                };
                if let Some(text) = notice {
                    self.push(seq, Item::Notice { text: text.into() });
                }
            }
            Event::Notice { text } => self.push(seq, Item::Notice { text }),
        }
    }

    fn push(&mut self, seq: u64, item: Item) {
        self.rows.push(Row {
            generation: self.generation,
            seq,
            version: 0,
            item,
        });
    }

    /// The rows of the last turn, newest first.
    fn current_turn(&mut self) -> impl Iterator<Item = &mut Row> {
        self.rows
            .iter_mut()
            .rev()
            .take_while(|row| !matches!(row.item, Item::User { .. }))
    }
}

/// Ends a running tool call; any other row is left alone.
fn set_tool_status(row: &mut Row, new: ToolStatus) {
    if let Item::Tool { status, .. } = &mut row.item
        && *status == ToolStatus::Running
    {
        *status = new;
        row.version += 1;
    }
}

/// Closes an open permission request; any other row is left alone.
fn set_permission_status(row: &mut Row, new: PermissionStatus) {
    if let Item::Permission { status, .. } = &mut row.item
        && *status == PermissionStatus::Open
    {
        *status = new;
        row.version += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seq: u64, event: Event) -> Entry {
        Entry {
            seq,
            at: "2026-11-16T10:00:00Z".parse().unwrap(),
            event,
        }
    }

    fn user(seq: u64, text: &str) -> Entry {
        entry(seq, Event::User { text: text.into() })
    }

    fn assistant(seq: u64, text: &str) -> Entry {
        entry(seq, Event::Assistant { text: text.into() })
    }

    fn ended(seq: u64, outcome: TurnOutcome) -> Entry {
        entry(seq, Event::TurnEnded { outcome })
    }

    fn items(chat: &Chat) -> Vec<Item> {
        chat.rows().iter().map(|row| row.item.clone()).collect()
    }

    fn apply_all(chat: &mut Chat, entries: impl IntoIterator<Item = Entry>) {
        for entry in entries {
            chat.apply(entry);
        }
    }

    #[test]
    fn merges_chunks_into_one_bubble() {
        let mut chat = Chat::default();
        apply_all(
            &mut chat,
            [
                user(1, "Salut"),
                assistant(2, "Bonjour "),
                assistant(3, "Alix !"),
            ],
        );
        let version = chat.rows()[1].version;
        assert_eq!(
            items(&chat),
            [
                Item::User {
                    text: "Salut".into()
                },
                Item::Assistant {
                    text: "Bonjour Alix !".into()
                },
            ]
        );
        assert_eq!(chat.rows()[1].seq, 2);

        // A tool call between two chunks starts a new bubble; a chunk changes its row's version.
        apply_all(
            &mut chat,
            [
                assistant(4, " Je regarde."),
                entry(
                    5,
                    Event::Tool {
                        id: "t".into(),
                        title: "Consulte l'agenda".into(),
                        state: ToolState::Running,
                    },
                ),
                assistant(6, "Rien demain."),
            ],
        );
        assert_ne!(chat.rows()[1].version, version);
        assert_eq!(
            items(&chat)[1..],
            [
                Item::Assistant {
                    text: "Bonjour Alix ! Je regarde.".into()
                },
                Item::Tool {
                    id: "t".into(),
                    title: "Consulte l'agenda".into(),
                    status: ToolStatus::Running,
                },
                Item::Assistant {
                    text: "Rien demain.".into()
                },
            ]
        );

        // The next turn's answer is a bubble of its own.
        apply_all(
            &mut chat,
            [
                ended(7, TurnOutcome::Done),
                user(8, "Merci"),
                assistant(9, "De rien."),
            ],
        );
        assert_eq!(chat.rows().len(), 6);
    }

    #[test]
    fn ignores_duplicate_seq() {
        let mut chat = Chat::default();
        apply_all(&mut chat, [user(1, "Salut"), assistant(2, "Bonjour")]);
        // Replayed after a reconnection: already seen.
        apply_all(&mut chat, [user(1, "Salut"), assistant(2, "Bonjour")]);
        assert_eq!(chat.last_seq(), 2);
        assert_eq!(
            items(&chat),
            [
                Item::User {
                    text: "Salut".into()
                },
                Item::Assistant {
                    text: "Bonjour".into()
                },
            ]
        );
    }

    #[test]
    fn running_until_turn_ended() {
        let mut chat = Chat::default();
        assert!(!chat.running());
        chat.apply(user(1, "Salut"));
        assert!(chat.running());
        chat.apply(assistant(2, "Bonjour"));
        assert!(chat.running());
        chat.apply(ended(3, TurnOutcome::Done));
        assert!(!chat.running());
        // A message refused because a turn runs is a notice, not a new turn.
        chat.apply(entry(
            4,
            Event::Notice {
                text: "Occupé.".into(),
            },
        ));
        assert!(!chat.running());
    }

    #[test]
    fn tool_line_follows_its_call() {
        let mut chat = Chat::default();
        let tool = |seq, id: &str, state| {
            entry(
                seq,
                Event::Tool {
                    id: id.into(),
                    title: if state == ToolState::Running {
                        format!("Outil {id}")
                    } else {
                        String::new()
                    },
                    state,
                },
            )
        };
        apply_all(
            &mut chat,
            [
                user(1, "Go"),
                tool(2, "a", ToolState::Running),
                tool(3, "b", ToolState::Running),
                tool(4, "a", ToolState::Done),
                tool(5, "b", ToolState::Failed),
                tool(6, "c", ToolState::Running),
                ended(7, TurnOutcome::Cancelled),
            ],
        );
        assert_eq!(
            items(&chat)[1..],
            [
                Item::Tool {
                    id: "a".into(),
                    title: "Outil a".into(),
                    status: ToolStatus::Done,
                },
                Item::Tool {
                    id: "b".into(),
                    title: "Outil b".into(),
                    status: ToolStatus::Failed,
                },
                Item::Tool {
                    id: "c".into(),
                    title: "Outil c".into(),
                    status: ToolStatus::Interrupted,
                },
                Item::Notice {
                    text: "Réponse interrompue.".into()
                },
            ]
        );
    }

    #[test]
    fn permission_is_open_only_while_its_turn_runs() {
        let mut chat = Chat::default();
        let asked = |seq, id: u64| {
            entry(
                seq,
                Event::Permission {
                    id,
                    title: "Écrire dans la mémoire".into(),
                },
            )
        };
        let status = |chat: &Chat, index: usize| match &chat.rows()[index].item {
            Item::Permission { status, .. } => *status,
            other => panic!("not a permission: {other:?}"),
        };
        apply_all(&mut chat, [user(1, "Go"), asked(2, 1), asked(3, 2)]);
        assert_eq!(status(&chat, 1), PermissionStatus::Open);
        chat.apply(entry(
            4,
            Event::PermissionAnswered {
                id: 1,
                allowed: true,
            },
        ));
        assert_eq!(
            status(&chat, 1),
            PermissionStatus::Answered { allowed: true }
        );
        assert_eq!(status(&chat, 2), PermissionStatus::Open);
        chat.apply(ended(5, TurnOutcome::Done));
        assert_eq!(status(&chat, 2), PermissionStatus::Expired);

        // Ids start again with a new agent connection: the new request is the one answered.
        apply_all(
            &mut chat,
            [
                user(6, "Encore"),
                asked(7, 1),
                entry(
                    8,
                    Event::PermissionAnswered {
                        id: 1,
                        allowed: false,
                    },
                ),
            ],
        );
        assert_eq!(
            status(&chat, 1),
            PermissionStatus::Answered { allowed: true }
        );
        assert_eq!(
            status(&chat, 4),
            PermissionStatus::Answered { allowed: false }
        );
    }

    #[test]
    fn starts_over_when_the_log_was_reset() {
        let mut chat = Chat::default();
        apply_all(
            &mut chat,
            [
                user(1, "Un"),
                assistant(2, "A"),
                ended(3, TurnOutcome::Done),
            ],
        );
        assert_eq!(chat.hello(), 3);
        // The server replays a log that restarted at 1: what the page had is gone.
        apply_all(&mut chat, [user(1, "Deux"), assistant(2, "B")]);
        assert_eq!(chat.last_seq(), 2);
        assert!(chat.running());
        assert_eq!(
            items(&chat),
            [
                Item::User {
                    text: "Deux".into()
                },
                Item::Assistant { text: "B".into() },
            ]
        );

        // The new rows get keys of their own: the page drops the old rows instead of keeping
        // them under a reused seq.
        let mut before = Chat::default();
        apply_all(&mut before, [user(1, "Un")]);
        assert_ne!(before.rows()[0].key(), chat.rows()[0].key());
        assert_eq!(chat.rows()[0].seq, before.rows()[0].seq);

        // Without a reset, the first entry after hello is a new one.
        assert_eq!(chat.hello(), 2);
        chat.apply(assistant(3, "C"));
        apply_all(&mut chat, [assistant(2, "B")]);
        assert_eq!(chat.rows().len(), 2);
    }

    #[test]
    fn turn_endings_say_why() {
        let mut chat = Chat::default();
        apply_all(
            &mut chat,
            [
                user(1, "a"),
                ended(2, TurnOutcome::Refused),
                user(3, "b"),
                ended(4, TurnOutcome::Limit),
                user(5, "c"),
                ended(6, TurnOutcome::Done),
                user(7, "d"),
                ended(8, TurnOutcome::Error),
            ],
        );
        let notices: Vec<Item> = items(&chat)
            .into_iter()
            .filter(|item| matches!(item, Item::Notice { .. }))
            .collect();
        assert_eq!(
            notices,
            [
                Item::Notice {
                    text: "Jarvis a refusé de répondre.".into()
                },
                Item::Notice {
                    text: "Limite d'utilisation atteinte.".into()
                },
            ]
        );
    }
}
