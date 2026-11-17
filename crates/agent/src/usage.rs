//! How much of the owner's subscription windows is used, read from what a provider's agent sends
//! during turns ([`UsageSource`]), and kept per provider in the [`UsageBook`].
//!
//! The Claude adapter (`@agentclientprotocol/claude-agent-acp` 0.107.0) sends a reading only
//! after the Claude Code CLI emits a `rate_limit_event`, which it does when the limits change:
//! the event's `rate_limit_info` rides in `_meta["_claude/rateLimit"]` of the next
//! `usage_update`, and in the `data.rateLimit` of a prompt refused for a limit
//! (`data.errorKind == "rate_limit"`). Everything there is untrusted input: a field that is
//! missing, of the wrong type or out of range is left out (logged at debug level, by name, never
//! the whole `_meta`), and never fails a turn.

mod book;

pub use book::UsageBook;

use crate::acp::AgentSpec;
use agent_client_protocol::v1::schema::UsageUpdate;
use chrono::{DateTime, Local, TimeZone, Utc};
use fronde_protocol::usage::{LimitStatus, Window, WindowKind};
use serde_json::{Map, Value};
use std::fmt;
use std::sync::Arc;

/// The key of the Claude adapter's `_meta` that holds the CLI's latest `rate_limit_info`.
pub const CLAUDE_RATE_LIMIT: &str = "_claude/rateLimit";

/// How far past 0 or 1 a `utilization` is still taken as a fraction, and clamped: a window
/// over its limit may read a little above 1. Anything further is not a fraction (`42` would be
/// a percentage) and is dropped rather than rescaled.
const FRACTION_SLACK: f64 = 0.05;

/// How far from the reading's time a `resetsAt` may be: the longest window is a week, extra
/// usage a month. Further off, it is not Unix seconds (milliseconds, say) and is dropped.
const RESET_RANGE_DAYS: i64 = 366;

/// Where a provider's agent says how much of its subscription is used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageSource {
    /// Nowhere Fronde has read: no reading.
    None,
    /// The Claude adapter's `_meta["_claude/rateLimit"]` on `usage_update`, and its prompt
    /// error's `data` for a turn refused for a limit.
    ClaudeRateLimitMeta,
}

/// A turn refused because a usage limit is reached, as the agent's error said it.
#[derive(Clone, Debug, PartialEq)]
pub struct RateLimit {
    /// The windows the error carried, maybe none.
    pub windows: Vec<Window>,
    /// When turns may run again: the latest reset of a window the provider says is rejected,
    /// else the reset of the window the limit is about.
    pub resumes_at: Option<DateTime<Utc>>,
}

impl UsageSource {
    /// The windows a `usage_update`'s `_meta` carries, received at `as_of`; `None` when it
    /// carries no reading.
    pub fn read(
        self,
        meta: Option<&Map<String, Value>>,
        as_of: DateTime<Utc>,
    ) -> Option<Vec<Window>> {
        match self {
            Self::None => None,
            Self::ClaudeRateLimitMeta => {
                let info = meta?.get(CLAUDE_RATE_LIMIT)?;
                let (windows, _) = claude_windows(info, as_of)?;
                (!windows.is_empty()).then_some(windows)
            }
        }
    }

    /// The limit a prompt error's `data` says the turn hit; `None` for any other error.
    pub fn read_error(self, data: Option<&Value>, as_of: DateTime<Utc>) -> Option<RateLimit> {
        match self {
            Self::None => None,
            Self::ClaudeRateLimitMeta => {
                let data = data?.as_object()?;
                if data.get("errorKind").and_then(Value::as_str) != Some("rate_limit") {
                    return None;
                }
                let (windows, representative) = data
                    .get("rateLimit")
                    .and_then(|info| claude_windows(info, as_of))
                    .unwrap_or_default();
                let rejected = windows
                    .iter()
                    .filter(|window| window.status == Some(LimitStatus::Rejected))
                    .filter_map(|window| window.resets_at)
                    .max();
                let resumes_at = rejected.or_else(|| {
                    let kind = representative?;
                    windows.iter().find(|window| window.kind == kind)?.resets_at
                });
                Some(RateLimit {
                    windows,
                    resumes_at,
                })
            }
        }
    }
}

