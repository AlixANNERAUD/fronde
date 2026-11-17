//! The pages of signed-in users: `/` (the main thread), `/t/<id>` (any thread), `/threads` (the
//! thread list, the whole screen on a phone) and `/new` (a chat whose first message makes a
//! thread). From `md` up the list sits beside the chat once there is more than one thread.
//!
//! [`Shell`] holds what the pages share, the threads and the socket, so that moving between
//! them keeps both; each page draws a [`Workspace`] from plain props.

use dioxus::prelude::*;
use fronde_client::{Chat, Threads, tab_title};
use fronde_protocol::ThreadId;

use crate::{
    app::Route,
    components::icons::{ArrowLeft, X},
    components::thread_list::{ICON_LINK, Place, PlaceLink, ThreadList},
    components::ui::{
        alert::{Alert, AlertDescription, AlertVariant},
        button::{Button, ButtonSize, ButtonVariant},
    },
    pages::chat::{ChatView, Kept, Target},
    socket::{Outgoing, Status, use_chat_socket},
};

/// What the pages share, from [`Shell`].
#[derive(Clone, Copy)]
struct Session {
    threads: Signal<Threads>,
    status: Signal<Status>,
    kept: Signal<Kept>,
    socket: Coroutine<Outgoing>,
}

/// The layout of the signed-in pages: one socket for all of them.
#[component]
pub fn Shell() -> Element {
    let route = use_route::<Route>();
    // Streams the thread the address names from the first hello.
    let threads = use_signal(|| Threads::new(route.thread().unwrap_or(ThreadId::MAIN)));
    let status = use_signal(|| Status::Connecting);
    let kept = use_signal(Kept::default);
    let navigator = navigator();
    let on_created = use_callback(move |id: ThreadId| {
        navigator.push(Route::from(Place::Thread(id)));
    });
    let socket = use_chat_socket(threads, status, on_created);
    use_context_provider(|| Session {
        threads,
        status,
        kept,
        socket,
    });
    rsx! {
        Outlet::<Route> {}
    }
}

#[component]
pub fn ChatPage() -> Element {
    rsx! {
        Screen { view: View::Thread(ThreadId::MAIN) }
    }
}

#[component]
pub fn ThreadPage(id: i64) -> Element {
    rsx! {
        Screen { view: View::Thread(ThreadId(id)) }
    }
}

#[component]
pub fn ThreadsPage() -> Element {
    rsx! {
        Screen { view: View::List }
    }
}

#[component]
pub fn NewThreadPage() -> Element {
    rsx! {
        Screen { view: View::New }
    }
}

/// What a page shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Thread(ThreadId),
    List,
    New,
}

/// A page: the workspace, streaming the thread it shows.
#[component]
fn Screen(view: View) -> Element {
    let session = use_context::<Session>();
    let navigator = navigator();
    let on_send = move |outgoing: Outgoing| session.socket.send(outgoing);
    use_effect(use_reactive!(|view| {
        if let View::Thread(id) = view {
            show(session.threads, id, on_send);
        }
    }));
    rsx! {
        Workspace {
            view,
            threads: session.threads,
            status: (session.status)(),
            kept: session.kept,
            on_send,
            on_go: move |place: Place| {
                navigator.push(Route::from(place));
            },
        }
    }
}

/// Shows thread `id`: unless the page already does, its hello, which asks only for what its chat
/// does not have yet.
pub fn show(mut threads: Signal<Threads>, id: ThreadId, on_send: impl Fn(Outgoing)) {
    if threads.peek().current() == id {
        return;
    }
    let hello = threads.write().open(id);
    on_send(Outgoing {
        message: hello,
        sent: None,
        dropped: None,
    });
}

