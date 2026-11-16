//! Minimal CalDAV client: calendar-query REPORT, PUT and DELETE on one collection.
//!
//! Requests go out through a [`Transport`]: WASI HTTP in the component, an in-memory server in
//! tests. They carry no credentials; the host adds them for the origins it allows.

use anyhow::{anyhow, bail};
use chrono::{DateTime, Utc};
use fronde_component::http::{Request, Transport};
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Event,
    Todo,
}

impl Kind {
    fn component(self) -> &'static str {
        match self {
            Kind::Event => "VEVENT",
            Kind::Todo => "VTODO",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Resource {
    pub href: String,
    pub etag: Option<String>,
    pub ics: String,
}

pub struct CalDav<T> {
    http: T,
    collection: Url,
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn stamp(t: DateTime<Utc>) -> String {
    t.format("%Y%m%dT%H%M%SZ").to_string()
}

/// "404 Not Found", as reqwest's status codes print.
fn status_text(status: u16) -> String {
    http::StatusCode::from_u16(status).map_or_else(|_| status.to_string(), |s| s.to_string())
}

/// Read a multistatus body; keeps only responses that carry calendar data.
fn parse_multistatus(body: &str) -> anyhow::Result<Vec<Resource>> {
    let doc = roxmltree::Document::parse(body)
        .map_err(|e| anyhow!("CalDAV server sent invalid XML: {e}"))?;
    let text_of = |node: roxmltree::Node, name: &str| {
        node.descendants()
            .find(|n| n.tag_name().name() == name)
            .and_then(|n| {
                let t: String = n
                    .descendants()
                    .filter(|c| c.is_text())
                    .filter_map(|c| c.text())
                    .collect();
                (!t.trim().is_empty()).then_some(t)
            })
    };
    Ok(doc
        .descendants()
        .filter(|n| n.tag_name().name() == "response")
        .filter_map(|r| {
            Some(Resource {
                href: text_of(r, "href")?.trim().to_string(),
                etag: text_of(r, "getetag").map(|e| e.trim().to_string()),
                ics: text_of(r, "calendar-data")?,
            })
        })
        .collect())
}

impl<T: Transport> CalDav<T> {
    pub fn new(mut collection: Url, http: T) -> Self {
        if !collection.path().ends_with('/') {
            let p = format!("{}/", collection.path());
            collection.set_path(&p);
        }
        Self { http, collection }
    }

    /// The response body of a successful request.
    async fn send(&self, req: Request) -> anyhow::Result<String> {
        let resp = self
            .http
            .send(req)
            .await
            .map_err(|e| anyhow!("CalDAV server unreachable: {e:#}"))?;
        match resp.status {
            200..=299 => Ok(resp.text()),
            401 | 403 => {
                bail!("CalDAV refused the credentials (check calendar.username/password)")
            }
            412 => bail!("the item changed meanwhile; read it again and retry"),
            s => {
                let body: String = resp.text().chars().take(200).collect();
                bail!("CalDAV error {}: {body}", status_text(s))
            }
        }
    }

    async fn report(&self, filter: &str, expand: &str) -> anyhow::Result<Vec<Resource>> {
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><D:getetag/><C:calendar-data>{expand}</C:calendar-data></D:prop>
  <C:filter><C:comp-filter name="VCALENDAR">{filter}</C:comp-filter></C:filter>
</C:calendar-query>"#
        );
        let req = Request::new("REPORT", self.collection.clone())
            .with_header("Depth", "1")
            .with_header("Content-Type", "application/xml; charset=utf-8")
            .with_body(body);
        parse_multistatus(&self.send(req).await?)
    }

    pub async fn query(
        &self,
        kind: Kind,
        range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> anyhow::Result<Vec<Resource>> {
        let comp = kind.component();
        match range {
            Some((from, to)) => {
                let (s, e) = (stamp(from), stamp(to));
                let filter = format!(
                    r#"<C:comp-filter name="{comp}"><C:time-range start="{s}" end="{e}"/></C:comp-filter>"#
                );
                self.report(&filter, &format!(r#"<C:expand start="{s}" end="{e}"/>"#))
                    .await
            }
            None => {
                self.report(&format!(r#"<C:comp-filter name="{comp}"/>"#), "")
                    .await
            }
        }
    }

    /// The resource whose UID is exactly `uid` (servers match substrings, so results are re-checked).
    pub async fn find_uid(&self, kind: Kind, uid: &str) -> anyhow::Result<Option<Resource>> {
        if uid.is_empty() {
            return Ok(None);
        }
        let filter = format!(
            r#"<C:comp-filter name="{}"><C:prop-filter name="UID"><C:text-match collation="i;octet">{}</C:text-match></C:prop-filter></C:comp-filter>"#,
            kind.component(),
            xml_escape(uid)
        );
        let mut exact: Vec<Resource> = self
            .report(&filter, "")
            .await?
            .into_iter()
            .filter(|r| crate::ics::has_uid(&r.ics, uid))
            .collect();
        match exact.len() {
            0 | 1 => Ok(exact.pop()),
            n => bail!("{n} items share the uid {uid:?}; fix them in your calendar app"),
        }
    }

    fn put(url: Url, ics: &str) -> Request {
        Request::new("PUT", url)
            .with_header("Content-Type", "text/calendar; charset=utf-8")
            .with_body(ics)
    }

    fn href_url(&self, href: &str) -> anyhow::Result<Url> {
        self.collection
            .join(href)
            .map_err(|e| anyhow!("bad href {href:?}: {e}"))
    }

    pub async fn create(&self, uid: &str, ics: &str) -> anyhow::Result<()> {
        let name: String = uid
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let url = self.href_url(&format!("{name}.ics"))?;
        self.send(Self::put(url, ics).with_header("If-None-Match", "*"))
            .await
            .map(|_| ())
    }

    pub async fn update(&self, res: &Resource, ics: &str) -> anyhow::Result<()> {
        let mut req = Self::put(self.href_url(&res.href)?, ics);
        if let Some(e) = &res.etag {
            req = req.with_header("If-Match", e);
        }
        self.send(req).await.map(|_| ())
    }

    pub async fn delete(&self, res: &Resource) -> anyhow::Result<()> {
        let mut req = Request::new("DELETE", self.href_url(&res.href)?);
        if let Some(e) = &res.etag {
            req = req.with_header("If-Match", e);
        }
        self.send(req).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeServer, block_on, fixture};
    use chrono::TimeZone;

    fn client(server: &FakeServer) -> CalDav<FakeServer> {
        CalDav::new(
            "http://cal.test/cal/alix/personal".parse().unwrap(),
            server.clone(),
        )
    }

    #[test]
    fn query_sends_time_range_and_parses_multistatus() {
        let server = FakeServer::default();
        server.respond("REPORT", 207, fixture("multistatus_events.xml"));
        let c = client(&server);
        let from = Utc.with_ymd_and_hms(2026, 11, 16, 0, 0, 0).unwrap();
        let res =
            block_on(c.query(Kind::Event, Some((from, from + chrono::Duration::days(7))))).unwrap();
        let [req] = &server.requests("REPORT")[..] else {
            panic!("expected one REPORT")
        };
        assert_eq!(req.url.as_str(), "http://cal.test/cal/alix/personal/");
        assert_eq!(req.header("depth"), Some("1"));
        assert_eq!(
            req.header("authorization"),
            None,
            "the host adds credentials"
        );
        assert!(
            req.text()
                .contains(r#"time-range start="20261116T000000Z" end="20261123T000000Z""#)
        );
        assert!(req.text().contains(r#"name="VEVENT""#));
        assert!(req.text().contains("expand"));
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].href, "/cal/alix/personal/a.ics");
        assert_eq!(res[0].etag.as_deref(), Some("\"etag-a\""));
        assert!(res[0].ics.contains("SUMMARY:Rendez-vous & café"));
        assert!(res[1].ics.contains("UID:b"));
    }

    #[test]
    fn find_uid_filters_by_uid() {
        let server = FakeServer::default();
        server.respond("REPORT", 207, fixture("multistatus_events.xml"));
        // The server matches substrings; none of the returned items has exactly this UID.
        assert!(
            block_on(client(&server).find_uid(Kind::Todo, "a&b"))
                .unwrap()
                .is_none()
        );
        let [req] = &server.requests("REPORT")[..] else {
            panic!("expected one REPORT")
        };
        assert!(req.text().contains(r#"prop-filter name="UID""#));
        assert!(req.text().contains(">a&amp;b<"));
        assert!(req.text().contains(r#"name="VTODO""#));
    }

    #[test]
    fn find_uid_requires_exact_match() {
        let server = FakeServer::default();
        server.respond("REPORT", 207, fixture("multistatus_events.xml"));
        let c = client(&server);
        // Both a.ics (UID:a) and b.ics (UID:b) come back; only b is exact.
        assert_eq!(
            block_on(c.find_uid(Kind::Event, "b"))
                .unwrap()
                .unwrap()
                .href,
            "/cal/alix/personal/b.ics"
        );
        assert!(
            block_on(c.find_uid(Kind::Event, "")).unwrap().is_none(),
            "empty uid never matches"
        );
    }

    #[test]
    fn create_uses_if_none_match() {
        let server = FakeServer::default();
        server.respond("PUT", 201, "");
        block_on(client(&server).create("u1", "BEGIN:VCALENDAR")).unwrap();
        let [req] = &server.requests("PUT")[..] else {
            panic!("expected one PUT")
        };
        assert_eq!(
            Url::parse(&req.url).unwrap().path(),
            "/cal/alix/personal/u1.ics"
        );
        assert_eq!(req.header("if-none-match"), Some("*"));
        assert_eq!(
            req.header("content-type"),
            Some("text/calendar; charset=utf-8")
        );
        assert_eq!(req.text(), "BEGIN:VCALENDAR");
    }

    #[test]
    fn update_and_delete_use_if_match() {
        let server = FakeServer::default();
        server.respond("PUT", 204, "");
        server.respond("DELETE", 204, "");
        let c = client(&server);
        let res = Resource {
            href: "/cal/alix/personal/a.ics".into(),
            etag: Some("\"etag-a\"".into()),
            ics: String::new(),
        };
        block_on(c.update(&res, "X")).unwrap();
        block_on(c.delete(&res)).unwrap();
        for method in ["PUT", "DELETE"] {
            let [req] = &server.requests(method)[..] else {
                panic!("expected one {method}")
            };
            assert_eq!(
                Url::parse(&req.url).unwrap().path(),
                "/cal/alix/personal/a.ics"
            );
            assert_eq!(req.header("if-match"), Some("\"etag-a\""));
        }
    }

    /// The host enforces the time limits; a request it gives up on comes back as an error.
    #[test]
    fn slow_server_times_out() {
        let server = FakeServer::default();
        server.fail("REPORT", "connection-timeout");
        let e = block_on(client(&server).query(Kind::Event, None))
            .unwrap_err()
            .to_string();
        assert!(e.contains("unreachable"), "{e}");
    }

    #[test]
    fn unauthorized_explains() {
        let server = FakeServer::default();
        server.respond("REPORT", 401, "");
        let e = block_on(client(&server).query(Kind::Event, None))
            .unwrap_err()
            .to_string();
        assert!(e.contains("calendar.username"), "{e}");
    }

    #[test]
    fn precondition_failed_explains() {
        let server = FakeServer::default();
        server.respond("PUT", 412, "");
        let res = Resource {
            href: "/cal/alix/personal/a.ics".into(),
            etag: Some("x".into()),
            ics: String::new(),
        };
        let e = block_on(client(&server).update(&res, "X"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("changed meanwhile"), "{e}");
    }

    #[test]
    fn other_errors_show_status_and_body() {
        let server = FakeServer::default();
        server.respond("REPORT", 500, "x".repeat(300));
        let e = block_on(client(&server).query(Kind::Event, None))
            .unwrap_err()
            .to_string();
        assert_eq!(
            e,
            format!(
                "CalDAV error 500 Internal Server Error: {}",
                "x".repeat(200)
            )
        );
    }
}
