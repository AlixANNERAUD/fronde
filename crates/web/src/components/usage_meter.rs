//! The subscription meter in the chat header: a pill with the window nearest its line
//! (« 5 h 42 % »), coloured by how full it is, which opens a panel with each window, when it
//! resets, how old the reading is, and whether background work is paused.

use dioxus::html::Key;
use dioxus::prelude::*;
use fronde_client::usage::{Level, Meter, Usage, meter};

use crate::components::request_card::focus;
use crate::components::thread_list::{now_and_offset, offset_at};

/// The meter's element ids: one meter to a page.
const BOX: &str = "usage-meter-box";
const BUTTON: &str = "usage-meter";
const PANEL: &str = "usage-panel";

/// The pill's colours by level, a word beside each so that colour is never the only cue.
pub fn level_class(level: Level) -> &'static str {
    match level {
        Level::Low => "bg-muted text-muted-foreground",
        Level::Near => "bg-amber-100 text-amber-950 dark:bg-amber-900/50 dark:text-amber-50",
        Level::Paused => "bg-orange-200 text-orange-950 dark:bg-orange-900/60 dark:text-orange-50",
        Level::Rejected => "bg-destructive text-white",
    }
}

/// The level in words, for the panel.
fn level_word(level: Level) -> &'static str {
    match level {
        Level::Low => "",
        Level::Near => "proche de la réserve",
        Level::Paused => "réserve atteinte",
        Level::Rejected => "limite atteinte",
    }
}

/// The meter of `usage`: the pill is the page's user's own account's; its panel lists that
/// account's windows, then, for an admin, every other account's by name. Nothing without a
/// window read.
#[component]
pub fn UsageMeter(usage: Usage) -> Element {
    let mut shown = use_signal(|| false);
    let reserve = usage.reserve;
    // Read at each render, opening the panel included: the pause line and the ages follow the
    // clock, a reset passed since the last reading among them.
    let own = usage.own.first().and_then(|reading| {
        let (now, _) = now_and_offset(reading.known_since);
        meter(reading, reserve, now, &offset_at)
    });
    let others: Vec<(String, Meter)> = usage
        .others
        .iter()
        .filter_map(|reading| {
            let (now, _) = now_and_offset(reading.known_since);
            Some((
                reading.account.clone(),
                meter(reading, reserve, now, &offset_at)?,
            ))
        })
        .collect();
    let (pill_text, level, label) = match (&own, others.is_empty()) {
        (Some(own), _) => (own.pill.clone(), own.level, own.label.clone()),
        (None, false) => (
            "Abonnements".to_owned(),
            Level::Low,
            "pas encore de relevé pour le tien".to_owned(),
        ),
        (None, true) => return rsx! {},
    };
    let pill = format!(
        "inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center rounded-full px-3 text-xs font-medium whitespace-nowrap tabular-nums outline-none focus-visible:ring-[3px] focus-visible:ring-ring/50 md:min-h-8 {}",
        level_class(level)
    );
    rsx! {
        div {
            id: BOX,
            class: "relative shrink-0",
            onkeydown: move |event: KeyboardEvent| {
                if event.key() == Key::Escape && shown() {
                    event.prevent_default();
                    shown.set(false);
                    focus(BUTTON);
                }
            },
            button {
                id: BUTTON,
                r#type: "button",
                class: pill,
                "data-level": "{level:?}",
                aria_label: "Abonnement : {label}",
                aria_expanded: if shown() { "true" } else { "false" },
                aria_controls: PANEL,
                onclick: move |_| shown.toggle(),
                "{pill_text}"
            }
            if shown() {
                // A tap anywhere else closes the panel.
                div {
                    class: "fixed inset-0 z-10",
                    "aria-hidden": "true",
                    onclick: move |_| shown.set(false),
                }
                div {
                    id: PANEL,
                    role: "group",
                    aria_label: "Utilisation de l'abonnement",
                    class: "bg-popover text-popover-foreground absolute top-full right-0 z-20 mt-1 flex max-h-[70dvh] w-72 max-w-[calc(100vw-2rem)] flex-col gap-2 overflow-y-auto rounded-md border p-3 text-sm shadow-md",
                    if let Some(own) = own {
                        MeterDetails { meter: own }
                    } else {
                        p { class: "text-muted-foreground text-xs",
                            "Pas encore de relevé pour ton abonnement."
                        }
                    }
                    for (account, other) in others {
                        section {
                            key: "{account}",
                            class: "flex flex-col gap-2 border-t pt-2",
                            aria_label: "Compte {account}",
                            div { class: "flex items-baseline justify-between gap-2",
                                h3 { class: "font-medium", "Compte {account}" }
                                span {
                                    class: "rounded-full px-2 text-xs tabular-nums {level_class(other.level)}",
                                    "{other.pill}"
                                }
                            }
                            MeterDetails { meter: other.clone() }
                        }
                    }
                }
            }
        }
    }
}

