//! The thread list: one row per conversation, the newest first, each sub-agent's thread under its
//! parent, the archived ones folded apart; and each thread's menu, to rename or archive it.

use chrono::{DateTime, Utc};
use dioxus::html::Key;
use dioxus::prelude::*;
use fronde_client::{ListRow, Threads};
use fronde_protocol::{ThreadId, ThreadStatus};

use crate::{
    components::icons::{Archive, ArchiveRestore, Check, EllipsisVertical, Pencil, Plus, X},
    components::request_card::focus,
    components::ui::{
        badge::{Badge, BadgeSize},
        button::{Button, ButtonSize, ButtonVariant},
        input::Input,
    },
    socket::Outgoing,
};

/// Where a link of the interface goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    Thread(ThreadId),
    /// The thread list, the whole screen on a phone.
    List,
    /// A chat whose first message makes a thread.
    New,
}

impl Place {
    pub fn href(self) -> String {
        match self {
            Self::Thread(ThreadId::MAIN) => "/".into(),
            Self::Thread(id) => format!("/t/{id}"),
            Self::List => "/threads".into(),
            Self::New => "/new".into(),
        }
    }
}

/// A link that the page follows itself, without loading again; one opened in a new tab or
/// window loads as any link.
#[component]
pub fn PlaceLink(
    place: Place,
    on_go: EventHandler<Place>,
    #[props(into, optional)] id: Option<String>,
    #[props(into, optional)] class: Option<String>,
    #[props(into, optional)] style: Option<String>,
    #[props(into, optional)] aria_label: Option<String>,
    #[props(optional)] current: bool,
    #[props(optional)] onkeydown: Option<EventHandler<KeyboardEvent>>,
    children: Element,
) -> Element {
    rsx! {
        a {
            id,
            href: place.href(),
            class,
            style,
            "aria-label": aria_label,
            "aria-current": current.then_some("page"),
            onclick: move |event: MouseEvent| {
                let modifiers = event.modifiers();
                if modifiers.ctrl() || modifiers.meta() || modifiers.shift() || modifiers.alt() {
                    return;
                }
                event.prevent_default();
                on_go.call(place);
            },
            onkeydown: move |event| {
                if let Some(handler) = &onkeydown {
                    handler.call(event);
                }
            },
            {children}
        }
    }
}

/// An icon link's classes: a 44 px target on a phone.
pub const ICON_LINK: &str = "hover:bg-accent hover:text-accent-foreground focus-visible:ring-ring/50 inline-flex size-11 shrink-0 items-center justify-center rounded-md outline-none focus-visible:ring-[3px] md:size-9 [&_svg]:size-4";

pub fn status_label(status: ThreadStatus) -> &'static str {
    match status {
        ThreadStatus::Idle => "Inactive",
        ThreadStatus::Queued => "En file d'attente",
        ThreadStatus::Running => "En cours",
        ThreadStatus::Done => "Terminée",
        ThreadStatus::Failed => "Échec",
        ThreadStatus::Cancelled => "Interrompue",
    }
}

fn status_dot(status: ThreadStatus) -> &'static str {
    match status {
        ThreadStatus::Running => "bg-emerald-500 animate-pulse",
        ThreadStatus::Queued => "bg-amber-500",
        ThreadStatus::Failed => "bg-destructive",
        ThreadStatus::Idle | ThreadStatus::Done | ThreadStatus::Cancelled => {
            "bg-muted-foreground/40"
        }
    }
}

/// The last activity as the list shows it, in the page's time (`offset` minutes east of UTC):
/// the time today, else the day.
pub fn activity(at: DateTime<Utc>, now: DateTime<Utc>, offset: i32) -> String {
    let shift = chrono::Duration::minutes(i64::from(offset));
    let (at, now) = ((at + shift).naive_utc(), (now + shift).naive_utc());
    if at.date() == now.date() {
        at.format("%H:%M").to_string()
    } else if at.format("%Y").to_string() == now.format("%Y").to_string() {
        at.format("%d/%m").to_string()
    } else {
        at.format("%d/%m/%Y").to_string()
    }
}