/// The windows of one Claude `rate_limit_info`, and the kind of the window it is about (its
/// `rateLimitType`). `None` when it is not an object.
///
/// The undocumented `unifiedWindows` (`{ five_hour, seven_day, seven_day_overage_included }`,
/// each `{ utilization, resetsAt, status?, surpassedThreshold? }`) gives both windows at once
/// when present; then the documented fields describe the event's one window, and fill in what
/// `unifiedWindows` left out of it; then `isUsingOverage` (or `overageInUse`) marks paid extra
/// usage being spent. `overageStatus` alone is not read: an account without extra usage
/// reports it too.
fn claude_windows(info: &Value, as_of: DateTime<Utc>) -> Option<(Vec<Window>, Option<WindowKind>)> {
    let Some(info) = info.as_object() else {
        tracing::debug!("ignored a Claude rate limit that is not an object");
        return None;
    };
    let mut windows: Vec<Window> = Vec::new();
    match info.get("unifiedWindows") {
        None | Some(Value::Null) => {}
        Some(Value::Object(unified)) => {
            for (id, entry) in unified {
                let Some(kind) = claude_kind(id) else {
                    continue;
                };
                let Some(entry) = entry.as_object() else {
                    tracing::debug!(window = %kind, "ignored a Claude usage window that is not an object");
                    continue;
                };
                if windows.iter().any(|window| window.kind == kind) {
                    continue;
                }
                if let Some(window) = claude_window(kind, entry, as_of) {
                    windows.push(window);
                }
            }
        }
        Some(_) => tracing::debug!("ignored Claude's unifiedWindows: not an object"),
    }
    let representative = match info.get("rateLimitType") {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) => claude_kind(id),
        Some(_) => {
            tracing::debug!("ignored Claude's rateLimitType: not a string");
            None
        }
    };
    if let Some(kind) = &representative {
        let documented = claude_window(kind.clone(), info, as_of);
        match (
            windows.iter_mut().find(|window| &window.kind == kind),
            documented,
        ) {
            (Some(known), Some(documented)) => {
                known.used = known.used.or(documented.used);
                known.status = known.status.or(documented.status);
                known.resets_at = known.resets_at.or(documented.resets_at);
            }
            (None, Some(documented)) => windows.push(documented),
            (_, None) => {}
        }
    }
    let overage = ["isUsingOverage", "overageInUse"]
        .iter()
        .any(|key| info.get(*key).and_then(Value::as_bool) == Some(true));
    if overage
        && !windows
            .iter()
            .any(|window| window.kind == WindowKind::Extra)
    {
        windows.push(Window {
            kind: WindowKind::Extra,
            used: None,
            status: Some(LimitStatus::Warning),
            resets_at: info
                .get("overageResetsAt")
                .and_then(|value| reset_time(WindowKind::Extra, value, as_of)),
            as_of,
            reset_since_reading: false,
        });
    }
    Some((windows, representative))
}

/// One window from an object with Claude's `utilization`, `status` and `resetsAt`; `None` when
/// none of them reads.
fn claude_window(
    kind: WindowKind,
    fields: &Map<String, Value>,
    as_of: DateTime<Utc>,
) -> Option<Window> {
    let used = fields
        .get("utilization")
        .and_then(|value| fraction(&kind, value));
    let status = fields.get("status").and_then(|value| status(&kind, value));
    let resets_at = fields
        .get("resetsAt")
        .and_then(|value| reset_time(kind.clone(), value, as_of));
    if used.is_none() && status.is_none() && resets_at.is_none() {
        return None;
    }
    Some(Window {
        kind,
        used,
        status,
        resets_at,
        as_of,
        reset_since_reading: false,
    })
}

/// The kind of a Claude window id (`rateLimitType`, or a key of `unifiedWindows`); an id Fronde
/// has no name for is kept as [`WindowKind::Other`] when it looks like one.
fn claude_kind(id: &str) -> Option<WindowKind> {
    Some(match id {
        "five_hour" => WindowKind::Session,
        "seven_day" => WindowKind::Weekly,
        "seven_day_opus" => WindowKind::WeeklyModel("opus".into()),
        "seven_day_sonnet" => WindowKind::WeeklyModel("sonnet".into()),
        "overage" => WindowKind::Extra,
        _ if !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_') =>
        {
            WindowKind::Other(id.to_owned())
        }
        _ => {
            tracing::debug!(
                len = id.len(),
                "ignored a Claude usage window with an odd id"
            );
            return None;
        }
    })
}

