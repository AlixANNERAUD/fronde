use dioxus::prelude::*;
use tw_merge::tw_merge;

use super::button::{Button, ButtonSize};
use super::input_group::{InputGroup, InputGroupAddon, InputGroupAddonAlign};

/* ========================================================== */
/*                     ✨ CLX COMPONENTS ✨                   */
/* ========================================================== */

#[component]
pub fn InputPromptTools(
    #[props(into, optional)] class: Option<String>,
    children: Element,
) -> Element {
    let merged = tw_merge!("flex items-center gap-1", class.as_deref().unwrap_or(""));
    rsx! { div { "data-name": "InputPromptTools", class: "{merged}", {children} } }
}

/* ========================================================== */
/*                     ✨ FUNCTIONS ✨                        */
/* ========================================================== */

/// Outer wrapper — `InputGroup` with overflow clipping.
#[component]
pub fn InputPrompt(#[props(into, optional)] class: Option<String>, children: Element) -> Element {
    let merged = tw_merge!("overflow-hidden", class.as_deref().unwrap_or(""));
    rsx! { InputGroup { class: merged, {children} } }
}

/// Auto-growing textarea bound to a Signal<String>.
/// Enter (without Shift) fires `on_submit`.
#[component]
pub fn InputPromptTextarea(
    value: Signal<String>,
    #[props(into, optional)] placeholder: Option<String>,
    #[props(into, optional)] class: Option<String>,
    #[props(optional)] on_submit: Option<EventHandler<()>>,
    // jarvis: the textarea's accessible name.
    #[props(into, optional)] aria_label: Option<String>,
) -> Element {
    let placeholder = placeholder.unwrap_or_else(|| "Write a message...".to_string());
    let merged = tw_merge!(
        "flex-1 resize-none rounded-none border-0 bg-transparent py-3 px-3 \
         field-sizing-content min-h-[52px] max-h-48 text-sm shadow-none \
         focus-visible:ring-0 dark:bg-transparent placeholder:text-muted-foreground",
        class.as_deref().unwrap_or("")
    );

    rsx! {
        textarea {
            "data-slot": "input-group-control",
            class: "{merged}",
            placeholder: "{placeholder}",
            "aria-label": aria_label,
            value: "{value()}",
            oninput: move |ev| value.set(ev.value()),
            onkeydown: move |ev| {
                // jarvis: Enter while an input method composes confirms the text; it does not send.
                if ev.key() == Key::Enter && !ev.modifiers().shift() && !ev.is_composing() {
                    ev.prevent_default();
                    if let Some(cb) = on_submit {
                        cb.call(());
                    }
                }
            }
        }
    }
}

/// Block-end footer row. Holds tools on the left, submit on the right.
#[component]
pub fn InputPromptFooter(
    #[props(into, optional)] class: Option<String>,
    children: Element,
) -> Element {
    let merged = tw_merge!(
        "border-t px-2 py-2 justify-between gap-1",
        class.as_deref().unwrap_or("")
    );
    rsx! {
        InputGroupAddon { align: InputGroupAddonAlign::BlockEnd, class: merged,
            {children}
        }
    }
}

/// Send button. Pass disabled=true to disable when the textarea is empty.
#[component]
pub fn InputPromptSubmit(
    #[props(optional)] disabled: bool,
    #[props(into, optional)] class: Option<String>,
    // jarvis: what a click does, and the button's accessible name.
    #[props(optional)] onclick: Option<EventHandler<MouseEvent>>,
    #[props(into, optional)] aria_label: Option<String>,
    children: Element,
) -> Element {
    let merged = tw_merge!("size-8 rounded-full", class.as_deref().unwrap_or(""));

    rsx! {
        Button {
            size: ButtonSize::Icon,
            class: merged,
            button_type: "button",
            disabled: disabled,
            aria_label,
            onclick: move |e| {
                if let Some(handler) = &onclick {
                    handler.call(e);
                }
            },
            {children}
        }
    }
}
