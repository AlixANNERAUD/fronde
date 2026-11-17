//! What the chat shows, built from the conversation log's entries. Pure logic, without browser
//! APIs, so it is tested natively.

use fronde_protocol::{
    Card, Entry, Event, Outcome, RequestId, ServerMsg, ThreadId, ToolState, TurnOutcome, WokenBy,
};

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

/// Where a request to the user stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestStatus {
    /// Answerable, until its end is logged: a turn ending does not close it.
    Open,
    Closed(Outcome),
}

/// One line or bubble of the chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    User {
        text: String,
        /// Who wrote it, when it was not the page's user: a parent thread for a child's task.
        by: Option<String>,
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
    Request {
        id: RequestId,
        /// Boxed: a card is far bigger than any other item.
        card: Box<Card>,
        /// Who asks, when it is not Fronde's assistant.
        from: Option<String>,
        /// `from` is one of Fronde's upstream MCP servers, not a client.
        server: bool,
        /// The sub-agent whose result started the turn that asks: the card says so.
        woken_by: Option<WokenBy>,
        /// How many sub-agents' results the owner's message carried in the turn that asks:
        /// the card says so ([`carries_line`]).
        carries: u32,
        status: RequestStatus,
        /// Why the server refused this page's last answer, while the request is open.
        refusal: Option<String>,
        /// Who answered, once closed, when it was not this page's user.
        by: Option<String>,
    },
    /// A sub-agent's turn ended and its result woke this thread: a link to the sub-agent's
    /// thread. Its answer is the agent's to read, not shown here.
    ChildEnded {
        thread: ThreadId,
        title: String,
        outcome: TurnOutcome,
    },
}

/// An item with the seq of its first entry and a version that changes each time the item does.
/// The page keys rows by [`Row::key`], which ignores the version, so a changed row re-renders
/// in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Which log the row comes from: seqs start again after a reset.
    pub generation: u32,
    pub seq: u64,
    pub version: u32,
    pub item: Item,
}