/// A `utilization` as a fraction from 0 to 1: clamped when just outside, dropped when further or
/// not a finite number.
fn fraction(kind: &WindowKind, value: &Value) -> Option<f64> {
    let Some(used) = value.as_f64().filter(|used| used.is_finite()) else {
        tracing::debug!(window = %kind, "ignored a usage that is not a finite number");
        return None;
    };
    if !(-FRACTION_SLACK..=1.0 + FRACTION_SLACK).contains(&used) {
        tracing::debug!(window = %kind, used, "ignored a usage that is not a fraction");
        return None;
    }
    Some(used.clamp(0.0, 1.0))
}

fn status(kind: &WindowKind, value: &Value) -> Option<LimitStatus> {
    match value.as_str() {
        Some("allowed") => Some(LimitStatus::Allowed),
        Some("allowed_warning") => Some(LimitStatus::Warning),
        Some("rejected") => Some(LimitStatus::Rejected),
        _ => {
            tracing::debug!(window = %kind, "ignored an unknown usage status");
            None
        }
    }
}

/// A `resetsAt` in Unix seconds, within [`RESET_RANGE_DAYS`] of `as_of`.
fn reset_time(kind: WindowKind, value: &Value, as_of: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let seconds = value.as_f64().filter(|seconds| seconds.is_finite());
    let range = (RESET_RANGE_DAYS * 86_400) as f64;
    let at = seconds
        .filter(|seconds| (seconds - as_of.timestamp() as f64).abs() <= range)
        .and_then(|seconds| DateTime::from_timestamp_millis((seconds * 1000.0).round() as i64));
    if at.is_none() {
        tracing::debug!(window = %kind, "ignored a reset time that is not Unix seconds near now");
    }
    at
}

/// A turn the agent refused because a usage limit is reached: what the turn's failure says.
#[derive(Clone, Debug, PartialEq)]
pub struct RateLimited {
    pub provider: &'static str,
    pub resumes_at: Option<DateTime<Utc>>,
}

impl RateLimited {
    /// The notice the turn ends with, its time in the server's time zone.
    pub fn notice(&self) -> String {
        rate_limit_notice(self.resumes_at, Utc::now(), &Local)
    }
}

impl fmt::Display for RateLimited {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the {} subscription's usage limit is reached",
            self.provider
        )?;
        match self.resumes_at {
            Some(at) => write!(f, "; turns run again from {}", at.to_rfc3339()),
            None => f.write_str("; the agent gave no reset time"),
        }
    }
}

impl std::error::Error for RateLimited {}

/// « Limite d'usage atteinte, reprise vers 16:40. », the day added when the reset is a day or
/// more away; without a reset time, only that the limit is reached.
pub fn rate_limit_notice<Tz: TimeZone>(
    resumes_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    zone: &Tz,
) -> String
where
    Tz::Offset: fmt::Display,
{
    let Some(at) = resumes_at else {
        return "Limite d'usage atteinte : réessaie plus tard.".into();
    };
    let local = at.with_timezone(zone);
    if at - now < chrono::Duration::hours(24) {
        format!(
            "Limite d'usage atteinte, reprise vers {}.",
            local.format("%H:%M")
        )
    } else {
        format!(
            "Limite d'usage atteinte, reprise vers {} le {}.",
            local.format("%H:%M"),
            local.format("%d/%m")
        )
    }
}

/// What one agent connection does with the readings it receives: read them with its provider's
/// [`UsageSource`] and record them in the book, if it has one.
#[derive(Clone)]
pub(crate) struct Capture {
    provider: &'static str,
    source: UsageSource,
    book: Option<Arc<UsageBook>>,
}

impl Capture {
    pub(crate) fn of(spec: &AgentSpec) -> Self {
        Self {
            provider: spec.provider.id,
            source: spec.provider.usage,
            book: spec.usage.clone(),
        }
    }

    /// Records the reading a `usage_update` carries, if any. Context tokens and cost are not
    /// kept.
    pub(crate) fn update(&self, update: &UsageUpdate) {
        if let Some(windows) = self.source.read(update.meta.as_ref(), Utc::now()) {
            self.record(windows);
        }
    }

    /// The limit a prompt error says the turn hit, its windows recorded; `None` for any other
    /// error.
    pub(crate) fn error(&self, error: &agent_client_protocol::Error) -> Option<RateLimited> {
        let limit = self.source.read_error(error.data.as_ref(), Utc::now())?;
        if !limit.windows.is_empty() {
            self.record(limit.windows);
        }
        Some(RateLimited {
            provider: self.provider,
            resumes_at: limit.resumes_at,
        })
    }