/// One account's windows, whether background work runs on it and how old its reading is.
#[component]
fn MeterDetails(meter: Meter) -> Element {
    rsx! {
        ul { class: "flex flex-col gap-2",
            for row in meter.rows {
                li { key: "{row.name}", class: "flex flex-col",
                    div { class: "flex items-baseline justify-between gap-2",
                        span { class: "font-medium", "{row.name}" }
                        span {
                            class: "rounded-full px-2 text-xs tabular-nums {level_class(row.level)}",
                            "{row.used}"
                        }
                    }
                    if !level_word(row.level).is_empty() || row.status.is_some() {
                        span { class: "text-xs",
                            {row.status.clone().unwrap_or_else(|| level_word(row.level).to_owned())}
                        }
                    }
                    if let Some(reset) = row.reset {
                        span { class: "text-muted-foreground text-xs", "{reset}" }
                    }
                }
            }
        }
        p { class: "border-t pt-2 text-xs", "{meter.background}" }
        p { class: "text-muted-foreground text-xs", "{meter.age}" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::chat::ChatView;
    use crate::socket::{Outgoing, Status};
    use crate::testing::Live;
    use chrono::{DateTime, Duration, Utc};
    use dioxus::html::{Code, Key};
    use fronde_client::Chat;
    use fronde_protocol::usage::{AccountUsage, LimitStatus, Window, WindowKind};

    fn window(kind: WindowKind, used: Option<f64>, status: Option<LimitStatus>) -> Window {
        Window {
            kind,
            used,
            status,
            resets_at: Some(Utc::now() + Duration::hours(2)),
            as_of: Utc::now(),
            reset_since_reading: false,
        }
    }

    fn reading(session: Option<f64>, status: Option<LimitStatus>) -> AccountUsage {
        AccountUsage {
            account: "claude".into(),
            known_since: Utc::now() - Duration::minutes(12),
            windows: vec![
                window(WindowKind::Session, session, status),
                window(WindowKind::Weekly, Some(0.18), Some(LimitStatus::Allowed)),
            ],
        }
    }

    /// The usage a page gets with `reading` its user's own, under a reserve of 30 %.
    fn own(reading: AccountUsage) -> Usage {
        Usage {
            reserve: 30,
            own: vec![reading],
            others: Vec::new(),
        }
    }

    fn render_usage(usage: Usage) -> String {
        let mut dom = VirtualDom::new_with_props(UsageMeter, UsageMeterProps { usage });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn render(usage: AccountUsage) -> String {
        render_usage(own(usage))
    }

    /// The pill's text and colour follow the level, with its words for a screen reader.
    #[test]
    fn meter_renders_levels() {
        for (used, status, pill, level) in [
            (Some(0.42), None, "5 h 42 %", Level::Low),
            (Some(0.63), None, "5 h 63 %", Level::Near),
            (Some(0.72), None, "5 h 72 %", Level::Paused),
            (
                Some(0.5),
                Some(LimitStatus::Rejected),
                "5 h 50 %",
                Level::Rejected,
            ),
            (None, Some(LimitStatus::Warning), "5 h ?", Level::Paused),
        ] {
            let page = render(reading(used, status));
            assert!(page.contains(&format!(">{pill}</button>")), "{page}");
            assert!(
                page.contains(&format!(r#"data-level="{level:?}""#)),
                "{page}"
            );
            assert!(page.contains(level_class(level)), "{page}");
            assert!(
                page.contains(r#"aria-label="Abonnement : 5 h : "#),
                "{page}"
            );
            assert!(page.contains(r#"aria-expanded="false""#), "{page}");
            // A 44 px target on a phone.
            assert!(page.contains("min-h-11 min-w-11"), "{page}");
        }
    }

    #[test]
    fn meter_hidden_without_reading() {
        let empty = AccountUsage {
            account: "claude".into(),
            known_since: DateTime::<Utc>::MIN_UTC,
            windows: Vec::new(),
        };
        assert_eq!(render(empty.clone()), "");
        // Nor without any reading at all, or with only empty ones of others.
        assert_eq!(
            render_usage(Usage {
                reserve: 30,
                own: Vec::new(),
                others: vec![empty],
            }),
            ""
        );
    }

    /// A tap opens the panel: each window with its use and reset, how old the reading is, and
    /// whether background work runs; Escape closes it. Every text is escaped.
    #[test]
    fn panel_shows_the_windows() {
        let mut usage = reading(Some(0.72), None);
        usage.windows.push(window(
            WindowKind::Other("<b>x</b>".into()),
            Some(0.1),
            None,
        ));
        let mut page = Live::new(VirtualDom::new_with_props(
            UsageMeter,
            UsageMeterProps { usage: own(usage) },
        ));
        assert!(
            !page.html().contains(r#"id="usage-panel""#),
            "closed at first"
        );
        page.click("usage-meter");
        let html = page.html();
        for text in [
            "5 h",
            "72 % utilisé",
            "réserve atteinte",
            "semaine",
            "18 % utilisé",
            "réinitialisée",
            "Tâches de fond en pause jusqu&#39;à",
            "(réserve 30 %)",
            "relevé il y a 12 min",
            "&#60;b&#62;x&#60;/b&#62;",
        ] {
            assert!(html.contains(text), "{text}: {html}");
        }
        assert!(!html.contains("<b>x</b>"), "{html}");
        assert!(html.contains(r#"aria-expanded="true""#), "{html}");
        // A page that gets only its own account's lists no other.
        assert!(!html.contains("Compte "), "{html}");
        page.key("usage-meter-box", Key::Escape, Code::Escape);
        assert!(
            !page.html().contains(r#"id="usage-panel""#),
            "closed by Escape"
        );
    }

    /// An admin's panel lists every other account's windows under its name, after their own.
    #[test]
    fn admin_panel_lists_every_account() {
        let mut hers = reading(Some(0.72), None);
        hers.account = "marie".into();
        let mut odd = reading(Some(0.1), None);
        odd.account = "<b>paul</b>".into();
        let mut page = Live::new(VirtualDom::new_with_props(
            UsageMeter,
            UsageMeterProps {
                usage: Usage {
                    reserve: 30,
                    own: vec![reading(Some(0.42), None)],
                    others: vec![hers, odd],
                },
            },
        ));
        // The pill is the admin's own account's.
        assert!(
            page.html().contains(">5 h 42 %</button>"),
            "{}",
            page.html()
        );
        page.click("usage-meter");
        let html = page.html();
        let (mine, rest) = html.split_once("Compte marie").expect("her account listed");
        assert!(mine.contains("42 % utilisé"), "{html}");
        assert!(rest.contains("5 h 72 %"), "{html}");
        assert!(rest.contains("72 % utilisé"), "{html}");
        assert!(rest.contains("réserve atteinte"), "{html}");
        assert!(
            html.contains("Compte &#60;b&#62;paul&#60;/b&#62;"),
            "{html}"
        );
        assert!(!html.contains("<b>paul</b>"), "{html}");
    }

    /// An admin whose own account has no reading yet still reaches the others' from the pill.
    #[test]
    fn admin_without_a_reading_sees_the_others() {
        let mut hers = reading(Some(0.72), None);
        hers.account = "marie".into();
        let mut page = Live::new(VirtualDom::new_with_props(
            UsageMeter,
            UsageMeterProps {
                usage: Usage {
                    reserve: 30,
                    own: Vec::new(),
                    others: vec![hers],
                },
            },
        ));
        assert!(
            page.html().contains(">Abonnements</button>"),
            "{}",
            page.html()
        );
        page.click("usage-meter");
        let html = page.html();
        assert!(
            html.contains("Pas encore de relevé pour ton abonnement."),
            "{html}"
        );
        assert!(html.contains("Compte marie"), "{html}");
        assert!(html.contains("72 % utilisé"), "{html}");
    }

    #[component]
    fn Header(usage: Option<Usage>) -> Element {
        rsx! {
            ChatView {
                chat: Chat::default(),
                status: Status::Open,
                on_send: move |_: Outgoing| {},
                usage,
            }
        }
    }

    fn header(usage: Option<Usage>) -> String {
        let mut dom = VirtualDom::new_with_props(Header, HeaderProps { usage });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// The chat's header shows the meter when the page got a usage it may see; a page that
    /// got none shows none.
    #[test]
    fn header_shows_the_meter_to_whom_gets_the_usage() {
        let admin = header(Some(own(reading(Some(0.42), None))));
        assert!(admin.contains(r#"id="usage-meter""#), "{admin}");
        assert!(admin.contains(">5 h 42 %</button>"), "{admin}");
        let family = header(None);
        assert!(!family.contains("usage-meter"), "{family}");
    }
}
