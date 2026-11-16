//! Events and reminders <-> iCalendar. Times are stored in UTC; input and output use the user's zone.

use anyhow::anyhow;
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
    ICalendar::parse(ics).map_err(|e| anyhow!("invalid iCalendar data: {e:?}"))
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

/// True when the document has a component whose UID is exactly `uid`.
pub fn has_uid(ics: &str, uid: &str) -> bool {
    !uid.is_empty() && ICalendar::parse(ics).is_ok_and(|cal| cal.uids().any(|u| u == uid))
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
    ev.add_property(ICalendarProperty::Summary, e.title.as_str());
    ev.entries
        .push(when_entry(ICalendarProperty::Dtstart, &e.start));
    if let Some(end) = &e.end {
        ev.entries.push(when_entry(ICalendarProperty::Dtend, end));
    }
    if let Some(l) = &e.location {
        ev.add_property(ICalendarProperty::Location, l.as_str());
    }
    if let Some(d) = &e.description {
        ev.add_property(ICalendarProperty::Description, d.as_str());
    }
    calendar_with(ev).to_string()
}

pub fn reminder_to_ics(r: &Reminder) -> String {
    let mut todo = ICalendarComponent::new(ICalendarComponentType::VTodo);
    todo.add_uid(&r.uid);
    todo.add_dtstamp(utc_pdt(Utc::now()));
    todo.add_property(ICalendarProperty::Summary, r.text.as_str());
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
    let comp = cal
        .components
        .iter_mut()
        .find(|c| c.component_type.is_event() && c.uid() == Some(&new.uid))
        .ok_or_else(|| anyhow!("no VEVENT with uid {:?} in the stored item", new.uid))?;

    set_property(
        comp,
        ICalendarEntry::new(ICalendarProperty::Dtstamp).with_value(utc_pdt(Utc::now())),
    );
    if new.title != old.title {
        set_property(
            comp,
            ICalendarEntry::new(ICalendarProperty::Summary).with_value(new.title.as_str()),
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
                ICalendarEntry::new(ICalendarProperty::Location).with_value(l.as_str()),
            ),
            None => remove_property(comp, &ICalendarProperty::Location),
        }
    }
    if new.description != old.description {
        match &new.description {
            Some(d) => set_property(
                comp,
                ICalendarEntry::new(ICalendarProperty::Description).with_value(d.as_str()),
            ),
            None => remove_property(comp, &ICalendarProperty::Description),
        }
    }
    Ok(cal.to_string())
}

/// The stored task text with only the fields that changed rewritten.
pub fn update_reminder_ics(
    original: &str,
    old: &Reminder,
    new: &Reminder,
) -> anyhow::Result<String> {
    let mut cal = parse(original)?;
    let comp = cal
        .components
        .iter_mut()
        .find(|c| c.component_type.is_todo() && c.uid() == Some(&new.uid))
        .ok_or_else(|| anyhow!("no VTODO with uid {:?} in the stored item", new.uid))?;

    set_property(
        comp,
        ICalendarEntry::new(ICalendarProperty::Dtstamp).with_value(utc_pdt(Utc::now())),
    );
    if new.text != old.text {
        set_property(
            comp,
            ICalendarEntry::new(ICalendarProperty::Summary).with_value(new.text.as_str()),
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
    Ok(cal.to_string())
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
        assert!(alarm.has_property(&ICalendarProperty::Trigger));
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
