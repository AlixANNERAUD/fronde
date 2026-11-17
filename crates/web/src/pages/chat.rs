//! `/` and `/t/<id>`: a thread's chat; `/new`: a chat whose first message makes a thread. The
//! server only serves them with a session; the conversation comes over `/ws`.

use std::collections::HashMap;

use dioxus::html::{ScrollBehavior, geometry::PixelsVector2D};
use dioxus::prelude::*;
use fronde_client::{Chat, Item, PermissionStatus, Threads, ToolStatus};
use fronde_protocol::{ClientMsg, ThreadId, ThreadSummary, TurnOutcome};

use crate::{
    components::icons::{ArrowUp, Check, LogOut, Menu, Plus, Square, Wrench, X},
    components::request_card::RequestCard,
    components::thread_list::{ICON_LINK, Place, PlaceLink, Renamer, ThreadMenu},
    components::ui::{
        alert::{Alert, AlertDescription, AlertVariant},
        badge::{Badge, BadgeVariant},
        bubble::{Bubble, BubbleAlign, BubbleContent, BubbleVariant},
        button::{Button, ButtonSize, ButtonVariant},
        card::{Card, CardContent, CardDescription, CardFooter, CardHeader, CardSize},
        input_prompt::{InputPrompt, InputPromptFooter, InputPromptSubmit, InputPromptTextarea},
        message::{Message, MessageAlign, MessageContent},
        scroll_area::ScrollAreaViewport,
        spinner::Spinner,
    },
    socket::{Outgoing, Status},
};

/// Whom the composer writes to.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Target {
    /// The main thread, unnamed, as a page without threads writes to it.
    #[default]
    Main,
    Thread(ThreadId),
    /// The thread the first message makes, through the page's threads.
    New(Signal<Threads>),
}

impl Target {
    /// Thread `id`'s; the main thread's stays unnamed.
    pub fn of(id: ThreadId) -> Self {
        if id == ThreadId::MAIN {
            Self::Main
        } else {
            Self::Thread(id)
        }
    }

    /// Whose draft and scroll the chat keeps: `None` for a thread not made yet.
    fn kept(self) -> Option<ThreadId> {
        match self {
            Self::Main => Some(ThreadId::MAIN),
            Self::Thread(id) => Some(id),
            Self::New(_) => None,
        }
    }

    /// The `thread` of the messages sent.
    fn thread(self) -> Option<ThreadId> {
        match self {
            Self::Thread(id) => Some(id),
            Self::Main | Self::New(_) => None,
        }
    }
}

/// The key of a new thread's first message, shown while the server makes the thread; no row's
/// key (`<generation>-<seq>`) looks like it.
const PENDING: &str = "pending";

/// What the page keeps of each thread while it shows another: the draft (`None`: a new
/// thread's) and where the chat was scrolled.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Kept {
    pub drafts: HashMap<Option<ThreadId>, String>,
    pub scrolls: HashMap<ThreadId, f64>,
}

