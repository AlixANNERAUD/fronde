//! The hub as an MCP server: a view of the registry.

use crate::Hub;
use crate::guide::instructions;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone)]
pub struct HubServer {
    hub: Arc<Hub>,
}

impl HubServer {
    pub fn new(hub: Arc<Hub>) -> Self {
        Self { hub }
    }
}

impl ServerHandler for HubServer {
    fn get_info(&self) -> ServerConfig {
        let text = instructions(
            &self.hub.config.data.user,
            &self.hub.registry.deferred(),
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
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request
            .arguments
            .map(Value::Object)
            .unwrap_or(Value::Object(Default::default()));
        let out = self.hub.registry.call(&request.name, args).await;
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

    async fn start_hub(dir: &std::path::Path) -> Arc<Hub> {
        let toml = format!(
            "[server]\ntoken = {{ env = \"UNUSED\" }}\n[data]\ndir = {:?}\ntimezone = \"Europe/Paris\"\nuser = \"alix\"\n[memory]\ngit = false\n",
            dir.display().to_string()
        );
        let config: crate::config::Config = toml::from_str(&toml).unwrap();
        let hub = Hub::start(config).await.unwrap();
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
