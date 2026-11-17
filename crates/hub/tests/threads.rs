//! The `threads_*` tools as the hub serves them: only a thread's agent reaches the threads. An
//! MCP client that reaches the hub without a thread's token (here, over an in-process pipe, where
//! no gate names a thread) is never offered them, and is refused them without the threads being
//! asked anything.

use async_trait::async_trait;
use fronde_hub::Hub;
use fronde_hub::mcp::HubServer;
use fronde_hub::threads::{
    Child, NewChild, ProviderModels, ReadThread, Started, ThreadControl, ThreadError, Waited,
};
use fronde_protocol::{ThreadId, ThreadStatus};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientConfig, Implementation};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Records every call it gets, and starts nothing.
#[derive(Default)]
struct Recording(Mutex<Vec<String>>);

impl Recording {
    fn record(&self, what: &str) {
        self.0.lock().unwrap().push(what.to_owned());
    }
}

#[async_trait]
impl ThreadControl for Recording {
    async fn delegation(&self, _caller: ThreadId) -> Result<(), ThreadError> {
        self.record("delegation");
        Ok(())
    }

    async fn providers(&self) -> Result<Vec<ProviderModels>, ThreadError> {
        self.record("providers");
        Ok(Vec::new())
    }

    async fn start(
        &self,
        _parent: ThreadId,
        _turn: Option<u64>,
        _child: NewChild,
    ) -> Result<Started, ThreadError> {
        self.record("start");
        Ok(Started {
            thread: ThreadId(2),
            title: "x".into(),
            status: ThreadStatus::Running,
            joined: false,
            seq: 1,
        })
    }

    async fn send(&self, _: ThreadId, _: ThreadId, _: String) -> Result<u64, ThreadError> {
        self.record("send");
        Ok(1)
    }

    async fn read(&self, _: ThreadId, _: ThreadId, _: u64) -> Result<ReadThread, ThreadError> {
        self.record("read");
        Err(ThreadError::NoSuchThread)
    }

    async fn children(&self, _: ThreadId) -> Result<(Vec<Child>, usize), ThreadError> {
        self.record("children");
        Ok((Vec::new(), 0))
    }

    async fn cancel(
        &self,
        _: ThreadId,
        _: ThreadId,
        _: Option<String>,
    ) -> Result<bool, ThreadError> {
        self.record("cancel");
        Ok(false)
    }

    async fn wait(
        &self,
        _: ThreadId,
        _: Option<u64>,
        _: ThreadId,
        _: u64,
        _: Duration,
        _: CancellationToken,
    ) -> Result<Waited, ThreadError> {
        self.record("wait");
        Ok(Waited::TimedOut)
    }
}

async fn hub(dir: &std::path::Path, control: Arc<Recording>) -> Arc<Hub> {
    let toml = format!(
        "[server]\ntoken = {{ env = \"UNUSED\" }}\n[data]\ndir = {:?}\ntimezone = \"Europe/Paris\"\nuser = \"alix\"\n[memory]\ngit = false\n",
        dir.display().to_string()
    );
    let config: fronde_hub::config::Config = toml::from_str(&toml).unwrap();
    fronde_hub::private::prepare_data_dir(dir).unwrap();
    let db = fronde_db::open(dir).await.unwrap();
    let control: Arc<dyn ThreadControl> = control;
    Arc::new(
        Hub::start(config, db, vec![], None, Some(control))
            .await
            .unwrap(),
    )
}

fn text(result: &rmcp::model::CallToolResult) -> Value {
    match &result.content[0] {
        rmcp::model::ContentBlock::Text(text) => serde_json::from_str(&text.text).unwrap(),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn an_outside_client_never_reaches_the_threads() {
    let dir = tempfile::tempdir().unwrap();
    let control = Arc::new(Recording::default());
    let hub = hub(dir.path(), control.clone()).await;
    // Registered, for the threads' agents.
    for name in [
        "threads_start",
        "threads_send",
        "threads_read",
        "threads_list",
        "threads_cancel",
    ] {
        assert!(hub.registry.get(name).is_some(), "{name}");
    }
    let (a, b) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        let running = HubServer::new(hub).serve(a).await.unwrap();
        let _ = running.waiting().await;
    });
    // Even named like Fronde's own agents.
    let mut info = ClientConfig::default();
    info.client_info = Implementation::new("Fronde", "1.0");
    let client = info.serve(b).await.unwrap();
    let info = client.peer_info().unwrap();
    // Its instructions do not speak of tools it does not get.
    let instructions = info.instructions.as_deref().unwrap_or_default();
    assert!(!instructions.contains("threads_"), "{instructions}");
    assert!(instructions.contains("memory_add"), "{instructions}");
    let names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect();
    assert!(
        !names.iter().any(|name| name.starts_with("threads_")),
        "{names:?}"
    );
    for (tool, arguments) in [
        ("threads_start", json!({"task": "Résume.", "thread": 1})),
        ("threads_send", json!({"thread": 2, "text": "Salut"})),
        ("threads_read", json!({"thread": 2})),
        ("threads_list", json!({})),
        ("threads_cancel", json!({"thread": 2})),
    ] {
        let direct = client
            .call_tool(
                CallToolRequestParams::new(tool)
                    .with_arguments(arguments.as_object().unwrap().clone()),
            )
            .await
            .unwrap();
        let through = client
            .call_tool(
                CallToolRequestParams::new("use_tool").with_arguments(
                    json!({"name": tool, "arguments": arguments})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        for result in [direct, through] {
            assert_eq!(result.is_error, Some(true), "{tool}");
            let value = text(&result);
            assert!(
                value["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("outside MCP client")),
                "{tool}: {value}"
            );
        }
    }
    assert!(
        control.0.lock().unwrap().is_empty(),
        "the threads were asked: {:?}",
        control.0.lock().unwrap()
    );
}