/// The chat as drawn from its model: no socket, so it renders anywhere. Without `on_go` it is
/// the page of before threads: the main thread, no links.
#[component]
pub fn ChatView(
    chat: Chat,
    status: Status,
    on_send: EventHandler<Outgoing>,
    #[props(default)] target: Target,
    /// The thread as the list has it: its title and menu in the header; read-only if archived.
    #[props(default)]
    summary: Option<ThreadSummary>,
    #[props(default)] kept: Option<Signal<Kept>>,
    /// Links to the thread list and to a new thread.
    #[props(default)]
    on_go: Option<EventHandler<Place>>,
    /// The list sits beside the chat from `md` up: its link hides there.
    #[props(default)]
    sidebar: bool,
    /// A thread message the server refused, and how to dismiss it.
    #[props(default)]
    refusal: Option<String>,
    #[props(default)] on_dismiss: Option<EventHandler<()>>,
) -> Element {
    let open = status == Status::Open;
    let mut renaming = use_signal(|| false);
    let new = matches!(target, Target::New(_));
    let pending = match target {
        Target::New(threads) => threads.read().creating_text().map(str::to_owned),
        _ => None,
    };
    let archived = summary.as_ref().is_some_and(|summary| summary.archived);
    let title = if new {
        rsx! {
            h1 { class: "min-w-0 truncate font-semibold", "Nouvelle conversation" }
        }
    } else if let Some(summary) = &summary {
        if renaming() {
            rsx! {
                Renamer {
                    field_id: "th-{summary.id}-title",
                    id: summary.id,
                    title: summary.title.clone(),
                    open,
                    on_send,
                    on_done: move |()| renaming.set(false),
                }
            }
        } else {
            rsx! {
                h1 { class: "min-w-0 truncate font-semibold", "{summary.title}" }
                ThreadMenu {
                    prefix: "th-{summary.id}",
                    id: summary.id,
                    title: summary.title.clone(),
                    archived,
                    open,
                    on_rename: move |()| renaming.set(true),
                    on_send,
                }
            }
        }
    } else {
        rsx! {
            span { class: "font-semibold", "Fronde" }
        }
    };
    let list_link = if sidebar {
        format!("{ICON_LINK} md:hidden")
    } else {
        ICON_LINK.to_owned()
    };
    let scrolled = target.kept().filter(|_| kept.is_some());
    rsx! {
        div { class: "flex h-dvh min-w-0 flex-1 flex-col",
            header { class: "shrink-0 border-b pt-[env(safe-area-inset-top)]",
                div { class: "mx-auto flex h-12 max-w-3xl items-center gap-1 px-4",
                    if let Some(on_go) = on_go {
                        PlaceLink {
                            place: Place::List,
                            on_go,
                            class: list_link,
                            aria_label: "Conversations",
                            Menu {}
                        }
                    }
                    {title}
                    div { class: "ml-auto flex items-center gap-1",
                        if let Some(on_go) = on_go.filter(|_| !new) {
                            PlaceLink {
                                place: Place::New,
                                on_go,
                                class: ICON_LINK,
                                aria_label: "Nouvelle conversation",
                                Plus {}
                            }
                        }
                        form { method: "post", action: "/auth/sign-out",
                            Button {
                                variant: ButtonVariant::Ghost,
                                size: ButtonSize::Sm,
                                button_type: "submit",
                                aria_label: "Se déconnecter",
                                LogOut {}
                                "Se déconnecter"
                            }
                        }
                    }
                }
            }
            if let Some(reason) = refusal {
                div { class: "mx-auto w-full max-w-3xl shrink-0 px-4 pt-3",
                    Alert {
                        variant: AlertVariant::Destructive,
                        class: "flex items-center gap-2",
                        AlertDescription { class: "min-w-0 flex-1", "{reason}" }
                        if let Some(on_dismiss) = on_dismiss {
                            Button {
                                variant: ButtonVariant::Ghost,
                                size: ButtonSize::Icon,
                                class: "size-11 md:size-9",
                                aria_label: "Fermer",
                                onclick: move |_| on_dismiss.call(()),
                                X {}
                            }
                        }
                    }
                }
            }
            // Reversed, so the view starts at the end and stays there as the chat grows.
            ScrollAreaViewport {
                class: "flex min-h-0 flex-1 flex-col-reverse",
                onscroll: move |event: ScrollEvent| {
                    if let (Some(mut kept), Some(id)) = (kept, scrolled) {
                        kept.write().scrolls.insert(id, event.data().scroll_top());
                    }
                },
                onmounted: move |event: MountedEvent| async move {
                    let top = kept
                        .zip(scrolled)
                        .and_then(|(kept, id)| kept.peek().scrolls.get(&id).copied());
                    if let Some(top) = top.filter(|top| *top != 0.0) {
                        let to = PixelsVector2D::new(0.0, top);
                        let _ = event.data().scroll(to, ScrollBehavior::Instant).await;
                    }
                },
                ol {
                    aria_label: "Conversation",
                    class: "mx-auto flex w-full max-w-3xl flex-col gap-3 px-4 py-4",
                    for row in chat.rows() {
                        li { key: "{row.generation}-{row.seq}", class: "flex flex-col",
                            RowView {
                                item: row.item.clone(),
                                open,
                                admin: chat.admin(),
                                on_send,
                                on_go,
                            }
                        }
                    }
                    if let Some(text) = pending {
                        li { key: "{PENDING}", class: "flex flex-col gap-1",
                            RowView {
                                item: Item::User { text, by: None },
                                open,
                                admin: chat.admin(),
                                on_send,
                            }
                            p {
                                role: "status",
                                class: "text-muted-foreground flex items-center justify-end gap-2 text-xs",
                                Spinner { class: "size-3" }
                                "Création de la conversation…"
                            }
                        }
                    }
                }
            }
            div { class: "shrink-0 border-t px-4 pt-3 pb-[max(0.75rem,env(safe-area-inset-bottom))]",
                if archived {
                    p {
                        role: "status",
                        class: "text-muted-foreground mx-auto max-w-3xl py-2 text-center text-sm",
                        "Conversation archivée : désarchive-la pour lui écrire."
                    }
                } else {
                    Composer {
                        running: chat.running(),
                        status,
                        target,
                        kept,
                        on_send,
                    }
                }
            }
        }
    }
}

