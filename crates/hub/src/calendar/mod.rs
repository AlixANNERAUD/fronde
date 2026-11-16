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

pub struct CalDavBackend {
    pub events: CalDav,
    pub reminders: CalDav,
    pub tz: Tz,
}

const RECURRING: &str =
    "this is a recurring series; change it in your calendar app (not supported here yet)";

fn is_recurring(res: &Resource) -> bool {
    res.ics
        .lines()
        .any(|l| l.starts_with("RRULE") || l.starts_with("RECURRENCE-ID"))
}

#[async_trait]
impl CalendarBackend for CalDavBackend {
    async fn list_events(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> anyhow::Result<Vec<Event>> {
        let mut out = Vec::new();
        for res in self.events.query(Kind::Event, Some((from, to))).await? {
            match ics::parse_events(&res.ics, self.tz) {
                Ok(events) => out.extend(events),
                Err(e) => tracing::warn!("skipping unreadable event {}: {e:#}", res.href),
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
            match ics::parse_reminders(&res.ics, self.tz) {
                Ok(rs) => out.extend(rs),
                Err(e) => tracing::warn!("skipping unreadable task {}: {e:#}", res.href),
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
        Mock::given(method("REPORT"))
            .respond_with(ResponseTemplate::new(207).set_body_string(body))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(204))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(204))
            .expect(0)
            .mount(&server)
            .await;
        let url: url::Url = format!("{}/c/", server.uri()).parse().unwrap();
        let b = CalDavBackend {
            events: CalDav::new(url.clone(), "u".into(), "p".into()),
            reminders: CalDav::new(url, "u".into(), "p".into()),
            tz: chrono_tz::Europe::Paris,
        };
        let e = b.get_event("r").await.unwrap().unwrap();
        assert!(
            b.put_event(&e)
                .await
                .unwrap_err()
                .to_string()
                .contains("recurring")
        );
        assert!(
            b.delete_event("r")
                .await
                .unwrap_err()
                .to_string()
                .contains("recurring")
        );
    }

    fn multistatus(ics: &str) -> String {
        format!(
            r#"<multistatus xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><response><href>/c/x.ics</href><propstat><prop><getetag>"e"</getetag><C:calendar-data><![CDATA[{ics}]]></C:calendar-data></prop></propstat></response></multistatus>"#
        )
    }

    async fn backend_with(server: &MockServer) -> CalDavBackend {
        let url: url::Url = format!("{}/c/", server.uri()).parse().unwrap();
        CalDavBackend {
            events: CalDav::new(url.clone(), "u".into(), "p".into()),
            reminders: CalDav::new(url, "u".into(), "p".into()),
            tz: chrono_tz::Europe::Paris,
        }
    }

    #[tokio::test]
    async fn update_patches_existing_resource() {
        let rich = std::fs::read_to_string(format!(
            "{}/tests/fixtures/rich.ics",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let server = MockServer::start().await;
        Mock::given(method("REPORT"))
            .respond_with(ResponseTemplate::new(207).set_body_string(multistatus(&rich)))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let b = backend_with(&server).await;
        let mut e = b.get_event("rich-1").await.unwrap().unwrap();
        e.title = "Nouveau titre".into();
        b.put_event(&e).await.unwrap();
        let put = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.method.as_str() == "PUT")
            .unwrap();
        let body = String::from_utf8(put.body).unwrap();
        for kept in [
            "BEGIN:VALARM",
            "ATTENDEE;CN=Bob",
            "DTSTART;TZID=Romance Standard Time:20261120T100000",
            "SUMMARY:Nouveau titre",
        ] {
            assert!(body.contains(kept), "lost {kept:?} in\n{body}");
        }
    }

    #[tokio::test]
    async fn done_patches_existing_task() {
        let todo = std::fs::read_to_string(format!(
            "{}/tests/fixtures/daytodo.ics",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let server = MockServer::start().await;
        Mock::given(method("REPORT"))
            .respond_with(ResponseTemplate::new(207).set_body_string(multistatus(&todo)))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let b = backend_with(&server).await;
        let mut r = b.get_reminder("day-todo").await.unwrap().unwrap();
        r.done = true;
        b.put_reminder(&r).await.unwrap();
        let put = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.method.as_str() == "PUT")
            .unwrap();
        let body = String::from_utf8(put.body).unwrap();
        assert!(
            body.contains("DUE;VALUE=DATE:20261118")
                && body.contains("PRIORITY:1")
                && body.contains("STATUS:COMPLETED"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn one_broken_item_does_not_break_the_list() {
        let server = MockServer::start().await;
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
        Mock::given(method("REPORT"))
            .respond_with(ResponseTemplate::new(207).set_body_string(body))
            .mount(&server)
            .await;
        let b = backend_with(&server).await;
        let now = Utc::now();
        let events = b.list_events(now, now).await.unwrap();
        assert_eq!(
            events.iter().map(|e| e.uid.as_str()).collect::<Vec<_>>(),
            vec!["ok"]
        );
    }

    /// Live round trip against a real server: JARVIS_TEST_CALDAV_URL (+ _USER, _PASSWORD).
    #[tokio::test]
    #[ignore]
    async fn caldav_roundtrip_live() {
        let url: url::Url = std::env::var("JARVIS_TEST_CALDAV_URL")
            .expect("JARVIS_TEST_CALDAV_URL")
            .parse()
            .unwrap();
        let user = std::env::var("JARVIS_TEST_CALDAV_USER").unwrap_or_default();
        let pw = std::env::var("JARVIS_TEST_CALDAV_PASSWORD").unwrap_or_default();
        let tz = chrono_tz::Europe::Paris;
        let b = CalDavBackend {
            events: CalDav::new(url.clone(), user.clone(), pw.clone()),
            reminders: CalDav::new(url, user, pw),
            tz,
        };
        let start = Utc::now() + chrono::Duration::days(1);
        let e = Event {
            uid: uuid::Uuid::new_v4().to_string(),
            title: "jarvis test".into(),
            start: ics::When::At(start),
            end: None,
            location: None,
            description: None,
        };
        b.put_event(&e).await.unwrap();
        let listed = b
            .list_events(
                start - chrono::Duration::hours(1),
                start + chrono::Duration::hours(1),
            )
            .await
            .unwrap();
        assert!(listed.iter().any(|x| x.uid == e.uid));
        assert!(b.delete_event(&e.uid).await.unwrap());
    }
}