    fn record(&self, windows: Vec<Window>) {
        let kinds: Vec<String> = windows
            .iter()
            .map(|window| window.kind.to_string())
            .collect();
        match &self.book {
            Some(book) => {
                let changed = book.record(self.provider, windows);
                tracing::debug!(provider = self.provider, ?kinds, changed, "usage reading");
            }
            None => tracing::debug!(
                provider = self.provider,
                ?kinds,
                "usage reading not kept: no book"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;
    use serde_json::json;

    /// The `rate_limit_info` shape read from the CLI's code: both windows in `unifiedWindows`,
    /// the five-hour one as the event's own. U7 replaces it with a real capture.
    const FIXTURE: &str = include_str!("../tests/fixtures/claude-rate-limit.json");

    /// 2026-11-17 12:00 UTC.
    fn as_of() -> DateTime<Utc> {
        DateTime::from_timestamp(1_794_916_800, 0).unwrap()
    }

    fn at(seconds: i64) -> Option<DateTime<Utc>> {
        Some(DateTime::from_timestamp(seconds, 0).unwrap())
    }

    fn meta(info: Value) -> Map<String, Value> {
        let Value::Object(meta) = json!({
            CLAUDE_RATE_LIMIT: info,
            "_claude/model": "claude-opus-4-5",
        }) else {
            unreachable!()
        };
        meta
    }

    fn read(info: Value) -> Option<Vec<Window>> {
        UsageSource::ClaudeRateLimitMeta.read(Some(&meta(info)), as_of())
    }

    fn window(
        kind: WindowKind,
        used: Option<f64>,
        status: Option<LimitStatus>,
        resets_at: Option<DateTime<Utc>>,
    ) -> Window {
        Window {
            kind,
            used,
            status,
            resets_at,
            as_of: as_of(),
            reset_since_reading: false,
        }
    }

    #[test]
    fn usage_reads_both_windows_from_unified_windows() {
        let windows = read(serde_json::from_str(FIXTURE).unwrap()).unwrap();
        assert_eq!(
            windows,
            [
                window(
                    WindowKind::Session,
                    Some(0.91),
                    Some(LimitStatus::Warning),
                    at(1_794_931_200)
                ),
                window(
                    WindowKind::Weekly,
                    Some(0.18),
                    Some(LimitStatus::Allowed),
                    at(1_795_424_400)
                ),
            ]
        );
        // The overage-included weekly window is kept under its id.
        let windows = read(json!({
            "status": "allowed",
            "unifiedWindows": {"seven_day_overage_included": {"utilization": 0.5, "resetsAt": 1_795_424_400}},
        }))
        .unwrap();
        assert_eq!(
            windows[0].kind,
            WindowKind::Other("seven_day_overage_included".into())
        );
        assert_eq!(windows[0].used, Some(0.5));
    }

    #[test]
    fn usage_reads_the_documented_window_alone() {
        let windows = read(json!({
            "status": "allowed_warning",
            "resetsAt": 1_795_424_400,
            "rateLimitType": "seven_day",
            "utilization": 0.8,
        }))
        .unwrap();
        assert_eq!(
            windows,
            [window(
                WindowKind::Weekly,
                Some(0.8),
                Some(LimitStatus::Warning),
                at(1_795_424_400)
            )]
        );
        // A status and a reset, no percentage: kept as read.
        let windows = read(
            json!({"status": "rejected", "resetsAt": 1_794_931_200, "rateLimitType": "five_hour"}),
        )
        .unwrap();
        assert_eq!(
            windows,
            [window(
                WindowKind::Session,
                None,
                Some(LimitStatus::Rejected),
                at(1_794_931_200)
            )]
        );
        // Per-model weekly windows, only ever the event's own.
        for (id, model) in [("seven_day_opus", "opus"), ("seven_day_sonnet", "sonnet")] {
            let windows =
                read(json!({"status": "allowed", "rateLimitType": id, "utilization": 0.1}))
                    .unwrap();
            assert_eq!(windows[0].kind, WindowKind::WeeklyModel(model.into()));
        }
        // The documented fields fill what unifiedWindows left out of the same window.
        let windows = read(json!({
            "status": "rejected",
            "rateLimitType": "five_hour",
            "unifiedWindows": {"five_hour": {"utilization": 1.0, "resetsAt": 1_794_931_200}},
        }))
        .unwrap();
        assert_eq!(
            windows,
            [window(
                WindowKind::Session,
                Some(1.0),
                Some(LimitStatus::Rejected),
                at(1_794_931_200)
            )]
        );
        // A status with no type names no window.
        assert_eq!(read(json!({"status": "allowed"})), None);
    }

    #[test]
    fn usage_drops_a_used_out_of_range() {
        for bad in [
            json!(1.5),
            json!(42),
            json!(-0.1),
            json!("NaN"),
            json!("0.5"),
            json!(null),
            json!([0.5]),
        ] {
            let windows = read(json!({
                "status": "allowed",
                "rateLimitType": "five_hour",
                "utilization": bad,
                "resetsAt": 1_794_931_200,
            }))
            .unwrap();
            assert_eq!(windows[0].used, None, "{bad}");
            assert_eq!(windows[0].status, Some(LimitStatus::Allowed), "{bad}");
        }
        // Just over the edges: clamped.
        for (given, used) in [(1.02, 1.0), (-0.01, 0.0), (0.0, 0.0), (1.0, 1.0)] {
            let windows = read(
                json!({"status": "allowed", "rateLimitType": "five_hour", "utilization": given}),
            )
            .unwrap();
            assert_eq!(windows[0].used, Some(used), "{given}");
        }
        // Reset times in milliseconds, far off or not numbers are dropped.
        for bad in [
            json!(1_794_931_200_000_i64),
            json!(0),
            json!("1794931200"),
            json!(-1),
        ] {
            let windows =
                read(json!({"status": "allowed", "rateLimitType": "five_hour", "resetsAt": bad}))
                    .unwrap();
            assert_eq!(windows[0].resets_at, None, "{bad}");
        }
    }

    #[test]
    fn usage_keeps_an_unknown_kind() {
        let windows =
            read(json!({"status": "allowed", "rateLimitType": "thirty_day", "utilization": 0.3}))
                .unwrap();
        assert_eq!(
            windows,
            [window(
                WindowKind::Other("thirty_day".into()),
                Some(0.3),
                Some(LimitStatus::Allowed),
                None
            )]
        );
        // An id that does not look like one is dropped, not shown.
        let long = "a".repeat(65);
        for odd in ["", "Five Hour", "five-hour", "x\u{202e}y", long.as_str()] {
            assert_eq!(
                read(json!({"status": "allowed", "rateLimitType": odd})),
                None,
                "{odd:?}"
            );
        }
        // An unknown status is left out, the rest kept.
        let windows =
            read(json!({"status": "paused", "rateLimitType": "five_hour", "utilization": 0.3}))
                .unwrap();
        assert_eq!(windows[0].status, None);
        assert_eq!(windows[0].used, Some(0.3));
    }

    #[test]
    fn usage_ignores_malformed_meta() {
        let source = UsageSource::ClaudeRateLimitMeta;
        assert_eq!(source.read(None, as_of()), None);
        assert_eq!(source.read(Some(&Map::new()), as_of()), None);
        for bad in [
            json!(null),
            json!("allowed"),
            json!(42),
            json!([1, 2]),
            json!({}),
            json!({"status": 3}),
        ] {
            assert_eq!(read(bad.clone()), None, "{bad}");
        }
        // A malformed unifiedWindows, or entries in it, leave the documented window.
        for unified in [
            json!("five_hour"),
            json!([{"utilization": 0.5}]),
            json!({"five_hour": 0.5}),
            json!({"five_hour": {"utilization": "half"}}),
        ] {
            let windows = read(json!({
                "status": "allowed",
                "rateLimitType": "seven_day",
                "utilization": 0.2,
                "unifiedWindows": unified,
            }))
            .unwrap();
            assert_eq!(
                windows,
                [window(
                    WindowKind::Weekly,
                    Some(0.2),
                    Some(LimitStatus::Allowed),
                    None
                )],
                "{unified}"
            );
        }
        // Wrong types everywhere: nothing read, nothing panics.
        assert_eq!(
            read(json!({
                "status": [],
                "rateLimitType": 5,
                "utilization": {},
                "resetsAt": true,
                "isUsingOverage": "yes",
                "unifiedWindows": 7,
            })),
            None
        );
    }

    #[test]
    fn usage_marks_overage_in_use() {
        let windows = read(json!({
            "status": "allowed",
            "rateLimitType": "five_hour",
            "utilization": 1.0,
            "isUsingOverage": true,
            "overageStatus": "allowed",
            "overageResetsAt": 1_796_083_200,
        }))
        .unwrap();
        assert_eq!(
            windows[1],
            window(
                WindowKind::Extra,
                None,
                Some(LimitStatus::Warning),
                at(1_796_083_200)
            )
        );
        // Extra usage that is not being spent is not a window.
        let windows = read(json!({
            "status": "allowed",
            "rateLimitType": "five_hour",
            "utilization": 0.1,
            "isUsingOverage": false,
            "overageStatus": "rejected",
            "overageDisabledReason": "overage_not_provisioned",
        }))
        .unwrap();
        assert_eq!(windows.len(), 1);
        // The overage window as the event's own.
        let windows = read(
            json!({"status": "allowed_warning", "rateLimitType": "overage", "utilization": 0.7}),
        )
        .unwrap();
        assert_eq!(
            windows,
            [window(
                WindowKind::Extra,
                Some(0.7),
                Some(LimitStatus::Warning),
                None
            )]
        );
    }

    #[test]
    fn usage_reads_a_rate_limit_error() {
        let source = UsageSource::ClaudeRateLimitMeta;
        // As `turnErrorData` builds it.
        let data = json!({
            "errorKind": "rate_limit",
            "rateLimit": {
                "status": "rejected",
                "resetsAt": 1_794_931_200,
                "rateLimitType": "five_hour",
                "utilization": 1.0,
                "unifiedWindows": {
                    "five_hour": {"utilization": 1.0, "resetsAt": 1_794_931_200, "status": "rejected"},
                    "seven_day": {"utilization": 0.4, "resetsAt": 1_795_424_400, "status": "allowed"},
                },
            },
        });
        let limit = source.read_error(Some(&data), as_of()).unwrap();
        assert_eq!(limit.resumes_at, at(1_794_931_200));
        assert_eq!(limit.windows.len(), 2);
        // Both windows rejected: the later reset.
        let mut both = data.clone();
        both["rateLimit"]["unifiedWindows"]["seven_day"]["status"] = json!("rejected");
        assert_eq!(
            source.read_error(Some(&both), as_of()).unwrap().resumes_at,
            at(1_795_424_400)
        );
        // No window says rejected: the event's own window.
        let documented = json!({"errorKind": "rate_limit", "rateLimit": {"status": "allowed_warning", "rateLimitType": "seven_day", "resetsAt": 1_795_424_400}});
        assert_eq!(
            source
                .read_error(Some(&documented), as_of())
                .unwrap()
                .resumes_at,
            at(1_795_424_400)
        );
        // A rate limit with no info, or garbage info: still a rate limit, with no time.
        for data in [
            json!({"errorKind": "rate_limit"}),
            json!({"errorKind": "rate_limit", "rateLimit": "soon"}),
        ] {
            assert_eq!(
                source.read_error(Some(&data), as_of()),
                Some(RateLimit {
                    windows: Vec::new(),
                    resumes_at: None
                })
            );
        }
        // Any other error is not a rate limit.
        for data in [
            json!({"errorKind": "overloaded"}),
            json!({"errorKind": 1}),
            json!("rate_limit"),
            json!(null),
            json!({}),
        ] {
            assert_eq!(source.read_error(Some(&data), as_of()), None, "{data}");
        }
        assert_eq!(source.read_error(None, as_of()), None);
    }

    #[test]
    fn bare_provider_reads_nothing() {
        let bare = Provider::bare("codex");
        assert_eq!(bare.usage, UsageSource::None);
        let info: Value = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(bare.usage.read(Some(&meta(info.clone())), as_of()), None);
        let data = json!({"errorKind": "rate_limit", "rateLimit": info});
        assert_eq!(bare.usage.read_error(Some(&data), as_of()), None);
        assert_eq!(Provider::CLAUDE.usage, UsageSource::ClaudeRateLimitMeta);
    }

    #[test]
    fn rate_limit_notice_says_when() {
        let now = as_of();
        assert_eq!(
            rate_limit_notice(at(1_794_931_200), now, &Utc),
            "Limite d'usage atteinte, reprise vers 16:00."
        );
        let paris = chrono::FixedOffset::east_opt(3600).unwrap();
        assert_eq!(
            rate_limit_notice(at(1_794_931_200), now, &paris),
            "Limite d'usage atteinte, reprise vers 17:00."
        );
        assert_eq!(
            rate_limit_notice(at(1_795_424_400), now, &Utc),
            "Limite d'usage atteinte, reprise vers 09:00 le 23/11."
        );
        assert_eq!(
            rate_limit_notice(None, now, &Utc),
            "Limite d'usage atteinte : réessaie plus tard."
        );
    }
}
