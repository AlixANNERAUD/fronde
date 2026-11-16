//! The chat: the conversation streamed over `/ws`, and the composer. The server renders it
//! empty; in the browser it connects, says hello with the last seq it has, and keeps the
//! entries it receives. It reconnects after 1 s, then waits twice as long each time up to 30 s,
//! and at once when the page comes back to the foreground.

use std::time::Duration;

use jarvis_protocol::{CLOSE_SIGNED_OUT, ClientMsg, ServerMsg};
use leptos::{ev, prelude::*};

use super::browser::{self, Socket};
use crate::{
    chat_model::{Chat, Item, PermissionStatus, Row, ToolStatus},
    components::{
        icons::{Icon, IconKind},
        ui::{
            Badge, BadgeVariant, Bubble, ButtonSize, ButtonVariant, Side, Spinner, TEXTAREA,
            button_class,
        },
    },
};

const FIRST_RETRY: Duration = Duration::from_secs(1);
const LAST_RETRY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The first connection, before it opened.
    Connecting,
    Open,
    /// Closed, waiting to reconnect.
    Retrying,
}

/// The chat's state and its socket; every field is a handle, so it is `Copy` into callbacks.
#[derive(Clone, Copy)]
struct Connection {
    chat: RwSignal<Chat>,
    phase: RwSignal<Phase>,
    socket: StoredValue<Option<Socket>, LocalStorage>,
    retry: StoredValue<Duration>,
    /// Counts connections, so the callbacks of a replaced socket do nothing.
    attempt: StoredValue<u64>,
}

impl Connection {
    fn open(self) {
        let attempt = self.attempt.get_value() + 1;
        self.attempt.set_value(attempt);
        let current = move || self.attempt.get_value() == attempt;
        let socket = Socket::open(
            "/ws",
            move || {
                if current() {
                    self.phase.set(Phase::Open);
                    self.retry.set_value(FIRST_RETRY);
                    let last_seq = self.chat.try_update_untracked(Chat::hello).unwrap_or(0);
                    self.send(&ClientMsg::Hello { last_seq });
                }
            },
            move |text| {
                let Ok(ServerMsg::Entry(entry)) = serde_json::from_str(&text) else {
                    return;
                };
                if current() {
                    let follow = browser::near_end();
                    self.chat.update(|chat| chat.apply(entry));
                    if follow {
                        browser::scroll_to_end_soon();
                    }
                }
            },
            move |code| {
                if !current() {
                    return;
                }
                if code == CLOSE_SIGNED_OUT {
                    // The session expired, or the server restarted and forgot it.
                    browser::go_to("/sign-in");
                } else {
                    self.retry_later(attempt);
                }
            },
        );
        let opened = socket.is_some();
        // Replacing the previous socket drops it, which closes it.
        self.socket.set_value(socket);
        if !opened {
            self.retry_later(attempt);
        }
    }

    fn retry_later(self, attempt: u64) {
        self.phase.set(Phase::Retrying);
        let delay = self.retry.get_value();
        self.retry.set_value((delay * 2).min(LAST_RETRY));
        browser::after(delay, move || {
            if self.attempt.get_value() == attempt {
                self.open();
            }
        });
    }

    /// Back in the foreground: a closed connection is retried at once.
    fn wake(self) {
        if self.phase.get_untracked() == Phase::Retrying {
            self.retry.set_value(FIRST_RETRY);
            self.open();
        }
    }

    /// False when the socket is not open: nothing was sent.
    fn send(self, message: &ClientMsg) -> bool {
        let Ok(json) = serde_json::to_string(message) else {
            return false;
        };
        self.socket
            .with_value(|socket| socket.as_ref().is_some_and(|socket| socket.send(&json)))
    }

