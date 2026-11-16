//! The calendar component's tools, as the agent sees them (names, descriptions, input schemas,
//! deferral), are pinned in `snapshots/calendar-tools.json`: the snapshot was taken from the
//! native CalDAV tools the component replaced, so the component keeps their exact contract.
//! Set `FRONDE_UPDATE_SNAPSHOTS=1` to rewrite it after a deliberate change.

use fronde_hub::tools::ToolDef;
use fronde_wasm::{ComponentHost, ComponentSpec, LoadedComponent};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

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
    if std::env::var_os("FRONDE_UPDATE_SNAPSHOTS").is_some() {
        let text = serde_json::to_string_pretty(&component).unwrap() + "\n";
        std::fs::write(SNAPSHOT, text).unwrap();
    }
    let snapshot: Value =
        serde_json::from_str(&std::fs::read_to_string(SNAPSHOT).unwrap()).unwrap();
    assert_eq!(
        component, snapshot,
        "the component's tools differ from the native ones"
    );
}
