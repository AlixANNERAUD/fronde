//! Line icons drawn in the current text color, after Lucide (ISC). They are decorative: every
//! icon sits next to a visible label or inside a control that has an accessible name.

use leptos::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconKind {
    Send,
    Stop,
    SignOut,
    LoaderCircle,
    Check,
    Cross,
    Wrench,
}

impl IconKind {
    fn paths(self) -> &'static [&'static str] {
        match self {
            IconKind::Send => &["M12 19V5", "m5 12 7-7 7 7"],
            IconKind::Stop => &["M7 7h10v10H7z"],
            IconKind::SignOut => &[
                "M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4",
                "m16 17 5-5-5-5",
                "M21 12H9",
            ],
            IconKind::LoaderCircle => &["M21 12a9 9 0 1 1-6.219-8.56"],
            IconKind::Check => &["M20 6 9 17l-5-5"],
            IconKind::Cross => &["M18 6 6 18", "m6 6 12 12"],
            IconKind::Wrench => &[
                "M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94l-3.76 3.76z",
            ],
        }
    }
}

/// `class` replaces the default size.
#[component]
pub fn Icon(kind: IconKind, #[prop(optional, into)] class: String) -> impl IntoView {
    let class = if class.is_empty() {
        "size-4".to_owned()
    } else {
        class
    };
    view! {
        <svg
            viewBox="0 0 24 24"
            aria-hidden="true"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            stroke-linecap="round"
            stroke-linejoin="round"
            class=format!("shrink-0 {class}")
        >
            {kind.paths().iter().map(|d| view! { <path d=*d /> }).collect_view()}
        </svg>
    }
}
