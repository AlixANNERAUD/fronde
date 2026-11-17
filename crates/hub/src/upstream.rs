//! Upstream MCP servers: their tools become deferred hub tools; the supervisor reconnects them.
//!
//! A server may ask the user to fill a form (MCP `elicitation/create`, during a tool call, or as
//! an `input_required` round of protocol 2026-07-28, which rmcp's `call_tool` hands to the same
//! handler). With the chat's requests, Fronde advertises form elicitation and shows each form as a
//! card for the admin, marked with the server's name, through the rules of
//! [`fronde_requests::elicitation`]: a form that looks like it asks for a secret is declined
//! unseen. Without them it advertises nothing, and a form that comes anyway is cancelled.

use crate::config::UpstreamConfig;
use crate::source::{Session, ToolSource, unreachable};
use crate::tools::{CallContext, ToolDef, ToolHandler, ToolOutput};
use async_trait::async_trait;
use fronde_protocol::Response;
use fronde_requests::elicitation::{DEFAULT_TIMEOUT, MAX_TIMEOUT, Refused, form};
use fronde_requests::{Ask, Caller, Requests, Resolution};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientConfig, ContentBlock, ElicitRequestParams,
    ElicitResult, ElicitationAction, ElicitationCapability, FormElicitationCapability, MetaObject,
};
use rmcp::service::{RequestContext, RunningService, ServiceError};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{ClientHandler, ErrorData, RoleClient, ServiceExt};
use secrecy::ExposeSecret;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

type Client = RunningService<RoleClient, UpstreamClient>;

/// Where a refusal's reason goes in a declined form's answer, which has no field for it.
pub const REFUSED_META: &str = "fronde/refused";

/// Who answers upstream servers' forms: the admin, on a card in the chat.
#[derive(Clone)]
pub struct Asking {
    pub requests: Arc<Requests>,
    /// The admin, the only one who answers.
    pub user: String,
    /// The stream that shows the card.
    pub topic: String,
    /// How long a card waits: [`DEFAULT_TIMEOUT`] by default, at most [`MAX_TIMEOUT`].
    pub timeout: Duration,
}

