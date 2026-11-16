//! Events and reminders <-> iCalendar. Times are stored in UTC; input and output use the user's zone.

use anyhow::anyhow;
use chrono::{DateTime, Duration, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use icalendar::{Calendar, CalendarDateTime, Component, DatePerhapsTime, EventLike, TodoStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    Date(NaiveDate),
    At(DateTime<Utc>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub uid: String,
    pub title: String,
    pub start: When,
    pub end: Option<When>,
    pub location: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reminder {
    pub uid: String,
    pub text: String,
    pub due: DateTime<Utc>,
    pub done: bool,
}

fn local_to_utc(ndt: NaiveDateTime, tz: Tz) -> Result<DateTime<Utc>, String> {
    match tz.from_local_datetime(&ndt) {
        LocalResult::Single(t) => Ok(t.with_timezone(&Utc)),
        LocalResult::Ambiguous(a, _) => Ok(a.with_timezone(&Utc)),
        LocalResult::None => Err(format!(
            "{} does not exist in {tz} (clock change); pick another time",
            ndt.format("%Y-%m-%dT%H:%M")
        )),
    }
}

/// Times stored by other clients are never rejected: a time in a DST gap moves one hour later.
fn lenient(ndt: NaiveDateTime, tz: Tz) -> DateTime<Utc> {
    local_to_utc(ndt, tz)
        .unwrap_or_else(|_| local_to_utc(ndt + Duration::hours(1), tz).unwrap_or(ndt.and_utc()))
}

fn to_when(d: DatePerhapsTime, tz: Tz) -> When {
    match d {
        DatePerhapsTime::Date(d) => When::Date(d),
        DatePerhapsTime::DateTime(CalendarDateTime::Utc(t)) => When::At(t),
        DatePerhapsTime::DateTime(CalendarDateTime::Floating(n)) => When::At(lenient(n, tz)),
        DatePerhapsTime::DateTime(CalendarDateTime::WithTimezone { date_time, tzid }) => {
            When::At(lenient(date_time, tzid.parse::<Tz>().unwrap_or(tz)))
        }
    }
}

fn to_dpt(w: &When) -> DatePerhapsTime {
    match w {
        When::Date(d) => DatePerhapsTime::Date(*d),
        When::At(t) => DatePerhapsTime::DateTime(CalendarDateTime::Utc(*t)),
    }
}

/// Content lines of an iCalendar text, unfolded, without line endings.
pub fn unfold(ics: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in ics.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        match raw.strip_prefix([' ', '\t']) {
            Some(rest) if !lines.is_empty() => lines.last_mut().expect("non-empty").push_str(rest),
            _ if raw.is_empty() => {}
            _ => lines.push(raw.to_string()),
        }
    }
    lines
}

/// Property name of a content line, uppercased ("DTSTART" for "DTSTART;TZID=X:…").
pub fn prop_name(line: &str) -> String {
    line.split([';', ':'])
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase()
}

/// Value of a content line (after the first ':' outside parameter quotes).
pub fn prop_value(line: &str) -> &str {
    let mut quoted = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ':' if !quoted => return &line[i + 1..],
            _ => {}
        }
    }
    ""
}

/// True when the document has a component whose UID is exactly `uid`.
pub fn has_uid(ics: &str, uid: &str) -> bool {
    !uid.is_empty()
        && unfold(ics)
            .iter()
            .any(|l| prop_name(l) == "UID" && prop_value(l) == uid)
}

fn parse(ics: &str) -> anyhow::Result<Calendar> {
    ics.parse::<Calendar>()
        .map_err(|e| anyhow!("invalid iCalendar data: {e}"))
}

fn calendar() -> Calendar {
    let mut cal = Calendar::empty();
    cal.append_property(("VERSION", "2.0"));
    cal.append_property(("PRODID", "-//fronde//hub//EN"));
    cal
}

pub fn parse_events(ics: &str, tz: Tz) -> anyhow::Result<Vec<Event>> {
    let cal = parse(ics)?;
    Ok(cal
        .events()
        .filter_map(|e| {
            Some(Event {
                uid: e.get_uid()?.to_string(),
                title: e.get_summary().unwrap_or_default().to_string(),
                start: to_when(e.get_start()?, tz),
                end: e.get_end().map(|d| to_when(d, tz)),
                location: e.get_location().map(str::to_string),
                description: e.get_description().map(str::to_string),
            })
        })
        .collect())
}

