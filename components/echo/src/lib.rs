//! Test component for `jarvis-wasm`: each tool exercises one thing the host must get right.

#![deny(unsafe_code)]

// The generated export glue needs `unsafe`; nothing else in the crate may use it.
#[allow(unsafe_code)]
mod bindings {
    wit_bindgen::generate!({ world: "tool", path: "../../wit" });
    use crate::Component;
    export!(Component with_types_in crate::bindings);
}

use bindings::exports::jarvis::component::tools::{Guest, Tool, ToolOutput};
use bindings::jarvis::component::settings;
use serde::Deserialize;
use serde_json::{Value, json};
use wstd::http::{Body, BodyExt, Client, Request};

struct Component;

fn tool(name: &str, description: &str, deferred: bool, schema: Value) -> Tool {
    Tool {
        name: name.into(),
        description: description.into(),
        input_schema: schema.to_string(),
        deferred,
    }
}

fn ok(value: Value) -> ToolOutput {
    ToolOutput {
        value: value.to_string(),
        next: None,
        is_error: false,
    }
}

fn error(message: String) -> ToolOutput {
    ToolOutput {
        value: Value::String(message).to_string(),
        next: None,
        is_error: true,
    }
}

impl Guest for Component {
    fn list_tools() -> Vec<Tool> {
        vec![
            tool(
                "echo_text",
                "Return the text given.",
                false,
                json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}),
            ),
            tool(
                "echo_setting",
                "Return the value of a component setting.",
                false,
                json!({"type": "object", "properties": {"key": {"type": "string"}}, "required": ["key"]}),
            ),
            tool(
                "echo_fetch",
                "Fetch a web page with GET and report its status and body.",
                true,
                json!({"type": "object", "properties": {
                    "url": {"type": "string"},
                    "authorization": {"type": "string"}
                }, "required": ["url"]}),
            ),
            tool(
                "echo_spin",
                "Loop forever.",
                true,
                json!({"type": "object"}),
            ),
            tool(
                "echo_sleep",
                "Wait for a number of milliseconds.",
                true,
                json!({"type": "object", "properties": {"ms": {"type": "integer"}}, "required": ["ms"]}),
            ),
        ]
    }

    fn call_tool(name: String, arguments: String) -> ToolOutput {
        match name.as_str() {
            "echo_text" => with_args(&arguments, |a: TextArgs| ok(json!(a.text))),
            "echo_setting" => with_args(&arguments, |a: SettingArgs| {
                ok(json!(settings::get(&a.key)))
            }),
            "echo_fetch" => with_args(&arguments, |a: FetchArgs| {
                match wstd::runtime::block_on(fetch(a)) {
                    Ok(v) => ok(v),
                    Err(e) => error(format!("{e:#}")),
                }
            }),
            "echo_spin" => loop {
                std::hint::black_box(());
            },
            "echo_sleep" => with_args(&arguments, |a: SleepArgs| {
                // The timer needs the runtime that block_on starts, so it is made inside it.
                wstd::runtime::block_on(async {
                    wstd::task::sleep(wstd::time::Duration::from_millis(a.ms)).await
                });
                ok(json!(a.ms))
            }),
            other => error(format!("unknown tool {other}")),
        }
    }
}

fn with_args<A: for<'de> Deserialize<'de>>(
    arguments: &str,
    f: impl FnOnce(A) -> ToolOutput,
) -> ToolOutput {
    match serde_json::from_str(arguments) {
        Ok(a) => f(a),
        Err(e) => error(format!("invalid arguments: {e}")),
    }
}

#[derive(Deserialize)]
struct TextArgs {
    text: String,
}

#[derive(Deserialize)]
struct SettingArgs {
    key: String,
}

#[derive(Deserialize)]
struct SleepArgs {
    ms: u64,
}

#[derive(Deserialize)]
struct FetchArgs {
    url: String,
    /// Sent as the request's own Authorization header, which the host must refuse.
    authorization: Option<String>,
}

/// The test server echoes the Authorization header it received in `x-echo-authorization`.
async fn fetch(a: FetchArgs) -> wstd::http::Result<Value> {
    let mut req = Request::get(a.url);
    if let Some(auth) = a.authorization {
        req = req.header("authorization", auth);
    }
    let resp = Client::new().send(req.body(Body::empty())?).await?;
    let status = resp.status().as_u16();
    let echoed = resp
        .headers()
        .get("x-echo-authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = resp
        .into_body()
        .into_boxed_body()
        .collect()
        .await?
        .to_bytes();
    Ok(json!({
        "status": status,
        "body": String::from_utf8_lossy(&body),
        "authorization": echoed,
    }))
}