impl Asking {
    pub fn new(requests: Arc<Requests>, user: String, topic: String) -> Self {
        Self {
            requests,
            user,
            topic,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// Fronde as an upstream server's client: it answers the server's forms through [`Asking`].
#[derive(Clone)]
struct UpstreamClient {
    source: String,
    asking: Option<Asking>,
}

impl ClientHandler for UpstreamClient {
    fn get_info(&self) -> ClientConfig {
        let mut info = ClientConfig::default();
        if self.asking.is_some() {
            info.capabilities.elicitation =
                Some(ElicitationCapability::new().with_form(FormElicitationCapability::new()));
        }
        info
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        Ok(self.elicit(request, context.ct).await)
    }
}

impl UpstreamClient {
    /// The server's form as a card for the admin, and the admin's answer: accept with the form's
    /// content, decline when refused, cancel when nobody answered (it expired, the server
    /// cancelled its request, the call it belongs to was given up).
    async fn elicit(
        &self,
        request: ElicitRequestParams,
        cancelled: CancellationToken,
    ) -> ElicitResult {
        let source = &self.source;
        let Some(asking) = &self.asking else {
            tracing::warn!("{source}: sent a form while Fronde shows none: cancelled");
            return ElicitResult::new(ElicitationAction::Cancel);
        };
        let ElicitRequestParams::FormElicitationParams {
            message,
            requested_schema,
            ..
        } = request
        else {
            tracing::warn!(
                "{source}: sent a URL elicitation, which Fronde does not show: cancelled"
            );
            return ElicitResult::new(ElicitationAction::Cancel);
        };
        let elicited = match serde_json::to_value(&requested_schema)
            .map_err(|error| Refused {
                reason: format!("the requested schema cannot be read: {error}"),
                secret: false,
            })
            .and_then(|schema| form(&message, &schema))
        {
            Ok(elicited) => elicited,
            Err(refused) => {
                tracing::warn!("{source}: refused its form: {refused}");
                return refusal(&refused);
            }
        };
        let ask = Ask {
            topic: asking.topic.clone(),
            turn: None,
            caller: Caller::Source(source.clone()),
            client_request_id: None,
            card: elicited.card.clone(),
            for_user: asking.user.clone(),
            admin_only: true,
            timeout: asking.timeout.min(MAX_TIMEOUT),
        };
        // Given up when the server cancels its request, or when this future is dropped (the
        // call that waits on an `input_required` round was given up): the card then closes.
        // The card is asked and awaited in a task of its own, so that a drop in the middle of
        // showing it cannot leave it open.
        let gone = cancelled.child_token();
        let _dropped = gone.clone().drop_guard();
        let requests = asking.requests.clone();
        let source = source.clone();
        let shown = tokio::spawn(async move {
            let waiter = match requests.ask(ask).await {
                Ok(waiter) => waiter,
                Err(error) => {
                    tracing::error!("{source}: cannot show its form: {error:#}");
                    return Resolution::Cancelled;
                }
            };
            let id = waiter.id();
            tokio::select! {
                biased;
                resolution = waiter.wait() => resolution,
                () = gone.cancelled() => {
                    if let Err(error) = requests.cancel(id).await {
                        tracing::error!("cannot cancel request {id}: {error:#}");
                    }
                    Resolution::Cancelled
                }
            }
        });
        let resolution = shown.await.unwrap_or(Resolution::Cancelled);
        match resolution {
            Resolution::Answered(Response::Form { values }) => {
                ElicitResult::new(ElicitationAction::Accept)
                    .with_content(Value::Object(elicited.content(&values)))
            }
            Resolution::Declined => ElicitResult::new(ElicitationAction::Decline),
            Resolution::Answered(_)
            | Resolution::Saved(_)
            | Resolution::Expired
            | Resolution::Cancelled => ElicitResult::new(ElicitationAction::Cancel),
        }
    }
}

/// The answer to a form Fronde refuses to show: a decline, with the reason in `_meta`.
fn refusal(refused: &Refused) -> ElicitResult {
    let mut meta = serde_json::Map::new();
    meta.insert(REFUSED_META.into(), Value::String(refused.reason.clone()));
    ElicitResult::new(ElicitationAction::Decline).with_meta(MetaObject(meta))
}

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
    /// A call its caller gives up is given up upstream too, so that a form the server shows
    /// meanwhile closes.
    async fn call_as(&self, context: &CallContext, args: Value) -> ToolOutput {
        tokio::select! {
            output = self.call(args) => output,
            () = context.cancelled.cancelled() => ToolOutput::error(format!(
                "the call to {} was cancelled; call it again if you still need it",
                self.tool
            )),
        }
    }

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
async fn connect(
    cfg: &UpstreamConfig,
    handler: UpstreamClient,
) -> anyhow::Result<(Client, Vec<rmcp::model::Tool>)> {
    let attempt = async {
        let client = open(cfg, handler).await?;
        let tools = client.list_all_tools().await?;
        anyhow::Ok((client, tools))
    };
    tokio::time::timeout(CONNECT_TIMEOUT, attempt)
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {}s", CONNECT_TIMEOUT.as_secs()))?
}

async fn open(cfg: &UpstreamConfig, handler: UpstreamClient) -> anyhow::Result<Client> {
    if let Some(url) = &cfg.url {
        let mut config = StreamableHttpClientTransportConfig::with_uri(url.as_str());
        if let Some(t) = &cfg.token {
            config = config.auth_header(t.resolve()?.expose_secret());
        }
        Ok(handler
            .serve(StreamableHttpClientTransport::from_config(config))
            .await?)
    } else if let Some([program, args @ ..]) = cfg.command.as_deref() {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        Ok(handler.serve(TokioChildProcess::new(cmd)?).await?)
    } else {
        anyhow::bail!("mcp {:?}: set exactly one of url or command", cfg.name)
    }
}

/// An upstream MCP server: its tools become deferred hub tools named `{name}_{tool}`.
pub struct Upstream {
    cfg: UpstreamConfig,
    asking: Option<Asking>,
}

impl Upstream {
    /// A server whose forms are cancelled: Fronde shows them nowhere.
    pub fn new(cfg: UpstreamConfig) -> Self {
        Self { cfg, asking: None }
    }

    /// A server whose forms are shown as cards, as `asking` says.
    pub fn asking(cfg: UpstreamConfig, asking: Option<Asking>) -> Self {
        Self { cfg, asking }
    }
}

#[async_trait]
impl ToolSource for Upstream {
    fn name(&self) -> &str {
        &self.cfg.name
    }

    /// Connect and list the tools; the session ends when the connection is lost.
    async fn start(&self) -> anyhow::Result<Session> {
        let handler = UpstreamClient {
            source: self.cfg.name.clone(),
            asking: self.asking.clone(),
        };
        let (client, tools) = connect(&self.cfg, handler).await?;
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
        let handler = UpstreamClient {
            source: "mcphub".into(),
            asking: None,
        };
        let Err(e) = connect(&cfg(addr), handler).await else {
            panic!("connected to a silent server");
        };
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

    /// Elicitations from upstream servers, with the chat's requests behind them.
    mod forms {
        use super::*;
        use async_trait::async_trait;
        use fronde_protocol::{Card, ChoiceAnswer, Field, FieldValue, Outcome, RequestId};
        use fronde_requests::{Journal, Refusal, User};
        use rmcp::model::*;
        use rmcp::service::{RequestContext, serve_directly};
        use rmcp::transport::streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        };
        use rmcp::{ErrorData, RoleServer, ServerHandler};
        use tokio::sync::mpsc;

        const WAIT: Duration = Duration::from_secs(10);

        /// What the journal saw: a card shown, or a request closed.
        #[derive(Debug)]
        enum Seen {
            Shown(RequestId, Card, Caller),
            Closed(RequestId, Outcome),
        }

        struct Seeing(mpsc::UnboundedSender<Seen>);

        #[async_trait]
        impl Journal for Seeing {
            async fn announce(
                &self,
                _topic: &str,
                id: RequestId,
                card: &Card,
                caller: &Caller,
            ) -> anyhow::Result<u64> {
                let _ = self.0.send(Seen::Shown(id, card.clone(), caller.clone()));
                Ok(id)
            }

            async fn close(
                &self,
                _topic: &str,
                id: RequestId,
                outcome: &Outcome,
                _by: Option<&str>,
            ) -> anyhow::Result<()> {
                let _ = self.0.send(Seen::Closed(id, outcome.clone()));
                Ok(())
            }
        }

        struct Bench {
            requests: Arc<Requests>,
            seen: mpsc::UnboundedReceiver<Seen>,
            _dir: tempfile::TempDir,
        }

        impl Bench {
            async fn new() -> Self {
                let dir = tempfile::tempdir().unwrap();
                let db = fronde_db::open(dir.path()).await.unwrap();
                let (send, seen) = mpsc::unbounded_channel();
                let requests = Requests::open(db, Arc::new(Seeing(send))).await.unwrap();
                Self {
                    requests,
                    seen,
                    _dir: dir,
                }
            }

            fn asking(&self, timeout: Duration) -> Asking {
                Asking {
                    timeout,
                    ..Asking::new(self.requests.clone(), "alix".into(), "conversation".into())
                }
            }

            async fn next(&mut self) -> Seen {
                tokio::time::timeout(WAIT, self.seen.recv())
                    .await
                    .expect("nothing seen")
                    .unwrap()
            }

            async fn card(&mut self) -> (RequestId, Card, Caller) {
                match self.next().await {
                    Seen::Shown(id, card, caller) => (id, card, caller),
                    other => panic!("not a card: {other:?}"),
                }
            }

            async fn closed(&mut self) -> (RequestId, Outcome) {
                match self.next().await {
                    Seen::Closed(id, outcome) => (id, outcome),
                    other => panic!("not a close: {other:?}"),
                }
            }
        }

        fn admin() -> User {
            User {
                name: "alix".into(),
                admin: true,
            }
        }

        fn booking() -> ElicitationSchema {
            serde_json::from_value(json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "title": "Nom"},
                    "nights": {"type": "integer", "title": "Nuits", "minimum": 1},
                    "room": {"type": "string", "title": "Chambre", "oneOf": [
                        {"const": "single", "title": "Simple"},
                        {"const": "double", "title": "Double"},
                    ]},
                },
                "required": ["name", "nights"],
            }))
            .unwrap()
        }

