//! Calendar backend and tools.

pub mod caldav;
pub mod ics;
pub mod tools;

use async_trait::async_trait;
use caldav::{CalDav, Kind, Resource};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use ics::{Event, Reminder};

#[async_trait]
pub trait CalendarBackend: Send + Sync {
    async fn list_events(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> anyhow::Result<Vec<Event>>;
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

pub struct CalDavBackend {
    pub events: CalDav,
    pub reminders: CalDav,
    pub tz: Tz,
}

const RECURRING: &str = "this is a recurring series; change it in your calendar app (not supported here yet)";

fn is_recurring(res: &Resource) -> bool {
    res.ics.lines().any(|l| l.starts_with("RRULE") || l.starts_with("RECURRENCE-ID"))
}

#[async_trait]
impl CalendarBackend for CalDavBackend {
    async fn list_events(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> anyhow::Result<Vec<Event>> {
        let mut out = Vec::new();
        for res in self.events.query(Kind::Event, Some((from, to))).await? {
            out.extend(ics::parse_events(&res.ics, self.tz)?);
        }
        Ok(out)
    }

    async fn get_event(&self, uid: &str) -> anyhow::Result<Option<Event>> {
        let Some(res) = self.events.find_uid(Kind::Event, uid).await? else { return Ok(None) };
        Ok(ics::parse_events(&res.ics, self.tz)?.into_iter().find(|e| e.uid == uid))
    }

    async fn put_event(&self, e: &Event) -> anyhow::Result<()> {
        match self.events.find_uid(Kind::Event, &e.uid).await? {
            Some(res) if is_recurring(&res) => anyhow::bail!(RECURRING),
            Some(res) => self.events.update(&res, &ics::event_to_ics(e)).await,
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
            out.extend(ics::parse_reminders(&res.ics, self.tz)?);
        }
        Ok(out)
    }

    async fn get_reminder(&self, uid: &str) -> anyhow::Result<Option<Reminder>> {
        let Some(res) = self.reminders.find_uid(Kind::Todo, uid).await? else { return Ok(None) };
        Ok(ics::parse_reminders(&res.ics, self.tz)?.into_iter().find(|r| r.uid == uid))
    }

    async fn put_reminder(&self, r: &Reminder) -> anyhow::Result<()> {
        match self.reminders.find_uid(Kind::Todo, &r.uid).await? {
            Some(res) if is_recurring(&res) => anyhow::bail!(RECURRING),
            Some(res) => self.reminders.update(&res, &ics::reminder_to_ics(r)).await,
            None => self.reminders.create(&r.uid, &ics::reminder_to_ics(r)).await,
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
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn recurring_event_is_not_modified() {
        let server = MockServer::start().await;
        let body = r#"<multistatus xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><response><href>/c/r.ics</href><propstat><prop><getetag>"e"</getetag><C:calendar-data>BEGIN:VCALENDAR
BEGIN:VEVENT
UID:r
SUMMARY:Sport
DTSTART:20261116T180000Z
RRULE:FREQ=WEEKLY
END:VEVENT
END:VCALENDAR
</C:calendar-data></prop></propstat></response></multistatus>"#;
        Mock::given(method("REPORT")).respond_with(ResponseTemplate::new(207).set_body_string(body)).mount(&server).await;
        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(204)).expect(0).mount(&server).await;
        Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(204)).expect(0).mount(&server).await;
        let url: url::Url = format!("{}/c/", server.uri()).parse().unwrap();
        let b = CalDavBackend {
            events: CalDav::new(url.clone(), "u".into(), "p".into()),
            reminders: CalDav::new(url, "u".into(), "p".into()),
            tz: chrono_tz::Europe::Paris,
        };
        let e = b.get_event("r").await.unwrap().unwrap();
        assert!(b.put_event(&e).await.unwrap_err().to_string().contains("recurring"));
        assert!(b.delete_event("r").await.unwrap_err().to_string().contains("recurring"));
    }

    /// Live round trip against a real server: JARVIS_TEST_CALDAV_URL (+ _USER, _PASSWORD).
    #[tokio::test]
    #[ignore]
    async fn caldav_roundtrip_live() {
        let url: url::Url = std::env::var("JARVIS_TEST_CALDAV_URL").expect("JARVIS_TEST_CALDAV_URL").parse().unwrap();
        let user = std::env::var("JARVIS_TEST_CALDAV_USER").unwrap_or_default();
        let pw = std::env::var("JARVIS_TEST_CALDAV_PASSWORD").unwrap_or_default();
        let tz = chrono_tz::Europe::Paris;
        let b = CalDavBackend { events: CalDav::new(url.clone(), user.clone(), pw.clone()), reminders: CalDav::new(url, user, pw), tz };
        let start = Utc::now() + chrono::Duration::days(1);
        let e = Event { uid: uuid::Uuid::new_v4().to_string(), title: "jarvis test".into(), start: ics::When::At(start), end: None, location: None, description: None };
        b.put_event(&e).await.unwrap();
        let listed = b.list_events(start - chrono::Duration::hours(1), start + chrono::Duration::hours(1)).await.unwrap();
        assert!(listed.iter().any(|x| x.uid == e.uid));
        assert!(b.delete_event(&e.uid).await.unwrap());
    }
}
