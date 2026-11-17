//! How much of a provider's subscription windows is used, as read from what the provider's agent
//! sends during turns. Only what was read is held: a field the provider did not give stays
//! `None`, and nothing here is estimated.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// The latest windows read for one provider.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct ProviderUsage {
    /// The provider's id, as `threads.provider` stores it (`claude`).
    pub provider: String,
    /// When the latest reading that changed these windows was received: how old they are.
    pub known_since: DateTime<Utc>,
    /// At most one per [`WindowKind`].
    pub windows: Vec<Window>,
}

impl ProviderUsage {
    /// The windows as they stand at `now`: see [`Window::now`].
    pub fn now(&self, now: DateTime<Utc>) -> Self {
        Self {
            windows: self.windows.iter().map(|window| window.now(now)).collect(),
            ..self.clone()
        }
    }

    /// The window of `kind`, if one was read.
    pub fn window(&self, kind: &WindowKind) -> Option<&Window> {
        self.windows.iter().find(|window| &window.kind == kind)
    }
}

/// The windows the owner's reserve guards: the 5-hour one and the weekly one, all models.
pub const GUARDED: [WindowKind; 2] = [WindowKind::Session, WindowKind::Weekly];

/// Whether background work (sub-agents started or sent a message, wake turns) may spend a
/// provider's subscription now, under the owner's reserve: `[threads] reserve`, the share of
/// each guarded window kept for the owner. See [`ProviderUsage::background`].
#[derive(Clone, PartialEq, Debug)]
pub enum Background {
    /// Every guarded window has a fresh percentage below the line; `headroom` is the smallest
    /// distance to it, as a fraction.
    Allowed { headroom: f64 },
    /// A window says the line is reached: background work waits.
    Paused(Pause),
    /// No reading decides: none yet, a guarded window missing, without a percentage or reset
    /// since it was read, or the reserve is off (`0%`). The daily counts apply.
    Unknown,
}

/// The window that pauses background work, as it was read.
#[derive(Clone, PartialEq, Debug)]
pub struct Pause {
    pub window: WindowKind,
    pub used: Option<f64>,
    pub status: Option<LimitStatus>,
    pub resets_at: Option<DateTime<Utc>>,
}

impl Pause {
    /// Its use in whole percent, when the provider gave one.
    pub fn used_percent(&self) -> Option<u8> {
        self.used.map(percent)
    }
}

/// `used`, a fraction from 0 to 1, in whole percent, rounded down: what is shown, and what the
/// line is compared with, so that both always agree. The small nudge keeps a fraction such as
/// 0.82, which is 81.999… in binary, at 82.
pub fn percent(used: f64) -> u8 {
    // Within 0..=100 once clamped: the cast loses nothing.
    (used * 100.0 + 1e-6).floor().clamp(0.0, 100.0) as u8
}

/// The line of a reserve of `reserve` percent, in whole percent: the use background work may
/// reach before it pauses (70 for a reserve of 30).
pub fn line(reserve: u8) -> u8 {
    100 - reserve.min(100)
}

/// Whether `used` is at the line of a reserve of `reserve` percent or past it, compared in
/// whole percent ([`percent`]).
pub fn at_line(used: f64, reserve: u8) -> bool {
    percent(used) >= line(reserve)
}

/// `at` rounded up to the next whole minute: a reset shown to the minute is never earlier than
/// the provider's.
pub fn ceil_minute(at: DateTime<Utc>) -> DateTime<Utc> {
    let seconds = at.timestamp();
    let whole = at.timestamp_subsec_nanos() == 0 && seconds.rem_euclid(60) == 0;
    let floor = seconds - seconds.rem_euclid(60);
    let minute = if whole { floor } else { floor + 60 };
    DateTime::from_timestamp(minute, 0).unwrap_or(at)
}

/// [`ProviderUsage::background`] of `usage`, [`Background::Unknown`] without a reading.
pub fn background(usage: Option<&ProviderUsage>, reserve: u8, now: DateTime<Utc>) -> Background {
    usage.map_or(Background::Unknown, |usage| usage.background(reserve, now))
}

