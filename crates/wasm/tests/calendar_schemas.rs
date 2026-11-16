//! The calendar component's tools, as the agent sees them (names, descriptions, input schemas,
//! deferral), are pinned in `snapshots/calendar-tools.json`: the snapshot was taken from the
//! native CalDAV tools the component replaced, so the component keeps their exact contract.
//! Set `FRONDE_UPDATE_SNAPSHOTS=1` to rewrite it after a deliberate change.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use fronde_hub::calendar::CalendarBackend;
use fronde_hub::calendar::ics::{Event, Reminder};
use fronde_hub::calendar::tools::register_calendar_tools;
use fronde_hub::tools::{Registry, ToolDef};
use fronde_wasm::{ComponentHost, ComponentSpec, LoadedComponent};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

const SNAPSHOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/snapshots/calendar-tools.json"
);

fn contract(defs: &[ToolDef]) -> Value {
    let mut defs: Vec<&ToolDef> = defs.iter().collect();
    defs.sort_by(|a, b| a.name.cmp(&b.name));
    Value::Array(
        defs.into_iter()
            .map(|d| {
                json!({
                    "name": d.name,
                    "description": d.description,
                    "deferred": d.deferred,
                    "input_schema": d.input_schema,
                })
            })
            .collect(),
    )
}

/// The native tools are only listed here, never called.
struct Unused;

#[async_trait]
impl CalendarBackend for Unused {
    async fn list_events(&self, _: DateTime<Utc>, _: DateTime<Utc>) -> anyhow::Result<Vec<Event>> {
        unreachable!()
    }
    async fn get_event(&self, _: &str) -> anyhow::Result<Option<Event>> {
        unreachable!()
    }
    async fn put_event(&self, _: &Event) -> anyhow::Result<()> {
        unreachable!()
    }
    async fn delete_event(&self, _: &str) -> anyhow::Result<bool> {
        unreachable!()
    }
    async fn list_reminders(&self) -> anyhow::Result<Vec<Reminder>> {
        unreachable!()
    }
    async fn get_reminder(&self, _: &str) -> anyhow::Result<Option<Reminder>> {
        unreachable!()
    }
    async fn put_reminder(&self, _: &Reminder) -> anyhow::Result<()> {
        unreachable!()
    }
    async fn delete_reminder(&self, _: &str) -> anyhow::Result<bool> {
        unreachable!()
    }
}

fn native() -> Value {
    let registry = Registry::new();
    register_calendar_tools(&registry, Arc::new(Unused), chrono_tz::Europe::Paris).unwrap();
    contract(&registry.all())
}

async fn component() -> Value {
    let path = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../components/dist/calendar.wasm"
    ));
    let spec = ComponentSpec {
        name: "calendar".into(),
        path,
        allow: vec![],
        credentials: vec![],
        settings: BTreeMap::new(),
        sha256: None,
    };
    let calendar = LoadedComponent::load(&ComponentHost::new().unwrap(), spec)
        .await
        .unwrap();
    contract(calendar.list_tools())
}

#[tokio::test]
async fn calendar_tool_schemas_match_the_native_ones() {
    let component = component().await;
    let native = native();
    if std::env::var_os("FRONDE_UPDATE_SNAPSHOTS").is_some() {
        let text = serde_json::to_string_pretty(&native).unwrap() + "\n";
        std::fs::create_dir_all(PathBuf::from(SNAPSHOT).parent().unwrap()).unwrap();
        std::fs::write(SNAPSHOT, text).unwrap();
    }
    let snapshot: Value =
        serde_json::from_str(&std::fs::read_to_string(SNAPSHOT).unwrap()).unwrap();
    assert_eq!(native, snapshot, "the native tools changed");
    assert_eq!(
        component, snapshot,
        "the component's tools differ from the native ones"
    );
}