pub fn parse_reminders(ics: &str, tz: Tz) -> anyhow::Result<Vec<Reminder>> {
    let cal = parse(ics)?;
    Ok(cal
        .todos()
        .filter_map(|t| {
            let due = match to_when(t.get_due()?, tz) {
                When::At(at) => at,
                When::Date(d) => lenient(d.and_hms_opt(9, 0, 0)?, tz),
            };
            Some(Reminder {
                uid: t.get_uid()?.to_string(),
                text: t.get_summary().unwrap_or_default().to_string(),
                due,
                done: t.get_status() == Some(TodoStatus::Completed) || t.get_completed().is_some(),
            })
        })
        .collect())
}

pub fn event_to_ics(e: &Event) -> String {
    let mut ev = icalendar::Event::new();
    ev.uid(&e.uid)
        .timestamp(Utc::now())
        .summary(&e.title)
        .starts(to_dpt(&e.start));
    if let Some(end) = &e.end {
        ev.ends(to_dpt(end));
    }
    if let Some(l) = &e.location {
        ev.location(l);
    }
    if let Some(d) = &e.description {
        ev.description(d);
    }
    let mut cal = calendar();
    cal.push(ev.done());
    cal.to_string()
}

pub fn reminder_to_ics(r: &Reminder) -> String {
    let mut todo = icalendar::Todo::new();
    todo.uid(&r.uid)
        .timestamp(Utc::now())
        .summary(&r.text)
        .due(CalendarDateTime::Utc(r.due));
    if r.done {
        todo.status(TodoStatus::Completed);
    }
    let mut cal = calendar();
    cal.push(todo.done());
    cal.to_string()
}

fn escape_text(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace('\r', "")
        .replace('\n', "\\n")
}

fn when_line(name: &str, w: &When) -> String {
    match w {
        When::Date(d) => format!("{name};VALUE=DATE:{}", d.format("%Y%m%d")),
        When::At(t) => format!("{name}:{}", t.format("%Y%m%dT%H%M%SZ")),
    }
}

/// Fold a content line at 75 octets (RFC 5545 §3.1), on character boundaries.
fn fold(line: &str) -> String {
    let mut out = String::new();
    let mut width = 0;
    for c in line.chars() {
        let len = c.len_utf8();
        if width + len > 75 {
            out.push_str("\r\n ");
            width = 1;
        }
        out.push(c);
        width += len;
    }
    out
}