impl ProviderUsage {
    /// Whether background work may spend this provider's windows at `now`, keeping `reserve`
    /// percent of each guarded window ([`GUARDED`]) for the owner. Read with [`Window::now`]:
    ///
    /// - [`Background::Paused`] when a guarded window is `rejected`, its use is at the line or
    ///   above, or it is `warning` without a percentage (the provider says it is close; how
    ///   close is not known), or when paid extra usage is being spent (`extra` in `warning`).
    ///   Of several, the one that resets last, one without a reset time first;
    /// - [`Background::Allowed`] when every guarded window has a percentage below the line;
    /// - [`Background::Unknown`] otherwise: a guarded window not read, read without a
    ///   percentage, or reset since it was read (what was used since is not known), and always
    ///   with a reserve of 0.
    pub fn background(&self, reserve: u8, now: DateTime<Utc>) -> Background {
        if reserve == 0 {
            return Background::Unknown;
        }
        let windows: Vec<Window> = self.windows.iter().map(|window| window.now(now)).collect();
        let pauses = |window: &Window| {
            if window.reset_since_reading {
                return false;
            }
            match window.kind {
                WindowKind::Session | WindowKind::Weekly => {
                    window.status == Some(LimitStatus::Rejected)
                        || window.used.is_some_and(|used| at_line(used, reserve))
                        || (window.status == Some(LimitStatus::Warning) && window.used.is_none())
                }
                WindowKind::Extra => window.status == Some(LimitStatus::Warning),
                WindowKind::WeeklyModel(_) | WindowKind::Other(_) => false,
            }
        };
        // The pause that lasts longest: no reset time, then the latest.
        let pause = windows
            .iter()
            .filter(|window| pauses(window))
            .max_by_key(|window| window.resets_at.map_or(i64::MAX, |at| at.timestamp()));
        if let Some(window) = pause {
            return Background::Paused(Pause {
                window: window.kind.clone(),
                used: window.used,
                status: window.status,
                resets_at: window.resets_at,
            });
        }
        let mut headroom = f64::INFINITY;
        for kind in &GUARDED {
            let Some(window) = windows.iter().find(|window| &window.kind == kind) else {
                return Background::Unknown;
            };
            match window.used {
                Some(used) if !window.reset_since_reading => {
                    let left = f64::from(line(reserve)) - f64::from(percent(used));
                    headroom = headroom.min(left / 100.0);
                }
                _ => return Background::Unknown,
            }
        }
        Background::Allowed { headroom }
    }
}

impl WindowKind {
    /// The window's name in a sentence, in English: `5-hour window`, `weekly window`.
    pub fn describe(&self) -> String {
        match self {
            Self::Session => "5-hour window".to_owned(),
            Self::Weekly => "weekly window".to_owned(),
            Self::WeeklyModel(model) => format!("weekly {model} window"),
            Self::Extra => "paid extra usage".to_owned(),
            Self::Other(id) => format!("{id} window"),
        }
    }
}

/// One usage window of a subscription, as last read.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Window {
    pub kind: WindowKind,
    /// The fraction used, from 0 to 1, when the provider gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<LimitStatus>,
    /// When the provider says the window starts again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<DateTime<Utc>>,
    /// When Fronde received the reading.
    pub as_of: DateTime<Utc>,
    /// Set by [`Window::now`] once `resets_at` has passed: the window started again since it
    /// was read.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reset_since_reading: bool,
}

impl Window {
    /// The window as it stands at `now`: once its `resets_at` has passed, it reads as reset,
    /// marked `reset_since_reading`, with neither use nor status: what was used since the reset
    /// is not known. That follows the provider's own reset time; nothing else is inferred.
    pub fn now(&self, now: DateTime<Utc>) -> Self {
        match self.resets_at {
            Some(at) if at <= now => Self {
                used: None,
                status: None,
                reset_since_reading: true,
                ..self.clone()
            },
            _ => self.clone(),
        }
    }

