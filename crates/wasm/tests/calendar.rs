//! Loads the calendar component (components/calendar), built into components/dist by
//! `mise run components`.

use fronde_hub::config::ComponentConfig;
use fronde_hub::source::Supervisor;
use fronde_hub::tools::Registry;
use fronde_wasm::{
    Allowed, Checks, ComponentHost, ComponentSource, ComponentSpec, Hashes, LoadedComponent,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn calendar_wasm() -> PathBuf {
    let p = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../components/dist/calendar.wasm"
    ));
    assert!(
        p.exists(),
        "{} is missing: run `mise run components`",
        p.display()
    );
    p
}

#[tokio::test]
async fn calendar_component_lists_its_tools() {
    let host = ComponentHost::new().unwrap();
    let spec = ComponentSpec {
        name: "calendar".into(),
        path: calendar_wasm(),
        allow: vec![],
        credentials: vec![],
        settings: BTreeMap::new(),
        tool_prefixes: vec!["calendar_".into(), "reminders_".into()],
    };
    let calendar = LoadedComponent::load_unapproved(&host, spec).await.unwrap();
    let names: Vec<&str> = calendar
        .list_tools()
        .iter()
        .map(|d| d.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "calendar_list",
            "calendar_create",
            "calendar_update",
            "calendar_delete",
            "reminders_create",
            "reminders_list",
            "reminders_done",
            "reminders_delete",
        ]
    );
    let out = calendar.call("calendar_list", &json!({})).await;
    assert!(out.is_error);
    assert!(
        out.to_text().contains("no events_url setting"),
        "{}",
        out.to_text()
    );
}

/// Runs calcard's parser inside wasm32, where it needs its `wasm` feature (see
/// components/calendar/Cargo.toml), against a CalDAV server.
#[tokio::test]
async fn calendar_component_reads_events_from_caldav() {
    let event = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTIMEZONE\r\n\
        TZID:Romance Standard Time\r\nBEGIN:STANDARD\r\nDTSTART:16010101T030000\r\n\
        TZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\nRRULE:FREQ=YEARLY;BYDAY=-1SU;BYMONTH=10\r\n\
        END:STANDARD\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:wasm-1\r\n\
        SUMMARY:Dentiste\\, Lyon\r\nDTSTART;TZID=Romance Standard Time:20261120T100000\r\n\
        DURATION:PT1H\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let body = format!(
        r#"<multistatus xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><response><href>/cal/a.ics</href><propstat><prop><getetag>"1"</getetag><C:calendar-data><![CDATA[{event}]]></C:calendar-data></prop><status>HTTP/1.1 200 OK</status></propstat></response></multistatus>"#
    );
    let server = MockServer::start().await;
    Mock::given(method("REPORT"))
        .and(path("/cal/"))
        .respond_with(ResponseTemplate::new(207).set_body_string(body))
        .mount(&server)
        .await;

    let host = ComponentHost::new().unwrap();
    let url = format!("{}/cal/", server.uri());
    let spec = ComponentSpec {
        name: "calendar".into(),
        path: calendar_wasm(),
        // The test server listens on 127.0.0.1, a local address.
        allow: vec![Allowed {
            origin: server.uri().parse().unwrap(),
            local: true,
        }],
        credentials: vec![],
        settings: BTreeMap::from([
            ("events_url".to_string(), url.clone()),
            ("reminders_url".to_string(), url),
            ("timezone".to_string(), "America/New_York".to_string()),
        ]),
        tool_prefixes: vec!["calendar_".into(), "reminders_".into()],
    };
    let calendar = LoadedComponent::load_unapproved(&host, spec).await.unwrap();
    let out = calendar
        .call(
            "calendar_list",
            &json!({"from": "2026-11-16", "to": "2026-11-22"}),
        )
        .await;
    assert!(!out.is_error, "{}", out.to_text());
    // 10:00 in Paris (the Windows zone name) is 04:00 in New York.
    assert_eq!(
        out.value,
        json!([{
            "uid": "wasm-1",
            "title": "Dentiste, Lyon",
            "start": "2026-11-20T04:00-05:00",
        }]),
        "{}",
        out.to_text()
    );
}

/// What a component's [[sources]] entry holds, approved with the current hashes.
fn approved(entry: &str) -> ComponentConfig {
    let mut config: ComponentConfig = toml::from_str(entry).unwrap();
    let hashes = Hashes::of(&config).unwrap();
    config.sha256 = Some(hashes.sha256);
    config.manifest_sha256 = Some(hashes.manifest_sha256);
    config
}

fn multistatus(items: &[(&str, &str)]) -> String {
    let responses: String = items
        .iter()
        .map(|(href, ics)| {
            format!(
                r#"<response><href>{href}</href><propstat><prop><getetag>"1"</getetag><C:calendar-data><![CDATA[{ics}]]></C:calendar-data></prop><status>HTTP/1.1 200 OK</status></propstat></response>"#
            )
        })
        .collect();
    format!(
        r#"<multistatus xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">{responses}</multistatus>"#
    )
}

