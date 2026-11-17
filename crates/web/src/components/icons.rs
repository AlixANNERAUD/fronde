//! Line icons drawn in the current text color: Lucide's (ISC, see `LICENSE-lucide`) as Rust/UI's
//! `icons` crate draws them, without the English `<title>` that crate puts in every icon. They are
//! decorative: each sits next to a visible label or inside a control with an accessible name.

use dioxus::prelude::*;

#[component]
fn Svg(class: Option<String>, paths: &'static [&'static str]) -> Element {
    rsx! {
        svg {
            xmlns: "http://www.w3.org/2000/svg",
            width: "24",
            height: "24",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            "stroke-width": "2",
            "stroke-linecap": "round",
            "stroke-linejoin": "round",
            "aria-hidden": "true",
            class: class.unwrap_or_default(),
            for d in paths {
                path { d: *d }
            }
        }
    }
}

macro_rules! icons {
    ($($name:ident => [$($path:literal),+ $(,)?];)+) => {
        $(
            #[component]
            pub fn $name(class: Option<String>) -> Element {
                rsx! { Svg { class, paths: &[$($path),+] } }
            }
        )+
    };
}

icons! {
    // Lucide draws the box as a rect.
    Archive => [
        "M3 3h18a1 1 0 0 1 1 1v3a1 1 0 0 1-1 1H3a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1z",
        "M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8",
        "M10 12h4",
    ];
    ArchiveRestore => [
        "M3 3h18a1 1 0 0 1 1 1v3a1 1 0 0 1-1 1H3a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1z",
        "M4 8v11a2 2 0 0 0 2 2h2",
        "M20 8v11a2 2 0 0 1-2 2h-2",
        "m9 15 3-3 3 3",
        "M12 12v9",
    ];
    ArrowLeft => ["m12 19-7-7 7-7", "M19 12H5"];
    ArrowUp => ["m5 12 7-7 7 7", "M12 19V5"];
    Check => ["M20 6 9 17l-5-5"];
    // Lucide draws its three dots as circles of radius 1.
    EllipsisVertical => [
        "M13 12a1 1 0 1 1-2 0 1 1 0 0 1 2 0z",
        "M13 5a1 1 0 1 1-2 0 1 1 0 0 1 2 0z",
        "M13 19a1 1 0 1 1-2 0 1 1 0 0 1 2 0z",
    ];
    ExternalLink => [
        "M15 3h6v6",
        "M10 14 21 3",
        "M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6",
    ];
    // Lucide's dot is a circle of radius 0.5; a round-capped stroke of no length draws the same.
    KeyRound => [
        "M2.586 17.414A2 2 0 0 0 2 18.828V21a1 1 0 0 0 1 1h3a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h1a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h.172a2 2 0 0 0 1.414-.586l.814-.814a6.5 6.5 0 1 0-4-4z",
        "M16.5 7.5h.01",
    ];
    Loader => [
        "M12 2v4",
        "m16.2 7.8 2.9-2.9",
        "M18 12h4",
        "m16.2 16.2 2.9 2.9",
        "M12 18v4",
        "m4.9 19.1 2.9-2.9",
        "M2 12h4",
        "m4.9 4.9 2.9 2.9",
    ];
    LoaderCircle => ["M21 12a9 9 0 1 1-6.219-8.56"];
    LogOut => ["m16 17 5-5-5-5", "M21 12H9", "M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"];
    Menu => ["M4 5h16", "M4 12h16", "M4 19h16"];
    Pencil => [
        "M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z",
        "m15 5 4 4",
    ];
    Plus => ["M5 12h14", "M12 5v14"];
    // Lucide draws it as a rounded rect.
    Square => ["M5 3h14a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2z"];
    Wrench => [
        "M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.106-3.105c.32-.322.863-.22.983.218a6 6 0 0 1-8.259 7.057l-7.91 7.91a1 1 0 0 1-2.999-3l7.91-7.91a6 6 0 0 1 7.057-8.259c.438.12.54.662.219.984z",
    ];
    X => ["M18 6 6 18", "m6 6 12 12"];
}