/// The page as drawn from the threads: no socket and no router, so it renders anywhere.
#[component]
pub fn Workspace(
    view: View,
    threads: Signal<Threads>,
    status: Status,
    kept: Signal<Kept>,
    on_send: EventHandler<Outgoing>,
    on_go: EventHandler<Place>,
) -> Element {
    let model = threads.read();
    let open = status == Status::Open;
    let rows = model.rows();
    let archived = model.archived_rows();
    let refusal = model.refusal().map(|refusal| refusal.reason.clone());
    let shown = model.current();
    let current = match view {
        View::Thread(id) => Some(id),
        View::List | View::New => None,
    };
    let sidebar = rows.len() + archived.len() > 1;
    // The shown thread's open cards from its chat, the others' from the list.
    let tray = model.tray(current);
    let title = tab_title(tray.count());
    let list = rsx! {
        ThreadList { rows, archived, current, open, on_go, on_send }
    };
    let dismiss = move |()| {
        let mut threads = threads;
        threads.write().dismiss_refusal();
    };
    let page = match view {
        View::List => rsx! {
            div { class: "flex h-dvh flex-col",
                header { class: "shrink-0 border-b pt-[env(safe-area-inset-top)]",
                    div { class: "mx-auto flex h-12 w-full max-w-3xl items-center gap-1 px-4",
                        PlaceLink {
                            place: Place::Thread(shown),
                            on_go,
                            class: ICON_LINK,
                            aria_label: "Retour à la conversation",
                            ArrowLeft {}
                        }
                        h1 { class: "font-semibold", "Conversations" }
                    }
                }
                if let Some(reason) = refusal {
                    div { class: "mx-auto w-full max-w-3xl px-4 pt-3",
                        Alert {
                            variant: AlertVariant::Destructive,
                            class: "flex items-center gap-2",
                            AlertDescription { class: "min-w-0 flex-1", "{reason}" }
                            Button {
                                variant: ButtonVariant::Ghost,
                                size: ButtonSize::Icon,
                                class: "size-11 md:size-9",
                                aria_label: "Fermer",
                                onclick: move |_| dismiss(()),
                                X {}
                            }
                        }
                    }
                }
                div { class: "mx-auto flex min-h-0 w-full max-w-3xl flex-1 flex-col", {list} }
            }
        },
        View::Thread(_) | View::New => {
            let (key, chat, target, summary) = match view {
                View::Thread(id) => (
                    id.to_string(),
                    model
                        .chat_of(id)
                        .cloned()
                        .unwrap_or_else(|| Chat::for_thread(id)),
                    Target::of(id),
                    model.summary(id).cloned(),
                ),
                View::List | View::New => (
                    "new".to_owned(),
                    Chat::default(),
                    Target::New(threads),
                    None,
                ),
            };
            // One per thread, keyed by it: its draft, scroll and menus are its own.
            let chat = rsx! {
                ChatView {
                    key: "{key}",
                    chat,
                    status,
                    on_send,
                    target,
                    summary,
                    kept: Some(kept),
                    on_go: Some(on_go),
                    sidebar,
                    refusal,
                    on_dismiss: Some(EventHandler::new(dismiss)),
                    elsewhere: tray.elsewhere,
                }
            };
            rsx! {
                div { class: "flex h-dvh",
                    if sidebar {
                        aside { class: "hidden w-72 shrink-0 flex-col border-r md:flex", {list} }
                    }
                    {chat}
                }
            }
        }
    };
    rsx! {
        // « (N) Fronde » while cards wait for an answer, in any thread.
        document::Title { "{title}" }
        {page}
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use dioxus::html::{Code, Key};
    use fronde_protocol::{ClientMsg, Entry, Event, ServerMsg, ThreadStatus, ThreadSummary};

    use super::*;
    use crate::testing::{Live, deliver, sent};

    thread_local! {
        /// The threads of the page under test, for the server's messages.
        static THREADS: RefCell<Option<Signal<Threads>>> = const { RefCell::new(None) };
    }

    /// The workspace, with links followed as the router does: the address changes, and the page
    /// it leads to streams its thread.
    #[component]
    fn Harness(start: Threads, view: View, status: Status) -> Element {
        let threads = use_signal(|| start);
        let mut view = use_signal(|| view);
        let kept = use_signal(Kept::default);
        THREADS.with_borrow_mut(|shared| *shared = Some(threads));
        rsx! {
            Workspace {
                view: view(),
                threads,
                status,
                kept,
                on_send: deliver,
                on_go: move |place: Place| {
                    if let Place::Thread(id) = place {
                        show(threads, id, deliver);
                    }
                    view.set(
                        match place {
                            Place::Thread(id) => View::Thread(id),
                            Place::List => View::List,
                            Place::New => View::New,
                        },
                    );
                },
            }
        }
    }

    /// The chat page as it was before threads.
    #[component]
    fn Before(chat: Chat) -> Element {
        rsx! {
            ChatView { chat, status: Status::Open, on_send: deliver }
        }
    }

    fn live(start: Threads, view: View) -> Live {
        Live::new(VirtualDom::new_with_props(
            Harness,
            HarnessProps {
                start,
                view,
                status: Status::Open,
            },
        ))
    }

    /// What the server sends, while the page runs.
    fn receive(page: &mut Live, message: ServerMsg) -> Option<ClientMsg> {
        let threads = THREADS.with_borrow(|threads| threads.expect("a page runs"));
        let reply = page.dom.in_runtime(|| {
            let mut threads = threads;
            threads.write().apply(&message)
        });
        page.update();
        reply
    }

    fn summary(id: i64, title: &str, updated: &str) -> ThreadSummary {
        ThreadSummary {
            id: ThreadId(id),
            title: title.into(),
            parent: None,
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

    fn entry(seq: u64, event: Event) -> ServerMsg {
        ServerMsg::Entry(Entry {
            seq,
            at: "2026-11-17T10:00:00Z".parse().unwrap(),
            event,
        })
    }

    fn user(seq: u64, text: &str, by: Option<&str>) -> ServerMsg {
        entry(
            seq,
            Event::User {
                text: text.into(),
                by: by.map(str::to_owned),
            },
        )
    }

    /// The main thread's chat with two entries, and these threads listed.
    fn threads(list: Vec<ThreadSummary>) -> Threads {
        let mut threads = Threads::default();
        threads.apply(&hello(1, "main", 2));
        threads.apply(&user(1, "Salut", None));
        threads.apply(&entry(
            2,
            Event::TurnEnded {
                outcome: fronde_protocol::TurnOutcome::Done,
            },
        ));
        threads.apply(&ServerMsg::Threads { threads: list });
        threads
    }

    /// The main thread, a thread asking a question with its sub-agent's queued under it, and a
    /// thread whose last turn failed.
    fn several() -> Threads {
        let main = summary(1, "Fronde", "09:00");
        let mut garage = summary(2, "Garage", "11:00");
        garage.status = ThreadStatus::Running;
        garage.open_requests = 1;
        let mut child = summary(3, "Mesurer la porte", "10:30");
        child.parent = Some(ThreadId(2));
        child.status = ThreadStatus::Queued;
        let mut failed = summary(4, "Impôts", "10:00");
        failed.status = ThreadStatus::Failed;
        let mut old = summary(5, "Vacances", "08:00");
        old.archived = true;
        old.status = ThreadStatus::Done;
        threads(vec![main, garage, child, failed, old])
    }

    /// Where `needle` is in `page`.
    fn at(page: &str, needle: &str) -> usize {
        page.find(needle)
            .unwrap_or_else(|| panic!("no {needle} in:\n{page}"))
    }

    /// The row of thread `id` in the list.
    fn row(page: &str, id: i64) -> String {
        let start = at(page, &format!(r#"id="tl-{id}""#));
        let start = page[..start].rfind("<li").unwrap();
        let end = start + page[start..].find("</li>").unwrap();
        page[start..end].to_owned()
    }

    #[test]
    fn list_shows_statuses_badges_and_children() {
        let page = live(several(), View::List).html();
        // The newest first, each sub-agent's thread under its parent.
        let order: Vec<usize> = [2, 3, 4, 1]
            .iter()
            .map(|id| at(&page, &format!(r#"id="tl-{id}""#)))
            .collect();
        assert!(order.is_sorted(), "{page}");

        let garage = row(&page, 2);
        assert!(garage.contains(r#"href="/t/2""#), "{garage}");
        assert!(garage.contains("En cours"), "{garage}");
        assert!(garage.contains("animate-pulse"), "{garage}");
        assert!(garage.contains("attend ta réponse"), "{garage}");
        assert!(
            garage.contains(r#"datetime="2026-11-17T11:00:00+00:00""#),
            "{garage}"
        );
        assert!(!garage.contains("sous-agent"), "{garage}");
        assert!(garage.contains("padding-inline-start: 0.75rem"), "{garage}");

        let child = row(&page, 3);
        assert!(child.contains("En file d&#39;attente"), "{child}");
        assert!(child.contains("sous-agent"), "{child}");
        assert!(child.contains("padding-inline-start: 2rem"), "{child}");
        assert!(!child.contains("attend ta réponse"), "{child}");

        assert!(row(&page, 4).contains("Échec"), "{page}");
        let main = row(&page, 1);
        assert!(main.contains(r#"href="/""#), "{main}");
        assert!(main.contains("Inactive"), "{main}");
        assert_eq!(page.matches("attend ta réponse").count(), 1, "{page}");
    }

    #[test]
    fn archived_threads_are_folded_apart() {
        let page = live(several(), View::List).html();
        let section = at(&page, "<details");
        assert!(page[section..].contains("Archivées (1)"), "{page}");
        assert!(at(&page, r#"id="tl-5""#) > section, "{page}");
        for id in [1, 2, 3, 4] {
            assert!(at(&page, &format!(r#"id="tl-{id}""#)) < section, "{page}");
        }
        assert!(row(&page, 5).contains("Terminée"), "{page}");

        // Without archived threads, no section.
        let mut threads = several();
        threads.apply(&ServerMsg::Threads {
            threads: vec![summary(1, "Fronde", "09:00"), summary(2, "Garage", "10:00")],
        });
        let page = live(threads, View::List).html();
        assert!(!page.contains("<details"), "{page}");
        assert!(!page.contains("Archivées"), "{page}");
    }

    #[test]
    fn first_message_creates_the_thread_with_a_client_id() {
        let mut page = live(several(), View::New);
        assert!(
            page.html().contains("Nouvelle conversation"),
            "{}",
            page.html()
        );
        page.type_text("message", "  Range le garage  ");
        page.key("message", Key::Enter, Code::Enter);
        let messages = sent();
        let [ClientMsg::Create { client_id, text }] = messages.as_slice() else {
            panic!("{:?}", sent());
        };
        assert_eq!(text, "Range le garage");
        // What the server takes as an attempt's id.
        assert!(!client_id.is_empty() && client_id.len() <= fronde_protocol::MAX_CLIENT_ID);
        assert!(
            client_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "{client_id}"
        );
        let client_id = client_id.clone();

        // The message shows while the server makes the thread; nothing more can be sent.
        let html = page.html();
        assert!(html.contains("Range le garage"), "{html}");
        assert!(html.contains("Création de la conversation…"), "{html}");
        page.type_text("message", "Encore");
        page.key("message", Key::Enter, Code::Enter);
        assert_eq!(sent().len(), 1);

        // Another page's creation changes nothing here; this page's is streamed at once.
        let other = ServerMsg::Created {
            client_id: "other".into(),
            thread: ThreadId(8),
        };
        assert_eq!(receive(&mut page, other), None);
        let created = ServerMsg::Created {
            client_id,
            thread: ThreadId(9),
        };
        assert_eq!(
            receive(&mut page, created),
            Some(ClientMsg::Hello {
                last_seq: 0,
                log_id: None,
                thread: Some(ThreadId(9)),
            })
        );
    }

    #[test]
    fn switching_threads_says_hello_with_what_each_chat_has() {
        let mut page = live(several(), View::Thread(ThreadId::MAIN));
        assert!(page.html().contains("Salut"), "{}", page.html());
        page.click("tl-2");
        assert_eq!(
            sent(),
            [ClientMsg::Hello {
                last_seq: 0,
                log_id: None,
                thread: Some(ThreadId(2)),
            }]
        );
        receive(&mut page, hello(2, "deux", 1));
        receive(&mut page, user(1, "Range le garage", None));
        let html = page.html();
        assert!(html.contains("Range le garage"), "{html}");
        assert!(!html.contains("Salut"), "{html}");
        assert!(html.contains(r#"aria-current="page""#), "{html}");
        assert!(row(&html, 2).contains(r#"aria-current="page""#), "{html}");

        // Back to the main thread: only what came after its last entry.
        page.click("tl-1");
        assert_eq!(
            sent()[1],
            ClientMsg::Hello {
                last_seq: 2,
                log_id: Some("main".into()),
                thread: Some(ThreadId::MAIN),
            }
        );
        assert!(page.html().contains("Salut"), "{}", page.html());
        // The thread shown already streams: no second hello.
        page.click("tl-1");
        assert_eq!(sent().len(), 2);
    }

    #[test]
    fn each_thread_keeps_its_draft() {
        let mut page = live(several(), View::Thread(ThreadId::MAIN));
        page.type_text("message", "Pour le fil principal");
        page.click("tl-2");
        assert!(page.html().contains(r#"value="""#), "{}", page.html());
        page.type_text("message", "Pour le garage");
        page.click("tl-1");
        assert!(
            page.html().contains(r#"value="Pour le fil principal""#),
            "{}",
            page.html()
        );
        page.click("tl-2");
        assert!(
            page.html().contains(r#"value="Pour le garage""#),
            "{}",
            page.html()
        );
    }

    #[test]
    fn rename_and_archive_send_their_messages() {
        let mut page = live(several(), View::List);
        let html = page.html();
        assert!(row(&html, 2).contains(r#"aria-expanded="false""#), "{html}");
        assert!(!html.contains(r#"id="tl-2-rename""#), "{html}");
        page.click("tl-2-menu");
        let html = page.html();
        assert!(row(&html, 2).contains(r#"aria-expanded="true""#), "{html}");
        assert!(
            row(&html, 2).contains(r#"aria-controls="tl-2-actions""#),
            "{html}"
        );
        page.click("tl-2-rename");
        let field = page.element("tl-2-title");
        assert!(page.html().contains(r#"value="Garage""#), "{}", page.html());

        // A title the protocol refuses is not sent: the field says why.
        page.type_text("tl-2-title", "Ga\u{202E}rage");
        page.key("tl-2-title", Key::Enter, Code::Enter);
        assert_eq!(sent(), []);
        assert!(
            page.html().contains("caractère invisible"),
            "{}",
            page.html()
        );

        page.type_text("tl-2-title", "  Garage et cave ");
        // The field stays the same element as it is typed in, so it keeps the focus.
        assert_eq!(page.element("tl-2-title"), field);
        page.key("tl-2-title", Key::Enter, Code::Enter);
        assert_eq!(
            sent(),
            [ClientMsg::Rename {
                thread: ThreadId(2),
                title: "Garage et cave".into(),
            }]
        );
        // Sent: the row is a link again.
        assert!(
            !page.html().contains(r#"id="tl-2-title""#),
            "{}",
            page.html()
        );

        // Escape cancels.
        page.click("tl-4-menu");
        page.click("tl-4-rename");
        page.type_text("tl-4-title", "Autre");
        page.key("tl-4-title", Key::Escape, Code::Escape);
        assert!(
            !page.html().contains(r#"id="tl-4-title""#),
            "{}",
            page.html()
        );
        assert_eq!(sent().len(), 1);

        page.click("tl-2-menu");
        page.click("tl-2-archive");
        assert_eq!(
            sent()[1],
            ClientMsg::Archive {
                thread: ThreadId(2),
                archived: true,
            }
        );
        // An archived thread comes back.
        page.click("tl-5-menu");
        assert!(page.html().contains("Désarchiver"), "{}", page.html());
        page.click("tl-5-archive");
        assert_eq!(
            sent()[2],
            ClientMsg::Archive {
                thread: ThreadId(5),
                archived: false,
            }
        );

        // The main thread is renamed, never archived.
        page.click("tl-1-menu");
        let html = page.html();
        assert!(html.contains(r#"id="tl-1-rename""#), "{html}");
        assert!(!html.contains(r#"id="tl-1-archive""#), "{html}");
        // Escape closes the menu.
        page.key_in("tl-1-rename", Key::Escape, Code::Escape);
        assert!(
            !page.html().contains(r#"id="tl-1-rename""#),
            "{}",
            page.html()
        );
    }

    #[test]
    fn the_chat_header_renames_and_archives_its_thread() {
        let mut page = live(several(), View::Thread(ThreadId(4)));
        assert!(page.html().contains("<h1"), "{}", page.html());
        page.click("th-4-menu");
        page.click("th-4-rename");
        page.type_text("th-4-title", "Impôts 2026");
        page.key("th-4-title", Key::Enter, Code::Enter);
        page.click("th-4-menu");
        page.click("th-4-archive");
        assert_eq!(
            sent(),
            [
                ClientMsg::Rename {
                    thread: ThreadId(4),
                    title: "Impôts 2026".into(),
                },
                ClientMsg::Archive {
                    thread: ThreadId(4),
                    archived: true,
                },
            ]
        );
    }

    #[test]
    fn a_refusal_is_shown_until_dismissed() {
        let mut page = live(several(), View::Thread(ThreadId(2)));
        let refused = ServerMsg::ThreadRefused {
            thread: Some(ThreadId(2)),
            client_id: None,
            reason: "Cette conversation est archivée.".into(),
        };
        receive(&mut page, refused.clone());
        let html = page.html();
        assert!(html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains("Cette conversation est archivée."), "{html}");
        // The list page shows it too.
        page.click("tl-1");
        assert!(page.html().contains("Cette conversation est archivée."));

        let mut list = live(several(), View::List);
        receive(&mut list, refused);
        assert!(list.html().contains("Cette conversation est archivée."));
    }

    #[test]
    fn archived_thread_is_read_only() {
        let mut page = live(several(), View::Thread(ThreadId(5)));
        let html = page.html();
        assert!(html.contains("Conversation archivée"), "{html}");
        assert!(!html.contains(r#"id="message""#), "{html}");
        page.click("th-5-menu");
        page.click("th-5-archive");
        assert_eq!(
            sent(),
            [ClientMsg::Archive {
                thread: ThreadId(5),
                archived: false,
            }]
        );
    }

    #[test]
    fn a_child_thread_says_who_wrote_its_task() {
        let mut threads = several();
        threads.open(ThreadId(3));
        threads.apply(&hello(3, "trois", 1));
        // Text from agents is text: never markup.
        threads.apply(&user(1, "Mesure la porte <b>vite</b>", Some("Garage <i>")));
        let page = live(threads, View::Thread(ThreadId(3))).html();
        assert!(page.contains("Écrit par Garage &#60;i&#62;"), "{page}");
        assert!(
            page.contains("Mesure la porte &#60;b&#62;vite&#60;/b&#62;"),
            "{page}"
        );
        assert!(!page.contains("<b>"), "{page}");
    }

    #[test]
    fn a_sub_agents_result_links_to_its_thread() {
        let ended = |seq, outcome| {
            entry(
                seq,
                Event::ChildEnded {
                    thread: ThreadId(3),
                    title: "Mesurer <b>la porte</b>".into(),
                    outcome,
                    summary: "La porte fait 83 cm.".into(),
                    truncated: false,
                },
            )
        };
        let mut threads = several();
        threads.open(ThreadId(2));
        threads.apply(&hello(2, "deux", 3));
        threads.apply(&ended(1, fronde_protocol::TurnOutcome::Done));
        threads.apply(&entry(2, Event::Woken { child_ended: 1 }));
        threads.apply(&ended(3, fronde_protocol::TurnOutcome::Error));
        let page = live(threads, View::Thread(ThreadId(2))).html();
        let line = |label: &str| {
            let start = at(&page, label);
            page[start..start + page[start..].find("</p>").unwrap()].to_owned()
        };
        // A small line, the sub-agent's title a link to its thread; its answer is not shown.
        let done = line("Sous-agent terminé :");
        assert!(done.contains(r#"href="/t/3""#), "{done}");
        assert!(
            done.contains("Mesurer &#60;b&#62;la porte&#60;/b&#62;"),
            "{done}"
        );
        let failed = line("Sous-agent terminé (échec) :");
        assert!(failed.contains(r#"href="/t/3""#), "{failed}");
        assert!(!page.contains("83 cm"), "{page}");
        assert!(!page.contains("<b>"), "{page}");
        // The wake turn runs: the composer offers Stop.
        assert!(page.contains(r#"aria-label="Stop""#), "{page}");

        // A page without threads links the same way.
        let mut chat = Chat::default();
        chat.apply(&ended(1, fronde_protocol::TurnOutcome::Done));
        let mut dom = VirtualDom::new_with_props(Before, BeforeProps { chat });
        dom.rebuild_in_place();
        let page = dioxus_ssr::render(&dom);
        assert!(page.contains(r#"href="/t/3""#), "{page}");
        assert!(page.contains("Sous-agent terminé :"), "{page}");
    }

    #[test]
    fn main_chat_is_unchanged_with_one_thread() {
        let mut page = live(
            threads(vec![summary(1, "Fronde", "09:00")]),
            View::Thread(ThreadId::MAIN),
        );
        let html = page.html();
        // No list beside the chat, which is the one of before threads.
        assert!(!html.contains("<aside"), "{html}");
        assert!(html.contains("Fronde"), "{html}");
        assert!(html.contains(r#"aria-label="Se déconnecter""#), "{html}");
        let conversation = |html: &str| {
            let start = at(html, r#"<ol aria-label="Conversation""#);
            html[start..start + at(&html[start..], "</ol>")].to_owned()
        };
        let mut before = Chat::default();
        before.apply(&hello(1, "main", 2));
        before.apply(&user(1, "Salut", None));
        before.apply(&entry(
            2,
            Event::TurnEnded {
                outcome: fronde_protocol::TurnOutcome::Done,
            },
        ));
        let mut old = VirtualDom::new_with_props(Before, BeforeProps { chat: before });
        old.rebuild_in_place();
        assert_eq!(conversation(&html), conversation(&dioxus_ssr::render(&old)));

        // Messages go to the main thread as they did, unnamed.
        page.type_text("message", "Bonjour");
        page.key("message", Key::Enter, Code::Enter);
        assert_eq!(
            sent(),
            [ClientMsg::Send {
                text: "Bonjour".into(),
                thread: None,
            }]
        );
    }

    #[test]
    fn every_action_is_a_link_or_a_button() {
        let mut page = live(several(), View::Thread(ThreadId(2)));
        page.click("tl-2-menu");
        let html = page.html();
        // Reached by the keyboard as any link or button: nothing taken out of the tab order,
        // no click on anything else.
        assert!(!html.contains("tabindex"), "{html}");
        for id in ["thread-new", "tl-1", "tl-2", "tl-3", "tl-4", "tl-5"] {
            let start = at(&html, &format!(r#"id="{id}""#));
            let tag = &html[html[..start].rfind('<').unwrap()..start];
            assert!(tag.starts_with("<a "), "{id}: {tag}");
        }
        for id in ["tl-2-menu", "tl-2-rename", "tl-2-archive", "th-2-menu"] {
            let start = at(&html, &format!(r#"id="{id}""#));
            let tag = &html[html[..start].rfind('<').unwrap()..start];
            assert!(tag.starts_with("<button"), "{id}: {tag}");
        }
        // Every icon-only control has a name.
        for name in [
            "Conversations",
            "Nouvelle conversation",
            "Actions : Garage",
            "Se déconnecter",
        ] {
            assert!(
                html.contains(&format!(r#"aria-label="{name}""#)),
                "{name}: {html}"
            );
        }
        // Arrow keys move along the list; the links listen to them.
        page.key("tl-2", Key::ArrowDown, Code::ArrowDown);
        page.key("tl-3", Key::ArrowUp, Code::ArrowUp);
        // 44 px targets on a phone.
        assert!(html.contains("min-h-11"), "{html}");
        assert!(html.contains("size-11"), "{html}");
    }

    thread_local! {
        /// The tab titles the page under test set, oldest first.
        static TITLES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    /// A document that keeps the titles set, as a browser tab would show them.
    struct Tab;

    impl dioxus::document::Document for Tab {
        fn eval(&self, js: String) -> dioxus::document::Eval {
            dioxus::document::NoOpDocument.eval(js)
        }

        fn set_title(&self, title: String) {
            TITLES.with_borrow_mut(|titles| titles.push(title));
        }
    }

    /// The workspace in a tab.
    #[component]
    fn InTab(start: Threads, view: View) -> Element {
        use_context_provider(|| {
            std::rc::Rc::new(Tab) as std::rc::Rc<dyn dioxus::document::Document>
        });
        rsx! {
            Harness { start, view, status: Status::Open }
        }
    }

    fn tab_title() -> String {
        TITLES.with_borrow(|titles| titles.last().cloned().unwrap_or_default())
    }

    fn question(id: u64, text: &str) -> Event {
        Event::Request {
            id,
            card: fronde_protocol::Card::Form {
                message: String::new(),
                fields: vec![fronde_protocol::Field::Choice {
                    key: "q1".into(),
                    header: None,
                    question: text.into(),
                    options: vec![fronde_protocol::ChoiceOption {
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
            },
            from: None,
            server: false,
            woken_by: None,
        }
    }

    /// The tab says how many cards wait for an answer, in every thread not archived; the tray
    /// above the message box lists this thread's by title, then the other threads that wait.
    #[test]
    fn tab_title_counts_open_requests() {
        TITLES.with_borrow_mut(Vec::clear);
        let mut garage = summary(2, "Garage", "11:00");
        garage.open_requests = 2;
        let mut old = summary(3, "Vacances", "08:00");
        old.open_requests = 1;
        old.archived = true;
        let list = vec![summary(1, "Fronde", "09:00"), garage.clone(), old];
        let mut page = Live::new(VirtualDom::new_with_props(
            InTab,
            InTabProps {
                start: threads(list.clone()),
                view: View::Thread(ThreadId::MAIN),
            },
        ));
        assert_eq!(tab_title(), "(2) Fronde");
        let html = page.html();
        assert!(html.contains("2 demandes en attente"), "{html}");
        assert!(html.contains(r#"href="/t/2""#), "{html}");
        assert!(html.contains("Garage (2)"), "{html}");
        assert!(!html.contains("Vacances (1)"), "{html}");

        // A card of this thread: counted, and listed by its question; its row is where a tap
        // scrolls to.
        receive(&mut page, entry(3, question(7, "Quel agenda ?")));
        assert_eq!(tab_title(), "(3) Fronde");
        let html = page.html();
        assert!(html.contains("3 demandes en attente"), "{html}");
        let tray = &html[at(&html, r#"aria-label="Demandes en attente""#)..];
        assert!(tray.contains("Quel agenda ?"), "{tray}");
        assert!(html.contains(r#"id="request-7""#), "{html}");
        assert!(at(&html, r#"id="request-7""#) < at(&html, r#"aria-label="Demandes en attente""#));

        // Closed: no longer counted.
        receive(
            &mut page,
            entry(
                4,
                Event::RequestClosed {
                    id: 7,
                    outcome: fronde_protocol::Outcome::Declined,
                    by: None,
                },
            ),
        );
        assert_eq!(tab_title(), "(2) Fronde");
        // Nothing waits any more: the plain title, and no tray.
        garage.open_requests = 0;
        let list = vec![summary(1, "Fronde", "09:00"), garage];
        receive(&mut page, ServerMsg::Threads { threads: list });
        assert_eq!(tab_title(), "Fronde");
        assert!(!page.html().contains("en attente"), "{}", page.html());
    }
}
