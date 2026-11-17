//! The tools as the agent sees them (names, descriptions, input schemas, deferral) are pinned in
//! `snapshots/calendar-tools.json`: the snapshot was taken from the native CalDAV tools the
//! component replaced, so the component keeps their exact contract. Set
//! `FRONDE_UPDATE_SNAPSHOTS=1` to rewrite it after a deliberate change.

use fronde_component::Tool;
use serde_json::{Value, json};

const SNAPSHOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/snapshots/calendar-tools.json"
);

fn contract(tools: &[Tool]) -> Value {
    let mut tools: Vec<&Tool> = tools.iter().collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    Value::Array(
        tools
            .into_iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "deferred": t.deferred,
                    // As the host reads it: the schema crosses the WIT boundary as JSON text.
                    "input_schema": serde_json::from_str::<Value>(&t.input_schema.to_string())
                        .unwrap(),
                })
            })
            .collect(),
    )
}

#[test]
fn calendar_tool_schemas_match_the_native_ones() {
    let component = contract(&calendar::exported_tools());
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