    /// Whether `other` says the same as this window, whenever either was received.
    pub fn same_reading(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.used == other.used
            && self.status == other.status
            && self.resets_at == other.resets_at
    }
}

/// Which window: its length comes from its kind (5 hours, 7 days).
///
/// As JSON, a string: `session`, `weekly`, `weekly_model:<model>`, `extra`, `other:<id>`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum WindowKind {
    /// The 5-hour window.
    Session,
    /// The weekly window, all models.
    Weekly,
    /// A weekly window of one model family (`opus`, `sonnet`).
    WeeklyModel(String),
    /// Paid extra usage beyond the subscription.
    Extra,
    /// A window Fronde has no name for, under the provider's id.
    Other(String),
}

impl fmt::Display for WindowKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session => f.write_str("session"),
            Self::Weekly => f.write_str("weekly"),
            Self::WeeklyModel(model) => write!(f, "weekly_model:{model}"),
            Self::Extra => f.write_str("extra"),
            Self::Other(id) => write!(f, "other:{id}"),
        }
    }
}

/// A string that names no window kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownKind(pub String);

impl fmt::Display for UnknownKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown usage window kind {:?}", self.0)
    }
}

impl std::error::Error for UnknownKind {}

impl FromStr for WindowKind {
    type Err = UnknownKind;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "session" => Ok(Self::Session),
            "weekly" => Ok(Self::Weekly),
            "extra" => Ok(Self::Extra),
            _ => match text.split_once(':') {
                Some(("weekly_model", model)) if !model.is_empty() => {
                    Ok(Self::WeeklyModel(model.to_owned()))
                }
                Some(("other", id)) if !id.is_empty() => Ok(Self::Other(id.to_owned())),
                _ => Err(UnknownKind(text.to_owned())),
            },
        }
    }
}

