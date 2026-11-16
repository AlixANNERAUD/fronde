//! Interface primitives in the style of shadcn/ui: buttons, fields, cards, badges, chat bubbles
//! and a spinner. Their classes come from Rust/UI's Leptos components (MIT, see
//! `LICENSE-rust-ui`), kept as plain class strings so they render on the server and in the
//! island alike.

use leptos::prelude::*;

use super::icons::{Icon, IconKind};

const BUTTON_BASE: &str = "inline-flex items-center justify-center gap-2 whitespace-nowrap rounded-md text-sm font-medium transition-all disabled:pointer-events-none disabled:opacity-50 [&_svg]:pointer-events-none [&_svg:not([class*='size-'])]:size-4 shrink-0 [&_svg]:shrink-0 outline-none focus-visible:border-ring focus-visible:ring-ring/50 focus-visible:ring-[3px] hover:cursor-pointer active:scale-[0.98] touch-manipulation [-webkit-tap-highlight-color:transparent] select-none";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ButtonVariant {
    #[default]
    Default,
    Destructive,
    Outline,
    Ghost,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ButtonSize {
    #[default]
    Default,
    Small,
    Icon,
}

pub fn button_class(variant: ButtonVariant, size: ButtonSize) -> String {
    let variant = match variant {
        ButtonVariant::Default => {
            "bg-primary text-primary-foreground shadow-xs hover:bg-primary/90"
        }
        ButtonVariant::Destructive => {
            "bg-destructive text-white shadow-xs hover:bg-destructive/90 focus-visible:ring-destructive/20 dark:focus-visible:ring-destructive/40 dark:bg-destructive/60"
        }
        ButtonVariant::Outline => {
            "border bg-background shadow-xs hover:bg-accent hover:text-accent-foreground dark:bg-input/30 dark:border-input dark:hover:bg-input/50"
        }
        ButtonVariant::Ghost => {
            "hover:bg-accent hover:text-accent-foreground dark:hover:bg-accent/50"
        }
    };
    let size = match size {
        ButtonSize::Default => "h-9 px-4 py-2 has-[>svg]:px-3",
        ButtonSize::Small => "h-8 rounded-md gap-1.5 px-3 has-[>svg]:px-2.5",
        ButtonSize::Icon => "size-9",
    };
    format!("{BUTTON_BASE} {variant} {size}")
}

/// `text-base` on phones keeps iOS from zooming in on focus.
pub const INPUT: &str = "border-input placeholder:text-muted-foreground focus-visible:border-ring focus-visible:ring-ring/50 aria-invalid:border-destructive dark:bg-input/30 flex h-9 w-full min-w-0 rounded-md border bg-transparent px-3 py-1 text-base shadow-xs transition-[color,box-shadow] outline-none focus-visible:ring-[3px] disabled:cursor-not-allowed disabled:opacity-50 md:text-sm";

/// Grows with its content (`field-sizing-content`) up to a few lines, then scrolls.
pub const TEXTAREA: &str = "border-input placeholder:text-muted-foreground focus-visible:border-ring focus-visible:ring-ring/50 dark:bg-input/30 flex field-sizing-content min-h-9 max-h-40 w-full resize-none rounded-md border bg-transparent px-3 py-2 text-base shadow-xs transition-[color,box-shadow] outline-none focus-visible:ring-2 disabled:cursor-not-allowed disabled:opacity-50 md:text-sm";

pub const LABEL: &str = "flex items-center gap-2 text-sm leading-none font-medium select-none";

#[component]
pub fn Card(#[prop(optional, into)] class: String, children: Children) -> impl IntoView {
    view! {
        <div class=format!(
            "bg-card text-card-foreground flex flex-col gap-4 rounded-xl border py-6 shadow-sm {class}",
        )>{children()}</div>
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BadgeVariant {
    #[default]
    Secondary,
    Destructive,
    Muted,
}

#[component]
pub fn Badge(#[prop(optional)] variant: BadgeVariant, children: Children) -> impl IntoView {
    let variant = match variant {
        BadgeVariant::Secondary => "border-transparent bg-secondary text-secondary-foreground",
        BadgeVariant::Destructive => "border-transparent bg-destructive/10 text-destructive",
        BadgeVariant::Muted => "border-transparent bg-muted text-muted-foreground",
    };
    view! {
        <span class=format!(
            "inline-flex w-fit items-center rounded-md border px-2.5 py-0.5 text-xs font-semibold {variant}",
        )>{children()}</span>
    }
}

/// Which side of the conversation a bubble is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Jarvis, on the left.
    Start,
    /// The user, on the right.
    End,
}

/// A chat message: Rust/UI's `Message` with one `Bubble` inside. The text keeps its line breaks.
#[component]
pub fn Bubble(side: Side, #[prop(into)] text: String) -> impl IntoView {
    let (row, colors) = match side {
        Side::Start => ("", "bg-muted text-foreground"),
        Side::End => ("flex-row-reverse", "bg-primary text-primary-foreground"),
    };
    view! {
        <div class=format!("relative flex w-full min-w-0 gap-2 text-sm {row}")>
            <div class="relative flex w-fit max-w-[85%] min-w-0 flex-col gap-1">
                <div class=format!(
                    "w-fit max-w-full min-w-0 overflow-hidden rounded-xl border border-transparent px-3 py-2 text-sm leading-relaxed wrap-break-word whitespace-pre-wrap {colors}",
                )>{text}</div>
            </div>
        </div>
    }
}

#[component]
pub fn Spinner(#[prop(optional, into)] class: String) -> impl IntoView {
    view! { <Icon kind=IconKind::LoaderCircle class=format!("size-4 animate-spin {class}") /> }
}

/// A message about the page, such as a refused sign-in.
#[component]
pub fn Alert(children: Children) -> impl IntoView {
    view! {
        <div
            role="alert"
            class="border-destructive/50 bg-card text-destructive relative w-full rounded-lg border px-4 py-3 text-sm"
        >
            {children()}
        </div>
    }
}