type Change = (&'static str, Option<String>);

/// Edit one component in place: drop its own lines named in `changes`, add the new ones.
/// Everything else (alarms, attendees, time zones, unknown properties) is kept as is.
fn patch(ics: &str, comp: &str, uid: &str, changes: &[Change]) -> anyhow::Result<String> {
    let lines = unfold(ics);
    // Nesting depth after each line: a component's own properties sit at depth 1 relative to its BEGIN.
    let depth_after = |slice: &[String]| -> Vec<usize> {
        let mut d = 0usize;
        slice
            .iter()
            .map(|l| {
                match prop_name(l).as_str() {
                    "BEGIN" => d += 1,
                    "END" => d = d.saturating_sub(1),
                    _ => {}
                }
                d
            })
            .collect()
    };
    let begin = format!("BEGIN:{comp}");
    let mut start = 0;
    while let Some(off) = lines[start..]
        .iter()
        .position(|l| l.eq_ignore_ascii_case(&begin))
    {
        let b = start + off;
        let depths = depth_after(&lines[b..]);
        let len = depths
            .iter()
            .position(|&d| d == 0)
            .unwrap_or(depths.len() - 1);
        let block = &lines[b..=b + len];
        let own =
            |k: usize| depths[k] == 1 && !matches!(prop_name(&block[k]).as_str(), "BEGIN" | "END");
        if (0..block.len())
            .any(|k| own(k) && prop_name(&block[k]) == "UID" && prop_value(&block[k]) == uid)
        {
            let mut out: Vec<String> = lines[..b].to_vec();
            for (k, l) in block[..block.len() - 1].iter().enumerate() {
                if !(own(k) && changes.iter().any(|(name, _)| prop_name(l) == *name)) {
                    out.push(l.clone());
                }
            }
            out.extend(changes.iter().filter_map(|(_, line)| line.clone()));
            out.extend_from_slice(&lines[b + len..]);
            return Ok(out.iter().map(|l| fold(l)).collect::<Vec<_>>().join("\r\n") + "\r\n");
        }
        start = b + len + 1;
    }
    anyhow::bail!("no {comp} with uid {uid:?} in the stored item")
}

fn stamp_line(name: &'static str) -> Option<String> {
    Some(format!("{name}:{}", Utc::now().format("%Y%m%dT%H%M%SZ")))
}

/// The stored event text with only the fields that changed between `old` and `new` rewritten.
pub fn update_event_ics(original: &str, old: &Event, new: &Event) -> anyhow::Result<String> {
    let mut changes: Vec<Change> = vec![("DTSTAMP", stamp_line("DTSTAMP"))];
    if new.title != old.title {
        changes.push((
            "SUMMARY",
            Some(format!("SUMMARY:{}", escape_text(&new.title))),
        ));
    }
    if new.start != old.start {
        changes.push(("DTSTART", Some(when_line("DTSTART", &new.start))));
    }
    if new.end != old.end {
        changes.push(("DTEND", new.end.as_ref().map(|w| when_line("DTEND", w))));
        changes.push(("DURATION", None));
    }
    if new.location != old.location {
        changes.push((
            "LOCATION",
            new.location
                .as_deref()
                .map(|l| format!("LOCATION:{}", escape_text(l))),
        ));
    }
    if new.description != old.description {
        changes.push((
            "DESCRIPTION",
            new.description
                .as_deref()
                .map(|d| format!("DESCRIPTION:{}", escape_text(d))),
        ));
    }
    patch(original, "VEVENT", &new.uid, &changes)
}

/// The stored task text with only the fields that changed rewritten.
pub fn update_reminder_ics(
    original: &str,
    old: &Reminder,
    new: &Reminder,
) -> anyhow::Result<String> {
    let mut changes: Vec<Change> = vec![("DTSTAMP", stamp_line("DTSTAMP"))];
    if new.text != old.text {
        changes.push((
            "SUMMARY",
            Some(format!("SUMMARY:{}", escape_text(&new.text))),
        ));
    }
    if new.due != old.due {
        changes.push(("DUE", Some(when_line("DUE", &When::At(new.due)))));
    }
    if new.done != old.done {
        if new.done {
            changes.push(("STATUS", Some("STATUS:COMPLETED".into())));
            changes.push(("COMPLETED", stamp_line("COMPLETED")));
        } else {
            changes.push(("STATUS", Some("STATUS:NEEDS-ACTION".into())));
            changes.push(("COMPLETED", None));
        }
    }
    patch(original, "VTODO", &new.uid, &changes)
}

/// Parse a date or time given by an agent: "YYYY-MM-DD", "YYYY-MM-DDTHH:MM" (local) or RFC 3339.
pub fn parse_local(s: &str, tz: Tz) -> Result<When, String> {
    let s = s.trim();
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(When::Date(d));
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(When::At(t.with_timezone(&Utc)));
    }
    for f in [
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%d %H:%M:%S",
    ] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s, f) {
            return local_to_utc(n, tz).map(When::At);
        }
    }
    Err(format!(
        "cannot read {s:?}; use YYYY-MM-DD or YYYY-MM-DDTHH:MM (local time, {tz})"
    ))
}

