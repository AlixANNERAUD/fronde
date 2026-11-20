//! The tray above the message box: the cards waiting for an answer, the shown thread's first,
//! each by a short title, then the other threads that have some. Their total is what the tab's
//! title counts: « (N) Fronde ».

use fronde_protocol::{Card, RequestId, ThreadId};

use crate::{Chat, Item, Threads};

/// How many characters a card's title keeps in the tray.
pub const MAX_TRAY_TITLE: usize = 48;

/// An open card of the shown thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayCard {
    pub id: RequestId,
    /// What the card asks, in a line of at most [`MAX_TRAY_TITLE`] characters.
    pub title: String,
}

/// Another thread with cards waiting for an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Elsewhere {
    pub thread: ThreadId,
    pub title: String,
    pub count: u32,
}

/// What the tray shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tray {
    /// The shown thread's open cards, oldest first.
    pub here: Vec<TrayCard>,
    /// The other threads, not archived, with open cards, as the list orders them.
    pub elsewhere: Vec<Elsewhere>,
}

impl Tray {
    /// How many cards wait for an answer, in all threads.
    pub fn count(&self) -> usize {
        self.here.len()
            + self
                .elsewhere
                .iter()
                .map(|thread| thread.count as usize)
                .sum::<usize>()
    }

    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }

    /// « N demande(s) en attente ».
    pub fn heading(&self) -> String {
        match self.count() {
            1 => "1 demande en attente".to_owned(),
            count => format!("{count} demandes en attente"),
        }
    }
}

/// The tab's title: « (N) Fronde » while N cards wait for an answer, else « Fronde ».
pub fn tab_title(open: usize) -> String {
    if open == 0 {
        "Fronde".to_owned()
    } else {
        format!("({open}) Fronde")
    }
}

/// What `card` asks, in a line of at most [`MAX_TRAY_TITLE`] characters.
pub fn card_title(card: &Card) -> String {
    let title = match card {
        Card::Permission { title, .. } => title.as_str(),
        Card::Secret { label, .. } => label.as_str(),
        Card::Form {
            message, fields, ..
        } => fields
            .first()
            .map(|field| match field {
                fronde_protocol::Field::Choice {
                    header: Some(header),
                    ..
                } if fields.len() > 1 => header.as_str(),
                field => field.label(),
            })
            .filter(|label| !label.trim().is_empty())
            .unwrap_or(message.as_str()),
    };
    let line = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return "Demande".to_owned();
    }
    if line.chars().count() > MAX_TRAY_TITLE {
        let head: String = line.chars().take(MAX_TRAY_TITLE - 1).collect();
        format!("{}…", head.trim_end())
    } else {
        line
    }
}

impl Chat {
    /// The open cards, oldest first, as the tray lists them.
    pub fn tray(&self) -> Vec<TrayCard> {
        self.open_requests()
            .filter_map(|row| match &row.item {
                Item::Request { id, card, .. } => Some(TrayCard {
                    id: *id,
                    title: card_title(card),
                }),
                _ => None,
            })
            .collect()
    }
}

