//! `/`: the chat. The server only serves it with a session; the conversation comes over `/ws`.

use dioxus::prelude::*;
use fronde_client::{Chat, Item, PermissionStatus, ToolStatus};
use fronde_protocol::ClientMsg;

use crate::{
    components::icons::{ArrowUp, Check, LogOut, Square, Wrench, X},
    components::request_card::RequestCard,
    components::ui::{
        badge::{Badge, BadgeVariant},
        bubble::{Bubble, BubbleAlign, BubbleContent, BubbleVariant},
        button::{Button, ButtonSize, ButtonVariant},
        card::{Card, CardContent, CardDescription, CardFooter, CardHeader, CardSize},
        input_prompt::{InputPrompt, InputPromptFooter, InputPromptSubmit, InputPromptTextarea},
        message::{Message, MessageAlign, MessageContent},
        scroll_area::ScrollAreaViewport,
        spinner::Spinner,
    },
    socket::{Outgoing, Status, use_chat_socket},
};

#[component]
pub fn ChatPage() -> Element {
    let chat = use_signal(Chat::default);
    let status = use_signal(|| Status::Connecting);
    let socket = use_chat_socket(chat, status);
    rsx! {
        ChatView { chat: chat(), status: status(), on_send: move |outgoing| socket.send(outgoing) }
    }
}

/// The chat as drawn from its model: no socket, so it renders anywhere.
#[component]
pub fn ChatView(chat: Chat, status: Status, on_send: EventHandler<Outgoing>) -> Element {
    let open = status == Status::Open;
    rsx! {
        div { class: "flex h-dvh flex-col",
            header { class: "shrink-0 border-b pt-[env(safe-area-inset-top)]",
                div { class: "mx-auto flex h-12 max-w-3xl items-center gap-3 px-4",
                    span { class: "font-semibold", "Fronde" }
                    form { method: "post", action: "/auth/sign-out", class: "ml-auto",
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
            // Reversed, so the view starts at the end and stays there as the chat grows.
            ScrollAreaViewport { class: "flex min-h-0 flex-1 flex-col-reverse",
                ol {
                    aria_label: "Conversation",
                    class: "mx-auto flex w-full max-w-3xl flex-col gap-3 px-4 py-4",
                    for row in chat.rows() {
                        li { key: "{row.generation}-{row.seq}", class: "flex flex-col",
                            RowView { item: row.item.clone(), open, admin: chat.admin(), on_send }
                        }
                    }
                }
            }
            div { class: "shrink-0 border-t px-4 pt-3 pb-[max(0.75rem,env(safe-area-inset-bottom))]",
                Composer { running: chat.running(), status, on_send }
            }
        }
    }
}

#[component]
fn RowView(item: Item, open: bool, admin: bool, on_send: EventHandler<Outgoing>) -> Element {
    match item {
        Item::User { text } => rsx! {
            Message { align: MessageAlign::End,
                MessageContent {
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

/// The message box: Enter sends, Shift+Enter adds a line; Stop while a turn runs.
#[component]
fn Composer(running: bool, status: Status, on_send: EventHandler<Outgoing>) -> Element {
    let mut draft = use_signal(String::new);
    // The draft goes only once the message went out.
    let delivered = use_callback(move |()| draft.set(String::new()));
    let open = status == Status::Open;
    let submit = move || {
        // The server refuses a message while a turn runs: the draft stays for later.
        if running || !open {
            return;
        }
        let text = draft.peek().trim().to_owned();
        if !text.is_empty() {
            on_send.call(Outgoing {
                message: ClientMsg::Send { text },
                sent: Some(delivered),
                dropped: None,
            });
        }
    };
    rsx! {
        div { class: "mx-auto max-w-3xl",
            InputPrompt {
                InputPromptTextarea {
                    value: draft,
                    placeholder: "Écris à Fronde…",
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
                                    message: ClientMsg::Cancel,
                                    sent: None,
                                    dropped: None,
                                })
                            },
                            Square { class: "size-3 fill-current" }
                            "Stop"
                        }
                    } else {
                        InputPromptSubmit {
                            disabled: !open || draft().trim().is_empty(),
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
                text: "Bonjour".into()
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