impl Serialize for WindowKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for WindowKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// What the provider says of a window.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum LimitStatus {
    Allowed,
    /// Allowed, but close to the limit.
    Warning,
    /// The limit is reached: turns are refused until the window resets.
    Rejected,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0).unwrap()
    }

    fn window(resets_at: Option<i64>) -> Window {
        Window {
            kind: WindowKind::Session,
            used: Some(0.42),
            status: Some(LimitStatus::Warning),
            resets_at: resets_at.map(at),
            as_of: at(1_000),
            reset_since_reading: false,
        }
    }

    #[test]
    fn window_kind_json_shape() {
        let kinds = [
            (WindowKind::Session, "session"),
            (WindowKind::Weekly, "weekly"),
            (WindowKind::WeeklyModel("opus".into()), "weekly_model:opus"),
            (WindowKind::Extra, "extra"),
            (
                WindowKind::Other("seven_day_overage_included".into()),
                "other:seven_day_overage_included",
            ),
        ];
        for (kind, text) in kinds {
            assert_eq!(serde_json::to_value(&kind).unwrap(), json!(text));
            assert_eq!(
                serde_json::from_value::<WindowKind>(json!(text)).unwrap(),
                kind
            );
        }
        for bad in ["", "monthly", "weekly_model:", "other:", "Session"] {
            assert!(
                serde_json::from_value::<WindowKind>(json!(bad)).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn window_json_shape() {
        let window = window(Some(2_000));
        let value = serde_json::to_value(&window).unwrap();
        assert_eq!(
            value,
            json!({
                "kind": "session",
                "used": 0.42,
                "status": "warning",
                "resets_at": "1970-01-01T00:33:20Z",
                "as_of": "1970-01-01T00:16:40Z",
            })
        );
        assert_eq!(serde_json::from_value::<Window>(value).unwrap(), window);
        // Only what was read: a window with a status alone.
        let bare: Window = serde_json::from_value(
            json!({"kind": "weekly", "status": "rejected", "as_of": "1970-01-01T00:16:40Z"}),
        )
        .unwrap();
        assert_eq!(bare.used, None);
        assert_eq!(bare.resets_at, None);
        assert_eq!(bare.status, Some(LimitStatus::Rejected));
    }

    #[test]
    fn window_reads_as_reset_after_its_reset() {
        let window = window(Some(2_000));
        assert_eq!(window.now(at(1_999)), window);
        for now in [at(2_000), at(9_000)] {
            let reset = window.now(now);
            assert_eq!(reset.used, None, "not known since the reset");
            assert_eq!(reset.status, None);
            assert!(reset.reset_since_reading);
            assert_eq!(reset.resets_at, window.resets_at);
            assert_eq!(reset.as_of, window.as_of);
        }
        // Without a reset time, nothing is inferred.
        let unknown = super::Window {
            resets_at: None,
            ..window
        };
        assert_eq!(unknown.now(at(1_000_000)), unknown);
    }

    fn reading(windows: Vec<Window>) -> ProviderUsage {
        ProviderUsage {
            provider: "claude".into(),
            known_since: at(1_000),
            windows,
        }
    }

    fn guarded(kind: WindowKind, used: Option<f64>, status: Option<LimitStatus>) -> Window {
        Window {
            kind,
            used,
            status,
            resets_at: Some(at(10_000)),
            as_of: at(1_000),
            reset_since_reading: false,
        }
    }

    fn both(session: f64, weekly: f64) -> ProviderUsage {
        reading(vec![
            guarded(
                WindowKind::Session,
                Some(session),
                Some(LimitStatus::Allowed),
            ),
            guarded(WindowKind::Weekly, Some(weekly), Some(LimitStatus::Allowed)),
        ])
    }

    #[test]
    fn background_pauses_at_the_line_of_either_window() {
        let now = at(2_000);
        match both(0.42, 0.18).background(30, now) {
            Background::Allowed { headroom } => assert!((headroom - 0.28).abs() < 1e-9),
            other => panic!("{other:?}"),
        }
        for (session, weekly, kind) in [
            (0.70, 0.10, WindowKind::Session),
            (0.10, 0.85, WindowKind::Weekly),
        ] {
            let Background::Paused(pause) = both(session, weekly).background(30, now) else {
                panic!("{session} {weekly}");
            };
            assert_eq!(pause.window, kind);
            assert_eq!(pause.resets_at, Some(at(10_000)));
        }
        assert_eq!(
            both(0.72, 0.1).background(30, now),
            Background::Paused(Pause {
                window: WindowKind::Session,
                used: Some(0.72),
                status: Some(LimitStatus::Allowed),
                resets_at: Some(at(10_000)),
            })
        );
        // Another reserve, another line.
        assert!(matches!(
            both(0.72, 0.1).background(20, now),
            Background::Allowed { .. }
        ));
        assert!(matches!(
            both(0.01, 0.0).background(100, now),
            Background::Paused(_)
        ));
    }

    #[test]
    fn warning_without_percentage_pauses() {
        let usage = reading(vec![
            guarded(WindowKind::Session, None, Some(LimitStatus::Warning)),
            guarded(WindowKind::Weekly, Some(0.1), None),
        ]);
        let Background::Paused(pause) = usage.background(30, at(2_000)) else {
            panic!();
        };
        assert_eq!((pause.window, pause.used), (WindowKind::Session, None));
        // So does a rejected window, whatever its percentage says.
        let usage = reading(vec![
            guarded(WindowKind::Session, Some(0.1), None),
            guarded(WindowKind::Weekly, Some(0.2), Some(LimitStatus::Rejected)),
        ]);
        assert!(matches!(
            usage.background(30, at(2_000)),
            Background::Paused(Pause {
                window: WindowKind::Weekly,
                ..
            })
        ));
        // A warning with a percentage below the line is judged by the percentage.
        let usage = reading(vec![
            guarded(WindowKind::Session, Some(0.5), Some(LimitStatus::Warning)),
            guarded(WindowKind::Weekly, Some(0.2), None),
        ]);
        assert!(matches!(
            usage.background(30, at(2_000)),
            Background::Allowed { .. }
        ));
    }

    #[test]
    fn overage_in_use_pauses() {
        let mut usage = both(0.1, 0.1);
        usage.windows.push(Window {
            kind: WindowKind::Extra,
            used: None,
            status: Some(LimitStatus::Warning),
            resets_at: None,
            as_of: at(1_000),
            reset_since_reading: false,
        });
        assert!(matches!(
            usage.background(30, at(2_000)),
            Background::Paused(Pause {
                window: WindowKind::Extra,
                resets_at: None,
                ..
            })
        ));
    }

    #[test]
    fn reset_time_ends_the_pause() {
        let usage = both(0.9, 0.1);
        assert!(matches!(
            usage.background(30, at(9_999)),
            Background::Paused(_)
        ));
        // Reset since the reading: what was used since is not known, so the counts decide.
        assert_eq!(usage.background(30, at(10_000)), Background::Unknown);
    }

    #[test]
    fn nothing_decides_without_a_full_reading() {
        let now = at(2_000);
        assert_eq!(background(None, 30, now), Background::Unknown);
        // The weekly window was never read.
        let session_only = reading(vec![guarded(
            WindowKind::Session,
            Some(0.1),
            Some(LimitStatus::Allowed),
        )]);
        assert_eq!(session_only.background(30, now), Background::Unknown);
        // A status that tells nothing, and no percentage.
        let bare = reading(vec![
            guarded(WindowKind::Session, None, Some(LimitStatus::Allowed)),
            guarded(WindowKind::Weekly, Some(0.1), None),
        ]);
        assert_eq!(bare.background(30, now), Background::Unknown);
        // A model's own weekly window does not count.
        let mut usage = both(0.1, 0.1);
        usage.windows.push(guarded(
            WindowKind::WeeklyModel("opus".into()),
            Some(0.99),
            Some(LimitStatus::Rejected),
        ));
        assert!(matches!(
            usage.background(30, now),
            Background::Allowed { .. }
        ));
    }

    #[test]
    fn reserve_zero_is_off() {
        let usage = both(1.0, 1.0);
        assert_eq!(usage.background(0, at(2_000)), Background::Unknown);
    }

    #[test]
    fn percent_rounds_down_within_bounds() {
        assert_eq!(percent(0.724), 72);
        assert_eq!(percent(0.729), 72);
        assert_eq!(percent(0.82), 82);
        assert_eq!(percent(0.07), 7);
        assert_eq!(percent(1.03), 100);
        assert_eq!(percent(-0.02), 0);
    }

    /// The line compares in whole percent: a reserve of 18 % pauses at exactly 82 %, as shown.
    #[test]
    fn a_reading_exactly_at_the_line_pauses() {
        assert_eq!(line(18), 82);
        assert!(at_line(0.82, 18));
        assert!(!at_line(0.8199, 18));
        for reserve in 1..=100u8 {
            let edge = f64::from(line(reserve)) / 100.0;
            assert!(at_line(edge, reserve), "{reserve}");
        }
        let usage = both(0.82, 0.1);
        assert!(matches!(
            usage.background(18, at(2_000)),
            Background::Paused(Pause {
                window: WindowKind::Session,
                ..
            })
        ));
        assert!(matches!(
            both(0.8199, 0.1).background(18, at(2_000)),
            Background::Allowed { .. }
        ));
    }

    #[test]
    fn reset_times_round_up_to_the_minute() {
        assert_eq!(ceil_minute(at(120)), at(120));
        assert_eq!(ceil_minute(at(121)), at(180));
        assert_eq!(ceil_minute(at(179)), at(180));
        let with_millis = DateTime::from_timestamp_millis(120_001).unwrap();
        assert_eq!(ceil_minute(with_millis), at(180));
    }

    #[test]
    fn provider_usage_reads_its_windows_now() {
        let usage = ProviderUsage {
            provider: "claude".into(),
            known_since: at(1_000),
            windows: vec![window(Some(2_000))],
        };
        let now = usage.now(at(3_000));
        assert!(
            now.window(&WindowKind::Session)
                .unwrap()
                .reset_since_reading
        );
        assert_eq!(now.window(&WindowKind::Weekly), None);
        assert_eq!(now.known_since, usage.known_since);
    }
}