/// Runs the async `f` and returns it with what it logged.
async fn logging<T>(f: impl Future<Output = T>) -> (T, String) {
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Buffer {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buffer = Buffer::default();
    let out = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || out.clone())
        .with_ansi(false)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    let value = f.await;
    drop(guard);
    let text = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
    (value, text)
}

/// The calendar as the hub runs it, from a [[sources]] entry: an event made on a phone (Windows
/// time zone, attendees, an alarm) is listed in local time and edited without losing what the
/// tools do not model; the host signs requests for the allowed origin and refuses another one.
#[tokio::test]
async fn calendar_component_roundtrip() {
    let rich = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../components/calendar/tests/fixtures/rich.ics"
    ))
    .unwrap();
    let server = MockServer::start().await;
    let auth = ("authorization", "Basic YWxpeDpzM2NyZXQ=");
    Mock::given(method("REPORT"))
        .and(path("/cal/"))
        .and(header(auth.0, auth.1))
        .and(body_string_contains("time-range"))
        .respond_with(ResponseTemplate::new(207).set_body_string(multistatus(&[
            ("/cal/rich-1.ics", &rich),
            ("/cal/broken.ics", "UID:lost-1\r\nthis is not iCalendar\r\n"),
        ])))
        .mount(&server)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/cal/"))
        .and(header(auth.0, auth.1))
        .and(body_string_contains("prop-filter"))
        .respond_with(
            ResponseTemplate::new(207).set_body_string(multistatus(&[("/cal/rich-1.ics", &rich)])),
        )
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/cal/rich-1.ics"))
        .and(header(auth.0, auth.1))
        .and(header("if-match", "\"1\""))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let password = dir.path().join("caldav");
    fronde_hub::private::write(&password, "s3cret\n").unwrap();
    let origin = server.uri();
    // The reminders live on another server, which this entry does not allow.
    let config = approved(&format!(
        r#"
        name = "calendar"
        path = {wasm:?}
        allow = [{{ origin = {origin:?}, local = true }}]
        [settings]
        events_url = "{origin}/cal/"
        reminders_url = "http://127.0.0.1:9/tasks/"
        [[credentials]]
        origin = {origin:?}
        basic = {{ username = "alix", password = {{ file = {password:?} }} }}
        "#,
        wasm = calendar_wasm().display().to_string(),
        password = password.display().to_string(),
    ));
    let host = ComponentHost::new().unwrap();
    let source = ComponentSource::new(
        config,
        Checks {
            timezone: "Europe/Paris".into(),
            ..Checks::default()
        },
        host,
    );
    let registry = Arc::new(Registry::new());
    let supervisor = Supervisor::new(registry.clone());
    supervisor.start(Arc::new(source)).await.unwrap();
    assert_eq!(registry.get("calendar_list").unwrap().source, "calendar");

    let (out, log) = logging(registry.call(
        "calendar_list",
        json!({"from": "2026-11-16", "to": "2026-11-22"}),
    ))
    .await;
    assert!(!out.is_error, "{}", out.to_text());
    // 10:00 in "Romance Standard Time" is 10:00 in Paris, the user's zone given by the host.
    assert_eq!(
        out.value,
        json!([{
            "uid": "rich-1",
            "title": "Point projet",
            "start": "2026-11-20T10:00+01:00",
        }]),
        "{}",
        out.to_text()
    );
    assert!(
        log.contains("skipping unreadable event /cal/broken.ics") && log.contains("calendar"),
        "the component's standard error reaches the log: {log}"
    );

    let out = registry
        .call(
            "calendar_update",
            json!({"uid": "rich-1", "title": "Point projet (salle B)"}),
        )
        .await;
    assert!(!out.is_error, "{}", out.to_text());
    let put = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.method.as_str() == "PUT")
        .unwrap();
    let sent = String::from_utf8(put.body).unwrap();
    assert!(sent.contains("SUMMARY:Point projet (salle B)"), "{sent}");
    for kept in [
        "BEGIN:VALARM",
        "TRIGGER:-PT15M",
        "ATTENDEE;CN=Bob;PARTSTAT=ACCEPTED:mailto:bob@example.org",
        "ATTENDEE;CN=Eve:mailto:eve@example.org",
        "ORGANIZER;CN=Alix:mailto:alix@example.org",
        "X-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC",
    ] {
        assert!(sent.contains(kept), "{kept} lost: {sent}");
    }

    let out = registry.call("reminders_list", json!({})).await;
    assert!(out.is_error);
    assert!(
        out.to_text().to_lowercase().contains("denied"),
        "{}",
        out.to_text()
    );
}
