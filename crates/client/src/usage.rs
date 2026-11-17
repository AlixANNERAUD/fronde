//! The subscription meter: which window the chat header's pill shows and how full it is, and
//! what its panel says of each window, from the server's `usage` message. Pure: the page gives
//! the time and its offset from UTC, so it is tested natively.

use chrono::{DateTime, Datelike, Duration, Utc};
use fronde_protocol::usage::{
    self, Background, GUARDED, LimitStatus, ProviderUsage, Window, WindowKind, percent,
};

/// A window's short name, as the chat shows it: « 5 h », « semaine ».
pub fn window_label(kind: &WindowKind) -> String {
    match kind {
        WindowKind::Session => "5 h".to_owned(),
        WindowKind::Weekly => "semaine".to_owned(),
        WindowKind::WeeklyModel(model) => format!("semaine {model}"),
        WindowKind::Extra => "usage payant".to_owned(),
        WindowKind::Other(id) => id.clone(),
    }
}

/// How full a window is, which colours it; ordered from the least to the most worrying.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// More than 10 points below the line, or not known to be close.
    Low,
    /// Within 10 points of the line.
    Near,
    /// At the line or past it: background work waits.
    Paused,
    /// The provider refuses turns until the window resets.
    Rejected,
}

/// The meter of one account's subscription.
#[derive(Clone, Debug, PartialEq)]
pub struct Meter {
    /// The pill: the window nearest its line, « 5 h 42 % ».
    pub pill: String,
    pub level: Level,
    /// The pill in words, for a screen reader: « 5 h : 42 % utilisé ».
    pub label: String,
    /// Each window read, the guarded ones first.
    pub rows: Vec<MeterRow>,
    /// « Tâches de fond permises (réserve 30 %) », « Tâches de fond en pause jusqu'à 16:40
    /// (réserve 30 %) ».
    pub background: String,
    /// How old the reading is: « relevé il y a 12 min ».
    pub age: String,
}

/// One window in the meter's panel.
#[derive(Clone, Debug, PartialEq)]
pub struct MeterRow {
    /// « 5 h ».
    pub name: String,
    /// « 42 % utilisé », « utilisation inconnue ».
    pub used: String,
    pub level: Level,
    /// What the provider says besides: « limite atteinte », « proche de la limite ».
    pub status: Option<String>,
    /// « réinitialisée à 16:40 », « réinitialisée lun. 09:00 », « réinitialisée depuis le
    /// relevé ».
    pub reset: Option<String>,
}

/// The page's offset from UTC at an instant, in minutes east: it changes with daylight saving
/// time, so each time shown asks for its own.
pub type Offset<'a> = &'a dyn Fn(DateTime<Utc>) -> i32;

