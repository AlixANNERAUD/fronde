//! Events and reminders <-> iCalendar. Times are stored in UTC; input and output use the user's zone.

use crate::raw;
use anyhow::{anyhow, bail};
use calcard::common::PartialDateTime;
use calcard::icalendar::timezone::TzResolver;
use calcard::icalendar::{
    ICalendar, ICalendarComponent, ICalendarComponentType, ICalendarEntry, ICalendarParameter,
    ICalendarParameterName, ICalendarProperty, ICalendarStatus, ICalendarValueType,
};
use chrono::{
    DateTime, Datelike, Duration, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc,
};
use chrono_tz::Tz;

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

fn entry_to_when(
    entry: &ICalendarEntry,
    tz_resolver: &TzResolver<&str>,
    default_tz: Tz,
) -> Option<When> {
    let dt = entry.values.first()?.as_partial_date_time()?;
    if entry.is_type(&ICalendarValueType::Date) || !dt.has_time() {
        let year = i32::from(dt.year?);
        let month = u32::from(dt.month?);
        let day = u32::from(dt.day?);
        return NaiveDate::from_ymd_opt(year, month, day).map(When::Date);
    }
    let dtr = dt.to_date_time()?;
    if let Some(offset) = dtr.offset {
        return offset
            .from_local_datetime(&dtr.date_time)
            .single()
            .map(|t| When::At(t.with_timezone(&Utc)));
    }
    let at = match tz_resolver.resolve_or_default(entry.tz_id()) {
        calcard::common::timezone::Tz::Tz(tz) => lenient(dtr.date_time, tz),
        calcard::common::timezone::Tz::Fixed(offset) => offset
            .from_local_datetime(&dtr.date_time)
            .single()?
            .with_timezone(&Utc),
        calcard::common::timezone::Tz::Floating => lenient(dtr.date_time, default_tz),
    };
    Some(When::At(at))
}

fn utc_pdt(t: DateTime<Utc>) -> PartialDateTime {
    PartialDateTime {
        year: u16::try_from(t.year()).ok(),
        month: u8::try_from(t.month()).ok(),
        day: u8::try_from(t.day()).ok(),
        hour: u8::try_from(t.hour()).ok(),
        minute: u8::try_from(t.minute()).ok(),
        second: u8::try_from(t.second()).ok(),
        tz_hour: Some(0),
        tz_minute: Some(0),
        tz_minus: false,
    }
}

fn when_entry(name: ICalendarProperty, w: &When) -> ICalendarEntry {
    match w {
        When::Date(d) => ICalendarEntry::new(name)
            .with_param(ICalendarParameter::new(
                ICalendarParameterName::Value,
                ICalendarValueType::Date,
            ))
            .with_value(PartialDateTime {
                year: u16::try_from(d.year()).ok(),
                month: u8::try_from(d.month()).ok(),
                day: u8::try_from(d.day()).ok(),
                ..Default::default()
            }),
        When::At(t) => ICalendarEntry::new(name).with_value(utc_pdt(*t)),
    }
}

fn prop_text<'a>(comp: &'a ICalendarComponent, prop: &ICalendarProperty) -> Option<&'a str> {
    comp.property(prop)
        .and_then(|e| e.values.first())
        .and_then(|v| v.as_text())
}

fn parse(ics: &str) -> anyhow::Result<ICalendar> {
    ICalendar::parse(ics)
        .map_err(|e| anyhow!("invalid iCalendar data: {}", excerpt(&format!("{e:?}"))))
}

/// The start of `s`: a parse error can carry the whole input (a vCard, say), which has no place
/// in a tool error.
fn excerpt(s: &str) -> String {
    const MAX: usize = 120;
    match s.char_indices().nth(MAX) {
        Some((end, _)) => format!("{}…", &s[..end]),
        None => s.to_string(),
    }
}

/// The components directly under VCALENDAR, with their index: events, tasks and time zones, not
/// their alarms or time-zone rules.
fn top_level(cal: &ICalendar) -> impl Iterator<Item = (usize, &ICalendarComponent)> {
    cal.components
        .first()
        .into_iter()
        .flat_map(|root| root.component_ids.iter())
        .filter_map(|id| {
            let id = *id as usize;
            cal.components.get(id).map(|c| (id, c))
        })
}

fn is_item(c: &ICalendarComponent) -> bool {
    c.component_type.is_event() || c.component_type.is_todo()
}

/// Text from an agent with every line break as `\n`, the only one iCalendar text keeps.
fn text(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

fn calendar_with(component: ICalendarComponent) -> ICalendar {
    let mut root = ICalendarComponent::new(ICalendarComponentType::VCalendar);
    root.add_property(ICalendarProperty::Version, "2.0");
    root.add_property(ICalendarProperty::Prodid, "-//fronde//hub//EN");
    root.component_ids.push(1);
    ICalendar {
        components: vec![root, component],
    }
}

/// `uid` as it appears in iCalendar text.
fn escape_text(uid: &str) -> String {
    let mut out = String::with_capacity(uid.len());
    for ch in uid.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
    out
}

/// True when a top-level VEVENT or VTODO has exactly this UID (an alarm's UID does not count).
/// Text the parser rejects is scanned line by line instead, so that a stored item it cannot read
/// is not taken for absent and written over by a new one.
pub fn has_uid(ics: &str, uid: &str) -> bool {
    if uid.is_empty() {
        return false;
    }
    let unfolded = raw::unfold(ics);
    let escaped = escape_text(uid);
    if !unfolded.contains(uid) && !unfolded.contains(&escaped) {
        return false;
    }
    match ICalendar::parse(ics) {
        Ok(cal) => top_level(&cal).any(|(_, c)| is_item(c) && c.uid() == Some(uid)),
        Err(_) => scan_uid(&unfolded, uid, &escaped),
    }
}

/// Whether a UID line outside an alarm has this value, without parsing the rest.
fn scan_uid(unfolded: &str, uid: &str, escaped: &str) -> bool {
    let mut in_alarm = false;
    for line in unfolded.lines().map(|l| l.trim_end_matches('\r')) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.split(';').next().unwrap_or_default();
        if name.eq_ignore_ascii_case("BEGIN") || name.eq_ignore_ascii_case("END") {
            if value.trim().eq_ignore_ascii_case("VALARM") {
                in_alarm = name.eq_ignore_ascii_case("BEGIN");
            }
        } else if !in_alarm
            && name.eq_ignore_ascii_case("UID")
            && (value == uid || value == escaped)
        {
            return true;
        }
    }
    false
}

