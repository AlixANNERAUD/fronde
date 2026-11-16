//! Loads the calendar component (components/calendar), built into components/dist by
//! `mise run components`.

use fronde_wasm::{Allowed, ComponentHost, ComponentSpec, LoadedComponent};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use wiremock::matchers::{method, path};
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
    };
    let calendar = LoadedComponent::load(&host, spec).await.unwrap();
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
    };
    let calendar = LoadedComponent::load(&host, spec).await.unwrap();
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
