//! Upstream MCP servers: their tools become deferred hub tools; the supervisor reconnects them.

use crate::config::UpstreamConfig;
use crate::source::{Session, ToolSource, unreachable};
use crate::tools::{ToolDef, ToolHandler, ToolOutput};
use async_trait::async_trait;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::{RunningService, ServiceError};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use secrecy::ExposeSecret;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

type Client = RunningService<RoleClient, ()>;

/// `{server}_{tool}`, lowercase, `[^a-z0-9_]` → `_`, at most 64 characters.
pub fn tool_name(server: &str, tool: &str) -> String {
    let mut name: String = format!("{server}_{tool}")
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    name.truncate(64);
    name
}

struct Link {
    name: String,
    client: Client,
    broken: Notify,
}

struct Forward {
    link: Arc<Link>,
    tool: String,
}

fn to_output(r: CallToolResult) -> ToolOutput {
    let is_error = r.is_error.unwrap_or(false);
    let value = match r.structured_content {
        Some(v) if !v.is_null() => v,
        _ => {
            let parts: Vec<String> = r
                .content
                .iter()
                .map(|c| match c {
                    ContentBlock::Text(t) => t.text.clone(),
                    ContentBlock::Image(_) => "[image omitted]".into(),
                    ContentBlock::Audio(_) => "[audio omitted]".into(),
                    _ => "[resource omitted]".into(),
                })
                .collect();
            Value::String(parts.join("\n"))
        }
    };
    ToolOutput {
        value,
        next: None,
        is_error,
    }
}

#[async_trait]
impl ToolHandler for Forward {
    async fn call(&self, args: Value) -> ToolOutput {
        let mut params = CallToolRequestParams::new(self.tool.clone());
        if let Value::Object(map) = args {
            params = params.with_arguments(map);
        }
        match self.link.client.call_tool(params).await {
            Ok(r) => to_output(r),
            // The server answered with an error: report it, the link is fine.
            Err(ServiceError::McpError(e)) => ToolOutput::error(e.message.to_string()),
            Err(e) => {
                tracing::warn!("{}: call to {} failed: {e}", self.link.name, self.tool);
                self.link.broken.notify_one();
                unreachable(&self.link.name)
            }
        }
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Connect and list tools, giving up after `CONNECT_TIMEOUT` (a silent server must not block retries).
async fn connect(cfg: &UpstreamConfig) -> anyhow::Result<(Client, Vec<rmcp::model::Tool>)> {
    let attempt = async {
        let client = open(cfg).await?;
        let tools = client.list_all_tools().await?;
        anyhow::Ok((client, tools))
    };
    tokio::time::timeout(CONNECT_TIMEOUT, attempt)
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {}s", CONNECT_TIMEOUT.as_secs()))?
}

async fn open(cfg: &UpstreamConfig) -> anyhow::Result<Client> {
    if let Some(url) = &cfg.url {
        let mut config = StreamableHttpClientTransportConfig::with_uri(url.as_str());
        if let Some(t) = &cfg.token {
            config = config.auth_header(t.resolve()?.expose_secret());
        }
        Ok(
            ().serve(StreamableHttpClientTransport::from_config(config))
                .await?,
        )
    } else if let Some([program, args @ ..]) = cfg.command.as_deref() {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        Ok(().serve(TokioChildProcess::new(cmd)?).await?)
    } else {
        anyhow::bail!("mcp {:?}: set exactly one of url or command", cfg.name)
    }
}

/// An upstream MCP server: its tools become deferred hub tools named `{name}_{tool}`.
pub struct Upstream {
    cfg: UpstreamConfig,
}

impl Upstream {
    pub fn new(cfg: UpstreamConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl ToolSource for Upstream {
    fn name(&self) -> &str {
        &self.cfg.name
    }

    /// Connect and list the tools; the session ends when the connection is lost.
    async fn start(&self) -> anyhow::Result<Session> {
        let (client, tools) = connect(&self.cfg).await?;
        let name = self.cfg.name.clone();
        let link = Arc::new(Link {
            name: name.clone(),
            client,
            broken: Notify::new(),
        });
        let tools = tools
            .into_iter()
            .map(|t| {
                let def = ToolDef {
                    name: tool_name(&name, &t.name),
                    description: t.description.as_deref().unwrap_or_default().to_string(),
                    input_schema: Value::Object((*t.input_schema).clone()),
                    deferred: true,
                    source: name.clone(),
                };
                let handler: Arc<dyn ToolHandler> = Arc::new(Forward {
                    link: link.clone(),
                    tool: t.name.to_string(),
                });
                (def, handler)
            })
            .collect();
        tracing::info!("{name}: connected");
        Ok(Session::until(tools, async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(5)) => if link.client.is_closed() { break },
                    _ = link.broken.notified() => break,
                }
            }
            tracing::warn!("{}: disconnected", link.name);
        }))
    }
}

