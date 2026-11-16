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
    local_to_utc(ndt, tz).unwrap_or_else(|_| local_to_utc(ndt + Duration::hours(1), tz).unwrap_or(ndt.and_utc()))
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

fn parse(ics: &str) -> anyhow::Result<Calendar> {
    ics.parse::<Calendar>().map_err(|e| anyhow!("invalid iCalendar data: {e}"))
}

fn calendar() -> Calendar {
    let mut cal = Calendar::empty();
    cal.append_property(("VERSION", "2.0"));
    cal.append_property(("PRODID", "-//jarvis//hub//EN"));
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
    ev.uid(&e.uid).timestamp(Utc::now()).summary(&e.title).starts(to_dpt(&e.start));
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
    todo.uid(&r.uid).timestamp(Utc::now()).summary(&r.text).due(CalendarDateTime::Utc(r.due));
    if r.done {
        todo.status(TodoStatus::Completed);
    }
    let mut cal = calendar();
    cal.push(todo.done());
    cal.to_string()
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
    for f in ["%Y-%m-%dT%H:%M", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s, f) {
            return local_to_utc(n, tz).map(When::At);
        }
    }
    Err(format!("cannot read {s:?}; use YYYY-MM-DD or YYYY-MM-DDTHH:MM (local time, {tz})"))
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
        std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
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
        assert_eq!(e.start, When::Date(NaiveDate::from_ymd_opt(2026, 11, 21).unwrap()));

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
        assert_eq!(rs[0].due, Utc.with_ymd_and_hms(2026, 11, 18, 8, 0, 0).unwrap());
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
        assert!(ics.contains("PRODID:-//jarvis//hub//EN"));
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
        let r = Reminder { uid: "r1".into(), text: "Sortir les poubelles".into(), due: Utc.with_ymd_and_hms(2026, 11, 18, 19, 0, 0).unwrap(), done: false };
        assert_eq!(parse_reminders(&reminder_to_ics(&r), Paris).unwrap(), vec![r.clone()]);
        let done = Reminder { done: true, ..r };
        assert!(reminder_to_ics(&done).contains("STATUS:COMPLETED"));
        assert_eq!(parse_reminders(&reminder_to_ics(&done), Paris).unwrap(), vec![done]);
    }

    #[test]
    fn parse_local_forms() {
        assert_eq!(parse_local("2026-11-16", Paris).unwrap(), When::Date(NaiveDate::from_ymd_opt(2026, 11, 16).unwrap()));
        assert_eq!(parse_local("2026-11-16T14:30", Paris).unwrap(), utc(2026, 11, 16, 13, 30));
        assert_eq!(parse_local("2026-11-16 14:30", Paris).unwrap(), utc(2026, 11, 16, 13, 30));
        assert_eq!(parse_local("2026-11-16T14:30:00+00:00", Paris).unwrap(), utc(2026, 11, 16, 14, 30));
        let gap = parse_local("2026-03-29T02:30", Paris).unwrap_err();
        assert!(gap.contains("does not exist"), "{gap}");
        assert_eq!(parse_local("2026-10-25T02:30", Paris).unwrap(), utc(2026, 10, 25, 0, 30), "ambiguous → earliest");
        assert!(parse_local("demain", Paris).unwrap_err().contains("YYYY-MM-DD"));
    }

    #[test]
    fn format_local_forms() {
        assert_eq!(format_local(&utc(2026, 11, 16, 13, 30), Paris), "2026-11-16T14:30+01:00");
        assert_eq!(format_local(&When::Date(NaiveDate::from_ymd_opt(2026, 11, 16).unwrap()), Paris), "2026-11-16");
    }
}