/// True when the top-level VEVENT or VTODO with this UID belongs to a series: it repeats (RRULE,
/// RDATE) or overrides one occurrence (RECURRENCE-ID). Time-zone rules do not count.
pub fn is_recurring(ics: &str, uid: &str) -> anyhow::Result<bool> {
    let cal = parse(ics)?;
    Ok(top_level(&cal).any(|(_, c)| {
        is_item(c)
            && c.uid() == Some(uid)
            && [
                ICalendarProperty::Rrule,
                ICalendarProperty::Rdate,
                ICalendarProperty::RecurrenceId,
            ]
            .iter()
            .any(|p| c.has_property(p))
    }))
}

/// True when a top-level VEVENT or VTODO has a UID other than `uid` (or none): the resource then
/// holds more than the one item, and removing it would remove the others.
pub fn has_other_items(ics: &str, uid: &str) -> anyhow::Result<bool> {
    let cal = parse(ics)?;
    Ok(top_level(&cal).any(|(_, c)| is_item(c) && c.uid() != Some(uid)))
}

/// The one top-level component of this type and UID that is not an occurrence override.
fn master<'a>(
    cal: &'a mut ICalendar,
    kind: ICalendarComponentType,
    uid: &str,
) -> anyhow::Result<&'a mut ICalendarComponent> {
    let ids: Vec<usize> = top_level(cal)
        .filter(|(_, c)| {
            c.component_type == kind
                && c.uid() == Some(uid)
                && !c.has_property(&ICalendarProperty::RecurrenceId)
        })
        .map(|(id, _)| id)
        .collect();
    let name = kind.as_str().to_string();
    match ids[..] {
        [id] => Ok(&mut cal.components[id]),
        [] => bail!("no {name} with uid {uid:?} outside an occurrence override in the stored item"),
        _ => bail!(
            "{} {name} components share the uid {uid:?} in the stored item; fix them in your calendar app",
            ids.len()
        ),
    }
}

pub fn parse_events(ics: &str, tz: Tz) -> anyhow::Result<Vec<Event>> {
    let cal = parse(ics)?;
    let tz_resolver = cal
        .build_tz_resolver()
        .with_default(calcard::common::timezone::Tz::Tz(tz));
    Ok(cal
        .components
        .iter()
        .filter(|c| c.component_type.is_event())
        .filter_map(|e| {
            Some(Event {
                uid: e.uid()?.to_string(),
                title: prop_text(e, &ICalendarProperty::Summary)
                    .unwrap_or_default()
                    .to_string(),
                start: entry_to_when(e.property(&ICalendarProperty::Dtstart)?, &tz_resolver, tz)?,
                end: e
                    .property(&ICalendarProperty::Dtend)
                    .and_then(|d| entry_to_when(d, &tz_resolver, tz)),
                location: prop_text(e, &ICalendarProperty::Location).map(str::to_string),
                description: prop_text(e, &ICalendarProperty::Description).map(str::to_string),
            })
        })
        .collect())
}

pub fn parse_reminders(ics: &str, tz: Tz) -> anyhow::Result<Vec<Reminder>> {
    let cal = parse(ics)?;
    let tz_resolver = cal
        .build_tz_resolver()
        .with_default(calcard::common::timezone::Tz::Tz(tz));
    Ok(cal
        .components
        .iter()
        .filter(|c| c.component_type.is_todo())
        .filter_map(|t| {
            let due = match entry_to_when(t.property(&ICalendarProperty::Due)?, &tz_resolver, tz)? {
                When::At(at) => at,
                When::Date(d) => lenient(d.and_hms_opt(9, 0, 0)?, tz),
            };
            Some(Reminder {
                uid: t.uid()?.to_string(),
                text: prop_text(t, &ICalendarProperty::Summary)
                    .unwrap_or_default()
                    .to_string(),
                due,
                done: t.status() == Some(&ICalendarStatus::Completed)
                    || t.has_property(&ICalendarProperty::Completed),
            })
        })
        .collect())
}

pub fn event_to_ics(e: &Event) -> String {
    let mut ev = ICalendarComponent::new(ICalendarComponentType::VEvent);
    ev.add_uid(&e.uid);
    ev.add_dtstamp(utc_pdt(Utc::now()));
    ev.add_property(ICalendarProperty::Summary, text(&e.title));
    ev.entries
        .push(when_entry(ICalendarProperty::Dtstart, &e.start));
    if let Some(end) = &e.end {
        ev.entries.push(when_entry(ICalendarProperty::Dtend, end));
    }
    if let Some(l) = &e.location {
        ev.add_property(ICalendarProperty::Location, text(l));
    }
    if let Some(d) = &e.description {
        ev.add_property(ICalendarProperty::Description, text(d));
    }
    calendar_with(ev).to_string()
}

pub fn reminder_to_ics(r: &Reminder) -> String {
    let mut todo = ICalendarComponent::new(ICalendarComponentType::VTodo);
    todo.add_uid(&r.uid);
    todo.add_dtstamp(utc_pdt(Utc::now()));
    todo.add_property(ICalendarProperty::Summary, text(&r.text));
    todo.entries
        .push(when_entry(ICalendarProperty::Due, &When::At(r.due)));
    if r.done {
        todo.add_property(ICalendarProperty::Status, ICalendarStatus::Completed);
    }
    calendar_with(todo).to_string()
}

