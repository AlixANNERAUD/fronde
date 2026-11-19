//! The thread list and each thread's chat: which thread the page shows, which one the server
//! streams, and what to send to switch, start, rename or archive one. Pure logic, without
//! browser APIs, so it is tested natively.

use std::collections::{HashMap, HashSet};

use crate::usage::Usage;
use fronde_protocol::{ClientMsg, ServerMsg, ThreadId, ThreadSummary, check_title};

use crate::Chat;

/// A thread as the list shows it: under its parent, one level further in per ancestor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListRow {
    pub summary: ThreadSummary,
    /// 0 for a thread the user started, 1 for its sub-agent's thread, and so on.
    pub depth: u32,
    /// It has cards waiting for an answer: « attend ta réponse ».
    pub waiting: bool,
}

/// A thread message the server refused, for the page to say why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRefusal {
    pub thread: Option<ThreadId>,
    pub client_id: Option<String>,
    pub reason: String,
}

/// The threads the page knows, and the chat of each thread it showed.
#[derive(Debug, Clone, PartialEq)]
pub struct Threads {
    list: Vec<ThreadSummary>,
    /// The thread the page shows.
    current: ThreadId,
    /// The thread the server streams, as its last hello said: its entries go to its chat. It
    /// differs from `current` between a switch and the server's answer.
    streamed: Option<ThreadId>,
    chats: HashMap<ThreadId, Chat>,
    /// The thread being made: its `client_id` and first message, until the server says which
    /// it is.
    creating: Option<(String, String)>,
    refusal: Option<ThreadRefusal>,
    /// The latest usage the page's user may see: their own accounts', and for an admin every
    /// other's; none before a reading.
    usage: Option<Usage>,
}

impl Default for Threads {
    fn default() -> Self {
        Self::new(ThreadId::MAIN)
    }
}

impl Threads {
    /// Showing thread `current` first.
    pub fn new(current: ThreadId) -> Self {
        Self {
            list: Vec::new(),
            current,
            streamed: None,
            chats: HashMap::from([(current, Chat::for_thread(current))]),
            creating: None,
            refusal: None,
            usage: None,
        }
    }

    /// The usage the meter shows: the page's user's own accounts' (one per provider, Claude's
    /// today; the pill is the first's), an admin's others, and the reserve in percent; `None`
    /// until the server sent a reading.
    pub fn usage(&self) -> Option<&Usage> {
        self.usage.as_ref()
    }

    /// The thread the page shows.
    pub fn current(&self) -> ThreadId {
        self.current
    }

    /// The chat of the thread the page shows: [`crate::Connection::on_open`] names it in its
    /// hello.
    pub fn chat(&self) -> &Chat {
        self.chats
            .get(&self.current)
            .expect("the current thread always has a chat")
    }

    /// Thread `id`'s chat, if the page showed it.
    pub fn chat_of(&self, id: ThreadId) -> Option<&Chat> {
        self.chats.get(&id)
    }

    /// The threads as the server last listed them, oldest first.
    pub fn list(&self) -> &[ThreadSummary] {
        &self.list
    }

    pub fn summary(&self, id: ThreadId) -> Option<&ThreadSummary> {
        self.list.iter().find(|thread| thread.id == id)
    }

    /// A thread is being made: the page waits for the server to name it.
    pub fn creating(&self) -> bool {
        self.creating.is_some()
    }

    /// The first message of the thread being made, to show while the server makes it.
    pub fn creating_text(&self) -> Option<&str> {
        self.creating.as_ref().map(|(_, text)| text.as_str())
    }

    /// The `Create` to send again on a new socket while the server has not said which thread it
    /// made: the same `client_id`, so that a create the server already did is not done twice.
    pub fn pending_create(&self) -> Option<ClientMsg> {
        self.creating
            .as_ref()
            .map(|(client_id, text)| ClientMsg::Create {
                client_id: client_id.clone(),
                text: text.clone(),
            })
    }

    /// The last thread message the server refused, until dismissed.
    pub fn refusal(&self) -> Option<&ThreadRefusal> {
        self.refusal.as_ref()
    }

    pub fn dismiss_refusal(&mut self) {
        self.refusal = None;
    }

    /// The threads not archived, the most recently active first, each followed by its
    /// children.
    pub fn rows(&self) -> Vec<ListRow> {
        self.ordered(false)
    }

    /// The archived threads, ordered as [`Threads::rows`].
    pub fn archived_rows(&self) -> Vec<ListRow> {
        self.ordered(true)
    }

    /// How many threads not archived wait for an answer.
    pub fn waiting(&self) -> usize {
        self.list
            .iter()
            .filter(|thread| !thread.archived && thread.open_requests > 0)
            .count()
    }

