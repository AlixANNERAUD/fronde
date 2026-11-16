//! Minimal CalDAV client: calendar-query REPORT, PUT and DELETE on one collection.

use anyhow::{anyhow, bail};
use chrono::{DateTime, Utc};
use reqwest::{Method, StatusCode};
use std::time::Duration;
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

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

fn http_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(10).min(timeout))
        .build()
        .expect("http client")
}

pub struct CalDav {
    http: reqwest::Client,
    collection: Url,
    user: String,
    password: String,
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

impl CalDav {
    pub fn new(mut collection: Url, user: String, password: String) -> Self {
        if !collection.path().ends_with('/') {
            let p = format!("{}/", collection.path());
            collection.set_path(&p);
        }
        Self {
            http: http_client(DEFAULT_TIMEOUT),
            collection,
            user,
            password,
        }
    }

    /// Give up on a request after `timeout` (default 30 s).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.http = http_client(timeout);
        self
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> anyhow::Result<reqwest::Response> {
        let resp = req
            .basic_auth(&self.user, Some(&self.password))
            .send()
            .await
            .map_err(|e| anyhow!("CalDAV server unreachable: {e}"))?;
        match resp.status() {
            s if s.is_success() => Ok(resp),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                bail!("CalDAV refused the credentials (check calendar.username/password)")
            }
            StatusCode::PRECONDITION_FAILED => {
                bail!("the item changed meanwhile; read it again and retry")
            }
            s => {
                let body: String = resp
                    .text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(200)
                    .collect();
                bail!("CalDAV error {s}: {body}")
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
        let req = self
            .http
            .request(
                Method::from_bytes(b"REPORT").expect("valid method"),
                self.collection.clone(),
            )
            .header("Depth", "1")
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(body);
        let text = self.send(req).await?.text().await?;
        parse_multistatus(&text)
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
            .filter(|r| super::ics::has_uid(&r.ics, uid))
            .collect();
        match exact.len() {
            0 | 1 => Ok(exact.pop()),
            n => bail!("{n} items share the uid {uid:?}; fix them in your calendar app"),
        }
    }

    fn put(&self, url: Url, ics: &str) -> reqwest::RequestBuilder {
        self.http
            .put(url)
            .header("Content-Type", "text/calendar; charset=utf-8")
            .body(ics.to_string())
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
        self.send(self.put(url, ics).header("If-None-Match", "*"))
            .await
            .map(|_| ())
    }

    pub async fn update(&self, res: &Resource, ics: &str) -> anyhow::Result<()> {
        let mut req = self.put(self.href_url(&res.href)?, ics);
        if let Some(e) = &res.etag {
            req = req.header("If-Match", e);
        }
        self.send(req).await.map(|_| ())
    }

    pub async fn delete(&self, res: &Resource) -> anyhow::Result<()> {
        let mut req = self.http.delete(self.href_url(&res.href)?);
        if let Some(e) = &res.etag {
            req = req.header("If-Match", e);
        }
        self.send(req).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    async fn client(server: &MockServer) -> CalDav {
        CalDav::new(
            format!("{}/cal/alix/personal", server.uri())
                .parse()
                .unwrap(),
            "alix".into(),
            "pw".into(),
        )
    }

    #[tokio::test]
    async fn query_sends_time_range_and_parses_multistatus() {
        let server = MockServer::start().await;
        Mock::given(method("REPORT"))
            .and(path("/cal/alix/personal/"))
            .and(header("depth", "1"))
            .and(header("authorization", "Basic YWxpeDpwdw=="))
            .and(body_string_contains(
                r#"time-range start="20261116T000000Z" end="20261123T000000Z""#,
            ))
            .and(body_string_contains(r#"name="VEVENT""#))
            .and(body_string_contains("expand"))
            .respond_with(
                ResponseTemplate::new(207).set_body_string(fixture("multistatus_events.xml")),
            )
            .expect(1)
            .mount(&server)
            .await;
        let c = client(&server).await;
        let from = Utc.with_ymd_and_hms(2026, 11, 16, 0, 0, 0).unwrap();
        let res = c
            .query(Kind::Event, Some((from, from + chrono::Duration::days(7))))
            .await
            .unwrap();
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].href, "/cal/alix/personal/a.ics");
        assert_eq!(res[0].etag.as_deref(), Some("\"etag-a\""));
        assert!(res[0].ics.contains("SUMMARY:Rendez-vous & café"));
        assert!(res[1].ics.contains("UID:b"));
    }

    #[tokio::test]
    async fn find_uid_filters_by_uid() {
        let server = MockServer::start().await;
        Mock::given(method("REPORT"))
            .and(body_string_contains(r#"prop-filter name="UID""#))
            .and(body_string_contains(">a&amp;b<"))
            .and(body_string_contains(r#"name="VTODO""#))
            .respond_with(
                ResponseTemplate::new(207).set_body_string(fixture("multistatus_events.xml")),
            )
            .expect(1)
            .mount(&server)
            .await;
        // The server matches substrings; none of the returned items has exactly this UID.
        assert!(
            client(&server)
                .await
                .find_uid(Kind::Todo, "a&b")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn find_uid_requires_exact_match() {
        let server = MockServer::start().await;
        Mock::given(method("REPORT"))
            .respond_with(
                ResponseTemplate::new(207).set_body_string(fixture("multistatus_events.xml")),
            )
            .mount(&server)
            .await;
        let c = client(&server).await;
        // Both a.ics (UID:a) and b.ics (UID:b) come back; only b is exact.
        assert_eq!(
            c.find_uid(Kind::Event, "b").await.unwrap().unwrap().href,
            "/cal/alix/personal/b.ics"
        );
        assert!(
            c.find_uid(Kind::Event, "").await.unwrap().is_none(),
            "empty uid never matches"
        );
    }

    #[tokio::test]
    async fn create_uses_if_none_match() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/cal/alix/personal/u1.ics"))
            .and(header("if-none-match", "*"))
            .and(header("content-type", "text/calendar; charset=utf-8"))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;
        client(&server)
            .await
            .create("u1", "BEGIN:VCALENDAR")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn update_and_delete_use_if_match() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/cal/alix/personal/a.ics"))
            .and(header("if-match", "\"etag-a\""))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/cal/alix/personal/a.ics"))
            .and(header("if-match", "\"etag-a\""))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let c = client(&server).await;
        let res = Resource {
            href: "/cal/alix/personal/a.ics".into(),
            etag: Some("\"etag-a\"".into()),
            ics: String::new(),
        };
        c.update(&res, "X").await.unwrap();
        c.delete(&res).await.unwrap();
    }

    #[tokio::test]
    async fn slow_server_times_out() {
        let server = MockServer::start().await;
        Mock::given(method("REPORT"))
            .respond_with(ResponseTemplate::new(207).set_delay(std::time::Duration::from_secs(5)))
            .mount(&server)
            .await;
        let c = client(&server)
            .await
            .with_timeout(std::time::Duration::from_millis(300));
        let e = c.query(Kind::Event, None).await.unwrap_err().to_string();
        assert!(e.contains("unreachable"), "{e}");
    }

    #[tokio::test]
    async fn unauthorized_explains() {
        let server = MockServer::start().await;
        Mock::given(method("REPORT"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let e = client(&server)
            .await
            .query(Kind::Event, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("calendar.username"), "{e}");
    }

    #[tokio::test]
    async fn precondition_failed_explains() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(412))
            .mount(&server)
            .await;
        let res = Resource {
            href: "/cal/alix/personal/a.ics".into(),
            etag: Some("x".into()),
            ics: String::new(),
        };
        let e = client(&server)
            .await
            .update(&res, "X")
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("changed meanwhile"), "{e}");
    }
}