impl Threads {
    /// The tray of the page showing thread `shown` (`None`: no thread, as on the list or a new
    /// thread): its open cards from its chat, then the other threads not archived that wait
    /// for an answer, from the list.
    pub fn tray(&self, shown: Option<ThreadId>) -> Tray {
        let here = shown
            .and_then(|id| self.chat_of(id))
            .map(Chat::tray)
            .unwrap_or_default();
        let elsewhere = self
            .rows()
            .into_iter()
            .filter(|row| Some(row.summary.id) != shown && row.summary.open_requests > 0)
            .map(|row| Elsewhere {
                thread: row.summary.id,
                title: row.summary.title,
                count: row.summary.open_requests,
            })
            .collect();
        Tray { here, elsewhere }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fronde_protocol::{
        ChoiceOption, Entry, Event, Field, Outcome, ServerMsg, ThreadStatus, ThreadSummary,
    };

    fn entry(seq: u64, event: Event) -> ServerMsg {
        ServerMsg::Entry(Entry {
            seq,
            at: "2026-11-17T10:00:00Z".parse().unwrap(),
            event,
        })
    }

    fn request(id: RequestId, card: Card) -> Event {
        Event::Request {
            id,
            card,
            from: None,
            server: false,
            woken_by: None,
            carries: 0,
        }
    }

    fn question(text: &str) -> Card {
        Card::Form {
            message: String::new(),
            fields: vec![Field::Choice {
                key: "q1".into(),
                header: Some("Agenda".into()),
                question: text.into(),
                options: vec![ChoiceOption {
                    label: "Perso".into(),
                    description: None,
                    preview: None,
                    recommended: false,
                }],
                multi: false,
                other: false,
                required: true,
                note: false,
            }],
            submit: None,
            truncated: false,
            replaced: false,
            during: Vec::new(),
        }
    }

    fn summary(id: i64, title: &str, open: u32, archived: bool) -> ThreadSummary {
        ThreadSummary {
            id: ThreadId(id),
            title: title.into(),
            parent: None,
            status: ThreadStatus::Done,
            open_requests: open,
            updated_at: format!("2026-11-17T09:{id:02}:00Z").parse().unwrap(),
            archived,
        }
    }

    #[test]
    fn tray_lists_open_requests() {
        let mut threads = Threads::new(ThreadId(2));
        threads.apply(&ServerMsg::Threads {
            threads: vec![
                summary(1, "Fronde", 1, false),
                summary(2, "Agenda", 2, false),
                summary(3, "Impôts", 3, false),
                summary(4, "Vieux", 1, true),
                summary(5, "Calme", 0, false),
            ],
        });
        let secret = Card::Secret {
            label: "Mot de passe Nextcloud".into(),
            reason: "Pour lire ton agenda.".into(),
            placeholder: None,
            url: None,
            remember: None,
        };
        let long = "Quel agenda faut-il utiliser pour les rendez-vous du garage cette semaine ?";
        for message in [
            ServerMsg::Hello {
                log_id: "l".into(),
                last_seq: 0,
                user: Some("alix".into()),
                admin: true,
                thread: Some(ThreadId(2)),
                home: None,
            },
            entry(1, request(7, question("Quel   agenda ?"))),
            entry(2, request(8, secret)),
            entry(3, request(9, question(long))),
            entry(
                4,
                Event::RequestClosed {
                    id: 8,
                    outcome: Outcome::Declined,
                    by: None,
                },
            ),
        ] {
            threads.apply(&message);
        }
        let tray = threads.tray(Some(ThreadId(2)));
        // The shown thread's cards first, oldest first, from its chat; a closed one is gone.
        assert_eq!(tray.here.len(), 2, "{tray:?}");
        assert_eq!(
            tray.here[0],
            TrayCard {
                id: 7,
                title: "Quel agenda ?".into()
            }
        );
        assert_eq!(tray.here[1].id, 9);
        assert_eq!(tray.here[1].title.chars().count(), MAX_TRAY_TITLE);
        assert!(tray.here[1].title.ends_with('…'), "{tray:?}");
        // Then the other threads that wait, as the list orders them; not an archived one.
        let elsewhere: Vec<(ThreadId, u32)> = tray
            .elsewhere
            .iter()
            .map(|thread| (thread.thread, thread.count))
            .collect();
        assert_eq!(elsewhere, [(ThreadId(3), 3), (ThreadId(1), 1)]);
        assert_eq!(tray.elsewhere[0].title, "Impôts");
        assert_eq!(tray.count(), 6);
        assert_eq!(tray.heading(), "6 demandes en attente");

        // From the list, every thread is elsewhere, counted as the list counts it.
        let tray = threads.tray(None);
        assert!(tray.here.is_empty());
        assert_eq!(tray.elsewhere.len(), 3);
        assert_eq!(tray.count(), 6);

        let calm = Threads::default();
        assert!(calm.tray(Some(ThreadId::MAIN)).is_empty());
        assert_eq!(tab_title(0), "Fronde");
        assert_eq!(tab_title(3), "(3) Fronde");
        let one = Tray {
            here: vec![TrayCard {
                id: 1,
                title: "x".into(),
            }],
            elsewhere: Vec::new(),
        };
        assert_eq!(one.heading(), "1 demande en attente");
    }
}