/// The meter of `reading` at `now`, under a reserve of `reserve` percent, its times in the
/// page's time (`offset`); `None` without any window read: no pill.
pub fn meter(
    reading: &ProviderUsage,
    reserve: u8,
    now: DateTime<Utc>,
    offset: Offset<'_>,
) -> Option<Meter> {
    let reading = reading.now(now);
    let mut windows: Vec<&Window> = reading.windows.iter().collect();
    if windows.is_empty() {
        return None;
    }
    // The guarded windows first, in their order, then the others as read.
    windows.sort_by_key(|window| {
        GUARDED
            .iter()
            .position(|kind| *kind == window.kind)
            .unwrap_or(GUARDED.len())
    });
    let level_of = |window: &Window| level(window, reserve);
    // The pill: the guarded window nearest its line, else the first one read.
    let shown = windows
        .iter()
        .filter(|window| GUARDED.contains(&window.kind))
        .max_by(|a, b| {
            (level_of(a), a.used.unwrap_or(0.0))
                .partial_cmp(&(level_of(b), b.used.unwrap_or(0.0)))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .or(windows.first())
        .copied()?;
    let name = window_label(&shown.kind);
    let (pill, label) = match shown.used {
        Some(used) => (
            format!("{name} {} %", percent(used)),
            format!("{name} : {} % utilisé", percent(used)),
        ),
        None => (
            format!("{name} ?"),
            format!("{name} : utilisation inconnue"),
        ),
    };
    let rows = windows
        .iter()
        .map(|window| MeterRow {
            name: window_label(&window.kind),
            // A window reset since its reading has no use known: never « 0 % ».
            used: match window.used {
                Some(used) => format!("{} % utilisé", percent(used)),
                None => "utilisation inconnue".to_owned(),
            },
            level: level_of(window),
            status: match window.status {
                Some(LimitStatus::Rejected) => Some("limite atteinte".to_owned()),
                Some(LimitStatus::Warning) if window.kind == WindowKind::Extra => {
                    Some("en cours d'utilisation".to_owned())
                }
                Some(LimitStatus::Warning) => Some("proche de la limite".to_owned()),
                Some(LimitStatus::Allowed) | None => None,
            },
            reset: if window.reset_since_reading {
                Some("réinitialisée depuis le relevé".to_owned())
            } else {
                window
                    .resets_at
                    .map(|at| format!("réinitialisée {}", on(at, now, offset)))
            },
        })
        .collect();
    Some(Meter {
        pill,
        level: level_of(shown),
        label,
        rows,
        background: background_line(&reading, reserve, now, offset),
        age: format!("relevé {}", age(reading.known_since, now)),
    })
}

/// How full `window` is, under a reserve of `reserve` percent. Only the guarded windows and
/// paid extra usage pause background work; the others are near past 90 %.
pub fn level(window: &Window, reserve: u8) -> Level {
    if window.reset_since_reading {
        return Level::Low;
    }
    if window.status == Some(LimitStatus::Rejected) {
        return Level::Rejected;
    }
    let pauses =
        reserve > 0 && (GUARDED.contains(&window.kind) || window.kind == WindowKind::Extra);
    // Compared in whole percent, as shown and as the server decides.
    let line = if pauses { usage::line(reserve) } else { 100 };
    match window.used.map(percent) {
        Some(used) if pauses && used >= line => Level::Paused,
        Some(used) if used + 10 >= line => Level::Near,
        Some(_) => Level::Low,
        None if pauses && window.status == Some(LimitStatus::Warning) => Level::Paused,
        None if window.status == Some(LimitStatus::Warning) => Level::Near,
        None => Level::Low,
    }
}

/// What the panel says of background work.
fn background_line(
    reading: &ProviderUsage,
    reserve: u8,
    now: DateTime<Utc>,
    offset: Offset<'_>,
) -> String {
    match reading.background(reserve, now) {
        Background::Allowed { .. } => format!("Tâches de fond permises (réserve {reserve} %)"),
        Background::Paused(pause) => match pause.resets_at {
            Some(at) => format!(
                "Tâches de fond en pause jusqu'à {} (réserve {reserve} %)",
                when(at, now, offset)
            ),
            None => format!("Tâches de fond en pause (réserve {reserve} %)"),
        },
        Background::Unknown if reserve == 0 => {
            "Réserve désactivée : les compteurs quotidiens limitent les tâches de fond".to_owned()
        }
        Background::Unknown => {
            "Relevé incomplet : les compteurs quotidiens limitent les tâches de fond".to_owned()
        }
    }
}

const WEEKDAYS: [&str; 7] = ["lun.", "mar.", "mer.", "jeu.", "ven.", "sam.", "dim."];

/// `at` in the page's time, rounded up to the minute: « 16:40 » the same day as `now`, else
/// « lun. 09:00 » within a week, else « 24/11 09:00 ».
pub fn when(at: DateTime<Utc>, now: DateTime<Utc>, offset: Offset<'_>) -> String {
    let at = usage::ceil_minute(at);
    let local = (at + Duration::minutes(i64::from(offset(at)))).naive_utc();
    let today = (now + Duration::minutes(i64::from(offset(now)))).naive_utc();
    let time = local.format("%H:%M");
    if local.date() == today.date() {
        time.to_string()
    } else if local.date() > today.date() && local.date() - today.date() < Duration::days(7) {
        let day = WEEKDAYS[local.weekday().num_days_from_monday() as usize];
        format!("{day} {time}")
    } else {
        format!("{} {time}", local.format("%d/%m"))
    }
}

/// [`when`] after a verb: « à 16:40 » the same day, else as `when` says it.
fn on(at: DateTime<Utc>, now: DateTime<Utc>, offset: Offset<'_>) -> String {
    let text = when(at, now, offset);
    if text.contains(' ') {
        text
    } else {
        format!("à {text}")
    }
}

/// How long ago `since` was: « à l'instant », « il y a 12 min », « il y a 3 h », « il y a 2 j ».
pub fn age(since: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let minutes = (now - since).num_minutes().max(0);
    match minutes {
        0 => "à l'instant".to_owned(),
        1..=59 => format!("il y a {minutes} min"),
        60..=1439 => format!("il y a {} h", minutes / 60),
        _ => format!("il y a {} j", minutes / 1440),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-17 12:00 UTC, a Tuesday.
    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_794_916_800, 0).unwrap()
    }

    fn window(
        kind: WindowKind,
        used: Option<f64>,
        status: Option<LimitStatus>,
        hours: i64,
    ) -> Window {
        Window {
            kind,
            used,
            status,
            resets_at: Some(now() + Duration::hours(hours)),
            as_of: now() - Duration::minutes(12),
            reset_since_reading: false,
        }
    }

    fn reading(windows: Vec<Window>) -> ProviderUsage {
        ProviderUsage {
            provider: "claude".into(),
            known_since: now() - Duration::minutes(12),
            windows,
        }
    }

    #[test]
    fn windows_have_short_names() {
        assert_eq!(window_label(&WindowKind::Session), "5 h");
        assert_eq!(window_label(&WindowKind::Weekly), "semaine");
        assert_eq!(
            window_label(&WindowKind::WeeklyModel("opus".into())),
            "semaine opus"
        );
        assert_eq!(window_label(&WindowKind::Extra), "usage payant");
        assert_eq!(window_label(&WindowKind::Other("x".into())), "x");
    }

    #[test]
    fn usage_view_picks_the_window_nearest_its_line() {
        let usage = reading(vec![
            window(
                WindowKind::Weekly,
                Some(0.18),
                Some(LimitStatus::Allowed),
                150,
            ),
            window(
                WindowKind::Session,
                Some(0.42),
                Some(LimitStatus::Allowed),
                4,
            ),
        ]);
        let meter = meter(&usage, 30, now(), &|_| 60).unwrap();
        assert_eq!(meter.pill, "5 h 42 %");
        assert_eq!(meter.label, "5 h : 42 % utilisé");
        assert_eq!(meter.level, Level::Low);
        // The guarded windows in order, each with its reset in the page's time (UTC+1).
        assert_eq!(
            meter.rows,
            [
                MeterRow {
                    name: "5 h".into(),
                    used: "42 % utilisé".into(),
                    level: Level::Low,
                    status: None,
                    reset: Some("réinitialisée à 17:00".into()),
                },
                MeterRow {
                    name: "semaine".into(),
                    used: "18 % utilisé".into(),
                    level: Level::Low,
                    status: None,
                    reset: Some("réinitialisée lun. 19:00".into()),
                },
            ]
        );
        assert_eq!(meter.background, "Tâches de fond permises (réserve 30 %)");
        assert_eq!(meter.age, "relevé il y a 12 min");
        // The weekly window, nearer its line, takes the pill.
        let usage = reading(vec![
            window(
                WindowKind::Session,
                Some(0.42),
                Some(LimitStatus::Allowed),
                4,
            ),
            window(
                WindowKind::Weekly,
                Some(0.65),
                Some(LimitStatus::Allowed),
                150,
            ),
        ]);
        let weekly = super::meter(&usage, 30, now(), &|_| 0).unwrap();
        assert_eq!(
            (weekly.pill.as_str(), weekly.level),
            ("semaine 65 %", Level::Near)
        );
    }

    #[test]
    fn usage_view_levels() {
        let at = |used: Option<f64>, status: Option<LimitStatus>| {
            level(&window(WindowKind::Session, used, status, 1), 30)
        };
        assert_eq!(at(Some(0.59), None), Level::Low);
        assert_eq!(at(Some(0.60), None), Level::Near);
        assert_eq!(at(Some(0.70), None), Level::Paused);
        assert_eq!(at(Some(0.2), Some(LimitStatus::Rejected)), Level::Rejected);
        assert_eq!(at(None, Some(LimitStatus::Warning)), Level::Paused);
        // A reserve of 0 pauses nothing.
        let off = level(&window(WindowKind::Session, Some(0.95), None, 1), 0);
        assert_eq!(off, Level::Near);
        let usage = reading(vec![
            window(
                WindowKind::Session,
                Some(0.72),
                Some(LimitStatus::Allowed),
                4,
            ),
            window(
                WindowKind::Weekly,
                Some(0.1),
                Some(LimitStatus::Allowed),
                150,
            ),
        ]);
        let meter = meter(&usage, 30, now(), &|_| 0).unwrap();
        assert_eq!(meter.level, Level::Paused);
        assert_eq!(
            meter.background,
            "Tâches de fond en pause jusqu'à 16:00 (réserve 30 %)"
        );
    }

    #[test]
    fn usage_view_says_reset_since_reading() {
        let usage = reading(vec![
            window(
                WindowKind::Session,
                Some(0.9),
                Some(LimitStatus::Allowed),
                -1,
            ),
            window(
                WindowKind::Weekly,
                Some(0.1),
                Some(LimitStatus::Allowed),
                150,
            ),
        ]);
        let meter = meter(&usage, 30, now(), &|_| 0).unwrap();
        assert_eq!(
            meter.rows[0].reset.as_deref(),
            Some("réinitialisée depuis le relevé")
        );
        // What was used since the reset is not known: never « 0 % ».
        assert_eq!(meter.rows[0].used, "utilisation inconnue");
        assert_eq!(meter.rows[0].level, Level::Low);
        assert_eq!(meter.pill, "semaine 10 %");
        assert_eq!(
            meter.background,
            "Relevé incomplet : les compteurs quotidiens limitent les tâches de fond"
        );
    }

    #[test]
    fn usage_view_unknown_percentage() {
        let usage = reading(vec![
            window(WindowKind::Session, None, Some(LimitStatus::Warning), 2),
            window(WindowKind::Weekly, Some(0.3), None, 150),
        ]);
        let meter = meter(&usage, 30, now(), &|_| 0).unwrap();
        assert_eq!(meter.pill, "5 h ?");
        assert_eq!(meter.label, "5 h : utilisation inconnue");
        assert_eq!(meter.level, Level::Paused);
        assert_eq!(meter.rows[0].used, "utilisation inconnue");
        assert_eq!(meter.rows[0].status.as_deref(), Some("proche de la limite"));
        // No window, no pill.
        assert_eq!(super::meter(&reading(Vec::new()), 30, now(), &|_| 0), None);
    }

    #[test]
    fn times_read_in_the_page_time() {
        assert_eq!(when(now() + Duration::hours(3), now(), &|_| 0), "15:00");
        assert_eq!(when(now() + Duration::hours(3), now(), &|_| 120), "17:00");
        assert_eq!(when(now() + Duration::days(1), now(), &|_| 0), "mer. 12:00");
        assert_eq!(
            when(now() + Duration::days(9), now(), &|_| 0),
            "26/11 12:00"
        );
        // Seconds round up: a reset is never shown earlier than it is.
        assert_eq!(
            when(now() + Duration::seconds(3 * 3600 + 1), now(), &|_| 0),
            "15:01"
        );
        // Each instant in its own offset: a reset after a change to daylight saving time.
        let change = now() + Duration::hours(2);
        let offset = |at: DateTime<Utc>| if at < change { 60 } else { 120 };
        assert_eq!(when(now() + Duration::hours(3), now(), &offset), "17:00");
        assert_eq!(when(now() + Duration::hours(1), now(), &offset), "14:00");
        assert_eq!(age(now(), now()), "à l'instant");
        assert_eq!(age(now() - Duration::hours(3), now()), "il y a 3 h");
        assert_eq!(age(now() - Duration::days(2), now()), "il y a 2 j");
    }
}