/// Now, and the browser's offset from UTC at `at`, in minutes east.
#[cfg(target_arch = "wasm32")]
fn now_and_offset(at: DateTime<Utc>) -> (DateTime<Utc>, i32) {
    let now = DateTime::from_timestamp_millis(js_sys::Date::now() as i64).unwrap_or(at);
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(
        at.timestamp_millis() as f64
    ));
    let west = date.get_timezone_offset();
    (now, if west.is_finite() { -(west as i32) } else { 0 })
}

/// Natively there is no browser: UTC.
#[cfg(not(target_arch = "wasm32"))]
fn now_and_offset(_at: DateTime<Utc>) -> (DateTime<Utc>, i32) {
    (Utc::now(), 0)
}

/// The id of thread `id`'s link in the list.
pub fn link_id(id: ThreadId) -> String {
    format!("tl-{id}")
}

/// The list. `current`: the thread the page shows, if any; `open`: the socket is, so actions
/// can be sent.
#[component]
pub fn ThreadList(
    rows: Vec<ListRow>,
    archived: Vec<ListRow>,
    current: Option<ThreadId>,
    open: bool,
    on_go: EventHandler<Place>,
    on_send: EventHandler<Outgoing>,
) -> Element {
    let count = archived.len();
    rsx! {
        nav { aria_label: "Conversations", class: "flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto p-2",
            PlaceLink {
                place: Place::New,
                on_go,
                id: "thread-new",
                class: "hover:bg-accent focus-visible:ring-ring/50 inline-flex h-11 shrink-0 items-center justify-center gap-2 rounded-md border px-3 text-sm font-medium outline-none focus-visible:ring-[3px] md:h-9 [&_svg]:size-4",
                Plus {}
                "Nouvelle"
            }
            Rows { rows, current, open, on_go, on_send }
            if count > 0 {
                details { class: "mt-2",
                    summary { class: "text-muted-foreground hover:bg-accent flex min-h-11 cursor-pointer items-center rounded-md px-3 text-sm font-medium",
                        "Archivées ({count})"
                    }
                    Rows { rows: archived, current, open, on_go, on_send }
                }
            }
        }
    }
}

#[component]
fn Rows(
    rows: Vec<ListRow>,
    current: Option<ThreadId>,
    open: bool,
    on_go: EventHandler<Place>,
    on_send: EventHandler<Outgoing>,
) -> Element {
    let ids: Vec<ThreadId> = rows.iter().map(|row| row.summary.id).collect();
    rsx! {
        ul { class: "flex flex-col gap-0.5",
            for (n , row) in rows.into_iter().enumerate() {
                ThreadRow {
                    key: "{row.summary.id}",
                    current: current == Some(row.summary.id),
                    previous: n.checked_sub(1).map(|n| ids[n]),
                    next: ids.get(n + 1).copied(),
                    row,
                    open,
                    on_go,
                    on_send,
                }
            }
        }
    }
}

