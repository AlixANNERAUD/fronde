//! Calendar backend: events and reminders kept in two CalDAV collections.

use crate::caldav::{CalDav, Kind, Resource};
use crate::ics::{self, Event, Reminder};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use fronde_component::http::Transport;
use url::Url;

// The component is single-threaded, so the futures need not be `Send`.
#[allow(async_fn_in_trait)]
pub trait CalendarBackend {
    async fn list_events(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> anyhow::Result<Vec<Event>>;
    async fn get_event(&self, uid: &str) -> anyhow::Result<Option<Event>>;
    /// Creates or replaces (by uid).
    async fn put_event(&self, e: &Event) -> anyhow::Result<()>;
    /// `false` when no such event.
    async fn delete_event(&self, uid: &str) -> anyhow::Result<bool>;
    async fn list_reminders(&self) -> anyhow::Result<Vec<Reminder>>;
    async fn get_reminder(&self, uid: &str) -> anyhow::Result<Option<Reminder>>;
    async fn put_reminder(&self, r: &Reminder) -> anyhow::Result<()>;
    async fn delete_reminder(&self, uid: &str) -> anyhow::Result<bool>;
}

/// What the host tells the component: the two collections and the user's time zone.
#[derive(Debug)]
pub struct Settings {
    pub events_url: Url,
    pub reminders_url: Url,
    pub tz: Tz,
}

impl Settings {
    /// `events_url` and `reminders_url` are required; `timezone` (IANA) defaults to UTC.
    pub fn read(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let url = |key: &str| {
            let value = get(key).ok_or_else(|| {
                format!(
                    "the calendar component has no {key} setting; set it to a CalDAV collection URL"
                )
            })?;
            value
                .parse::<Url>()
                .map_err(|e| format!("the {key} setting {value:?} is not a URL: {e}"))
        };
        let tz = match get("timezone") {
            None => Tz::UTC,
            Some(name) => name.parse().map_err(|_| {
                format!(
                    "the timezone setting {name:?} is not an IANA time zone such as Europe/Paris"
                )
            })?,
        };
        Ok(Self {
            events_url: url("events_url")?,
            reminders_url: url("reminders_url")?,
            tz,
        })
    }

    pub fn backend<T: Transport + Clone>(&self, http: T) -> CalDavBackend<T> {
        CalDavBackend {
            events: CalDav::new(self.events_url.clone(), http.clone()),
            reminders: CalDav::new(self.reminders_url.clone(), http),
            tz: self.tz,
        }
    }
}

pub struct CalDavBackend<T> {
    pub events: CalDav<T>,
    pub reminders: CalDav<T>,
    pub tz: Tz,
}

const RECURRING: &str =
    "this is a recurring series; change it in your calendar app (not supported here yet)";

fn refuse_series(res: &Resource, uid: &str) -> anyhow::Result<()> {
    if ics::is_recurring(&res.ics, uid)? {
        anyhow::bail!(RECURRING);
    }
    Ok(())
}

/// The stored resource has the UID but no item this tool can read: writing a new one over it
/// would lose it.
fn unreadable(uid: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "the stored item with uid {uid:?} cannot be read by this tool; change it in your calendar app"
    )
}

