//! Loads the calendar component (components/calendar), built into components/dist by
//! `mise run components`.

use fronde_wasm::{ComponentHost, ComponentSpec, LoadedComponent};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;

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
