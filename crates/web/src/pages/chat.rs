//! `/`: the chat. The server only serves it with a session; the conversation comes over `/ws`.

use dioxus::prelude::*;
use icons::{ArrowUp, Check, LogOut, Square, Wrench, X};
use jarvis_client::{Chat, Item, PermissionStatus, ToolStatus};
use jarvis_protocol::ClientMsg;

use crate::{
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
    socket::{Status, use_chat_socket},
};

#[component]
pub fn ChatPage() -> Element {
    let chat = use_signal(Chat::default);
    let status = use_signal(|| Status::Connecting);
    let socket = use_chat_socket(chat, status);
    rsx! {
        ChatView { chat: chat(), status: status(), on_send: move |message| socket.send(message) }
    }
}

/// The chat as drawn from its model: no socket, so it renders anywhere.
#[component]
pub fn ChatView(chat: Chat, status: Status, on_send: EventHandler<ClientMsg>) -> Element {
    let open = status == Status::Open;
    rsx! {
        div { class: "flex h-dvh flex-col",
            header { class: "shrink-0 border-b pt-[env(safe-area-inset-top)]",
                div { class: "mx-auto flex h-12 max-w-3xl items-center gap-3 px-4",
                    span { class: "font-semibold", "Jarvis" }
                    form { method: "post", action: "/sign-out", class: "ml-auto",
                        Button {
                            variant: ButtonVariant::Ghost,
                            size: ButtonSize::Sm,
                            button_type: "submit",
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
                        li { key: "{row.generation}-{row.seq}-{row.version}", class: "flex flex-col",
                            RowView { item: row.item.clone(), open, on_send }
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
fn RowView(item: Item, open: bool, on_send: EventHandler<ClientMsg>) -> Element {
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
        Item::Permission { id, title, status } => rsx! {
            PermissionCard { id, title, status, open, on_send }
        },
        Item::Notice { text } => rsx! {
            p { class: "text-muted-foreground px-4 text-center text-xs", "{text}" }
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

/// A request to use a tool: answerable while its turn runs and the socket is open, once.
#[component]
fn PermissionCard(
    id: u64,
    title: String,
    status: PermissionStatus,
    open: bool,
    on_send: EventHandler<ClientMsg>,
) -> Element {
    // Answered: the card changes when the server logs the answer.
    let mut sent = use_signal(|| false);
    let disabled = sent() || !open;
    let mut answer = move |allow: bool| {
        sent.set(true);
        on_send.call(ClientMsg::Answer { id, allow });
    };
    let footer = match status {
        PermissionStatus::Open => rsx! {
            Button { size: ButtonSize::Sm, disabled, onclick: move |_| answer(true), "Autoriser" }
            Button {
                variant: ButtonVariant::Outline,
                size: ButtonSize::Sm,
                disabled,
                onclick: move |_| answer(false),
                "Refuser"
            }
        },
        PermissionStatus::Answered { allowed: true } => rsx! {
            Badge { "Autorisé" }
        },
        PermissionStatus::Answered { allowed: false } => rsx! {
            Badge { variant: BadgeVariant::Destructive, "Refusé" }
        },
        PermissionStatus::Expired => rsx! {
            Badge { variant: BadgeVariant::Muted, "Expirée" }
        },
    };
    rsx! {
        Card { size: CardSize::Sm, class: "max-w-[85%]",
            CardHeader {
                CardDescription { class: "text-xs font-medium", "Jarvis demande l'autorisation" }
            }
            CardContent {
                p { class: "text-sm wrap-break-word", "{title}" }
            }
            CardFooter { {footer} }
        }
    }
}

/// The message box: Enter sends, Shift+Enter adds a line; Stop while a turn runs.
#[component]
fn Composer(running: bool, status: Status, on_send: EventHandler<ClientMsg>) -> Element {
    let mut draft = use_signal(String::new);
    let open = status == Status::Open;
    let mut submit = move || {
        // The server refuses a message while a turn runs: the draft stays for later.
        if running || !open {
            return;
        }
        let text = draft.peek().trim().to_owned();
        if !text.is_empty() {
            on_send.call(ClientMsg::Send { text });
            draft.set(String::new());
        }
    };
    rsx! {
        div { class: "mx-auto max-w-3xl",
            InputPrompt {
                InputPromptTextarea {
                    value: draft,
                    placeholder: "Écris à Jarvis…",
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
                            onclick: move |_| on_send.call(ClientMsg::Cancel),
                            Square { class: "size-3 fill-current" }
                            "Stop"
                        }
                    } else {
                        InputPromptSubmit {
                            disabled: !open || draft().trim().is_empty(),
                            onclick: move |_| submit(),
                            ArrowUp {}
                            span { class: "sr-only", "Envoyer" }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
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

    fn render(chat: Chat, status: Status) -> String {
        #[component]
        fn Harness(chat: Chat, status: Status) -> Element {
            rsx! { ChatView { chat, status, on_send: |_| {} } }
        }
        let mut dom = VirtualDom::new_with_props(Harness, HarnessProps { chat, status });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// The page's buttons: their text, and whether they are disabled.
    fn buttons(page: &str) -> Vec<(String, bool)> {
        page.split("<button")
            .skip(1)
            .map(|button| {
                let (attributes, rest) = button.split_once('>').unwrap();
                let mut content = rest.split("</button>").next().unwrap();
                // The text only: no tags, and no icon titles.
                let mut text = String::new();
                while let Some((before, after)) = content.split_once('<') {
                    text.push_str(before);
                    let tag_end = if after.starts_with("title>") {
                        after.find("</title>").unwrap() + "</title>".len()
                    } else {
                        after.find('>').unwrap() + 1
                    };
                    content = &after[tag_end..];
                }
                text.push_str(content);
                (text.trim().to_owned(), attributes.contains("disabled=true"))
            })
            .collect()
    }

    fn button(page: &str, text: &str) -> Option<bool> {
        buttons(page)
            .into_iter()
            .find(|(label, _)| label == text)
            .map(|(_, disabled)| disabled)
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
        // The user on the right, Jarvis on the left.
        assert!(messages[0].contains(r#"data-align="End""#), "{page}");
        assert!(messages[0].contains("Salut\nça va ?"), "{page}");
        assert!(messages[1].contains(r#"data-align="Start""#), "{page}");
        assert!(messages[1].contains("Bonjour"), "{page}");
        assert!(messages[2].contains("Rien."), "{page}");
        // The tool call the turn left running, then why the turn ended.
        assert!(page.contains("Lit la mémoire (interrompu)"), "{page}");
        assert!(page.contains("Réponse interrompue."), "{page}");
    }

    #[test]
    fn permission_card_disabled_after_turn() {
        let running = chat(&[USER, PERMISSION]);
        let page = render(running.clone(), Status::Open);
        assert!(page.contains("Écrire dans la mémoire"), "{page}");
        assert_eq!(button(&page, "Autoriser"), Some(false), "{page}");
        assert_eq!(button(&page, "Refuser"), Some(false), "{page}");

        // Nothing can be answered while the socket is closed.
        let page = render(running, Status::Reconnecting);
        assert_eq!(button(&page, "Autoriser"), Some(true), "{page}");
        assert_eq!(button(&page, "Refuser"), Some(true), "{page}");

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
}