fn set_property(comp: &mut ICalendarComponent, entry: ICalendarEntry) {
    let mut replaced = false;
    comp.entries.retain_mut(|e| {
        if e.name == entry.name {
            if !replaced {
                *e = entry.clone();
                replaced = true;
                true
            } else {
                false
            }
        } else {
            true
        }
    });
    if !replaced {
        comp.entries.push(entry);
    }
}

fn remove_property(comp: &mut ICalendarComponent, prop: &ICalendarProperty) {
    comp.entries.retain(|e| &e.name != prop);
}

/// The stored event text with only the fields that changed between `old` and `new` rewritten.
pub fn update_event_ics(original: &str, old: &Event, new: &Event) -> anyhow::Result<String> {
    let mut cal = parse(original)?;
    let comp = master(&mut cal, ICalendarComponentType::VEvent, &new.uid)?;

    set_property(
        comp,
        ICalendarEntry::new(ICalendarProperty::Dtstamp).with_value(utc_pdt(Utc::now())),
    );
    if new.title != old.title {
        set_property(
            comp,
            ICalendarEntry::new(ICalendarProperty::Summary).with_value(text(&new.title)),
        );
    }
    if new.start != old.start {
        set_property(comp, when_entry(ICalendarProperty::Dtstart, &new.start));
    }
    if new.end != old.end {
        remove_property(comp, &ICalendarProperty::Duration);
        match &new.end {
            Some(end) => set_property(comp, when_entry(ICalendarProperty::Dtend, end)),
            None => remove_property(comp, &ICalendarProperty::Dtend),
        }
    }
    if new.location != old.location {
        match &new.location {
            Some(l) => set_property(
                comp,
                ICalendarEntry::new(ICalendarProperty::Location).with_value(text(l)),
            ),
            None => remove_property(comp, &ICalendarProperty::Location),
        }
    }
    if new.description != old.description {
        match &new.description {
            Some(d) => set_property(
                comp,
                ICalendarEntry::new(ICalendarProperty::Description).with_value(text(d)),
            ),
            None => remove_property(comp, &ICalendarProperty::Description),
        }
    }
    raw::write(cal, original)
}

