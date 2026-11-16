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

fn is_recurring(res: &Resource) -> bool {
    res.ics
        .lines()
        .any(|l| l.starts_with("RRULE") || l.starts_with("RECURRENCE-ID"))
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
        match self.events.find_uid(Kind::Event, &e.uid).await? {
            Some(res) if is_recurring(&res) => anyhow::bail!(RECURRING),
            Some(res) => {
                let old = ics::parse_events(&res.ics, self.tz)?
                    .into_iter()
                    .find(|x| x.uid == e.uid);
                let text = match old {
                    Some(old) => ics::update_event_ics(&res.ics, &old, e)?,
                    None => ics::event_to_ics(e),
                };
                self.events.update(&res, &text).await
            }
            None => self.events.create(&e.uid, &ics::event_to_ics(e)).await,
        }
    }

    async fn delete_event(&self, uid: &str) -> anyhow::Result<bool> {
        match self.events.find_uid(Kind::Event, uid).await? {
            Some(res) if is_recurring(&res) => anyhow::bail!(RECURRING),
            Some(res) => self.events.delete(&res).await.map(|_| true),
            None => Ok(false),
        }
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
        match self.reminders.find_uid(Kind::Todo, &r.uid).await? {
            Some(res) if is_recurring(&res) => anyhow::bail!(RECURRING),
            Some(res) => {
                let old = ics::parse_reminders(&res.ics, self.tz)?
                    .into_iter()
                    .find(|x| x.uid == r.uid);
                let text = match old {
                    Some(old) => ics::update_reminder_ics(&res.ics, &old, r)?,
                    None => ics::reminder_to_ics(r),
                };
                self.reminders.update(&res, &text).await
            }
            None => {
                self.reminders
                    .create(&r.uid, &ics::reminder_to_ics(r))
                    .await
            }
        }
    }

    async fn delete_reminder(&self, uid: &str) -> anyhow::Result<bool> {
        match self.reminders.find_uid(Kind::Todo, uid).await? {
            Some(res) if is_recurring(&res) => anyhow::bail!(RECURRING),
            Some(res) => self.reminders.delete(&res).await.map(|_| true),
            None => Ok(false),
        }
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
    fn update_patches_existing_resource() {
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
        for kept in [
            "BEGIN:VALARM",
            "ATTENDEE;CN=Bob",
            "DTSTART;TZID=Romance Standard Time:20261120T100000",
            "SUMMARY:Nouveau titre",
        ] {
            assert!(
                put.text().contains(kept),
                "lost {kept:?} in\n{}",
                put.text()
            );
        }
    }

    #[test]
    fn done_patches_existing_task() {
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
