//! Test component for `fronde-wasm`: each tool exercises one thing the host must get right.

use fronde_component::http::{Request, Transport, WasiHttp};
use fronde_component::{Tool, ToolOutput, Tools, export_tool, settings, wstd};
use serde::Deserialize;
use serde_json::{Value, json};

struct Echo;

export_tool!(Echo);

fn tool(name: &str, description: &str, deferred: bool, schema: Value) -> Tool {
    let tool = Tool::new(name, description, schema);
    if deferred { tool.deferred() } else { tool }
}

impl Tools for Echo {
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
                "echo_alloc",
                "Allocate a number of mebibytes.",
                true,
                json!({"type": "object", "properties": {"mib": {"type": "integer"}}, "required": ["mib"]}),
            ),
            tool(
                "echo_sleep",
                "Wait for a number of milliseconds.",
                true,
                json!({"type": "object", "properties": {"ms": {"type": "integer"}}, "required": ["ms"]}),
            ),
        ]
    }

    fn call_tool(name: &str, arguments: &str) -> ToolOutput {
        match name {
            "echo_text" => with_args(arguments, |a: TextArgs| ToolOutput::ok(json!(a.text))),
            "echo_setting" => with_args(arguments, |a: SettingArgs| {
                ToolOutput::ok(json!(settings::get(&a.key)))
            }),
            "echo_fetch" => with_args(arguments, |a: FetchArgs| {
                match fronde_component::block_on(fetch(a)) {
                    Ok(v) => ToolOutput::ok(v),
                    Err(e) => ToolOutput::error(format!("{e:#}")),
                }
            }),
            "echo_spin" => loop {
                std::hint::black_box(());
            },
            "echo_alloc" => with_args(arguments, |a: AllocArgs| {
                let block = vec![1u8; a.mib << 20];
                ToolOutput::ok(json!(std::hint::black_box(&block).len()))
            }),
            "echo_sleep" => with_args(arguments, |a: SleepArgs| {
                // The timer needs the runtime that block_on starts, so it is made inside it.
                fronde_component::block_on(async {
                    wstd::task::sleep(wstd::time::Duration::from_millis(a.ms)).await
                });
                ToolOutput::ok(json!(a.ms))
            }),
            other => ToolOutput::error(format!("unknown tool {other}")),
        }
    }
}

fn with_args<A: for<'de> Deserialize<'de>>(
    arguments: &str,
    f: impl FnOnce(A) -> ToolOutput,
) -> ToolOutput {
    match serde_json::from_str(arguments) {
        Ok(a) => f(a),
        Err(e) => ToolOutput::error(format!("invalid arguments: {e}")),
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
struct AllocArgs {
    mib: usize,
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
async fn fetch(a: FetchArgs) -> Result<Value, fronde_component::http::Error> {
    let mut req = Request::new("GET", a.url);
    if let Some(auth) = a.authorization {
        req = req.with_header("authorization", auth);
    }
    let resp = WasiHttp.send(req).await?;
    Ok(json!({
        "status": resp.status,
        "body": resp.text(),
        "authorization": resp.header("x-echo-authorization"),
    }))
}