    fn ordered(&self, archived: bool) -> Vec<ListRow> {
        let threads: Vec<&ThreadSummary> = self
            .list
            .iter()
            .filter(|thread| thread.archived == archived)
            .collect();
        let ids: HashSet<ThreadId> = threads.iter().map(|thread| thread.id).collect();
        // A thread whose parent is not among these (archived apart, or not the user's) shows
        // at the top level.
        let children = |parent: Option<ThreadId>| {
            let mut children: Vec<&ThreadSummary> = threads
                .iter()
                .copied()
                .filter(|thread| match parent {
                    Some(parent) => thread.parent == Some(parent),
                    None => thread.parent.is_none_or(|parent| !ids.contains(&parent)),
                })
                .collect();
            children.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id)));
            children
        };
        let mut rows = Vec::with_capacity(threads.len());
        let mut seen = HashSet::new();
        let mut stack: Vec<(&ThreadSummary, u32)> =
            children(None).into_iter().rev().map(|t| (t, 0)).collect();
        while let Some((thread, depth)) = stack.pop() {
            if !seen.insert(thread.id) {
                continue;
            }
            rows.push(ListRow {
                summary: thread.clone(),
                depth,
                waiting: thread.open_requests > 0,
            });
            stack.extend(
                children(Some(thread.id))
                    .into_iter()
                    .rev()
                    .map(|child| (child, depth + 1)),
            );
        }
        rows
    }

    /// Shows thread `id`: the hello that streams it, from what its chat already has.
    pub fn open(&mut self, id: ThreadId) -> ClientMsg {
        self.current = id;
        let chat = self.chats.entry(id).or_insert_with(|| Chat::for_thread(id));
        ClientMsg::Hello {
            last_seq: chat.last_seq(),
            log_id: chat.log_id().map(str::to_owned),
            thread: Some(id),
        }
    }

    /// A message to the thread shown.
    pub fn send(&self, text: String) -> ClientMsg {
        ClientMsg::Send {
            text,
            thread: Some(self.current),
        }
    }

    /// Stops the thread shown's turn.
    pub fn cancel(&self) -> ClientMsg {
        ClientMsg::Cancel {
            thread: Some(self.current),
        }
    }

    /// Starts a thread with its first message; `client_id` is the page's own, random, and the
    /// same if the page sends it again. The server answers which thread it made, and the page
    /// shows it then.
    pub fn create(&mut self, client_id: String, text: String) -> ClientMsg {
        self.creating = Some((client_id.clone(), text.clone()));
        ClientMsg::Create { client_id, text }
    }

    /// Renames thread `id`, if `title` is one; else why not, in French.
    pub fn rename(id: ThreadId, title: &str) -> Result<ClientMsg, &'static str> {
        Ok(ClientMsg::Rename {
            thread: id,
            title: check_title(title)?,
        })
    }

    /// Archives thread `id`, or brings it back; `None` for the main thread, which never is.
    pub fn archive(id: ThreadId, archived: bool) -> Option<ClientMsg> {
        (id != ThreadId::MAIN).then_some(ClientMsg::Archive {
            thread: id,
            archived,
        })
    }

    /// Takes in what the server sent; returns what the page must send next, if anything: the
    /// hello of a thread just made.
    pub fn apply(&mut self, msg: &ServerMsg) -> Option<ClientMsg> {
        match msg {
            ServerMsg::Hello { thread, .. } => {
                let id = thread.unwrap_or(ThreadId::MAIN);
                self.streamed = Some(id);
                self.chats
                    .entry(id)
                    .or_insert_with(|| Chat::for_thread(id))
                    .apply(msg);
            }
            ServerMsg::Entry(_) => {
                if let Some(chat) = self.streamed.and_then(|id| self.chats.get_mut(&id)) {
                    chat.apply(msg);
                }
            }
            // Request ids are global: only the chat that shows the request changes.
            ServerMsg::Refused { .. } => {
                for chat in self.chats.values_mut() {
                    chat.apply(msg);
                }
            }
            ServerMsg::Threads { threads } => self.list = threads.clone(),
            ServerMsg::Usage {
                reserve,
                usage,
                others,
            } => {
                self.usage = Some(Usage {
                    reserve: *reserve,
                    own: usage.clone(),
                    others: others.clone(),
                })
            }
            ServerMsg::Created { client_id, thread } => {
                if self.creating.as_ref().map(|(id, _)| id) == Some(client_id) {
                    self.creating = None;
                    return Some(self.open(*thread));
                }
            }
            ServerMsg::ThreadRefused {
                thread,
                client_id,
                reason,
            } => {
                if client_id.is_some()
                    && self.creating.as_ref().map(|(id, _)| id) == client_id.as_ref()
                {
                    self.creating = None;
                }
                self.refusal = Some(ThreadRefusal {
                    thread: *thread,
                    client_id: client_id.clone(),
                    reason: reason.clone(),
                });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Connection, Item};
    use fronde_protocol::{Entry, Event, ThreadStatus};

    fn summary(id: i64, parent: Option<i64>, updated: &str) -> ThreadSummary {
        ThreadSummary {
            id: ThreadId(id),
            title: format!("Fil {id}"),
            parent: parent.map(ThreadId),
            status: ThreadStatus::Idle,
            open_requests: 0,
            updated_at: format!("2026-11-17T{updated}:00Z").parse().unwrap(),
            archived: false,
        }
    }

    fn hello(thread: i64, log_id: &str, last_seq: u64) -> ServerMsg {
        ServerMsg::Hello {
            log_id: log_id.into(),
            last_seq,
            user: Some("alix".into()),
            admin: true,
            thread: Some(ThreadId(thread)),
        }
    }

    fn user(seq: u64, text: &str) -> ServerMsg {
        ServerMsg::Entry(Entry {
            seq,
            at: "2026-11-17T10:00:00Z".parse().unwrap(),
            event: Event::User {
                text: text.into(),
                by: None,
            },
        })
    }

    fn ids(rows: &[ListRow]) -> Vec<(i64, u32)> {
        rows.iter()
            .map(|row| (row.summary.id.0, row.depth))
            .collect()
    }

    #[test]
    fn list_puts_the_most_recent_first_and_children_under_their_parent() {
        let mut threads = Threads::default();
        let mut archived = summary(6, None, "12:00");
        archived.archived = true;
        threads.apply(&ServerMsg::Threads {
            threads: vec![
                summary(1, None, "09:00"),
                summary(2, None, "11:00"),
                summary(3, Some(2), "08:00"),
                summary(4, None, "10:00"),
                summary(5, Some(2), "10:30"),
                archived,
                // Its parent is archived: it shows at the top level.
                summary(7, Some(6), "07:00"),
                summary(8, Some(5), "07:30"),
            ],
        });
        assert_eq!(
            ids(&threads.rows()),
            [(2, 0), (5, 1), (8, 2), (3, 1), (4, 0), (1, 0), (7, 0)]
        );
        assert_eq!(ids(&threads.archived_rows()), [(6, 0)]);
    }

    #[test]
    fn threads_waiting_for_an_answer_get_a_badge() {
        let mut threads = Threads::default();
        let mut asking = summary(2, None, "10:00");
        asking.open_requests = 2;
        let mut archived = summary(3, None, "10:00");
        archived.open_requests = 1;
        archived.archived = true;
        threads.apply(&ServerMsg::Threads {
            threads: vec![summary(1, None, "09:00"), asking, archived],
        });
        let rows = threads.rows();
        assert!(rows[0].waiting);
        assert_eq!(rows[0].summary.id, ThreadId(2));
        assert!(!rows[1].waiting);
        assert_eq!(threads.waiting(), 1);
    }

    #[test]
    fn switching_threads_keeps_each_chat_and_names_the_thread() {
        let mut threads = Threads::default();
        let mut connection = Connection::default();
        assert_eq!(
            connection.on_open(threads.chat()),
            ClientMsg::Hello {
                last_seq: 0,
                log_id: None,
                thread: Some(ThreadId::MAIN),
            }
        );
        threads.apply(&hello(1, "main", 1));
        threads.apply(&user(1, "Salut"));
        assert_eq!(threads.chat().last_seq(), 1);

        assert_eq!(
            threads.open(ThreadId(2)),
            ClientMsg::Hello {
                last_seq: 0,
                log_id: None,
                thread: Some(ThreadId(2)),
            }
        );
        // An entry of the main thread still on its way goes to the main thread's chat.
        threads.apply(&user(2, "Encore"));
        assert_eq!(threads.chat_of(ThreadId::MAIN).unwrap().last_seq(), 2);
        assert_eq!(threads.chat().last_seq(), 0);
        threads.apply(&hello(2, "deux", 1));
        threads.apply(&user(1, "Range le garage"));
        assert_eq!(threads.current(), ThreadId(2));
        assert_eq!(
            threads.chat().rows()[0].item,
            Item::User {
                text: "Range le garage".into(),
                by: None,
            }
        );

        // Back to the main thread: its hello asks only for what came after.
        assert_eq!(
            threads.open(ThreadId::MAIN),
            ClientMsg::Hello {
                last_seq: 2,
                log_id: Some("main".into()),
                thread: Some(ThreadId::MAIN),
            }
        );
        assert_eq!(
            threads.send("Et alors ?".into()),
            ClientMsg::Send {
                text: "Et alors ?".into(),
                thread: Some(ThreadId::MAIN),
            }
        );
        assert_eq!(
            threads.cancel(),
            ClientMsg::Cancel {
                thread: Some(ThreadId::MAIN),
            }
        );
    }

    #[test]
    fn created_thread_is_shown_once_the_server_names_it() {
        let mut threads = Threads::default();
        threads.apply(&hello(1, "main", 0));
        assert_eq!(
            threads.create("c1".into(), "Range le garage".into()),
            ClientMsg::Create {
                client_id: "c1".into(),
                text: "Range le garage".into(),
            }
        );
        assert!(threads.creating());
        assert_eq!(threads.creating_text(), Some("Range le garage"));
        // A new socket sends it again, under the same id.
        assert_eq!(
            threads.pending_create(),
            Some(ClientMsg::Create {
                client_id: "c1".into(),
                text: "Range le garage".into(),
            })
        );
        // Another page's thread: not this one.
        assert_eq!(
            threads.apply(&ServerMsg::Created {
                client_id: "other".into(),
                thread: ThreadId(3),
            }),
            None
        );
        assert_eq!(threads.current(), ThreadId::MAIN);
        assert_eq!(
            threads.apply(&ServerMsg::Created {
                client_id: "c1".into(),
                thread: ThreadId(4),
            }),
            Some(ClientMsg::Hello {
                last_seq: 0,
                log_id: None,
                thread: Some(ThreadId(4)),
            })
        );
        assert_eq!(threads.current(), ThreadId(4));
        assert!(!threads.creating());
        assert_eq!(threads.pending_create(), None);
    }

    #[test]
    fn refusals_are_kept_until_dismissed() {
        let mut threads = Threads::default();
        threads.create("c1".into(), "x".into());
        threads.apply(&ServerMsg::ThreadRefused {
            thread: None,
            client_id: Some("c1".into()),
            reason: "Identifiant de conversation invalide.".into(),
        });
        assert!(!threads.creating());
        assert_eq!(threads.pending_create(), None);
        assert_eq!(
            threads.refusal().map(|refusal| refusal.reason.as_str()),
            Some("Identifiant de conversation invalide.")
        );
        threads.dismiss_refusal();
        assert_eq!(threads.refusal(), None);
    }

    #[test]
    fn rename_checks_the_title_and_main_is_never_archived() {
        assert_eq!(
            Threads::rename(ThreadId(2), "  Garage "),
            Ok(ClientMsg::Rename {
                thread: ThreadId(2),
                title: "Garage".into(),
            })
        );
        assert!(Threads::rename(ThreadId(2), "a\u{202E}b").is_err());
        assert!(Threads::rename(ThreadId(2), &"x".repeat(81)).is_err());
        assert_eq!(Threads::archive(ThreadId::MAIN, true), None);
        assert_eq!(
            Threads::archive(ThreadId(2), true),
            Some(ClientMsg::Archive {
                thread: ThreadId(2),
                archived: true,
            })
        );
    }

    /// The page keeps the latest usage the server sent, for the meter; none before.
    #[test]
    fn usage_message_feeds_the_meter() {
        use fronde_protocol::usage::{Window, WindowKind};
        let mut threads = Threads::default();
        assert!(threads.usage().is_none());
        let at = "2026-11-17T12:00:00Z".parse().unwrap();
        let reading = fronde_protocol::usage::ProviderUsage {
            account: "claude".into(),
            known_since: at,
            windows: vec![Window {
                kind: WindowKind::Session,
                used: Some(0.42),
                status: None,
                resets_at: None,
                as_of: at,
                reset_since_reading: false,
            }],
        };
        let message = ServerMsg::Usage {
            reserve: 30,
            usage: vec![reading.clone()],
            others: Vec::new(),
        };
        assert_eq!(threads.apply(&message), None);
        let usage = threads.usage().unwrap();
        assert_eq!(usage.own, std::slice::from_ref(&reading));
        assert_eq!(usage.reserve, 30);
        assert!(usage.others.is_empty());
        // An admin's page also gets every other account's, by name.
        let hers = fronde_protocol::usage::ProviderUsage {
            account: "marie".into(),
            ..reading.clone()
        };
        let message = ServerMsg::Usage {
            reserve: 30,
            usage: vec![reading.clone()],
            others: vec![hers.clone()],
        };
        assert_eq!(threads.apply(&message), None);
        assert_eq!(threads.usage().unwrap().others, [hers]);
    }
}