    fn is_open(self) -> bool {
        self.phase.get() == Phase::Open
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
fn ToolLine(title: String, status: ToolStatus) -> impl IntoView {
    let icon = match status {
        ToolStatus::Running => view! { <Spinner /> }.into_any(),
        ToolStatus::Done => view! { <Icon kind=IconKind::Check /> }.into_any(),
        ToolStatus::Failed => view! { <Icon kind=IconKind::Cross /> }.into_any(),
        ToolStatus::Interrupted => view! { <Icon kind=IconKind::Wrench /> }.into_any(),
    };
    view! {
        <p class="text-muted-foreground flex items-center gap-2 px-1 text-xs">
            {icon}
            <span class="min-w-0 truncate">{tool_line(&title, status)}</span>
        </p>
    }
}

#[component]
fn PermissionCard(
    id: String,
    title: String,
    status: PermissionStatus,
    connection: Connection,
) -> impl IntoView {
    // Answered once: the card changes when the server logs the answer.
    let sent = RwSignal::new(false);
    let answer = move |allow: bool| {
        // The server numbers its requests; anything else cannot be answered.
        let id = id.parse::<u64>().ok();
        move |_: ev::MouseEvent| {
            if let Some(id) = id
                && connection.send(&ClientMsg::Answer { id, allow })
            {
                sent.set(true);
            }
        }
    };
    let disabled = move || sent.get() || !connection.is_open();
    let footer = match status {
        PermissionStatus::Open => view! {
            <div class="flex gap-2">
                <button
                    type="button"
                    class=button_class(ButtonVariant::Default, ButtonSize::Small)
                    disabled=disabled
                    on:click=answer(true)
                >
                    "Autoriser"
                </button>
                <button
                    type="button"
                    class=button_class(ButtonVariant::Outline, ButtonSize::Small)
                    disabled=disabled
                    on:click=answer(false)
                >
                    "Refuser"
                </button>
            </div>
        }
        .into_any(),
        PermissionStatus::Answered { allowed: true } => {
            view! { <Badge>"Autorisé"</Badge> }.into_any()
        }
        PermissionStatus::Answered { allowed: false } => {
            view! { <Badge variant=BadgeVariant::Destructive>"Refusé"</Badge> }.into_any()
        }
        PermissionStatus::Expired => {
            view! { <Badge variant=BadgeVariant::Muted>"Expirée"</Badge> }.into_any()
        }
    };
    view! {
        <div class="bg-card text-card-foreground grid max-w-[85%] gap-3 rounded-xl border p-4 shadow-sm">
            <p class="text-muted-foreground text-xs font-medium">"Jarvis demande l'autorisation"</p>
            <p class="text-sm wrap-break-word">{title}</p>
            {footer}
        </div>
    }
}

fn row_view(row: Row, connection: Connection) -> AnyView {
    match row.item {
        Item::User { text } => view! { <Bubble side=Side::End text /> }.into_any(),
        Item::Assistant { text } => view! { <Bubble side=Side::Start text /> }.into_any(),
        Item::Tool { title, status, .. } => view! { <ToolLine title status /> }.into_any(),
        Item::Permission { id, title, status } => {
            view! { <PermissionCard id title status connection /> }.into_any()
        }
        Item::Notice { text } => view! {
            <p class="text-muted-foreground px-4 text-center text-xs">{text}</p>
        }
        .into_any(),
    }
}

#[island]
pub fn ChatPanel() -> impl IntoView {
    let connection = Connection {
        chat: RwSignal::new(Chat::default()),
        phase: RwSignal::new(Phase::Connecting),
        socket: StoredValue::new_local(None),
        retry: StoredValue::new(FIRST_RETRY),
        attempt: StoredValue::new(0),
    };
    let chat = connection.chat;
    let draft = RwSignal::new(String::new());
    Effect::new(move |_| {
        untrack(|| connection.open());
        browser::on_visible(move || connection.wake());
    });
    let submit = move || {
        // The server refuses a message while a turn runs: the draft stays for later.
        if chat.with_untracked(Chat::running) {
            return;
        }
        let text = draft.with_untracked(|draft| draft.trim().to_owned());
        if !text.is_empty() && connection.send(&ClientMsg::Send { text }) {
            draft.set(String::new());
        }
    };
    let on_keydown = move |event: ev::KeyboardEvent| {
        // Enter sends; Shift+Enter, and Enter while an input method composes, add a line.
        if event.key() == "Enter" && !event.shift_key() && !event.is_composing() {
            event.prevent_default();
            submit();
        }
    };
    let action = move || {
        if chat.with(Chat::running) {
            view! {
                <button
                    type="button"
                    class=button_class(ButtonVariant::Outline, ButtonSize::Default)
                    disabled=move || !connection.is_open()
                    on:click=move |_| {
                        connection.send(&ClientMsg::Cancel);
                    }
                >
                    <Icon kind=IconKind::Stop />
                    "Stop"
                </button>
            }
            .into_any()
        } else {
            view! {
                <button
                    type="submit"
                    aria-label="Envoyer"
                    class=button_class(ButtonVariant::Default, ButtonSize::Icon)
                    disabled=move || {
                        !connection.is_open() || draft.with(|draft| draft.trim().is_empty())
                    }
                >
                    <Icon kind=IconKind::Send />
                </button>
            }
            .into_any()
        }
    };
    let status = move || {
        (connection.phase.get() == Phase::Retrying).then(|| {
            view! {
                <p role="status" class="text-muted-foreground mb-2 flex items-center gap-2 text-xs">
                    <Spinner class="size-3" />
                    "Reconnexion…"
                </p>
            }
        })
    };
    view! {
        <div class="flex min-h-[calc(100dvh-3rem)] flex-col">
            <ol aria-label="Conversation" class="flex flex-1 flex-col gap-3 px-4 py-4">
                <For
                    each=move || chat.with(|chat| chat.rows().to_vec())
                    key=Row::key
                    children=move |row| view! { <li class="flex flex-col">{row_view(row, connection)}</li> }
                />
            </ol>
            <div class="bg-background/95 supports-[backdrop-filter]:bg-background/80 sticky bottom-0 border-t px-4 pt-3 pb-[max(0.75rem,env(safe-area-inset-bottom))] backdrop-blur">
                {status}
                <form
                    class="flex items-end gap-2"
                    on:submit=move |event: ev::SubmitEvent| {
                        event.prevent_default();
                        submit();
                    }
                >
                    <textarea
                        aria-label="Message"
                        placeholder="Écris à Jarvis…"
                        rows="1"
                        class=TEXTAREA
                        prop:value=move || draft.get()
                        on:input=move |event| draft.set(event_target_value(&event))
                        on:keydown=on_keydown
                    ></textarea>
                    {action}
                </form>
            </div>
        </div>
    }
}