#[component]
fn RowView(
    item: Item,
    open: bool,
    admin: bool,
    on_send: EventHandler<Outgoing>,
    #[props(default)] on_go: Option<EventHandler<Place>>,
) -> Element {
    match item {
        Item::User { text, by } => rsx! {
            Message { align: MessageAlign::End,
                MessageContent {
                    // A sub-agent's task, written by its parent thread.
                    if let Some(by) = by {
                        p { class: "text-muted-foreground text-right text-xs", "Écrit par {by}" }
                    }
                    Bubble { align: BubbleAlign::End,
                        BubbleContent { class: "whitespace-pre-wrap", "{text}" }
                    }
                }
            }
        },
        Item::Assistant { text } => rsx! {
            Message {
                MessageContent {
                    Bubble { variant: BubbleVariant::Muted,
                        BubbleContent { class: "whitespace-pre-wrap", "{text}" }
                    }
                }
            }
        },
        Item::Tool { title, status, .. } => rsx! {
            ToolLine { title, status }
        },
        Item::Permission { title, status, .. } => rsx! {
            LegacyPermissionCard { title, status }
        },
        Item::Notice { text } => rsx! {
            p { class: "text-muted-foreground px-4 text-center text-xs", "{text}" }
        },
        Item::ChildEnded {
            thread,
            title,
            outcome,
        } => rsx! {
            ChildEndedLine { thread, title, outcome, on_go }
        },
        Item::Request {
            id,
            card,
            from,
            server,
            status,
            refusal,
            by,
        } => rsx! {
            RequestCard {
                id,
                card: *card,
                from,
                server,
                status,
                refusal,
                by,
                open,
                admin,
                on_send,
            }
        },
    }
}

/// How a sub-agent's turn ended, as its parent's chat says it.
fn child_ended_label(outcome: TurnOutcome) -> &'static str {
    match outcome {
        TurnOutcome::Done => "Sous-agent terminé",
        TurnOutcome::Cancelled => "Sous-agent terminé (interrompu)",
        TurnOutcome::Refused => "Sous-agent terminé (refus)",
        TurnOutcome::Limit => "Sous-agent terminé (limite atteinte)",
        TurnOutcome::Error => "Sous-agent terminé (échec)",
    }
}

const CHILD_LINK: &str = "hover:text-foreground focus-visible:ring-ring/50 min-w-0 truncate rounded-sm underline underline-offset-2 outline-none focus-visible:ring-[3px]";

