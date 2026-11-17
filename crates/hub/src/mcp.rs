//! The hub as an MCP server: a view of the registry.

use crate::Hub;
use crate::guide::instructions;
use crate::tools::CallContext;
use fronde_requests::Caller;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::Value;
use std::sync::Arc;

/// The stream where the owner sees what tools ask: the chat's conversation.
pub const TOPIC: &str = "conversation";

/// Who holds the bearer token of an HTTP request to `/mcp`, as the server's gate found it and
/// put it in the request's extensions. Without it, a caller is an outside client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpCaller {
    /// Fronde's assistant, with the seq of its running turn's user message.
    Assistant { turn: Option<u64> },
    /// Any other MCP client, holding `server.token`.
    Outside,
}

#[derive(Clone)]
pub struct HubServer {
    hub: Arc<Hub>,
}

impl HubServer {
    pub fn new(hub: Arc<Hub>) -> Self {
        Self { hub }
    }
}

impl HubServer {
    /// rmcp's streamable HTTP server puts the request's `http::request::Parts` in the
    /// context's extensions, and the gate put its [`McpCaller`] in theirs. An outside client is
    /// named from the `clientInfo` it gave at initialisation.
    fn call_context(&self, context: &RequestContext<RoleServer>) -> CallContext {
        let holder = context
            .extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<McpCaller>())
            .copied()
            .unwrap_or(McpCaller::Outside);
        let (caller, turn) = match holder {
            McpCaller::Assistant { turn } => (Caller::Assistant, turn),
            McpCaller::Outside => {
                let name = context
                    .peer
                    .peer_info()
                    .map(|info| info.client_info.name.trim().to_owned())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| "MCP".to_owned());
                (Caller::Mcp(name), None)
            }
        };
        CallContext {
            caller,
            user: self.hub.config.data.user.clone(),
            topic: TOPIC.to_owned(),
            turn,
        }
    }
}

impl ServerHandler for HubServer {
    fn get_info(&self) -> ServerConfig {
        let text = instructions(
            &self.hub.config.data.user,
            &self.hub.registry.all(),
            &self.hub.memory.core_now(),
        );
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("fronde-hub", env!("CARGO_PKG_VERSION")))
            .with_instructions(text)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = self
            .hub
            .registry
            .visible()
            .into_iter()
            .map(|d| {
                let schema = match d.input_schema {
                    Value::Object(m) => m,
                    _ => serde_json::Map::from_iter([(
                        "type".to_string(),
                        Value::String("object".into()),
                    )]),
                };
                Tool::new(d.name, d.description, schema)
            })
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request
            .arguments
            .map(Value::Object)
            .unwrap_or(Value::Object(Default::default()));
        let context = self.call_context(&context);
        let out = self
            .hub
            .registry
            .call_as(&context, &request.name, args)
            .await;
        let content = vec![ContentBlock::text(out.to_text())];
        let result = if out.is_error {
            CallToolResult::error(content)
        } else {
            CallToolResult::success(content)
        };
        Ok(CallToolResponse::Complete(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ToolOutput, typed_tool};
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;

    #[cfg(unix)]
    #[tokio::test]
    async fn start_creates_private_data_dir_and_tightens_existing() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let data = t.path().join("data");
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        start_hub(&data).await;
        for p in ["", "memory", "memory/journal"] {
            assert_eq!(mode(&data.join(p)), 0o700, "{p}");
        }
        assert!(!data.join("index.sqlite").exists());
        assert_eq!(mode(&data.join("fronde.db")), 0o600);
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();
        start_hub(&data).await;
        assert_eq!(mode(&data), 0o700);
    }

    #[tokio::test]
    async fn legacy_index_file_is_deleted() {
        let t = tempfile::tempdir().unwrap();
        let data = t.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        for name in ["index.sqlite", "index.sqlite-journal"] {
            std::fs::write(data.join(name), "an old index").unwrap();
        }
        std::fs::create_dir_all(data.join("memory")).unwrap();
        std::fs::write(data.join("memory/MEMORY.md"), "- has a cat named Tom\n").unwrap();
        let hub = start_hub(&data).await;
        assert!(!data.join("index.sqlite").exists());
        assert!(!data.join("index.sqlite-journal").exists());
        let found = hub
            .registry
            .call("memory_search", serde_json::json!({"query": "cat"}))
            .await;
        assert_eq!(found.value[0]["file"], "MEMORY.md", "{}", found.to_text());
    }

    async fn start_hub(dir: &std::path::Path) -> Arc<Hub> {
        let toml = format!(
            "[server]\ntoken = {{ env = \"UNUSED\" }}\n[data]\ndir = {:?}\ntimezone = \"Europe/Paris\"\nuser = \"alix\"\n[memory]\ngit = false\n",
            dir.display().to_string()
        );
        let config: crate::config::Config = toml::from_str(&toml).unwrap();
        crate::private::prepare_data_dir(dir).unwrap();
        let db = fronde_db::open(dir).await.unwrap();
        let hub = Hub::start(config, db, vec![]).await.unwrap();
        let (mut d, h) = typed_tool(
            "mcphub_secret_tool",
            "Hidden",
            true,
            |_: serde_json::Value| async { ToolOutput::ok(1) },
        );
        d.source = "mcphub".into();
        hub.registry.register(d, h).unwrap();
        Arc::new(hub)
    }