impl Row {
    /// Identifies the row's element: it survives the row's changes (`version`) and differs after a
    /// log reset, where seqs start again.
    pub fn key(&self) -> (u32, u64) {
        (self.generation, self.seq)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Chat {
    rows: Vec<Row>,
    last_seq: u64,
    /// A turn runs: from a user message, or a sub-agent's result logged while none ran, to its
    /// `turn_ended`.
    running: bool,
    /// Sub-agents' results logged during a turn: each `turn_ended` opens the next one's turn,
    /// as the server runs them.
    wakes: u32,
    /// The running turn is a wake whose prompt has not gone out: held results close it.
    unwoken: bool,
    /// The log the entries come from, as the server's last hello named it.
    log_id: Option<String>,
    /// Counts log resets.
    generation: u32,
    /// Whom the page is signed in as, from the server's hello.
    viewer: Option<String>,
    /// That user is an admin, who may keep a secret for good.
    admin: bool,
    /// The thread whose log this is, to name in the hello; `None` for the main thread of a page
    /// that knows nothing of threads.
    thread: Option<ThreadId>,
}

impl Chat {
    /// The chat of thread `thread`, empty until its hello.
    pub fn for_thread(thread: ThreadId) -> Self {
        Self {
            thread: Some(thread),
            ..Self::default()
        }
    }

    /// The thread whose log this is, as the hello names it.
    pub fn thread(&self) -> Option<ThreadId> {
        self.thread
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    pub fn running(&self) -> bool {
        self.running
    }

    /// Whether the page's user is an admin, as the server's hello said.
    pub fn admin(&self) -> bool {
        self.admin
    }

    /// The log the rows come from, to name in the next hello.
    pub fn log_id(&self) -> Option<&str> {
        self.log_id.as_deref()
    }

    /// Takes in what the server sent.
    pub fn apply(&mut self, msg: &ServerMsg) {
        match msg {
            ServerMsg::Hello {
                log_id,
                last_seq,
                user,
                admin,
                thread: _,
            } => {
                // Another log, or one shorter than what the page shows (deleted and started
                // again): what the page shows is gone, the server replays the whole new log.
                if self.log_id.as_deref().is_some_and(|known| known != log_id)
                    || *last_seq < self.last_seq
                {
                    *self = Self {
                        generation: self.generation + 1,
                        thread: self.thread,
                        ..Self::default()
                    };
                }
                self.log_id = Some(log_id.clone());
                self.viewer = user.clone();
                self.admin = *admin;
            }
            ServerMsg::Entry(entry) => self.apply_entry(entry.clone()),
            ServerMsg::Refused { id, reason } => {
                if let Some(row) = self.open_request(*id) {
                    let Item::Request { refusal, .. } = &mut row.item else {
                        unreachable!("an open request row");
                    };
                    *refusal = Some(reason.clone());
                    row.version += 1;
                }
            }
            // The thread list's messages: see [`crate::Threads`].
            ServerMsg::Threads { .. }
            | ServerMsg::Created { .. }
            | ServerMsg::ThreadRefused { .. } => {}
        }
    }

    /// The requests still open, oldest first.
    pub fn open_requests(&self) -> impl Iterator<Item = &Row> {
        self.rows.iter().filter(|row| {
            matches!(
                row.item,
                Item::Request {
                    status: RequestStatus::Open,
                    ..
                }
            )
        })
    }

    fn open_request(&mut self, id: RequestId) -> Option<&mut Row> {
        self.rows.iter_mut().rev().find(|row| {
            matches!(&row.item, Item::Request { id: open, status: RequestStatus::Open, .. } if *open == id)
        })
    }

    /// Adds an entry the server sent; one seen already is ignored.
    fn apply_entry(&mut self, entry: Entry) {
        if entry.seq <= self.last_seq {
            return;
        }
        self.last_seq = entry.seq;
        let seq = entry.seq;
        match entry.event {
            Event::User { text, by } => {
                self.running = true;
                self.unwoken = false;
                self.push(seq, Item::User { text, by });
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
            Event::ChildEnded {
                thread,
                title,
                outcome,
                ..
            } => {
                // An archived sub-agent's entry only tells; held results wake nothing, and the
                // `wakes_held` entry that follows says so.
                if outcome != TurnOutcome::Archived {
                    if self.running {
                        self.wakes += 1;
                    } else {
                        self.running = true;
                        self.unwoken = true;
                    }
                }
                self.push(
                    seq,
                    Item::ChildEnded {
                        thread,
                        title,
                        outcome,
                    },
                );
            }
            // The wake's prompt went out: nothing to show.
            Event::Woken { .. } => self.unwoken = false,
            // A late answer waiting for the turn's end: the notice logged with it says so, and
            // the owner's message that gives it shows it.
            Event::LateAnswer { .. } => {}
            Event::WakesHeld { count, .. } => {
                // Every result waiting is held, a wake turn that never gave its prompt included.
                self.wakes = 0;
                if self.unwoken {
                    self.unwoken = false;
                    self.running = false;
                }
                self.push(
                    seq,
                    Item::Notice {
                        text: held_notice(count),
                    },
                );
            }
            Event::TurnEnded { outcome } => {
                self.running = false;
                self.unwoken = false;
                for row in self.current_turn() {
                    set_tool_status(row, ToolStatus::Interrupted);
                    set_permission_status(row, PermissionStatus::Expired);
                }
                let notice = match outcome {
                    TurnOutcome::Cancelled => Some("Réponse interrompue."),
                    TurnOutcome::Refused => Some("Fronde a refusé de répondre."),
                    TurnOutcome::Limit => Some("Limite d'utilisation atteinte."),
                    // An error is already explained by the notice logged before.
                    TurnOutcome::Done | TurnOutcome::Error | TurnOutcome::Archived => None,
                };
                if let Some(text) = notice {
                    self.push(seq, Item::Notice { text: text.into() });
                }
                if self.wakes > 0 {
                    self.wakes -= 1;
                    self.running = true;
                    self.unwoken = true;
                }
            }
            Event::Notice { text } => self.push(seq, Item::Notice { text }),
            Event::Request {
                id,
                card,
                from,
                server,
                woken_by,
                carries,
            } => self.push(
                seq,
                Item::Request {
                    id,
                    card: Box::new(card),
                    from,
                    server,
                    woken_by,
                    carries,
                    status: RequestStatus::Open,
                    refusal: None,
                    by: None,
                },
            ),
            Event::RequestClosed { id, outcome, by } => {
                let someone_else = by.filter(|by| self.viewer.as_ref() != Some(by));
                if let Some(row) = self.open_request(id) {
                    let Item::Request {
                        status,
                        refusal,
                        by,
                        ..
                    } = &mut row.item
                    else {
                        unreachable!("an open request row");
                    };
                    *status = RequestStatus::Closed(outcome);
                    *refusal = None;
                    *by = someone_else;
                    row.version += 1;
                }
            }
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
/// What a card says above it when the owner's message carried `count` sub-agents' results to
/// the agent in the turn that asks: that turn follows their text too.
pub fn carries_line(count: u32) -> String {
    if count == 1 {
        "Ce tour transmet 1 résultat de sous-agent".to_owned()
    } else {
        format!("Ce tour transmet {count} résultats de sous-agents")
    }
}

/// What the chat says when `count` sub-agents' results wait for the owner's next message.
pub fn held_notice(count: u32) -> String {
    if count == 1 {
        "1 résultat de sous-agent en attente — envoie un message pour le recevoir".to_owned()
    } else {
        format!("{count} résultats de sous-agents en attente — envoie un message pour les recevoir")
    }
}

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
        entry(
            seq,
            Event::User {
                text: text.into(),
                by: None,
            },
        )
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
            chat.apply(&ServerMsg::Entry(entry));
        }
    }

    fn hello(log_id: &str, last_seq: u64) -> ServerMsg {
        ServerMsg::Hello {
            log_id: log_id.into(),
            last_seq,
            user: None,
            admin: false,
            thread: None,
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
        let key = chat.rows()[1].key();
        assert_eq!(
            items(&chat),
            [
                Item::User {
                    text: "Salut".into(),
                    by: None,
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
        // The key stays, so the page keeps the bubble's element while the reply streams.
        assert_eq!(chat.rows()[1].key(), key);
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
                    text: "Salut".into(),
                    by: None,
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
        apply_all(&mut chat, [user(1, "Salut")]);
        assert!(chat.running());
        apply_all(&mut chat, [assistant(2, "Bonjour")]);
        assert!(chat.running());
        apply_all(&mut chat, [ended(3, TurnOutcome::Done)]);
        assert!(!chat.running());
        // A message refused because a turn runs is a notice, not a new turn.
        apply_all(
            &mut chat,
            [entry(
                4,
                Event::Notice {
                    text: "Occupé.".into(),
                },
            )],
        );
        assert!(!chat.running());
    }

    fn child_ended(seq: u64, thread: i64) -> Entry {
        entry(
            seq,
            Event::ChildEnded {
                thread: ThreadId(thread),
                title: format!("Sous-tâche {thread}"),
                outcome: TurnOutcome::Done,
                summary: "Fini.".into(),
                truncated: false,
            },
        )
    }

    /// Held results open no turn, and the chat says how many wait; an archived sub-agent's
    /// entry only tells.
    #[test]
    fn held_results_and_an_archived_sub_agent_open_no_turn() {
        let mut chat = Chat::default();
        apply_all(
            &mut chat,
            [
                child_ended(1, 7),
                entry(
                    2,
                    Event::WakesHeld {
                        count: 1,
                        returned: false,
                    },
                ),
            ],
        );
        assert!(!chat.running(), "held, not woken");
        // Results waiting for a stopped turn are held at its end.
        apply_all(
            &mut chat,
            [
                user(3, "Va"),
                child_ended(4, 8),
                entry(
                    5,
                    Event::WakesHeld {
                        count: 2,
                        returned: false,
                    },
                ),
                ended(6, TurnOutcome::Cancelled),
            ],
        );
        assert!(!chat.running());
        let archived = entry(
            7,
            Event::ChildEnded {
                thread: ThreadId(9),
                title: "Archivé".into(),
                outcome: TurnOutcome::Archived,
                summary: String::new(),
                truncated: false,
            },
        );
        apply_all(&mut chat, [archived]);
        assert!(!chat.running());
        let notices: Vec<Item> = items(&chat)
            .into_iter()
            .filter(|item| matches!(item, Item::Notice { .. }))
            .collect();
        assert_eq!(
            notices,
            [
                Item::Notice {
                    text: "1 résultat de sous-agent en attente — envoie un message pour le recevoir"
                        .into()
                },
                Item::Notice {
                    text: "2 résultats de sous-agents en attente — envoie un message pour les recevoir"
                        .into()
                },
                Item::Notice {
                    text: "Réponse interrompue.".into()
                },
            ]
        );
    }

    #[test]
    fn a_sub_agents_result_shows_and_runs_its_wake_turns() {
        let mut chat = Chat::default();
        // While idle, a result opens its wake turn: its answer joins one bubble.
        apply_all(
            &mut chat,
            [
                child_ended(1, 7),
                entry(2, Event::Woken { child_ended: 1 }),
                assistant(3, "Le sous-agent "),
                assistant(4, "a fini."),
            ],
        );
        assert!(chat.running());
        apply_all(&mut chat, [ended(5, TurnOutcome::Done)]);
        assert!(!chat.running());
        // During a turn, two results wait: each end opens the next one's turn.
        apply_all(
            &mut chat,
            [user(6, "Salut"), child_ended(7, 8), child_ended(8, 9)],
        );
        apply_all(&mut chat, [ended(9, TurnOutcome::Done)]);
        assert!(chat.running(), "the first wake runs");
        apply_all(&mut chat, [ended(10, TurnOutcome::Cancelled)]);
        assert!(chat.running(), "then the second");
        apply_all(&mut chat, [ended(11, TurnOutcome::Done)]);
        assert!(!chat.running());
        let items = items(&chat);
        assert_eq!(
            items[..2],
            [
                Item::ChildEnded {
                    thread: ThreadId(7),
                    title: "Sous-tâche 7".into(),
                    outcome: TurnOutcome::Done,
                },
                Item::Assistant {
                    text: "Le sous-agent a fini.".into()
                },
            ]
        );
        assert_eq!(
            items
                .iter()
                .filter(|item| matches!(item, Item::ChildEnded { .. }))
                .count(),
            3
        );
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
        apply_all(
            &mut chat,
            [entry(
                4,
                Event::PermissionAnswered {
                    id: 1,
                    allowed: true,
                },
            )],
        );
        assert_eq!(
            status(&chat, 1),
            PermissionStatus::Answered { allowed: true }
        );
        assert_eq!(status(&chat, 2), PermissionStatus::Open);
        apply_all(&mut chat, [ended(5, TurnOutcome::Done)]);
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

    fn question(seq: u64, id: RequestId) -> Entry {
        let card = serde_json::from_str(
            r#"{"kind":"form","fields":[{"type":"choice","key":"q1","question":"Laquelle ?","options":[{"label":"Mealie"},{"label":"Seerr"}],"other":true,"required":true}]}"#,
        )
        .unwrap();
        entry(
            seq,
            Event::Request {
                id,
                card,
                from: None,
                server: false,
                woken_by: None,
                carries: 0,
            },
        )
    }

    fn request_status(chat: &Chat, index: usize) -> (RequestStatus, Option<String>) {
        match &chat.rows()[index].item {
            Item::Request {
                status, refusal, ..
            } => (status.clone(), refusal.clone()),
            other => panic!("not a request: {other:?}"),
        }
    }

    #[test]
    fn secret_request_closes_saved_and_admin_comes_from_hello() {
        let mut chat = Chat::default();
        chat.apply(&ServerMsg::Hello {
            log_id: "a".into(),
            last_seq: 0,
            user: Some("alix".into()),
            admin: true,
            thread: None,
        });
        assert!(chat.admin());
        let card = Card::Secret {
            label: "Mot de passe".into(),
            reason: "Pour l'agenda.".into(),
            placeholder: None,
            url: None,
            remember: None,
        };
        apply_all(
            &mut chat,
            [
                user(1, "Go"),
                entry(
                    2,
                    Event::Request {
                        id: 9,
                        card,
                        from: None,
                        server: false,
                        woken_by: None,
                        carries: 0,
                    },
                ),
            ],
        );
        assert_eq!(chat.open_requests().count(), 1);
        let saved = Outcome::Saved {
            kept: fronde_protocol::Keep::Conversation,
            new_key: None,
        };
        apply_all(
            &mut chat,
            [entry(
                3,
                Event::RequestClosed {
                    id: 9,
                    outcome: saved.clone(),
                    by: Some("alix".into()),
                },
            )],
        );
        assert_eq!(
            request_status(&chat, 1),
            (RequestStatus::Closed(saved), None)
        );
        // Another hello says who the page is now.
        chat.apply(&hello("a", 3));
        assert!(!chat.admin());
    }

    #[test]
    fn request_open_until_closed() {
        let mut chat = Chat::default();
        apply_all(&mut chat, [user(1, "Go"), question(2, 7)]);
        assert_eq!(request_status(&chat, 1), (RequestStatus::Open, None));
        // The turn ending does not close it: the server does, with its own entry.
        apply_all(&mut chat, [ended(3, TurnOutcome::Done)]);
        assert_eq!(request_status(&chat, 1), (RequestStatus::Open, None));

        // A refused answer shows on the card, which stays open.
        let version = chat.rows()[1].version;
        chat.apply(&ServerMsg::Refused {
            id: 7,
            reason: "Réponds à cette question.".into(),
        });
        assert_eq!(
            request_status(&chat, 1),
            (
                RequestStatus::Open,
                Some("Réponds à cette question.".into())
            )
        );
        assert_ne!(chat.rows()[1].version, version);
        // A refusal for another request changes nothing.
        let before = chat.clone();
        chat.apply(&ServerMsg::Refused {
            id: 8,
            reason: "x".into(),
        });
        assert_eq!(chat, before);

        let closed = |seq, id, outcome| {
            entry(
                seq,
                Event::RequestClosed {
                    id,
                    outcome,
                    by: Some("alix".into()),
                },
            )
        };
        let answered = Outcome::Answered {
            summary: "Seerr".into(),
        };
        apply_all(&mut chat, [closed(4, 7, answered.clone())]);
        assert_eq!(
            request_status(&chat, 1),
            (RequestStatus::Closed(answered.clone()), None)
        );
        // Closed once: a later close of the same id changes nothing.
        apply_all(&mut chat, [closed(5, 7, Outcome::Expired)]);
        assert_eq!(
            request_status(&chat, 1),
            (RequestStatus::Closed(answered), None)
        );
    }

    #[test]
    fn closed_request_names_who_answered_unless_the_viewer() {
        let answered_by = |chat: &Chat, index: usize| match &chat.rows()[index].item {
            Item::Request { by, .. } => by.clone(),
            other => panic!("not a request: {other:?}"),
        };
        let closed = |seq, id, by: &str| {
            entry(
                seq,
                Event::RequestClosed {
                    id,
                    outcome: Outcome::Declined,
                    by: Some(by.into()),
                },
            )
        };
        let mut chat = Chat::default();
        chat.apply(&ServerMsg::Hello {
            log_id: "a".into(),
            last_seq: 0,
            user: Some("alix".into()),
            admin: true,
            thread: None,
        });
        assert!(chat.admin());
        apply_all(
            &mut chat,
            [
                question(1, 1),
                question(2, 2),
                closed(3, 1, "alix"),
                closed(4, 2, "sam"),
            ],
        );
        assert_eq!(answered_by(&chat, 0), None, "the viewer answered");
        assert_eq!(answered_by(&chat, 1), Some("sam".into()));
    }

    #[test]
    fn open_requests_lists_pending() {
        let mut chat = Chat::default();
        apply_all(
            &mut chat,
            [
                user(1, "Go"),
                question(2, 1),
                question(3, 2),
                question(4, 3),
                entry(
                    5,
                    Event::RequestClosed {
                        id: 2,
                        outcome: Outcome::Declined,
                        by: None,
                    },
                ),
            ],
        );
        let open: Vec<RequestId> = chat
            .open_requests()
            .map(|row| match row.item {
                Item::Request { id, .. } => id,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(open, [1, 3]);
    }

    #[test]
    fn log_id_change_starts_over() {
        let mut chat = Chat::default();
        chat.apply(&hello("a", 3));
        apply_all(
            &mut chat,
            [
                user(1, "Un"),
                assistant(2, "A"),
                ended(3, TurnOutcome::Done),
            ],
        );
        assert_eq!(chat.log_id(), Some("a"));
        // The server replays a log that restarted at 1: what the page had is gone.
        chat.apply(&hello("b", 5));
        assert_eq!(chat.log_id(), Some("b"));
        assert_eq!(chat.last_seq(), 0);
        apply_all(&mut chat, [user(1, "Deux"), assistant(2, "B")]);
        assert_eq!(chat.last_seq(), 2);
        assert!(chat.running());
        assert_eq!(
            items(&chat),
            [
                Item::User {
                    text: "Deux".into(),
                    by: None,
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
    }

    #[test]
    fn same_log_id_keeps_the_rows() {
        let mut chat = Chat::default();
        chat.apply(&hello("a", 2));
        apply_all(&mut chat, [user(1, "Un"), assistant(2, "A")]);
        // Reconnection: same log, only newer entries follow.
        chat.apply(&hello("a", 3));
        apply_all(&mut chat, [assistant(3, "C"), assistant(2, "A")]);
        assert_eq!(chat.rows().len(), 2);
        assert_eq!(chat.last_seq(), 3);
    }

    #[test]
    fn a_shorter_log_with_the_same_id_starts_over() {
        let mut chat = Chat::default();
        chat.apply(&hello("a", 2));
        apply_all(&mut chat, [user(1, "Un"), assistant(2, "A")]);
        chat.apply(&hello("a", 1));
        apply_all(&mut chat, [user(1, "Deux")]);
        assert_eq!(chat.rows().len(), 1);
        assert_eq!(chat.last_seq(), 1);
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
                    text: "Fronde a refusé de répondre.".into()
                },
                Item::Notice {
                    text: "Limite d'utilisation atteinte.".into()
                },
            ]
        );
    }
}