/// A sub-agent's result woke the thread: a line linking to the sub-agent's thread.
#[component]
fn ChildEndedLine(
    thread: ThreadId,
    title: String,
    outcome: TurnOutcome,
    on_go: Option<EventHandler<Place>>,
) -> Element {
    let place = Place::Thread(thread);
    rsx! {
        p { class: "text-muted-foreground flex items-center justify-center gap-1 px-4 text-xs",
            span { class: "shrink-0", "{child_ended_label(outcome)} :" }
            if let Some(on_go) = on_go {
                PlaceLink { place, on_go, class: CHILD_LINK, "{title}" }
            } else {
                a { href: place.href(), class: CHILD_LINK, "{title}" }
            }
        }
    }
}

fn tool_line(title: &str, status: ToolStatus) -> String {
    let title = if title.is_empty() { "Outil" } else { title };
    match status {
        ToolStatus::Running => format!("{title}…"),
        ToolStatus::Done => title.to_owned(),
        ToolStatus::Failed => format!("{title} (échec)"),
        ToolStatus::Interrupted => format!("{title} (interrompu)"),
    }
}

#[component]
fn ToolLine(title: String, status: ToolStatus) -> Element {
    let icon = match status {
        ToolStatus::Running => rsx! { Spinner {} },
        ToolStatus::Done => rsx! { Check { class: "size-4" } },
        ToolStatus::Failed => rsx! { X { class: "size-4" } },
        ToolStatus::Interrupted => rsx! { Wrench { class: "size-4" } },
    };
    rsx! {
        p { class: "text-muted-foreground flex items-center gap-2 px-1 text-xs",
            {icon}
            span { class: "min-w-0 truncate", {tool_line(&title, status)} }
        }
    }
}

/// A permission request from before permissions were request cards, as an old log shows it.
/// It cannot be answered any more: the agent's call it guarded is gone.
#[component]
fn LegacyPermissionCard(title: String, status: PermissionStatus) -> Element {
    let badge = match status {
        PermissionStatus::Answered { allowed: true } => rsx! {
            Badge { "Autorisé" }
        },
        PermissionStatus::Answered { allowed: false } => rsx! {
            Badge { variant: BadgeVariant::Destructive, "Refusé" }
        },
        PermissionStatus::Open | PermissionStatus::Expired => rsx! {
            Badge { variant: BadgeVariant::Muted, "Expirée" }
        },
    };
    rsx! {
        Card { size: CardSize::Sm, class: "max-w-[85%]",
            CardHeader {
                CardDescription { class: "text-xs font-medium", "Fronde demande l'autorisation" }
            }
            CardContent {
                p { class: "text-sm wrap-break-word", "{title}" }
            }
            CardFooter { {badge} }
        }
    }
}

/// A new thread's `client_id`: random, so that no two pages ever share one.
#[cfg(target_arch = "wasm32")]
fn client_id() -> String {
    let random = || (js_sys::Math::random() * 2f64.powi(52)) as u64;
    format!("{:013x}{:013x}", random(), random())
}