#[cfg(test)]
pub(crate) mod test_server {
    //! A tiny MCP server with one `echo` tool, served over HTTP, that can be switched off.
    use rmcp::model::*;
    use rmcp::service::RequestContext;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    };
    use rmcp::{ErrorData, RoleServer, ServerHandler};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Clone)]
    pub struct Echo;

    impl ServerHandler for Echo {
        fn get_info(&self) -> ServerConfig {
            ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
        }

        async fn list_tools(
            &self,
            _r: Option<PaginatedRequestParams>,
            _c: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, ErrorData> {
            let schema =
                serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}});
            let schema = schema.as_object().unwrap().clone();
            Ok(ListToolsResult::with_all_items(vec![Tool::new(
                "echo",
                "Echo text back to the caller",
                schema,
            )]))
        }

        async fn call_tool(
            &self,
            r: CallToolRequestParams,
            _c: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, ErrorData> {
            let Some(text) = r
                .arguments
                .and_then(|a| a.get("text").and_then(|t| t.as_str()).map(str::to_string))
            else {
                return Err(ErrorData::invalid_params("missing text argument", None));
            };
            Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                ContentBlock::text(text),
            ])))
        }
    }

    /// Serve on `listener`; returns a switch: set it to true to answer 503 to everything.
    pub fn serve(listener: tokio::net::TcpListener) -> Arc<AtomicBool> {
        let down = Arc::new(AtomicBool::new(false));
        let svc = StreamableHttpService::new(
            || Ok(Echo),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );
        let flag = down.clone();
        let app = axum::Router::new()
            .nest_service("/mcp", svc)
            .layer(axum::middleware::from_fn(
                move |req: axum::extract::Request, next: axum::middleware::Next| {
                    let flag = flag.clone();
                    async move {
                        if flag.load(Ordering::SeqCst) {
                            axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
                        } else {
                            next.run(req).await
                        }
                    }
                },
            ));
        tokio::spawn(async move { axum::serve(listener, app).await });
        down
    }

    use axum::response::IntoResponse;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Supervisor;
    use crate::tools::Registry;
    use serde_json::json;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    fn cfg(addr: std::net::SocketAddr) -> UpstreamConfig {
        UpstreamConfig {
            name: "mcphub".into(),
            url: Some(format!("http://{addr}/mcp").parse().unwrap()),
            command: None,
            token: None,
        }
    }

    /// Through the supervisor, as the hub connects them.
    fn spawn_upstream(cfg: UpstreamConfig, registry: Arc<Registry>) {
        Supervisor::new(registry).spawn(Arc::new(Upstream::new(cfg)));
    }

    async fn wait_for(reg: &Registry, name: &str, secs: u64) -> bool {
        for _ in 0..secs * 10 {
            if reg.deferred_names().iter().any(|n| n == name) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    #[test]
    fn tool_name_sanitizes() {
        assert_eq!(tool_name("hub", "Add Item!"), "hub_add_item_");
        assert_eq!(tool_name("hub", &"x".repeat(100)).len(), 64);
    }

    #[tokio::test]
    async fn registers_deferred_and_calls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        test_server::serve(listener);
        let reg = Arc::new(Registry::new());
        spawn_upstream(cfg(addr), reg.clone());
        assert!(wait_for(&reg, "mcphub_echo", 10).await);
        let def = reg.get("mcphub_echo").unwrap();
        assert_eq!((def.source.as_str(), def.deferred), ("mcphub", true));
        let out = reg.call("mcphub_echo", json!({"text": "hello"})).await;
        assert!(!out.is_error, "{}", out.to_text());
        assert_eq!(out.value, json!("hello"));
    }

    #[tokio::test]
    async fn upstream_error_is_reported_and_link_kept() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        test_server::serve(listener);
        let reg = Arc::new(Registry::new());
        spawn_upstream(cfg(addr), reg.clone());
        assert!(wait_for(&reg, "mcphub_echo", 10).await);
        let bad = reg.call("mcphub_echo", json!({})).await;
        assert!(bad.is_error);
        assert!(
            bad.to_text().contains("missing text argument"),
            "{}",
            bad.to_text()
        );
        let ok = reg.call("mcphub_echo", json!({"text": "still here"})).await;
        assert!(
            !ok.is_error,
            "link must stay up after an application error: {}",
            ok.to_text()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn connect_times_out_on_silent_server() {
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = silent.local_addr().unwrap();
        let e = connect(&cfg(addr)).await.unwrap_err();
        assert!(format!("{e:#}").contains("timed out"), "{e:#}");
        drop(silent);
    }

    #[tokio::test]
    async fn upstream_down_at_start_retries() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let reg = Arc::new(Registry::new());
        spawn_upstream(cfg(addr), reg.clone());
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(reg.deferred_names().is_empty());
        test_server::serve(tokio::net::TcpListener::bind(addr).await.unwrap());
        assert!(wait_for(&reg, "mcphub_echo", 10).await);
    }

    #[tokio::test]
    async fn call_after_disconnect_explains() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let down = test_server::serve(listener);
        let reg = Arc::new(Registry::new());
        spawn_upstream(cfg(addr), reg.clone());
        assert!(wait_for(&reg, "mcphub_echo", 10).await);
        down.store(true, Ordering::SeqCst);
        let out = reg.call("mcphub_echo", json!({"text": "hello"})).await;
        assert!(out.is_error);
        assert!(
            out.to_text().contains("mcphub is unreachable"),
            "{}",
            out.to_text()
        );
        down.store(false, Ordering::SeqCst);
        for _ in 0..100 {
            if !reg
                .call("mcphub_echo", json!({"text": "back"}))
                .await
                .is_error
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("did not reconnect");
    }
}