/// One thread: title, badge, status, last activity; arrow keys move to the next or previous.
#[component]
fn ThreadRow(
    row: ListRow,
    current: bool,
    previous: Option<ThreadId>,
    next: Option<ThreadId>,
    open: bool,
    on_go: EventHandler<Place>,
    on_send: EventHandler<Outgoing>,
) -> Element {
    let mut renaming = use_signal(|| false);
    let summary = row.summary;
    let id = summary.id;
    let (now, offset) = now_and_offset(summary.updated_at);
    let when = activity(summary.updated_at, now, offset);
    let stamp = summary.updated_at.to_rfc3339();
    // Each level of sub-agents one step further in.
    let indent = format!(
        "padding-inline-start: {}rem",
        0.75 + 1.25 * f64::from(row.depth)
    );
    let status = status_label(summary.status);
    let dot = status_dot(summary.status);
    rsx! {
        li { class: "flex items-center gap-1",
            if renaming() {
                div { class: "min-w-0 flex-1", style: "{indent}",
                    Renamer {
                        field_id: "tl-{id}-title",
                        id,
                        title: summary.title.clone(),
                        open,
                        on_send,
                        on_done: move |()| {
                            renaming.set(false);
                            focus(&link_id(id));
                        },
                    }
                }
            } else {
                PlaceLink {
                    place: Place::Thread(id),
                    on_go,
                    id: link_id(id),
                    current,
                    style: indent,
                    class: "hover:bg-accent focus-visible:ring-ring/50 aria-[current=page]:bg-accent flex min-h-11 min-w-0 flex-1 flex-col justify-center gap-0.5 rounded-md py-2 pr-2 outline-none focus-visible:ring-[3px]",
                    onkeydown: move |event: KeyboardEvent| {
                        let to = match event.key() {
                            Key::ArrowDown => next,
                            Key::ArrowUp => previous,
                            _ => return,
                        };
                        event.prevent_default();
                        if let Some(to) = to {
                            focus(&link_id(to));
                        }
                    },
                    span { class: "flex min-w-0 items-center gap-2",
                        span { class: "truncate text-sm font-medium", "{summary.title}" }
                        if row.waiting {
                            Badge { size: BadgeSize::Sm, class: "shrink-0", "attend ta réponse" }
                        }
                    }
                    span { class: "text-muted-foreground flex items-center gap-1.5 text-xs",
                        span { class: "size-2 shrink-0 rounded-full {dot}", "aria-hidden": "true" }
                        span { "{status}" }
                        span { "aria-hidden": "true", "·" }
                        time { datetime: "{stamp}", "{when}" }
                        if row.depth > 0 {
                            span { "aria-hidden": "true", "·" }
                            span { "sous-agent" }
                        }
                    }
                }
                ThreadMenu {
                    prefix: "tl-{id}",
                    id,
                    title: summary.title.clone(),
                    archived: summary.archived,
                    open,
                    on_rename: move |()| renaming.set(true),
                    on_send,
                }
            }
        }
    }
}