/// Natively, in tests: one after the other.
#[cfg(not(target_arch = "wasm32"))]
fn client_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("page-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// The message box: Enter sends, Shift+Enter adds a line; Stop while a turn runs. Its draft is
/// kept in `kept` for when the page comes back to its thread.
#[component]
fn Composer(
    running: bool,
    status: Status,
    target: Target,
    kept: Option<Signal<Kept>>,
    on_send: EventHandler<Outgoing>,
) -> Element {
    let key = target.kept();
    let mut draft = use_signal(|| {
        kept.and_then(|kept| kept.peek().drafts.get(&key).cloned())
            .unwrap_or_default()
    });
    use_effect(move || {
        let text = draft();
        if let Some(mut kept) = kept {
            kept.write().drafts.insert(key, text);
        }
    });
    // The draft goes only once the message went out.
    let delivered = use_callback(move |()| draft.set(String::new()));
    let open = status == Status::Open;
    let creating = match target {
        Target::New(threads) => threads.read().creating(),
        _ => false,
    };
    let submit = move || {
        // The server refuses a message while a turn runs: the draft stays for later.
        if running || !open || creating {
            return;
        }
        let text = draft.peek().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let message = match target {
            Target::New(mut threads) => threads.write().create(client_id(), text),
            _ => ClientMsg::Send {
                text,
                thread: target.thread(),
            },
        };
        on_send.call(Outgoing {
            message,
            sent: Some(delivered),
            dropped: None,
        });
    };
    let placeholder = if matches!(target, Target::New(_)) {
        "Écris le premier message…"
    } else {
        "Écris à Fronde…"
    };
    rsx! {
        div { class: "mx-auto max-w-3xl",
            InputPrompt {
                InputPromptTextarea {
                    id: "message",
                    value: draft,
                    placeholder,
                    aria_label: "Message",
                    // 16 px at least on phones, or iOS zooms in on focus.
                    class: "text-base md:text-sm",
                    on_submit: move |()| submit(),
                }
                InputPromptFooter {
                    if status == Status::Reconnecting {
                        p {
                            role: "status",
                            class: "text-muted-foreground flex items-center gap-2 px-1 text-xs",
                            Spinner { class: "size-3" }
                            "Reconnexion…"
                        }
                    } else {
                        span {}
                    }
                    if running {
                        Button {
                            variant: ButtonVariant::Outline,
                            size: ButtonSize::Sm,
                            disabled: !open,
                            aria_label: "Stop",
                            onclick: move |_| {
                                on_send.call(Outgoing {
                                    message: ClientMsg::Cancel {
                                        thread: target.thread(),
                                    },
                                    sent: None,
                                    dropped: None,
                                })
                            },
                            Square { class: "size-3 fill-current" }
                            "Stop"
                        }
                    } else {
                        InputPromptSubmit {
                            disabled: !open || creating || draft().trim().is_empty(),
                            aria_label: "Envoyer",
                            onclick: move |_| submit(),
                            ArrowUp {}
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use dioxus::{
        dioxus_core::{ElementId, Mutation, Mutations, NoOpMutations},
        html::{
            Code, Key, Location, Modifiers, PlatformEventData, SerializedFormData,
            SerializedKeyboardData,
        },
    };

    use super::*;

    /// A chat that received these events, numbered from 1.
    fn chat(events: &[&str]) -> Chat {
        let mut chat = Chat::default();
        for (index, event) in events.iter().enumerate() {
            let seq = index + 1;
            let json = format!(
                r#"{{"type":"entry","seq":{seq},"at":"2026-11-16T10:00:00Z","event":{event}}}"#
            );
            chat.apply(&serde_json::from_str(&json).unwrap());
        }
        chat
    }

    thread_local! {
        /// What the page under test sent.
        static SENT: RefCell<Vec<ClientMsg>> = const { RefCell::new(Vec::new()) };
    }

    /// The chat page alone. `deliver`: whether the socket says each message went out.
    #[component]
    fn Harness(chat: Chat, status: Status, deliver: bool) -> Element {
        rsx! {
            ChatView {
                chat,
                status,
                on_send: move |outgoing: Outgoing| {
                    SENT.with_borrow_mut(|sent| sent.push(outgoing.message));
                    if deliver && let Some(sent) = outgoing.sent {
                        sent.call(());
                    }
                },
            }
        }
    }

    fn render(chat: Chat, status: Status) -> String {
        let props = HarnessProps {
            chat,
            status,
            deliver: true,
        };
        let mut dom = VirtualDom::new_with_props(Harness, props);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// The page's buttons: their accessible name, and whether they are disabled.
    fn buttons(page: &str) -> Vec<(String, bool)> {
        page.split("<button")
            .skip(1)
            .map(|button| {
                let (attributes, rest) = button.split_once('>').unwrap();
                let label = attributes
                    .split_once(r#"aria-label=""#)
                    .map(|(_, label)| label.split('"').next().unwrap().to_owned());
                let mut content = rest.split("</button>").next().unwrap();
                let mut text = String::new();
                while let Some((before, after)) = content.split_once('<') {
                    text.push_str(before);
                    content = &after[after.find('>').unwrap() + 1..];
                }
                text.push_str(content);
                let name = label.unwrap_or_else(|| text.trim().to_owned());
                (name, attributes.contains("disabled=true"))
            })
            .collect()
    }

    fn button(page: &str, name: &str) -> Option<bool> {
        buttons(page)
            .into_iter()
            .find(|(label, _)| label == name)
            .map(|(_, disabled)| disabled)
    }

    /// The page under test, live: events can be sent to it.
    struct Live {
        dom: VirtualDom,
        listeners: Vec<(String, ElementId)>,
    }

    impl Live {
        fn new(chat: Chat, status: Status, deliver: bool) -> Self {
            // Events are built from plain data, as they would be from a browser's.
            dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
            SENT.with_borrow_mut(Vec::clear);
            let props = HarnessProps {
                chat,
                status,
                deliver,
            };
            let mut dom = VirtualDom::new_with_props(Harness, props);
            let mut mutations = Mutations::default();
            dom.rebuild(&mut mutations);
            let listeners = mutations
                .edits
                .into_iter()
                .filter_map(|edit| match edit {
                    Mutation::NewEventListener { name, id } => Some((name, id)),
                    _ => None,
                })
                .collect();
            Self { dom, listeners }
        }

        /// Sends the event to the one element that listens to it.
        fn send(&mut self, name: &str, data: impl std::any::Any) {
            let ids: Vec<ElementId> = self
                .listeners
                .iter()
                .filter(|(listened, _)| listened == name)
                .map(|(_, id)| *id)
                .collect();
            assert_eq!(ids.len(), 1, "listeners of {name}");
            let data: Rc<dyn std::any::Any> = Rc::new(PlatformEventData::new(Box::new(data)));
            self.dom
                .runtime()
                .handle_event(name, Event::new(data, true), ids[0]);
            self.dom.render_immediate(&mut NoOpMutations);
        }

        fn type_text(&mut self, text: &str) {
            self.send(
                "input",
                SerializedFormData::new(text.to_owned(), Vec::new()),
            );
        }

        fn press_enter(&mut self, composing: bool) {
            let key = SerializedKeyboardData::new(
                Key::Enter,
                Code::Enter,
                Location::Standard,
                false,
                Modifiers::empty(),
                composing,
            );
            self.send("keydown", key);
        }

        fn html(&self) -> String {
            dioxus_ssr::render(&self.dom)
        }
    }

    fn sent() -> Vec<ClientMsg> {
        SENT.with_borrow(Clone::clone)
    }

    const USER: &str = r#"{"type":"user","text":"Salut\nça va ?"}"#;
    const PERMISSION: &str = r#"{"type":"permission","id":2,"title":"Écrire dans la mémoire"}"#;
    const ENDED: &str = r#"{"type":"turn_ended","outcome":"done"}"#;

    #[test]
    fn chat_renders_bubbles_from_model() {
        let page = render(
            chat(&[
                USER,
                r#"{"type":"assistant","text":"Bonjour"}"#,
                r#"{"type":"tool","id":"t","title":"Lit la mémoire","state":"running"}"#,
                r#"{"type":"assistant","text":"Rien."}"#,
                r#"{"type":"turn_ended","outcome":"cancelled"}"#,
            ]),
            Status::Open,
        );
        let messages: Vec<&str> = page.split(r#"data-name="Message""#).skip(1).collect();
        assert_eq!(messages.len(), 3, "{page}");
        // The user on the right, Fronde on the left.
        assert!(messages[0].contains(r#"data-align="End""#), "{page}");
        assert!(messages[0].contains("Salut\nça va ?"), "{page}");
        assert!(messages[1].contains(r#"data-align="Start""#), "{page}");
        assert!(messages[1].contains("Bonjour"), "{page}");
        assert!(messages[2].contains("Rien."), "{page}");
        // The tool call the turn left running, then why the turn ended.
        assert!(page.contains("Lit la mémoire (interrompu)"), "{page}");
        assert!(page.contains("Réponse interrompue."), "{page}");
    }

    const PERMISSION_REQUEST: &str = r#"{"type":"request","id":2,"card":{"kind":"permission","title":"Écrire dans la mémoire","options":[{"id":"allow-once","kind":"allow_once"},{"id":"reject","kind":"reject_once"}]}}"#;

    #[test]
    fn permission_card_disabled_after_turn() {
        // A permission is a request card: answerable while the socket is open.
        let running = chat(&[USER, PERMISSION_REQUEST]);
        let page = render(running.clone(), Status::Open);
        assert!(page.contains("Écrire dans la mémoire"), "{page}");
        assert_eq!(button(&page, "Autoriser une fois"), Some(false), "{page}");
        assert_eq!(button(&page, "Refuser"), Some(false), "{page}");

        // Nothing can be answered while the socket is closed.
        let page = render(running, Status::Reconnecting);
        assert_eq!(button(&page, "Autoriser une fois"), Some(true), "{page}");
        assert_eq!(button(&page, "Refuser"), Some(true), "{page}");

        // Once the server closed it, it is closed for good; the turn's end alone does not
        // close it: the server cancels it then.
        let page = render(chat(&[USER, PERMISSION_REQUEST, ENDED]), Status::Open);
        assert_eq!(button(&page, "Refuser"), Some(false), "{page}");
        let cancelled = r#"{"type":"request_closed","id":2,"outcome":{"type":"cancelled"}}"#;
        let page = render(
            chat(&[USER, PERMISSION_REQUEST, cancelled, ENDED]),
            Status::Open,
        );
        assert!(page.contains("Écrire dans la mémoire"), "{page}");
        assert_eq!(button(&page, "Autoriser une fois"), None, "{page}");
        assert_eq!(button(&page, "Refuser"), None, "{page}");
        assert!(page.contains("Annulée"), "{page}");
    }

    #[test]
    fn legacy_permission_entries_replay_without_buttons() {
        // An old log's permission, even while its turn seems to run: nothing answers it now.
        let running = chat(&[USER, PERMISSION]);
        let page = render(running, Status::Open);
        assert!(page.contains("Écrire dans la mémoire"), "{page}");
        assert_eq!(button(&page, "Autoriser"), None, "{page}");
        assert_eq!(button(&page, "Refuser"), None, "{page}");
        assert!(page.contains("Expirée"), "{page}");

        // Once its turn ended, the request is closed for good.
        let page = render(chat(&[USER, PERMISSION, ENDED]), Status::Open);
        assert!(page.contains("Écrire dans la mémoire"), "{page}");
        assert_eq!(button(&page, "Autoriser"), None, "{page}");
        assert_eq!(button(&page, "Refuser"), None, "{page}");
        assert!(page.contains("Expirée"), "{page}");

        let answered = r#"{"type":"permission_answered","id":2,"allowed":false}"#;
        let page = render(chat(&[USER, PERMISSION, answered]), Status::Open);
        assert_eq!(button(&page, "Autoriser"), None, "{page}");
        assert!(page.contains("Refusé"), "{page}");
    }

    #[test]
    fn question_card_stays_open_until_closed() {
        let request = r#"{"type":"request","id":3,"card":{"kind":"form","fields":[{"type":"choice","key":"q1","header":"Appli","question":"Quelle appli ?","options":[{"label":"Mealie"},{"label":"Seerr"}],"other":true,"required":true}]}}"#;
        // Asked during a turn, still open once the turn ended: the server closes it.
        let page = render(chat(&[USER, request, ENDED]), Status::Open);
        assert!(page.contains("Quelle appli ?"), "{page}");
        assert_eq!(button(&page, "Envoyer"), Some(false), "{page}");
        assert_eq!(button(&page, "Refuser"), Some(false), "{page}");

        let closed = r#"{"type":"request_closed","id":3,"outcome":{"type":"answered","summary":"Seerr"},"by":"alix"}"#;
        let page = render(chat(&[USER, request, ENDED, closed]), Status::Open);
        assert!(page.contains("Réponse : Seerr"), "{page}");
        assert_eq!(button(&page, "Refuser"), None, "{page}");
    }

    #[test]
    fn composer_follows_the_connection() {
        let idle = chat(&[USER, ENDED]);
        let page = render(idle.clone(), Status::Open);
        // Nothing to send yet.
        assert_eq!(button(&page, "Envoyer"), Some(true), "{page}");
        assert_eq!(button(&page, "Stop"), None, "{page}");
        assert!(!page.contains("Reconnexion…"), "{page}");

        let page = render(Chat::default(), Status::Connecting);
        assert!(!page.contains("Reconnexion…"), "{page}");

        let running = chat(&[USER]);
        let page = render(running.clone(), Status::Open);
        assert_eq!(button(&page, "Stop"), Some(false), "{page}");
        assert_eq!(button(&page, "Envoyer"), None, "{page}");

        let page = render(running, Status::Reconnecting);
        assert!(page.contains("Reconnexion…"), "{page}");
        assert_eq!(button(&page, "Stop"), Some(true), "{page}");
    }

    #[test]
    fn enter_sends_unless_composing() {
        let mut page = Live::new(chat(&[USER, ENDED]), Status::Open, true);
        page.type_text("  Bonjour  ");
        // Enter that confirms an input method's text is not a send.
        page.press_enter(true);
        assert_eq!(sent(), []);
        page.press_enter(false);
        assert_eq!(
            sent(),
            [ClientMsg::Send {
                text: "Bonjour".into(),
                thread: None,
            }]
        );
        assert!(page.html().contains(r#"value="""#), "{}", page.html());
    }

    #[test]
    fn draft_stays_until_sent() {
        // The socket closed in between: nothing went out.
        let mut page = Live::new(chat(&[USER, ENDED]), Status::Open, false);
        page.type_text("Bonjour");
        page.press_enter(false);
        assert_eq!(sent().len(), 1);
        assert!(
            page.html().contains(r#"value="Bonjour""#),
            "{}",
            page.html()
        );

        // A turn runs: Enter keeps the draft and sends nothing.
        let mut page = Live::new(chat(&[USER]), Status::Open, true);
        page.type_text("Encore");
        page.press_enter(false);
        assert_eq!(sent(), []);
        assert!(page.html().contains(r#"value="Encore""#), "{}", page.html());
    }

    #[test]
    fn labels_are_french_without_icon_titles() {
        let tools = [
            r#"{"type":"tool","id":"a","title":"A","state":"running"}"#,
            r#"{"type":"tool","id":"b","title":"B","state":"running"}"#,
            r#"{"type":"tool","id":"b","title":"","state":"done"}"#,
            r#"{"type":"tool","id":"c","title":"C","state":"running"}"#,
            r#"{"type":"tool","id":"c","title":"","state":"failed"}"#,
        ];
        let mut events = vec![USER];
        events.extend(tools);
        let running = chat(&events);
        events.push(ENDED);
        for page in [
            render(running.clone(), Status::Open),
            render(running, Status::Reconnecting),
            render(chat(&events), Status::Open),
        ] {
            assert!(!page.contains("Rust/UI Icons"), "{page}");
            assert!(!page.contains("<title"), "{page}");
            assert!(page.contains(r#"aria-label="Message""#), "{page}");
            assert!(button(&page, "Se déconnecter").is_some(), "{page}");
        }
    }
}