/// The stored task text with only the fields that changed rewritten.
pub fn update_reminder_ics(
    original: &str,
    old: &Reminder,
    new: &Reminder,
) -> anyhow::Result<String> {
    let mut cal = parse(original)?;
    let comp = master(&mut cal, ICalendarComponentType::VTodo, &new.uid)?;

    set_property(
        comp,
        ICalendarEntry::new(ICalendarProperty::Dtstamp).with_value(utc_pdt(Utc::now())),
    );
    if new.text != old.text {
        set_property(
            comp,
            ICalendarEntry::new(ICalendarProperty::Summary).with_value(text(&new.text)),
        );
    }
    if new.due != old.due {
        set_property(comp, when_entry(ICalendarProperty::Due, &When::At(new.due)));
    }
    if new.done != old.done {
        if new.done {
            set_property(
                comp,
                ICalendarEntry::new(ICalendarProperty::Status)
                    .with_value(ICalendarStatus::Completed),
            );
            set_property(
                comp,
                ICalendarEntry::new(ICalendarProperty::Completed).with_value(utc_pdt(Utc::now())),
            );
        } else {
            set_property(
                comp,
                ICalendarEntry::new(ICalendarProperty::Status)
                    .with_value(ICalendarStatus::NeedsAction),
            );
            remove_property(comp, &ICalendarProperty::Completed);
        }
    }
    raw::write(cal, original)
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
    use calcard::icalendar::ICalendarValue;
    use chrono::TimeZone;
    use chrono_tz::America::New_York;
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
        assert_eq!(es[1].start, utc(2026, 11, 20, 9, 0));
    }

    #[test]
    fn resolves_windows_tzid_independently_of_user_zone() {
        // floating.ics has a floating event and a Romance Standard Time event at 10:00.
        // Parsed in America/New_York (EST = UTC-5 in November):
        // - floating falls back to New_York -> 15:00Z
        // - Romance Standard Time maps to Europe/Paris (CET = UTC+1) -> 09:00Z
        let es = parse_events(&fixture("floating.ics"), New_York).unwrap();
        assert_eq!(es[0].start, utc(2026, 11, 20, 15, 0));
        assert_eq!(es[1].start, utc(2026, 11, 20, 9, 0));

        // rich.ics also uses Romance Standard Time (with a VTIMEZONE) -> 09:00Z in New_York.
        let rich = &parse_events(&fixture("rich.ics"), New_York).unwrap()[0];
        assert_eq!(rich.start, utc(2026, 11, 20, 9, 0));

        // An unknown TZID falls back to the user's zone.
        let unknown = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:u-1\r\n\
SUMMARY:Unknown zone\r\n\
DTSTART;TZID=Mars/Olympus:20261120T100000\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        assert_eq!(
            parse_events(unknown, New_York).unwrap()[0].start,
            utc(2026, 11, 20, 15, 0)
        );
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
    fn creates_and_reads_back_items() {
        let ev = Event {
            uid: "ev-create-1".into(),
            title: "Déjeuner; équipe".into(),
            start: utc(2026, 11, 22, 11, 30),
            end: Some(utc(2026, 11, 22, 13, 0)),
            location: Some("Bistrot, Lyon".into()),
            description: Some("Ligne A\nLigne B".into()),
        };
        let ev_ics = event_to_ics(&ev);
        assert!(has_uid(&ev_ics, "ev-create-1"));
        assert!(!has_uid(&ev_ics, "ev-create"));
        assert_eq!(parse_events(&ev_ics, New_York).unwrap(), vec![ev]);

        let rem = Reminder {
            uid: "rem-create-1".into(),
            text: "Acheter du pain".into(),
            due: Utc.with_ymd_and_hms(2026, 11, 23, 8, 0, 0).unwrap(),
            done: false,
        };
        let rem_ics = reminder_to_ics(&rem);
        assert!(has_uid(&rem_ics, "rem-create-1"));
        assert_eq!(parse_reminders(&rem_ics, New_York).unwrap(), vec![rem]);
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

    #[test]
    fn update_event_keeps_unmodeled_properties() {
        let original = fixture("rich.ics");
        let old = parse_events(&original, Paris).unwrap().remove(0);
        let new = Event {
            title: "Point projet (déplacé, salle 2)".into(),
            ..old.clone()
        };
        let out = update_event_ics(&original, &old, &new).unwrap();
        let orig_cal = ICalendar::parse(&original).unwrap();
        let out_cal = ICalendar::parse(&out).unwrap();

        // VTIMEZONE survives with its TZID and STANDARD subcomponent.
        let tz = out_cal
            .timezones()
            .next()
            .expect("VTIMEZONE should survive");
        assert_eq!(
            prop_text(tz, &ICalendarProperty::Tzid),
            Some("Romance Standard Time")
        );
        assert!(
            out_cal
                .components
                .iter()
                .any(|c| c.component_type == ICalendarComponentType::Standard)
        );

        let orig_ev = orig_cal
            .components
            .iter()
            .find(|c| c.component_type.is_event())
            .unwrap();
        let out_ev = out_cal
            .components
            .iter()
            .find(|c| c.component_type.is_event())
            .unwrap();

        assert_eq!(
            prop_text(out_ev, &ICalendarProperty::Summary),
            Some("Point projet (déplacé, salle 2)")
        );
        assert_eq!(
            out_ev.properties(&ICalendarProperty::Summary).count(),
            1,
            "the SUMMARY is replaced, not added"
        );
        // Every other VEVENT property (DTSTART with TZID, DURATION, ORGANIZER, ATTENDEEs, X-*) survives unchanged.
        for prop in [
            ICalendarProperty::Uid,
            ICalendarProperty::Dtstart,
            ICalendarProperty::Duration,
            ICalendarProperty::Organizer,
            ICalendarProperty::Attendee,
            ICalendarProperty::Other("X-APPLE-TRAVEL-ADVISORY-BEHAVIOR".into()),
        ] {
            let orig_vals: Vec<_> = orig_ev.properties(&prop).collect();
            let out_vals: Vec<_> = out_ev.properties(&prop).collect();
            assert_eq!(out_vals, orig_vals, "property {prop:?} changed in\n{out}");
        }

        // VALARM survives with all its entries.
        let orig_alarms: Vec<_> = orig_cal
            .components
            .iter()
            .filter(|c| c.component_type.is_alarm())
            .collect();
        let out_alarms: Vec<_> = out_cal
            .components
            .iter()
            .filter(|c| c.component_type.is_alarm())
            .collect();
        assert_eq!(out_alarms.len(), 1);
        assert_eq!(out_alarms[0].entries, orig_alarms[0].entries);

        let parsed = parse_events(&out, Paris).unwrap();
        assert_eq!(parsed[0].title, "Point projet (déplacé, salle 2)");
        assert_eq!(parsed[0].start, old.start, "start untouched");
    }

    #[test]
    fn update_summary_on_rich_item_keeps_x_properties_and_parameters() {
        let original = fixture("rich.ics")
            .replace(
                "ATTENDEE;CN=Bob;PARTSTAT=ACCEPTED:mailto:bob@example.org",
                "ATTENDEE;CN=Bob;PARTSTAT=ACCEPTED;X-ROLE=chair:mailto:bob@example.org\r\nX-CUSTOM-PROP;X-PARAM=yes:keep-value",
            )
            .replace(
                "TRIGGER:-PT15M",
                "TRIGGER:-PT15M\r\nX-ALARM-EXTRA:ring",
            );
        let old = parse_events(&original, Paris).unwrap().remove(0);
        let new = Event {
            title: "Point projet mis à jour".into(),
            ..old.clone()
        };
        let out = update_event_ics(&original, &old, &new).unwrap();

        let orig_cal = ICalendar::parse(&original).unwrap();
        let out_cal = ICalendar::parse(&out).unwrap();

        let orig_ev = orig_cal
            .components
            .iter()
            .find(|c| c.component_type.is_event())
            .unwrap();
        let out_ev = out_cal
            .components
            .iter()
            .find(|c| c.component_type.is_event())
            .unwrap();

        // Only SUMMARY and DTSTAMP changed on VEVENT.
        let non_edited = |e: &&ICalendarEntry| {
            !matches!(
                e.name,
                ICalendarProperty::Summary | ICalendarProperty::Dtstamp
            )
        };
        let orig_kept: Vec<_> = orig_ev.entries.iter().filter(non_edited).collect();
        let out_kept: Vec<_> = out_ev.entries.iter().filter(non_edited).collect();
        assert_eq!(out_kept, orig_kept);
        assert_eq!(
            prop_text(out_ev, &ICalendarProperty::Summary),
            Some("Point projet mis à jour")
        );
        assert_eq!(out_ev.properties(&ICalendarProperty::Summary).count(), 1);

        // Explicitly verify X- parameter on ATTENDEE, X- properties, VALARM, and VTIMEZONE.
        let bob = out_ev
            .properties(&ICalendarProperty::Attendee)
            .find(|e| {
                e.parameters(&ICalendarParameterName::Cn)
                    .any(|v| v.as_text() == Some("Bob"))
            })
            .expect("Bob attendee kept");
        assert!(
            bob.parameters(&ICalendarParameterName::Other("X-ROLE".into()))
                .any(|v| v.as_text() == Some("chair"))
        );
        let custom = out_ev
            .property(&ICalendarProperty::Other("X-CUSTOM-PROP".into()))
            .expect("X-CUSTOM-PROP kept");
        assert!(
            custom
                .parameters(&ICalendarParameterName::Other("X-PARAM".into()))
                .any(|v| v.as_text() == Some("yes"))
        );
        assert_eq!(
            custom.values.first().and_then(|v| v.as_text()),
            Some("keep-value")
        );

        let orig_tz: Vec<_> = orig_cal
            .components
            .iter()
            .filter(|c| {
                matches!(
                    c.component_type,
                    ICalendarComponentType::VTimezone
                        | ICalendarComponentType::Standard
                        | ICalendarComponentType::Daylight
                        | ICalendarComponentType::VAlarm
                )
            })
            .map(|c| (&c.component_type, &c.entries))
            .collect();
        let out_tz: Vec<_> = out_cal
            .components
            .iter()
            .filter(|c| {
                matches!(
                    c.component_type,
                    ICalendarComponentType::VTimezone
                        | ICalendarComponentType::Standard
                        | ICalendarComponentType::Daylight
                        | ICalendarComponentType::VAlarm
                )
            })
            .map(|c| (&c.component_type, &c.entries))
            .collect();
        assert_eq!(out_tz, orig_tz);
        assert!(
            out_cal
                .components
                .iter()
                .find(|c| c.component_type.is_alarm())
                .unwrap()
                .has_property(&ICalendarProperty::Other("X-ALARM-EXTRA".into()))
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
        let out_cal = ICalendar::parse(&out).unwrap();
        let ev = out_cal
            .components
            .iter()
            .find(|c| c.component_type.is_event())
            .unwrap();

        assert!(!ev.has_property(&ICalendarProperty::Duration));
        assert!(ev.has_property(&ICalendarProperty::Dtend));
        assert_eq!(
            parse_events(&out, Paris).unwrap()[0].end,
            Some(utc(2026, 11, 20, 11, 0))
        );
        let alarm = out_cal
            .components
            .iter()
            .find(|c| c.component_type.is_alarm())
            .expect("alarm kept");
        let trigger = alarm
            .property(&ICalendarProperty::Trigger)
            .and_then(|t| t.values.first());
        assert!(
            matches!(trigger, Some(ICalendarValue::Duration(d)) if d.to_string() == "-PT15M"),
            "{trigger:?}"
        );
        assert!(
            raw::unfold(&out).lines().any(|l| l == "TRIGGER:-PT15M"),
            "{out}"
        );
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
        let out_cal = ICalendar::parse(&out).unwrap();
        let todo = out_cal
            .components
            .iter()
            .find(|c| c.component_type.is_todo())
            .unwrap();

        let due = todo.property(&ICalendarProperty::Due).unwrap();
        assert!(due.is_type(&ICalendarValueType::Date));
        assert_eq!(
            due.values
                .first()
                .and_then(|v| v.as_partial_date_time())
                .and_then(|dt| dt.to_date_time())
                .map(|d| d.date_time.date()),
            NaiveDate::from_ymd_opt(2026, 11, 18)
        );
        assert_eq!(
            todo.property(&ICalendarProperty::Priority)
                .and_then(|e| e.values.first())
                .and_then(|v| v.as_integer()),
            Some(1)
        );
        assert_eq!(
            prop_text(todo, &ICalendarProperty::Description),
            Some("Notes de la tache")
        );
        assert_eq!(todo.status(), Some(&ICalendarStatus::Completed));
        assert!(todo.has_property(&ICalendarProperty::Completed));

        let parsed = parse_reminders(&out, Paris).unwrap();
        assert!(parsed[0].done);
        assert_eq!(parsed[0].due, old.due);
    }

    #[test]
    fn edited_long_lines_are_folded() {
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

    /// Content lines as text, with what may change on a write undone: folding, CRLF, parameter
    /// order and quoting, and the order of RRULE parts (calcard writes them in its own order).
    fn normalised(ics: &str) -> Vec<String> {
        raw::unfold(ics)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|line| {
                let mut in_quote = false;
                let split = line
                    .char_indices()
                    .find(|&(_, ch)| {
                        if ch == '"' {
                            in_quote = !in_quote;
                        }
                        ch == ':' && !in_quote
                    })
                    .map_or(line.len(), |(i, _)| i);
                let (head, value) = (&line[..split], line.get(split + 1..).unwrap_or_default());
                let mut parts = vec![String::new()];
                let mut in_quote = false;
                for ch in head.chars() {
                    match ch {
                        '"' => in_quote = !in_quote,
                        ';' if !in_quote => parts.push(String::new()),
                        _ => parts.last_mut().unwrap().push(ch),
                    }
                }
                let name = parts.remove(0);
                parts.sort();
                let value = match name.as_str() {
                    "RRULE" => {
                        let mut rule: Vec<&str> = value.split(';').collect();
                        rule.sort();
                        rule.join(";")
                    }
                    "DTSTAMP" => "(now)".to_string(),
                    _ => value.to_string(),
                };
                format!("{name};{}:{value}", parts.join(";"))
            })
            .collect()
    }

    fn edit_apple_summary() -> String {
        let original = fixture("apple.ics");
        let old = parse_events(&original, Paris).unwrap().remove(0);
        let new = Event {
            title: "Visite guidée".into(),
            ..old.clone()
        };
        update_event_ics(&original, &old, &new).unwrap()
    }

    /// The whole edited text against lines written by hand from apple.ics: anything the parser
    /// dropped, or the writer changed beyond formatting, shows as a difference.
    #[test]
    fn edit_matches_an_independent_expectation() {
        let expected = [
            "BEGIN:VCALENDAR",
            "VERSION:2.0",
            "PRODID:-//Apple Inc.//macOS 15.1//EN",
            "CALSCALE:GREGORIAN",
            "BEGIN:VTIMEZONE",
            "TZID:Europe/Paris",
            "BEGIN:DAYLIGHT",
            "TZOFFSETFROM:+0100",
            "RRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU",
            "DTSTART:19810329T020000",
            "TZNAME:CEST",
            "TZOFFSETTO:+0200",
            "END:DAYLIGHT",
            "BEGIN:STANDARD",
            "TZOFFSETFROM:+0200",
            "RRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU",
            "DTSTART:19961027T030000",
            "TZNAME:CET",
            "TZOFFSETTO:+0100",
            "END:STANDARD",
            "END:VTIMEZONE",
            "BEGIN:VEVENT",
            "CREATED:20261101T090000Z",
            "DTEND;TZID=Europe/Paris:20261120T110000",
            "DTSTAMP:20261101T090000Z",
            "DTSTART;TZID=Europe/Paris:20261120T100000",
            "LAST-MODIFIED:20261101T090000Z",
            r"LOCATION:Tour Eiffel\nChamp de Mars\, 5 Av. Anatole France\, 75007 Paris",
            "SEQUENCE:0",
            "SUMMARY:Visite guidée",
            "TRANSP:OPAQUE",
            "UID:6B3C1F4E-2D1A-4F7E-9C3B-1A2B3C4D5E6F",
            "URL;VALUE=URI:https://example.org/search?q=a,b;c=d",
            "ATTACH;FMTTYPE=text/plain:data:text/plain;base64,SGVsbG8sIHdvcmxkIQ==",
            "ATTACH;ENCODING=BASE64;VALUE=BINARY;FMTTYPE=text/plain;X-APPLE-FILENAME=note.txt:SGVsbG8=",
            "X-APPLE-STRUCTURED-LOCATION;VALUE=URI;X-ADDRESS=\"Champ de Mars, 5 Av. Anatole France, 75007 Paris\";X-APPLE-RADIUS=141.2;X-TITLE=Tour Eiffel:geo:48.858370,2.294481",
            "X-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC",
            "BEGIN:VALARM",
            "ACTION:DISPLAY",
            "DESCRIPTION:Rappel",
            "TRIGGER:-PT15M",
            "UID:3F1E2D4C-ALARM",
            "X-WR-ALARMUID:3F1E2D4C-ALARM",
            "END:VALARM",
            "END:VEVENT",
            "END:VCALENDAR",
        ]
        .join("\r\n");
        let out = edit_apple_summary();
        assert_eq!(normalised(&out), normalised(&expected), "{out}");
    }

    /// The line of `out` with this name whose parameters include all of `params`, unfolded.
    fn raw_line(out: &str, name: &str, params: &[&str]) -> Vec<String> {
        raw::unfold(out)
            .lines()
            .filter(|l| {
                l.split([':', ';']).next() == Some(name)
                    && params
                        .iter()
                        .all(|p| l.split(':').next().unwrap().contains(p))
            })
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn edit_writes_uri_values_unescaped() {
        let out = edit_apple_summary();
        assert!(!out.contains("\\,2.294481"), "{out}");
        assert!(out.split("\r\n").all(|l| l.len() <= 75), "folded:\n{out}");
        let [geo] = &raw_line(&out, "X-APPLE-STRUCTURED-LOCATION", &[])[..] else {
            panic!("one structured location in\n{out}")
        };
        assert!(geo.ends_with(":geo:48.858370,2.294481"), "{geo}");
        assert_eq!(
            raw_line(&out, "URL", &[]),
            ["URL;VALUE=URI:https://example.org/search?q=a,b;c=d"]
        );
        assert_eq!(
            raw_line(&out, "ATTACH", &["FMTTYPE=text/plain"])
                .iter()
                .filter(|l| l.contains("data:"))
                .collect::<Vec<_>>(),
            ["ATTACH;FMTTYPE=text/plain:data:text/plain;base64,SGVsbG8sIHdvcmxkIQ=="]
        );
        // A plain geo: URL too, and ORGANIZER, a CAL-ADDRESS, keeps its form.
        let original = fixture("rich.ics").replace(
            "X-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC",
            "URL:geo:48.85,2.35\r\nX-LINKS;VALUE=URI:https://a.test/x,y;https://b.test/z\r\n\
             X-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC",
        );
        let old = parse_events(&original, Paris).unwrap().remove(0);
        let new = Event {
            title: "Autre".into(),
            ..old.clone()
        };
        let out = update_event_ics(&original, &old, &new).unwrap();
        assert_eq!(raw_line(&out, "URL", &[]), ["URL:geo:48.85,2.35"]);
        // Several URIs in one X- property keep the separator the parser split them at.
        assert_eq!(
            raw_line(&out, "X-LINKS", &[]),
            ["X-LINKS;VALUE=URI:https://a.test/x,y;https://b.test/z"]
        );
        assert_eq!(
            raw_line(&out, "ORGANIZER", &[]),
            ["ORGANIZER;CN=Alix:mailto:alix@example.org"]
        );
    }

    #[test]
    fn edit_keeps_inline_binary_attachment_encoded() {
        let out = edit_apple_summary();
        let [line] = &raw_line(&out, "ATTACH", &["VALUE=BINARY"])[..] else {
            panic!("one binary attachment in\n{out}")
        };
        assert!(line.contains(";ENCODING=BASE64"), "{line}");
        assert!(line.contains(";X-APPLE-FILENAME=note.txt"), "{line}");
        assert!(line.ends_with(":SGVsbG8="), "{line}");
        assert!(!out.contains(":Hello"), "{out}");

        // Bytes that are not UTF-8 stay binary as well.
        let original = fixture("rich.ics").replace(
            "X-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC",
            "ATTACH;FMTTYPE=image/png;ENCODING=BASE64;VALUE=BINARY:iVBORw0KGgo=\r\nX-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC",
        );
        let old = parse_events(&original, Paris).unwrap().remove(0);
        let new = Event {
            title: "Autre".into(),
            ..old.clone()
        };
        let out = update_event_ics(&original, &old, &new).unwrap();
        let [line] = &raw_line(&out, "ATTACH", &["VALUE=BINARY"])[..] else {
            panic!("one binary attachment in\n{out}")
        };
        assert!(
            line.contains(";ENCODING=BASE64") && line.ends_with(":iVBORw0KGgo="),
            "{line}"
        );
    }

    #[test]
    fn edit_refuses_base64_without_value_binary() {
        let attach = "ATTACH;ENCODING=BASE64;FMTTYPE=text/plain:SGVsbG8=";
        let original = fixture("rich.ics").replace(
            "X-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC",
            &format!("{attach}\r\nX-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC"),
        );
        let old = parse_events(&original, Paris).unwrap().remove(0);
        let new = Event {
            title: "Autre".into(),
            ..old.clone()
        };
        let e = update_event_ics(&original, &old, &new)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("ATTACH holds inline data Fronde cannot keep exactly")
                && e.contains("calendar app"),
            "{e}"
        );
    }

    #[test]
    fn edit_refuses_data_uri_naming_the_property() {
        let original = fixture("rich.ics").replace(
            "X-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC",
            "IMAGE:data:text/plain,Hello%2C%20world\r\nX-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC",
        );
        let old = parse_events(&original, Paris).unwrap().remove(0);
        let new = Event {
            title: "Autre".into(),
            ..old.clone()
        };
        let e = update_event_ics(&original, &old, &new)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("IMAGE holds inline data Fronde cannot keep exactly"),
            "{e}"
        );
    }

    #[test]
    fn edit_survives_deeply_nested_components() {
        let depth = 5000;
        let nested = "BEGIN:VALARM\r\n".repeat(depth) + &"END:VALARM\r\n".repeat(depth);
        let original = fixture("rich.ics").replace(
            "X-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC",
            &format!("{nested}X-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC"),
        );
        let old = parse_events(&original, Paris).unwrap().remove(0);
        let new = Event {
            title: "Autre".into(),
            ..old.clone()
        };
        // Either refused or written back whole; never a stack overflow.
        if let Ok(out) = update_event_ics(&original, &old, &new) {
            for tag in ["BEGIN:VALARM", "END:VALARM"] {
                assert_eq!(out.matches(tag).count(), original.matches(tag).count());
            }
        }
    }

    #[test]
    fn edit_refuses_attachments_it_cannot_keep_exactly() {
        for attach in [
            // calcard keeps neither the charset nor the percent-encoded form.
            "ATTACH:data:text/plain;charset=utf-8;base64,SGVsbG8=",
            "ATTACH:data:text/plain,Hello%2C%20world",
            // Not base64: the parser keeps the payload as text.
            "ATTACH;ENCODING=BASE64;VALUE=BINARY:not*base64",
        ] {
            let original = fixture("rich.ics").replace(
                "X-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC",
                &format!("{attach}\r\nX-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC"),
            );
            let old = parse_events(&original, Paris).unwrap().remove(0);
            let new = Event {
                title: "Autre".into(),
                ..old.clone()
            };
            let e = update_event_ics(&original, &old, &new)
                .unwrap_err()
                .to_string();
            assert!(e.contains("calendar app"), "{attach}: {e}");
        }
    }

    #[test]
    fn recurring_looks_only_at_the_item() {
        // Apple's Europe/Paris VTIMEZONE has RRULEs in DAYLIGHT and STANDARD.
        let apple = fixture("apple.ics");
        let uid = "6B3C1F4E-2D1A-4F7E-9C3B-1A2B3C4D5E6F";
        assert!(!is_recurring(&apple, uid).unwrap());

        let item = |kind: &str, line: &str| {
            format!(
                "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:{kind}\r\nUID:s-1\r\n\
                 DTSTART:20261120T090000Z\r\n{line}\r\nEND:{kind}\r\nEND:VCALENDAR\r\n"
            )
        };
        for (kind, line) in [
            ("VEVENT", "RRULE:FREQ=WEEKLY"),
            ("VEVENT", "RDATE:20261127T090000Z,20261204T090000Z"),
            ("VEVENT", "RECURRENCE-ID:20261127T090000Z"),
            ("VEVENT", "rrule:FREQ=DAILY"),
            ("VTODO", "RRULE:FREQ=MONTHLY"),
        ] {
            let ics = item(kind, line);
            assert!(is_recurring(&ics, "s-1").unwrap(), "{line}");
            assert!(!is_recurring(&ics, "other").unwrap(), "{line}");
        }
        assert!(!is_recurring(&item("VEVENT", "SUMMARY:Once"), "s-1").unwrap());
        assert!(is_recurring("not iCalendar", "s-1").is_err());
    }

    fn with_override(ics: &str, kind: &str) -> String {
        // The override comes first, as some servers store it.
        let override_item = format!(
            "BEGIN:{kind}\r\nUID:dup\r\nRECURRENCE-ID:20261127T090000Z\r\nSUMMARY:Exception\r\n\
             DTSTART:20261127T100000Z\r\nDUE:20261127T100000Z\r\nEND:{kind}\r\n"
        );
        ics.replacen(
            &format!("BEGIN:{kind}\r\n"),
            &format!("{override_item}BEGIN:{kind}\r\n"),
            1,
        )
    }

    #[test]
    fn edit_picks_the_item_without_recurrence_id() {
        let master = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:dup\r\n\
                      SUMMARY:Main\r\nDTSTART:20261120T090000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let original = with_override(master, "VEVENT");
        let old = Event {
            uid: "dup".into(),
            title: "Main".into(),
            start: utc(2026, 11, 20, 9, 0),
            end: None,
            location: None,
            description: None,
        };
        let new = Event {
            title: "Main renamed".into(),
            ..old.clone()
        };
        let out = update_event_ics(&original, &old, &new).unwrap();
        let summaries: Vec<_> = raw::unfold(&out)
            .lines()
            .filter(|l| l.starts_with("SUMMARY"))
            .map(str::to_string)
            .collect();
        assert_eq!(summaries, ["SUMMARY:Exception", "SUMMARY:Main renamed"]);

        // Only the override: nothing to edit.
        let only_override = original.replace(
            "BEGIN:VEVENT\r\nUID:dup\r\nSUMMARY:Main\r\nDTSTART:20261120T090000Z\r\nEND:VEVENT\r\n",
            "",
        );
        assert!(
            update_event_ics(&only_override, &old, &new)
                .unwrap_err()
                .to_string()
                .contains("no VEVENT")
        );
        // Two items claim to be the series.
        let twice = master.replace(
            "END:VEVENT\r\n",
            "END:VEVENT\r\nBEGIN:VEVENT\r\nUID:dup\r\nSUMMARY:Twin\r\nDTSTART:20261121T090000Z\r\nEND:VEVENT\r\n",
        );
        assert!(
            update_event_ics(&twice, &old, &new)
                .unwrap_err()
                .to_string()
                .contains("2 VEVENT components")
        );
    }

    #[test]
    fn reminder_edit_picks_the_item_without_recurrence_id() {
        let master = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:dup\r\n\
                      SUMMARY:Main\r\nDUE:20261120T090000Z\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let original = with_override(master, "VTODO");
        let old = Reminder {
            uid: "dup".into(),
            text: "Main".into(),
            due: Utc.with_ymd_and_hms(2026, 11, 20, 9, 0, 0).unwrap(),
            done: false,
        };
        let new = Reminder {
            done: true,
            ..old.clone()
        };
        let out = update_reminder_ics(&original, &old, &new).unwrap();
        let cal = ICalendar::parse(&out).unwrap();
        let todos: Vec<_> = cal
            .components
            .iter()
            .filter(|c| c.component_type.is_todo())
            .collect();
        assert_eq!(todos.len(), 2);
        assert!(todos[0].has_property(&ICalendarProperty::RecurrenceId));
        assert_eq!(todos[0].status(), None, "the override is untouched");
        assert_eq!(todos[1].status(), Some(&ICalendarStatus::Completed));

        let twice = master.replace(
            "END:VTODO\r\n",
            "END:VTODO\r\nBEGIN:VTODO\r\nUID:dup\r\nSUMMARY:Twin\r\nEND:VTODO\r\n",
        );
        assert!(
            update_reminder_ics(&twice, &old, &new)
                .unwrap_err()
                .to_string()
                .contains("2 VTODO components")
        );
    }

    #[test]
    fn has_uid_checks_items_only() {
        let apple = fixture("apple.ics");
        assert!(has_uid(&apple, "6B3C1F4E-2D1A-4F7E-9C3B-1A2B3C4D5E6F"));
        assert!(
            !has_uid(&apple, "3F1E2D4C-ALARM"),
            "an alarm's UID is not the item's"
        );
        assert!(!has_uid(&apple, "Europe/Paris"));

        // A long UID, folded as Outlook writes it.
        let long =
            "040000008200E00074C5B7101A82E00800000000D0F5C1E0B9A2DA01000000000000000010000000";
        let folded = format!(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:{}\r\n {}\r\nDTSTART:20261120T090000Z\r\n\
             END:VEVENT\r\nEND:VCALENDAR\r\n",
            &long[..60],
            &long[60..]
        );
        assert!(has_uid(&folded, long));

        // A UID with characters iCalendar text escapes.
        let escaped =
            "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:a\\,b\\;c\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        assert!(has_uid(escaped, "a,b;c"));
    }

    #[test]
    fn has_uid_scans_text_it_cannot_parse() {
        let broken = "UID:lost-1\r\nthis is not iCalendar\r\n";
        assert!(ICalendar::parse(broken).is_err());
        assert!(has_uid(broken, "lost-1"));
        assert!(!has_uid(broken, "lost"));
        let alarm_only = "not iCalendar\r\nBEGIN:VALARM\r\nUID:lost-2\r\nEND:VALARM\r\n";
        assert!(ICalendar::parse(alarm_only).is_err());
        assert!(!has_uid(alarm_only, "lost-2"));
    }

    #[test]
    fn parse_error_is_short() {
        let card = format!(
            "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:{}\r\nEND:VCARD\r\n",
            "x".repeat(5000)
        );
        let e = parse_events(&card, Paris).unwrap_err().to_string();
        assert!(e.starts_with("invalid iCalendar data: "), "{e}");
        assert!(e.chars().count() < 200, "{e}");
    }

    #[test]
    fn text_line_breaks_become_newlines() {
        let e = Event {
            uid: "nl".into(),
            title: "Un\r\ndeux".into(),
            start: utc(2026, 11, 20, 9, 0),
            end: None,
            location: Some("A\rB".into()),
            description: Some("x\r\ny\rz\nw".into()),
        };
        let ics = event_to_ics(&e);
        assert!(!ics.contains("\\r"), "{ics}");
        let back = parse_events(&ics, Paris).unwrap().remove(0);
        assert_eq!(back.title, "Un\ndeux");
        assert_eq!(back.location.as_deref(), Some("A\nB"));
        assert_eq!(back.description.as_deref(), Some("x\ny\nz\nw"));

        let new = Event {
            title: "Trois\r\nquatre".into(),
            description: Some("p\rq".into()),
            ..back.clone()
        };
        let out = update_event_ics(&ics, &back, &new).unwrap();
        assert!(!out.contains("\\r"), "{out}");
        let edited = parse_events(&out, Paris).unwrap().remove(0);
        assert_eq!(edited.title, "Trois\nquatre");
        assert_eq!(edited.description.as_deref(), Some("p\nq"));

        let r = Reminder {
            uid: "nl-r".into(),
            text: "a\r\nb".into(),
            due: Utc.with_ymd_and_hms(2026, 11, 18, 19, 0, 0).unwrap(),
            done: false,
        };
        assert_eq!(
            parse_reminders(&reminder_to_ics(&r), Paris).unwrap()[0].text,
            "a\nb"
        );
    }
}