/// A thread's actions behind a button: rename it, archive it or bring it back (never the main
/// thread). Escape closes it and gives the focus back to its button.
#[component]
pub fn ThreadMenu(
    prefix: String,
    id: ThreadId,
    title: String,
    archived: bool,
    open: bool,
    on_rename: EventHandler<()>,
    on_send: EventHandler<Outgoing>,
) -> Element {
    let mut shown = use_signal(|| false);
    let toggle = format!("{prefix}-menu");
    let actions = format!("{prefix}-actions");
    let archive = Threads::archive(id, !archived);
    rsx! {
        div {
            class: "relative shrink-0",
            onkeydown: {
                let toggle = toggle.clone();
                move |event: KeyboardEvent| {
                    if event.key() == Key::Escape && shown() {
                        event.prevent_default();
                        shown.set(false);
                        focus(&toggle);
                    }
                }
            },
            Button {
                id: toggle.clone(),
                variant: ButtonVariant::Ghost,
                size: ButtonSize::Icon,
                class: "size-11 md:size-9",
                aria_label: "Actions : {title}",
                aria_expanded: if shown() { "true" } else { "false" },
                aria_controls: actions.clone(),
                onclick: move |_| shown.toggle(),
                EllipsisVertical {}
            }
            if shown() {
                // A tap anywhere else closes the menu.
                div {
                    class: "fixed inset-0 z-10",
                    "aria-hidden": "true",
                    onclick: move |_| shown.set(false),
                }
                div {
                    id: actions,
                    role: "group",
                    aria_label: "Actions : {title}",
                    class: "bg-popover text-popover-foreground absolute top-full right-0 z-20 mt-1 flex min-w-44 flex-col rounded-md border p-1 shadow-md",
                    Button {
                        id: "{prefix}-rename",
                        variant: ButtonVariant::Ghost,
                        class: "h-11 w-full justify-start md:h-9",
                        disabled: !open,
                        onclick: move |_| {
                            shown.set(false);
                            on_rename.call(());
                        },
                        Pencil {}
                        "Renommer"
                    }
                    if let Some(message) = archive {
                        Button {
                            id: "{prefix}-archive",
                            variant: ButtonVariant::Ghost,
                            class: "h-11 w-full justify-start md:h-9",
                            disabled: !open,
                            onclick: move |_| {
                                shown.set(false);
                                on_send
                                    .call(Outgoing {
                                        message: message.clone(),
                                        sent: None,
                                        dropped: None,
                                    });
                            },
                            if archived {
                                ArchiveRestore {}
                                "Désarchiver"
                            } else {
                                Archive {}
                                "Archiver"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A thread's title being renamed: Enter saves (if the protocol takes it as a title), Escape
/// cancels. It takes the focus when it shows.
#[component]
pub fn Renamer(
    field_id: String,
    id: ThreadId,
    title: String,
    open: bool,
    on_send: EventHandler<Outgoing>,
    on_done: EventHandler<()>,
) -> Element {
    let mut text = use_signal(|| title.clone());
    let mut error = use_signal(|| None::<&'static str>);
    let sent = use_callback(move |()| on_done.call(()));
    let dropped = use_callback(move |()| error.set(Some("Connexion perdue, réessaie.")));
    let save = use_callback(move |()| {
        if !open {
            return;
        }
        if text.peek().trim() == title.trim() {
            on_done.call(());
            return;
        }
        match Threads::rename(id, &text.peek()) {
            Ok(message) => {
                error.set(None);
                on_send.call(Outgoing {
                    message,
                    sent: Some(sent),
                    dropped: Some(dropped),
                });
            }
            Err(why) => error.set(Some(why)),
        }
    });
    let error_id = format!("{field_id}-error");
    rsx! {
        div { class: "flex min-w-0 flex-1 flex-col gap-1 py-1",
            div { class: "flex items-center gap-1",
                Input {
                    id: field_id.clone(),
                    value: text(),
                    aria_label: "Nouveau titre",
                    class: "h-11 md:h-9",
                    oninput: move |event: FormEvent| text.set(event.value()),
                    onkeydown: move |event: KeyboardEvent| {
                        if event.is_composing() {
                            return;
                        }
                        match event.key() {
                            Key::Enter => {
                                event.prevent_default();
                                save.call(());
                            }
                            Key::Escape => {
                                event.prevent_default();
                                on_done.call(());
                            }
                            _ => {}
                        }
                    },
                    onmounted: move |event: MountedEvent| async move {
                        let _ = event.data().set_focus(true).await;
                    },
                }
                Button {
                    id: "{field_id}-save",
                    variant: ButtonVariant::Ghost,
                    size: ButtonSize::Icon,
                    class: "size-11 md:size-9",
                    aria_label: "Enregistrer le titre",
                    disabled: !open,
                    onclick: move |_| save.call(()),
                    Check {}
                }
                Button {
                    id: "{field_id}-cancel",
                    variant: ButtonVariant::Ghost,
                    size: ButtonSize::Icon,
                    class: "size-11 md:size-9",
                    aria_label: "Annuler",
                    onclick: move |_| on_done.call(()),
                    X {}
                }
            }
            if let Some(why) = error() {
                p { id: error_id, role: "alert", class: "text-destructive text-xs", "{why}" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_is_the_time_today_else_the_day() {
        let at = |text: &str| text.parse::<DateTime<Utc>>().unwrap();
        let now = at("2026-11-17T22:30:00Z");
        assert_eq!(activity(at("2026-11-17T09:05:00Z"), now, 0), "09:05");
        assert_eq!(activity(at("2026-11-16T23:59:00Z"), now, 0), "16/11");
        // An hour east, both are on the 18th.
        let late = at("2026-11-17T23:30:00Z");
        assert_eq!(activity(at("2026-11-17T23:10:00Z"), late, 60), "00:10");
        assert_eq!(activity(at("2026-11-17T22:50:00Z"), late, 60), "17/11");
        assert_eq!(activity(at("2025-12-31T10:00:00Z"), now, 0), "31/12/2025");
    }

    #[test]
    fn places_have_their_addresses() {
        assert_eq!(Place::Thread(ThreadId::MAIN).href(), "/");
        assert_eq!(Place::Thread(ThreadId(7)).href(), "/t/7");
        assert_eq!(Place::List.href(), "/threads");
        assert_eq!(Place::New.href(), "/new");
    }
}
