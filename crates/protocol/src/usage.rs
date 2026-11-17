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
    /// with nothing used and no status, marked `reset_since_reading`. That follows the
    /// provider's own reset time; nothing else is inferred.
    pub fn now(&self, now: DateTime<Utc>) -> Self {
        match self.resets_at {
            Some(at) if at <= now => Self {
                used: Some(0.0),
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
            assert_eq!(reset.used, Some(0.0));
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