    #[tokio::test]
    async fn hub_tools_use_reserved_prefixes() {
        let dir = tempfile::tempdir().unwrap();
        let hub = start_hub(dir.path()).await;
        let own: Vec<String> = hub
            .registry
            .all()
            .into_iter()
            .filter(|d| d.source == "hub")
            .map(|d| d.name)
            .collect();
        assert!(own.len() >= 5, "{own:?}");
        for name in own {
            assert!(
                crate::config::RESERVED_TOOL_PREFIXES
                    .iter()
                    .any(|p| name.starts_with(p)),
                "{name} needs a reserved prefix"
            );
        }
    }

    async fn connect(hub: Arc<Hub>) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
        let (a, b) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            let running = HubServer::new(hub).serve(a).await.unwrap();
            let _ = running.waiting().await;
        });
        ().serve(b).await.unwrap()
    }

    #[tokio::test]
    async fn instructions_reach_the_client() {
        let dir = tempfile::tempdir().unwrap();
        let hub = start_hub(dir.path()).await;
        hub.memory
            .add(
                crate::memory::MemoryFile::Memory,
                "likes tea",
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        let client = connect(hub).await;
        let info = client.peer_info().unwrap();
        let text = info.instructions.clone().unwrap();
        assert!(text.contains("mcphub: mcphub_secret_tool"), "{text}");
        assert!(text.contains("- likes tea"), "{text}");
        assert_eq!(info.server_info.as_ref().unwrap().name, "fronde-hub");
    }

    #[tokio::test]
    async fn list_tools_shows_visible_only() {
        let dir = tempfile::tempdir().unwrap();
        let client = connect(start_hub(dir.path()).await).await;
        let names: Vec<String> = client
            .list_all_tools()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        for n in ["memory_add", "memory_search", "search_tools", "use_tool"] {
            assert!(names.contains(&n.to_string()), "{n} missing from {names:?}");
        }
        assert!(!names.contains(&"mcphub_secret_tool".to_string()));
    }

    #[tokio::test]
    async fn call_tool_roundtrip_memory_add() {
        let dir = tempfile::tempdir().unwrap();
        let client = connect(start_hub(dir.path()).await).await;
        let args = serde_json::json!({"file": "memory", "text": "owns a bike"})
            .as_object()
            .unwrap()
            .clone();
        let r = client
            .call_tool(CallToolRequestParams::new("memory_add").with_arguments(args))
            .await
            .unwrap();
        assert_ne!(r.is_error, Some(true));
        let text = match &r.content[0] {
            rmcp::model::ContentBlock::Text(t) => t.text.clone(),
            other => panic!("{other:?}"),
        };
        assert!(text.contains("\"file\":\"MEMORY.md\""), "{text}");
        assert!(
            std::fs::read_to_string(dir.path().join("memory/MEMORY.md"))
                .unwrap()
                .contains("owns a bike")
        );
    }

    #[tokio::test]
    async fn outside_caller_is_named_from_its_client_info() {
        let dir = tempfile::tempdir().unwrap();
        let hub = start_hub(dir.path()).await;
        let (d, h) = crate::tools::typed_tool_with_context(
            "test_whoami",
            "Who calls.",
            true,
            |context: CallContext, _: serde_json::Value| async move {
                ToolOutput::ok(serde_json::json!({
                    "caller": context.caller.id(),
                    "user": context.user,
                    "topic": context.topic,
                    "turn": context.turn,
                }))
            },
        );
        hub.registry.register(d, h).unwrap();
        let (a, b) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            let running = HubServer::new(hub).serve(a).await.unwrap();
            let _ = running.waiting().await;
        });
        let mut info = rmcp::model::ClientConfig::default();
        info.client_info = Implementation::new("Codex", "1.0");
        let client = info.serve(b).await.unwrap();
        // Through use_tool too: the context follows the call.
        let args = serde_json::json!({"name": "test_whoami", "arguments": {}})
            .as_object()
            .unwrap()
            .clone();
        let r = client
            .call_tool(CallToolRequestParams::new("use_tool").with_arguments(args))
            .await
            .unwrap();
        let text = match &r.content[0] {
            rmcp::model::ContentBlock::Text(t) => t.text.clone(),
            other => panic!("{other:?}"),
        };
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value["result"],
            serde_json::json!({
                "caller": "mcp:Codex",
                "user": "alix",
                "topic": "conversation",
                "turn": null,
            })
        );
    }

    #[tokio::test]
    async fn unknown_tool_is_tool_error_not_protocol_error() {
        let dir = tempfile::tempdir().unwrap();
        let client = connect(start_hub(dir.path()).await).await;
        let r = client
            .call_tool(CallToolRequestParams::new("nope"))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
    }
}