pub fn format_local(w: &When, tz: Tz) -> String {
    match w {
        When::Date(d) => d.format("%Y-%m-%d").to_string(),
        When::At(t) => t.with_timezone(&tz).format("%Y-%m-%dT%H:%M%:z").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use chrono_tz::Europe::Paris;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> When {
        When::At(Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap())
    }

    #[test]
    fn parses_tzid_allday_and_floating() {
        let e = &parse_events(&fixture("tzid.ics"), Paris).unwrap()[0];
        assert_eq!((e.uid.as_str(), e.title.as_str()), ("tz-1", "Dentiste"));
        assert_eq!(e.start, utc(2026, 11, 20, 9, 0));
        assert_eq!(e.end, Some(utc(2026, 11, 20, 10, 0)));
        assert_eq!(e.location.as_deref(), Some("Lyon"));

        let e = &parse_events(&fixture("allday.ics"), Paris).unwrap()[0];
        assert_eq!(
            e.start,
            When::Date(NaiveDate::from_ymd_opt(2026, 11, 21).unwrap())
        );

        let es = parse_events(&fixture("floating.ics"), Paris).unwrap();
        assert_eq!(es[0].start, utc(2026, 11, 20, 9, 0));
        // Unknown TZID (Outlook names) falls back to the user's zone.
        assert_eq!(es[1].start, utc(2026, 11, 20, 9, 0));
    }

    #[test]
    fn parses_reminders() {
        let rs = parse_reminders(&fixture("todo.ics"), Paris).unwrap();
        assert_eq!(rs.len(), 2, "VTODO without DUE is ignored");
        assert_eq!(rs[0].text, "Appeler la CAF");
        assert_eq!(
            rs[0].due,
            Utc.with_ymd_and_hms(2026, 11, 18, 8, 0, 0).unwrap()
        );
        assert!(!rs[0].done);
        assert!(rs[1].done);
    }

    #[test]
    fn roundtrip_event() {
        let e = Event {
            uid: "abc".into(),
            title: "Réunion, école".into(),
            start: utc(2026, 11, 20, 9, 0),
            end: Some(utc(2026, 11, 20, 10, 30)),
            location: Some("Salle 2".into()),
            description: Some("ligne 1\nligne 2".into()),
        };
        let ics = event_to_ics(&e);
        assert!(ics.contains("PRODID:-//fronde//hub//EN"));
        assert!(ics.contains("DTSTART:20261120T090000Z"));
        assert_eq!(parse_events(&ics, Paris).unwrap(), vec![e]);
        let day = Event {
            uid: "d".into(),
            title: "Vacances".into(),
            start: When::Date(NaiveDate::from_ymd_opt(2026, 12, 24).unwrap()),
            end: Some(When::Date(NaiveDate::from_ymd_opt(2026, 12, 26).unwrap())),
            location: None,
            description: None,
        };
        assert_eq!(parse_events(&event_to_ics(&day), Paris).unwrap(), vec![day]);
    }

    #[test]
    fn roundtrip_reminder() {
        let r = Reminder {
            uid: "r1".into(),
            text: "Sortir les poubelles".into(),
            due: Utc.with_ymd_and_hms(2026, 11, 18, 19, 0, 0).unwrap(),
            done: false,
        };
        assert_eq!(
            parse_reminders(&reminder_to_ics(&r), Paris).unwrap(),
            vec![r.clone()]
        );
        let done = Reminder { done: true, ..r };
        assert!(reminder_to_ics(&done).contains("STATUS:COMPLETED"));
        assert_eq!(
            parse_reminders(&reminder_to_ics(&done), Paris).unwrap(),
            vec![done]
        );
    }

    #[test]
    fn parse_local_forms() {
        assert_eq!(
            parse_local("2026-11-16", Paris).unwrap(),
            When::Date(NaiveDate::from_ymd_opt(2026, 11, 16).unwrap())
        );
        assert_eq!(
            parse_local("2026-11-16T14:30", Paris).unwrap(),
            utc(2026, 11, 16, 13, 30)
        );
        assert_eq!(
            parse_local("2026-11-16 14:30", Paris).unwrap(),
            utc(2026, 11, 16, 13, 30)
        );
        assert_eq!(
            parse_local("2026-11-16T14:30:00+00:00", Paris).unwrap(),
            utc(2026, 11, 16, 14, 30)
        );
        let gap = parse_local("2026-03-29T02:30", Paris).unwrap_err();
        assert!(gap.contains("does not exist"), "{gap}");
        assert_eq!(
            parse_local("2026-10-25T02:30", Paris).unwrap(),
            utc(2026, 10, 25, 0, 30),
            "ambiguous → earliest"
        );
        assert!(
            parse_local("demain", Paris)
                .unwrap_err()
                .contains("YYYY-MM-DD")
        );
    }

    #[test]
    fn format_local_forms() {
        assert_eq!(
            format_local(&utc(2026, 11, 16, 13, 30), Paris),
            "2026-11-16T14:30+01:00"
        );
        assert_eq!(
            format_local(
                &When::Date(NaiveDate::from_ymd_opt(2026, 11, 16).unwrap()),
                Paris
            ),
            "2026-11-16"
        );
    }

    fn kept_lines(ics: &str) -> Vec<String> {
        unfold(ics)
    }

    #[test]
    fn update_event_keeps_unmodeled_properties() {
        let original = fixture("rich.ics");
        let old = parse_events(&original, Paris).unwrap().remove(0);
        let new = Event {
            title: "Point projet (déplacé, salle 2)".into(),
            ..old.clone()
        };
        let out = update_event_ics(&original, &old, &new).unwrap();
        let lines = kept_lines(&out);
        for kept in [
            "DTSTART;TZID=Romance Standard Time:20261120T100000",
            "DURATION:PT1H",
            "ORGANIZER;CN=Alix:mailto:alix@example.org",
            "ATTENDEE;CN=Bob;PARTSTAT=ACCEPTED:mailto:bob@example.org",
            "ATTENDEE;CN=Eve:mailto:eve@example.org",
            "X-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC",
            "DESCRIPTION:Rappel",
            "TZID:Romance Standard Time",
        ] {
            assert!(lines.iter().any(|l| l == kept), "lost {kept:?} in\n{out}");
        }
        assert!(!lines.iter().any(|l| l == "SUMMARY:Point projet"));
        assert_eq!(
            parse_events(&out, Paris).unwrap()[0].title,
            "Point projet (déplacé, salle 2)"
        );
        assert_eq!(
            parse_events(&out, Paris).unwrap()[0].start,
            old.start,
            "start untouched"
        );
    }

    #[test]
    fn update_event_end_replaces_duration() {
        let original = fixture("rich.ics");
        let old = parse_events(&original, Paris).unwrap().remove(0);
        let new = Event {
            end: Some(utc(2026, 11, 20, 11, 0)),
            ..old.clone()
        };
        let out = update_event_ics(&original, &old, &new).unwrap();
        let lines = kept_lines(&out);
        assert!(!lines.iter().any(|l| prop_name(l) == "DURATION"));
        assert!(lines.iter().any(|l| l == "DTEND:20261120T110000Z"));
        assert!(lines.iter().any(|l| l == "TRIGGER:-PT15M"), "alarm kept");
    }

    #[test]
    fn update_reminder_done_keeps_date_due() {
        let original = fixture("daytodo.ics");
        let old = parse_reminders(&original, Paris).unwrap().remove(0);
        let new = Reminder {
            done: true,
            ..old.clone()
        };
        let out = update_reminder_ics(&original, &old, &new).unwrap();
        let lines = kept_lines(&out);
        for kept in [
            "DUE;VALUE=DATE:20261118",
            "PRIORITY:1",
            "DESCRIPTION:Notes de la tache",
            "STATUS:COMPLETED",
        ] {
            assert!(
                lines.iter().any(|l| l == kept),
                "missing {kept:?} in\n{out}"
            );
        }
        assert!(lines.iter().any(|l| prop_name(l) == "COMPLETED"));
        assert!(parse_reminders(&out, Paris).unwrap()[0].done);
    }

    #[test]
    fn patched_long_lines_are_folded() {
        let original = fixture("rich.ics");
        let old = parse_events(&original, Paris).unwrap().remove(0);
        let long = "é".repeat(100);
        let new = Event {
            description: Some(long.clone()),
            ..old.clone()
        };
        let out = update_event_ics(&original, &old, &new).unwrap();
        assert!(out.split("\r\n").all(|l| l.len() <= 75), "{out}");
        assert_eq!(
            parse_events(&out, Paris).unwrap()[0].description.as_deref(),
            Some(long.as_str())
        );
    }
}