impl<T: Transport> CalendarBackend for CalDavBackend<T> {
    async fn list_events(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> anyhow::Result<Vec<Event>> {
        let mut out = Vec::new();
        // An item no parser can read is skipped: one bad item must not hide the others.
        for res in self.events.query(Kind::Event, Some((from, to))).await? {
            if let Ok(events) = ics::parse_events(&res.ics, self.tz) {
                out.extend(events);
            }
        }
        Ok(out)
    }

    async fn get_event(&self, uid: &str) -> anyhow::Result<Option<Event>> {
        let Some(res) = self.events.find_uid(Kind::Event, uid).await? else {
            return Ok(None);
        };
        Ok(ics::parse_events(&res.ics, self.tz)?
            .into_iter()
            .find(|e| e.uid == uid))
    }

    async fn put_event(&self, e: &Event) -> anyhow::Result<()> {
        let Some(res) = self.events.find_uid(Kind::Event, &e.uid).await? else {
            return self.events.create(&e.uid, &ics::event_to_ics(e)).await;
        };
        refuse_series(&res, &e.uid)?;
        let old = ics::parse_events(&res.ics, self.tz)?
            .into_iter()
            .find(|x| x.uid == e.uid)
            .ok_or_else(|| unreadable(&e.uid))?;
        let text = ics::update_event_ics(&res.ics, &old, e)?;
        self.events.update(&res, &text).await
    }

    async fn delete_event(&self, uid: &str) -> anyhow::Result<bool> {
        let Some(res) = self.events.find_uid(Kind::Event, uid).await? else {
            return Ok(false);
        };
        refuse_series(&res, uid)?;
        self.events.delete(&res).await.map(|_| true)
    }

    async fn list_reminders(&self) -> anyhow::Result<Vec<Reminder>> {
        let mut out = Vec::new();
        for res in self.reminders.query(Kind::Todo, None).await? {
            if let Ok(rs) = ics::parse_reminders(&res.ics, self.tz) {
                out.extend(rs);
            }
        }
        Ok(out)
    }

    async fn get_reminder(&self, uid: &str) -> anyhow::Result<Option<Reminder>> {
        let Some(res) = self.reminders.find_uid(Kind::Todo, uid).await? else {
            return Ok(None);
        };
        Ok(ics::parse_reminders(&res.ics, self.tz)?
            .into_iter()
            .find(|r| r.uid == uid))
    }

    async fn put_reminder(&self, r: &Reminder) -> anyhow::Result<()> {
        let Some(res) = self.reminders.find_uid(Kind::Todo, &r.uid).await? else {
            return self
                .reminders
                .create(&r.uid, &ics::reminder_to_ics(r))
                .await;
        };
        refuse_series(&res, &r.uid)?;
        let old = ics::parse_reminders(&res.ics, self.tz)?
            .into_iter()
            .find(|x| x.uid == r.uid)
            .ok_or_else(|| unreadable(&r.uid))?;
        let text = ics::update_reminder_ics(&res.ics, &old, r)?;
        self.reminders.update(&res, &text).await
    }

    async fn delete_reminder(&self, uid: &str) -> anyhow::Result<bool> {
        let Some(res) = self.reminders.find_uid(Kind::Todo, uid).await? else {
            return Ok(false);
        };
        refuse_series(&res, uid)?;
        self.reminders.delete(&res).await.map(|_| true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeServer, block_on, fixture};
    use std::collections::BTreeMap;

    fn backend_with(server: &FakeServer) -> CalDavBackend<FakeServer> {
        Settings {
            events_url: "http://cal.test/c/".parse().unwrap(),
            reminders_url: "http://cal.test/c/".parse().unwrap(),
            tz: chrono_tz::Europe::Paris,
        }
        .backend(server.clone())
    }

    fn multistatus(ics: &str) -> String {
        format!(
            r#"<multistatus xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><response><href>/c/x.ics</href><propstat><prop><getetag>"e"</getetag><C:calendar-data><![CDATA[{ics}]]></C:calendar-data></prop></propstat></response></multistatus>"#
        )
    }

    #[test]
    fn recurring_event_is_not_modified() {
        let server = FakeServer::default();
        let body = r#"<multistatus xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><response><href>/c/r.ics</href><propstat><prop><getetag>"e"</getetag><C:calendar-data>BEGIN:VCALENDAR
BEGIN:VEVENT
UID:r
SUMMARY:Sport
DTSTART:20261116T180000Z
RRULE:FREQ=WEEKLY
END:VEVENT
END:VCALENDAR
</C:calendar-data></prop></propstat></response></multistatus>"#;
        server.respond("REPORT", 207, body);
        server.respond("PUT", 204, "");
        server.respond("DELETE", 204, "");
        let b = backend_with(&server);
        let e = block_on(b.get_event("r")).unwrap().unwrap();
        assert!(
            block_on(b.put_event(&e))
                .unwrap_err()
                .to_string()
                .contains("recurring")
        );
        assert!(
            block_on(b.delete_event("r"))
                .unwrap_err()
                .to_string()
                .contains("recurring")
        );
        assert!(server.requests("PUT").is_empty());
        assert!(server.requests("DELETE").is_empty());
    }

    #[test]
    fn update_rewrites_existing_resource() {
        let server = FakeServer::default();
        server.respond("REPORT", 207, multistatus(&fixture("rich.ics")));
        server.respond("PUT", 204, "");
        let b = backend_with(&server);
        let mut e = block_on(b.get_event("rich-1")).unwrap().unwrap();
        e.title = "Nouveau titre".into();
        block_on(b.put_event(&e)).unwrap();
        let [put] = &server.requests("PUT")[..] else {
            panic!("expected one PUT")
        };
        assert_eq!(Url::parse(&put.url).unwrap().path(), "/c/x.ics");
        assert_eq!(put.header("if-match"), Some("\"e\""));
        let body = put.text();
        let cal = calcard::icalendar::ICalendar::parse(&body).unwrap();
        assert!(cal.components.iter().any(|c| c.component_type.is_alarm()));
        let ev = cal
            .components
            .iter()
            .find(|c| c.component_type.is_event())
            .unwrap();
        assert!(
            ev.properties(&calcard::icalendar::ICalendarProperty::Attendee)
                .any(|a| {
                    a.parameters(&calcard::icalendar::ICalendarParameterName::Cn)
                        .any(|v| v.as_text() == Some("Bob"))
                })
        );
        let dtstart = ev
            .property(&calcard::icalendar::ICalendarProperty::Dtstart)
            .unwrap();
        assert_eq!(dtstart.tz_id(), Some("Romance Standard Time"));
        let parsed = ics::parse_events(&body, chrono_tz::Europe::Paris).unwrap();
        assert_eq!(parsed[0].title, "Nouveau titre");
        assert_eq!(parsed[0].start, e.start);
    }

    #[test]
    fn time_zone_rules_do_not_make_an_event_recurring() {
        // Apple, Outlook and Thunderbird write RRULEs inside VTIMEZONE.
        let server = FakeServer::default();
        server.respond("REPORT", 207, multistatus(&fixture("apple.ics")));
        server.respond("PUT", 204, "");
        let b = backend_with(&server);
        let uid = "6B3C1F4E-2D1A-4F7E-9C3B-1A2B3C4D5E6F";
        let mut e = block_on(b.get_event(uid)).unwrap().unwrap();
        e.title = "Visite guidée".into();
        block_on(b.put_event(&e)).unwrap();
        let [put] = &server.requests("PUT")[..] else {
            panic!("expected one PUT")
        };
        let body = put.text();
        assert!(body.contains("RRULE:FREQ=YEARLY"), "{body}");
        assert!(body.contains("SUMMARY:Visite guidée"), "{body}");
    }

    #[test]
    fn rdate_series_is_not_modified() {
        let server = FakeServer::default();
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:rd\r\nSUMMARY:Cours\r\n\
                   DTSTART:20261116T180000Z\r\nRDATE:20261123T180000Z\r\nEND:VEVENT\r\n\
                   END:VCALENDAR\r\n";
        server.respond("REPORT", 207, multistatus(ics));
        server.respond("PUT", 204, "");
        server.respond("DELETE", 204, "");
        let b = backend_with(&server);
        let e = block_on(b.get_event("rd")).unwrap().unwrap();
        let err = block_on(b.put_event(&e)).unwrap_err().to_string();
        assert!(err.contains("recurring"), "{err}");
        let err = block_on(b.delete_event("rd")).unwrap_err().to_string();
        assert!(err.contains("recurring"), "{err}");
        assert!(server.requests("PUT").is_empty());
        assert!(server.requests("DELETE").is_empty());
    }

    #[test]
    fn unreadable_item_is_not_overwritten() {
        // The resource has the UID, but its event has no DTSTART, so no event is read from it.
        let server = FakeServer::default();
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:odd\r\nSUMMARY:Sans date\r\n\
                   X-KEEP:me\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        server.respond("REPORT", 207, multistatus(ics));
        server.respond("PUT", 204, "");
        let b = backend_with(&server);
        let e = Event {
            uid: "odd".into(),
            title: "Nouveau".into(),
            start: ics::When::At(Utc::now()),
            end: None,
            location: None,
            description: None,
        };
        let err = block_on(b.put_event(&e)).unwrap_err().to_string();
        assert!(err.contains("cannot be read"), "{err}");
        assert!(server.requests("PUT").is_empty());

        let todo = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:odd-t\r\nSUMMARY:Sans echeance\r\n\
                    END:VTODO\r\nEND:VCALENDAR\r\n";
        let server = FakeServer::default();
        server.respond("REPORT", 207, multistatus(todo));
        server.respond("PUT", 204, "");
        let b = backend_with(&server);
        let r = Reminder {
            uid: "odd-t".into(),
            text: "Nouveau".into(),
            due: Utc::now(),
            done: true,
        };
        let err = block_on(b.put_reminder(&r)).unwrap_err().to_string();
        assert!(err.contains("cannot be read"), "{err}");
        assert!(server.requests("PUT").is_empty());
    }

    #[test]
    fn done_rewrites_existing_task() {
        let server = FakeServer::default();
        server.respond("REPORT", 207, multistatus(&fixture("daytodo.ics")));
        server.respond("PUT", 204, "");
        let b = backend_with(&server);
        let mut r = block_on(b.get_reminder("day-todo")).unwrap().unwrap();
        r.done = true;
        block_on(b.put_reminder(&r)).unwrap();
        let [put] = &server.requests("PUT")[..] else {
            panic!("expected one PUT")
        };
        let body = &put.text();
        assert!(
            body.contains("DUE;VALUE=DATE:20261118")
                && body.contains("PRIORITY:1")
                && body.contains("STATUS:COMPLETED"),
            "{body}"
        );
    }

    #[test]
    fn one_broken_item_does_not_break_the_list() {
        let server = FakeServer::default();
        let body = r#"<multistatus xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
<response><href>/c/bad.ics</href><propstat><prop><getetag>"1"</getetag><C:calendar-data>this is not iCalendar</C:calendar-data></prop></propstat></response>
<response><href>/c/ok.ics</href><propstat><prop><getetag>"2"</getetag><C:calendar-data>BEGIN:VCALENDAR
BEGIN:VEVENT
UID:ok
SUMMARY:Fine
DTSTART:20261116T180000Z
END:VEVENT
END:VCALENDAR
</C:calendar-data></prop></propstat></response></multistatus>"#;
        server.respond("REPORT", 207, body);
        let b = backend_with(&server);
        let now = Utc::now();
        let events = block_on(b.list_events(now, now)).unwrap();
        assert_eq!(
            events.iter().map(|e| e.uid.as_str()).collect::<Vec<_>>(),
            vec!["ok"]
        );
    }

    #[test]
    fn create_puts_a_new_resource() {
        let server = FakeServer::default();
        server.respond("REPORT", 207, "<multistatus xmlns=\"DAV:\"/>");
        server.respond("PUT", 201, "");
        let b = backend_with(&server);
        let e = Event {
            uid: "new-1".into(),
            title: "Nouveau".into(),
            start: ics::When::At(Utc::now()),
            end: None,
            location: None,
            description: None,
        };
        block_on(b.put_event(&e)).unwrap();
        let [put] = &server.requests("PUT")[..] else {
            panic!("expected one PUT")
        };
        assert_eq!(put.url.as_str(), "http://cal.test/c/new-1.ics");
        assert_eq!(put.header("if-none-match"), Some("*"));
        assert!(put.text().contains("UID:new-1"), "{}", put.text());
    }

    fn settings(pairs: &[(&str, &str)]) -> Result<Settings, String> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Settings::read(|k| map.get(k).cloned())
    }

    #[test]
    fn settings_need_both_urls() {
        let e = settings(&[("events_url", "https://dav.test/e/")]).unwrap_err();
        assert!(e.contains("reminders_url"), "{e}");
        let e = settings(&[
            ("events_url", "nope"),
            ("reminders_url", "https://dav.test/r/"),
        ])
        .unwrap_err();
        assert!(e.contains("events_url") && e.contains("not a URL"), "{e}");
    }

    #[test]
    fn settings_timezone_defaults_to_utc() {
        let urls = [
            ("events_url", "https://dav.test/e/"),
            ("reminders_url", "https://dav.test/r/"),
        ];
        assert_eq!(settings(&urls).unwrap().tz, Tz::UTC);
        let paris = settings(&[urls[0], urls[1], ("timezone", "Europe/Paris")]).unwrap();
        assert_eq!(paris.tz, chrono_tz::Europe::Paris);
        let e = settings(&[urls[0], urls[1], ("timezone", "Mars/Olympus")]).unwrap_err();
        assert!(e.contains("IANA"), "{e}");
    }
}