        fn form(message: &str, schema: ElicitationSchema) -> ElicitRequestParams {
            ElicitRequestParams::FormElicitationParams {
                meta: None,
                message: message.into(),
                requested_schema: schema,
            }
        }

        /// A server whose tools ask Fronde to fill a form during the call and return what
        /// they got: `book` a booking, `secret` an API token, `brief` a booking it gives up on
        /// after 300 ms; `forms` says whether Fronde said it takes forms.
        #[derive(Clone)]
        struct Asker;

        impl ServerHandler for Asker {
            fn get_info(&self) -> ServerConfig {
                ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            }

            async fn list_tools(
                &self,
                _r: Option<PaginatedRequestParams>,
                _c: RequestContext<RoleServer>,
            ) -> Result<ListToolsResult, ErrorData> {
                let schema = json!({"type": "object"}).as_object().unwrap().clone();
                let tool = |name: &'static str| Tool::new(name, "Ask a form", schema.clone());
                Ok(ListToolsResult::with_all_items(vec![
                    tool("book"),
                    tool("secret"),
                    tool("brief"),
                    tool("forms"),
                ]))
            }

            async fn call_tool(
                &self,
                r: CallToolRequestParams,
                c: RequestContext<RoleServer>,
            ) -> Result<CallToolResponse, ErrorData> {
                let asked = match r.name.as_ref() {
                    "forms" => {
                        let forms = c
                            .peer
                            .peer_info()
                            .and_then(|info| info.capabilities.elicitation.clone())
                            .is_some_and(|elicitation| elicitation.form.is_some());
                        return Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                            ContentBlock::text(forms.to_string()),
                        ])));
                    }
                    "book" => c.peer.create_elicitation(form("Réserver", booking())).await,
                    "secret" => {
                        let schema = serde_json::from_value(json!({
                            "type": "object",
                            "properties": {"api_token": {"type": "string"}},
                        }))
                        .unwrap();
                        c.peer.create_elicitation(form("Ta clé", schema)).await
                    }
                    _ => {
                        c.peer
                            .create_elicitation_with_timeout(
                                form("Vite", booking()),
                                Some(Duration::from_millis(300)),
                            )
                            .await
                    }
                };
                let text = match asked {
                    Ok(result) => serde_json::to_string(&result).unwrap(),
                    Err(error) => format!("error: {error}"),
                };
                Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                    ContentBlock::text(text),
                ])))
            }
        }

        /// The asking server over HTTP, connected through the supervisor as the hub does.
        async fn connected(asking: Option<Asking>) -> Arc<Registry> {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let service = StreamableHttpService::new(
                || Ok(Asker),
                Arc::new(LocalSessionManager::default()),
                StreamableHttpServerConfig::default(),
            );
            let app = axum::Router::new().nest_service("/mcp", service);
            tokio::spawn(async move { axum::serve(listener, app).await });
            let reg = Arc::new(Registry::new());
            Supervisor::new(reg.clone()).spawn(Arc::new(Upstream::asking(cfg(addr), asking)));
            assert!(wait_for(&reg, "mcphub_book", 10).await);
            reg
        }

        fn call(reg: &Arc<Registry>, tool: &str) -> tokio::task::JoinHandle<Value> {
            let (reg, tool) = (reg.clone(), tool.to_owned());
            tokio::spawn(async move {
                let out = reg.call(&tool, json!({})).await;
                assert!(!out.is_error, "{}", out.to_text());
                serde_json::from_str(out.value.as_str().unwrap()).unwrap_or(out.value)
            })
        }

        async fn result(call: tokio::task::JoinHandle<Value>) -> Value {
            tokio::time::timeout(WAIT, call).await.unwrap().unwrap()
        }

        fn fill(pairs: &[(&str, FieldValue)]) -> fronde_protocol::Response {
            fronde_protocol::Response::Form {
                values: pairs
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), value.clone()))
                    .collect(),
            }
        }

        #[tokio::test]
        async fn upstream_elicitation_reaches_a_card_and_back() {
            let mut bench = Bench::new().await;
            let reg = connected(Some(bench.asking(DEFAULT_TIMEOUT))).await;
            let book = call(&reg, "mcphub_book");
            let (id, card, caller) = bench.card().await;
            // For the admin, marked with the server's name.
            assert_eq!(caller, Caller::Source("mcphub".into()));
            let Card::Form {
                message, fields, ..
            } = &card
            else {
                panic!("{card:?}");
            };
            assert_eq!(message, "Réserver");
            let keys: Vec<&str> = fields.iter().map(Field::key).collect();
            assert_eq!(keys, ["name", "nights", "room"]);
            let answer = fill(&[
                ("name", FieldValue::Text("Alix".into())),
                ("nights", FieldValue::Number(2.0)),
                (
                    "room",
                    FieldValue::Choice(ChoiceAnswer {
                        selected: vec!["Double".into()],
                        ..ChoiceAnswer::default()
                    }),
                ),
            ]);
            // Only the admin answers.
            let member = User {
                name: "alix".into(),
                admin: false,
            };
            assert_eq!(
                bench.requests.answer(id, answer.clone(), &member).await,
                Err(Refusal::NotYours)
            );
            bench.requests.answer(id, answer, &admin()).await.unwrap();
            assert_eq!(
                result(book).await,
                json!({"action": "accept", "content": {"name": "Alix", "nights": 2, "room": "double"}})
            );

            // A refusal reaches the server as a decline.
            let book = call(&reg, "mcphub_book");
            let (_, _) = bench.closed().await;
            let (id, ..) = bench.card().await;
            bench
                .requests
                .answer(id, fronde_protocol::Response::Decline, &admin())
                .await
                .unwrap();
            assert_eq!(result(book).await, json!({"action": "decline"}));
        }

        #[tokio::test]
        async fn upstream_sensitive_form_is_declined() {
            let mut bench = Bench::new().await;
            let reg = connected(Some(bench.asking(DEFAULT_TIMEOUT))).await;
            let answer = result(call(&reg, "mcphub_secret")).await;
            assert_eq!(answer["action"], "decline");
            assert!(
                answer["_meta"][REFUSED_META]
                    .as_str()
                    .unwrap()
                    .contains("takes no secret in a form"),
                "{answer}"
            );
            // No card was shown.
            assert!(bench.seen.try_recv().is_err());
        }

        #[tokio::test]
        async fn upstream_elicitation_expires_and_follows_the_servers_cancel() {
            let mut bench = Bench::new().await;
            let reg = connected(Some(bench.asking(Duration::from_millis(200)))).await;
            let book = call(&reg, "mcphub_book");
            let (id, ..) = bench.card().await;
            assert_eq!(bench.closed().await, (id, Outcome::Expired));
            assert_eq!(result(book).await, json!({"action": "cancel"}));

            // The server gives up its own request: the card closes.
            let mut bench = Bench::new().await;
            let reg = connected(Some(bench.asking(DEFAULT_TIMEOUT))).await;
            let brief = call(&reg, "mcphub_brief");
            let (id, ..) = bench.card().await;
            assert_eq!(bench.closed().await, (id, Outcome::Cancelled));
            let answer = result(brief).await;
            assert!(answer.as_str().unwrap().starts_with("error:"), "{answer}");
            assert_eq!(
                bench
                    .requests
                    .answer(id, fronde_protocol::Response::Decline, &admin())
                    .await,
                Err(Refusal::Closed)
            );
        }

        #[tokio::test]
        async fn a_call_given_up_is_given_up_upstream() {
            let mut bench = Bench::new().await;
            let reg = connected(Some(bench.asking(DEFAULT_TIMEOUT))).await;
            let context = CallContext {
                caller: Caller::Assistant,
                user: "alix".into(),
                topic: "conversation".into(),
                turn: Some(1),
                cancelled: CancellationToken::new(),
            };
            let call = tokio::spawn({
                let (reg, context) = (reg.clone(), context.clone());
                async move { reg.call_as(&context, "mcphub_book", json!({})).await }
            });
            bench.card().await;
            context.cancelled.cancel();
            let out = tokio::time::timeout(WAIT, call).await.unwrap().unwrap();
            assert!(out.is_error);
            assert!(out.to_text().contains("was cancelled"), "{}", out.to_text());
        }

        #[tokio::test]
        async fn without_requests_forms_are_not_advertised() {
            let reg = connected(None).await;
            assert_eq!(result(call(&reg, "mcphub_forms")).await, json!(false));
            // A form sent anyway is cancelled: nobody could see it.
            assert_eq!(
                result(call(&reg, "mcphub_book")).await,
                json!({"action": "cancel"})
            );
            let bench = Bench::new().await;
            let reg = connected(Some(bench.asking(DEFAULT_TIMEOUT))).await;
            assert_eq!(result(call(&reg, "mcphub_forms")).await, json!(true));
        }

        /// A server of protocol 2026-07-28, which asks through an `input_required` result: the
        /// first call gets a form to fill, the second, with its answer, the booked name.
        #[derive(Clone)]
        struct Rounds;

        impl ServerHandler for Rounds {
            fn get_info(&self) -> ServerConfig {
                let mut info =
                    ServerConfig::new(ServerCapabilities::builder().enable_tools().build());
                info.protocol_version = ProtocolVersion::V_2026_07_28;
                info
            }

            async fn call_tool(
                &self,
                r: CallToolRequestParams,
                _c: RequestContext<RoleServer>,
            ) -> Result<CallToolResponse, ErrorData> {
                if r.request_state.is_none() {
                    let mut asks = InputRequests::new();
                    asks.insert(
                        "booking".into(),
                        InputRequest::Elicitation(ElicitRequest::new(form("Réserver", booking()))),
                    );
                    return Ok(CallToolResponse::InputRequired(InputRequiredResult::new(
                        Some(asks),
                        Some("asked".into()),
                    )));
                }
                let answer = r
                    .input_responses
                    .and_then(|responses| responses.get("booking").cloned())
                    .unwrap_or_default();
                Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                    ContentBlock::text(answer.to_string()),
                ])))
            }
        }

        /// A client of `Rounds`, in process, on protocol 2026-07-28.
        fn rounds(asking: Asking) -> Client {
            let (server, client) = tokio::io::duplex(8192);
            let mut peer = ClientConfig::default();
            peer.protocol_version = ProtocolVersion::V_2026_07_28;
            tokio::spawn(async move {
                let running = serve_directly::<RoleServer, _, _, _, _>(Rounds, server, Some(peer));
                let _ = running.waiting().await;
            });
            let mut info = ServerConfig::new(ServerCapabilities::builder().enable_tools().build());
            info.protocol_version = ProtocolVersion::V_2026_07_28;
            let handler = UpstreamClient {
                source: "rounds".into(),
                asking: Some(asking),
            };
            serve_directly::<RoleClient, _, _, _, _>(handler, client, Some(info.into()))
        }

        #[tokio::test]
        async fn mrtr_round_uses_the_same_handler() {
            let mut bench = Bench::new().await;
            let client = Arc::new(rounds(bench.asking(DEFAULT_TIMEOUT)));
            let call = tokio::spawn({
                let client = client.clone();
                async move { client.call_tool(CallToolRequestParams::new("book")).await }
            });
            let (id, _, caller) = bench.card().await;
            assert_eq!(caller, Caller::Source("rounds".into()));
            let answer = fill(&[
                ("name", FieldValue::Text("Alix".into())),
                ("nights", FieldValue::Number(1.0)),
            ]);
            bench.requests.answer(id, answer, &admin()).await.unwrap();
            let done = tokio::time::timeout(WAIT, call)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let text = &done.content[0].as_text().unwrap().text;
            assert_eq!(
                serde_json::from_str::<Value>(text).unwrap(),
                json!({"action": "accept", "content": {"name": "Alix", "nights": 1}})
            );

            // A call given up while its form is shown closes the card.
            let call = tokio::spawn({
                let client = client.clone();
                async move { client.call_tool(CallToolRequestParams::new("book")).await }
            });
            let (_, _) = bench.closed().await;
            let (id, ..) = bench.card().await;
            call.abort();
            assert_eq!(bench.closed().await, (id, Outcome::Cancelled));
        }
    }
}
