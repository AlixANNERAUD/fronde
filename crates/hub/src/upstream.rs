//! Upstream MCP servers: their tools become deferred hub tools; the supervisor reconnects them.
//!
//! A server may ask the user to fill a form (MCP `elicitation/create`, during a tool call, or as
//! an `input_required` round of protocol 2026-07-28, which rmcp's `call_tool` hands to the same
//! handler). With the chat's requests, Fronde advertises form elicitation and shows each form as a
//! card for the admin, marked with the server's name, through the rules of
//! [`fronde_requests::elicitation`]: a form that looks like it asks for a secret is declined
//! unseen. Without them it advertises nothing, and a form that comes anyway is cancelled.
//!
//! A server asks only on behalf of a call: a form is shown only while a call to that server is
//! in flight, one card at a time per server, and the card closes when the last call ends, when
//! the server cancels its request, or with the assistant's turn when the assistant alone is
//! calling. Anything else is cancelled unseen, so that a server cannot fill the chat with cards.

use crate::config::{Secret, UpstreamConfig};
use crate::origin::Origin;
use crate::source::{Session, ToolSource, unreachable};
use crate::stored::StoredSecrets;
use crate::tools::{CallContext, ToolDef, ToolHandler, ToolOutput};
use anyhow::Context;
use async_trait::async_trait;
use fronde_db::master_key::Backends;
use fronde_protocol::text::{Notes, line};
use fronde_protocol::{CallBy, Card, Response};
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
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
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

/// The longest a caller's name is on a card.
const MAX_CALLER: usize = 40;

/// At most one warning a minute per server for the forms it is refused: a server sending forms
/// in a loop must not fill the log either.
const WARN_EVERY: Duration = Duration::from_secs(60);

/// One server's calls in flight, who made them, and its one open card.
#[derive(Default)]
struct Calls {
    state: Mutex<CallsState>,
}

#[derive(Default)]
struct CallsState {
    next: u64,
    flying: Vec<Flying>,
    /// The open card's number and what closes it.
    card: Option<(u64, CancellationToken)>,
    /// When a refusal was last logged, and how many were not since.
    warned: Option<Instant>,
    quiet: u64,
}

/// A call in flight: who made it, and for the assistant its turn and the stream it runs in.
struct Flying {
    id: u64,
    by: Option<CallBy>,
    turn: Option<(String, u64)>,
}

/// What a new card gets: what closes it, whose calls it serves, and the assistant's turn (its
/// stream and seq) when it alone is calling.
struct Slot {
    calls: Arc<Calls>,
    id: u64,
    gone: CancellationToken,
    during: Vec<CallBy>,
    turn: Option<(String, u64)>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut state = self.calls.state();
        if state.card.as_ref().is_some_and(|(id, _)| *id == self.id) {
            state.card = None;
        }
    }
}

/// A call in flight until dropped; the last one to end closes the server's open card.
struct InFlight {
    calls: Arc<Calls>,
    id: u64,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let mut state = self.calls.state();
        state.flying.retain(|flying| flying.id != self.id);
        if state.flying.is_empty()
            && let Some((_, gone)) = state.card.take()
        {
            gone.cancel();
        }
    }
}

impl Calls {
    fn state(&self) -> MutexGuard<'_, CallsState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts a call in flight, made on behalf of `context` when it is known.
    fn start(self: &Arc<Self>, context: Option<&CallContext>) -> InFlight {
        let mut state = self.state();
        state.next += 1;
        let id = state.next;
        let (by, turn) = match context.map(|context| (&context.caller, context)) {
            Some((Caller::Thread(_), context)) => (
                Some(CallBy::Assistant),
                context.turn.map(|turn| (context.topic.clone(), turn)),
            ),
            Some((Caller::Mcp(name) | Caller::Source(name) | Caller::Unknown(name), _)) => {
                let name = line(name, MAX_CALLER, &mut Notes::default());
                (Some(CallBy::Client { name }), None)
            }
            None => (None, None),
        };
        state.flying.push(Flying { id, by, turn });
        InFlight {
            calls: self.clone(),
            id,
        }
    }

    /// The slot of a new card, closed with `server`'s cancel too; or why the form is not shown:
    /// no call is in flight, or a card is already open.
    fn open(self: &Arc<Self>, server: &CancellationToken) -> Result<Slot, &'static str> {
        let mut state = self.state();
        if state.flying.is_empty() {
            return Err("asked while no call to it was in flight");
        }
        if state.card.is_some() {
            return Err("asked while one of its forms was still open");
        }
        state.next += 1;
        let id = state.next;
        let gone = server.child_token();
        state.card = Some((id, gone.clone()));
        let mut during: Vec<CallBy> = Vec::new();
        for by in state.flying.iter().filter_map(|flying| flying.by.clone()) {
            if !during.contains(&by) {
                during.push(by);
            }
        }
        let first = state.flying[0].turn.clone();
        let turn = first.clone().filter(|_| {
            state
                .flying
                .iter()
                .all(|flying| flying.by == Some(CallBy::Assistant) && flying.turn == first)
        });
        Ok(Slot {
            calls: self.clone(),
            id,
            gone,
            during,
            turn,
        })
    }

    /// Logs a refused form, at most once per [`WARN_EVERY`], saying how many were not.
    fn warn(&self, source: &str, why: &str) {
        let mut state = self.state();
        if state
            .warned
            .is_some_and(|warned| warned.elapsed() < WARN_EVERY)
        {
            state.quiet += 1;
            return;
        }
        let quiet = std::mem::take(&mut state.quiet);
        state.warned = Some(Instant::now());
        drop(state);
        if quiet > 0 {
            tracing::warn!("{source}: form refused, {why} ({quiet} more refused, not logged)");
        } else {
            tracing::warn!("{source}: form refused, {why}");
        }
    }
}

/// Fronde as an upstream server's client: it answers the server's forms through [`Asking`].
#[derive(Clone)]
struct UpstreamClient {
    source: String,
    asking: Option<Asking>,
    calls: Arc<Calls>,
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
        let cancel = || ElicitResult::new(ElicitationAction::Cancel);
        let Some(asking) = &self.asking else {
            self.calls
                .warn(source, "Fronde shows no form without the chat");
            return cancel();
        };
        let ElicitRequestParams::FormElicitationParams {
            message,
            requested_schema,
            ..
        } = request
        else {
            self.calls.warn(
                source,
                "it is a URL elicitation, which Fronde does not show",
            );
            return cancel();
        };
        let slot = match self.calls.open(&cancelled) {
            Ok(slot) => slot,
            Err(why) => {
                self.calls.warn(source, why);
                return cancel();
            }
        };
        let mut elicited = match serde_json::to_value(&requested_schema)
            .map_err(|error| Refused::new(format!("the requested schema cannot be read: {error}")))
            .and_then(|schema| form(&message, &schema))
        {
            Ok(elicited) => elicited,
            Err(refused) => {
                self.calls.warn(source, &refused.detail);
                return refusal(&refused);
            }
        };
        if let Card::Form { during, .. } = &mut elicited.card {
            during.clone_from(&slot.during);
        }
        // With the assistant's turn when it alone is calling, in the stream that turn runs in:
        // Stop and the turn's end close the card with the call it serves. Else in the admin's.
        let (topic, turn) = match slot.turn.clone() {
            Some((topic, turn)) => (topic, Some(turn)),
            None => (asking.topic.clone(), None),
        };
        let ask = Ask {
            topic,
            turn,
            caller: Caller::Source(source.clone()),
            client_request_id: None,
            card: elicited.card.clone(),
            for_user: asking.user.clone(),
            admin_only: true,
            timeout: asking.timeout.min(MAX_TIMEOUT),
            tied: true,
            secret_fields: false,
        };
        // Given up when the server cancels its request, when the last call to the server ends,
        // or when this future is dropped (the call that waits on an `input_required` round was
        // given up): the card then closes. The card is asked and awaited in a task of its own,
        // so that a drop in the middle of showing it cannot leave it open.
        let gone = slot.gone.clone();
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
        drop(slot);
        match resolution {
            Resolution::Answered(Response::Form { values }) => {
                ElicitResult::new(ElicitationAction::Accept)
                    .with_content(Value::Object(elicited.content(&values)))
            }
            Resolution::Declined => ElicitResult::new(ElicitationAction::Decline),
            Resolution::Answered(_)
            | Resolution::Saved(_)
            | Resolution::Expired
            | Resolution::Cancelled => cancel(),
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
    calls: Arc<Calls>,
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
    /// A call its caller gives up is dropped, and with it the request to the server (rmcp sends
    /// the server a cancel); a form the server shows meanwhile closes once no other call to it
    /// is in flight.
    async fn call_as(&self, context: &CallContext, args: Value) -> ToolOutput {
        let _flying = self.link.calls.start(Some(context));
        tokio::select! {
            output = self.forward(args) => output,
            () = context.cancelled.cancelled() => ToolOutput::error(format!(
                "the call to {} was cancelled; call it again if you still need it",
                self.tool
            )),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let _flying = self.link.calls.start(None);
        self.forward(args).await
    }
}

impl Forward {
    async fn forward(&self, args: Value) -> ToolOutput {
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
    token: Option<SecretString>,
    handler: UpstreamClient,
) -> anyhow::Result<(Client, Vec<rmcp::model::Tool>)> {
    let attempt = async {
        let client = open(cfg, token, handler).await?;
        let tools = client.list_all_tools().await?;
        anyhow::Ok((client, tools))
    };
    tokio::time::timeout(CONNECT_TIMEOUT, attempt)
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {}s", CONNECT_TIMEOUT.as_secs()))?
}

async fn open(
    cfg: &UpstreamConfig,
    token: Option<SecretString>,
    handler: UpstreamClient,
) -> anyhow::Result<Client> {
    if let Some(url) = &cfg.url {
        let mut config = StreamableHttpClientTransportConfig::with_uri(url.as_str());
        if let Some(t) = &token {
            config = config.auth_header(t.expose_secret());
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
    /// The database and the master key's backends, where a `{ stored }` token and the key are
    /// read each time the server starts, so that a secret stored since applies.
    store: Option<(fronde_db::DatabaseConnection, Backends)>,
}

impl Upstream {
    /// A server whose forms are cancelled: Fronde shows them nowhere.
    pub fn new(cfg: UpstreamConfig) -> Self {
        Self {
            cfg,
            asking: None,
            store: None,
        }
    }

    /// A server whose forms are shown as cards, as `asking` says.
    pub fn asking(cfg: UpstreamConfig, asking: Option<Asking>) -> Self {
        Self {
            cfg,
            asking,
            store: None,
        }
    }

    /// Read a `{ stored }` token from `db`, and the master key from `backends`, each time the
    /// server starts.
    pub fn reading_secrets_from(
        mut self,
        db: fronde_db::DatabaseConnection,
        backends: Backends,
    ) -> Self {
        self.store = Some((db, backends));
        self
    }

    /// The token to send: a file or environment secret read now, a stored one opened for the
    /// origin of the URL. A stored token that is not set yet, or is stored for another origin,
    /// is [`Waiting`](crate::source::Waiting): `fronde secret set` fixes it while Fronde runs.
    async fn token(&self) -> anyhow::Result<Option<SecretString>> {
        let name = &self.cfg.name;
        match &self.cfg.token {
            None => Ok(None),
            Some(Secret::Stored { stored }) => {
                let Some(url) = &self.cfg.url else {
                    anyhow::bail!(
                        "mcp {name:?}: token = {{ stored = {stored:?} }}: a command has no \
                         origin to bind a stored secret to; use {{ file = \"…\" }}"
                    );
                };
                let Some((db, backends)) = &self.store else {
                    anyhow::bail!(
                        "mcp {name:?}: the stored secret {stored} cannot be opened: this server \
                         was not given the secret store"
                    );
                };
                let origin = Origin::of_url(url).with_context(|| format!("mcp {name:?}"))?;
                let secrets = StoredSecrets::load(db, backends).await?;
                secrets
                    .open(stored, &origin)
                    .map(Some)
                    .with_context(|| format!("mcp {name:?}: token"))
            }
            Some(secret) => Ok(Some(
                secret
                    .resolve()
                    .with_context(|| format!("mcp {name:?}: token"))?,
            )),
        }
    }
}

#[async_trait]
impl ToolSource for Upstream {
    fn name(&self) -> &str {
        &self.cfg.name
    }

    /// Connect and list the tools; the session ends when the connection is lost.
    async fn start(&self) -> anyhow::Result<Session> {
        let calls = Arc::new(Calls::default());
        let handler = UpstreamClient {
            source: self.cfg.name.clone(),
            asking: self.asking.clone(),
            calls: calls.clone(),
        };
        let token = self.token().await?;
        let (client, tools) = connect(&self.cfg, token, handler).await?;
        let name = self.cfg.name.clone();
        let link = Arc::new(Link {
            name: name.clone(),
            client,
            broken: Notify::new(),
            calls,
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

    /// Serve on `listener` like [`serve`], answering 401 unless the request carries
    /// `Authorization: <expected>`.
    pub fn serve_requiring(listener: tokio::net::TcpListener, expected: &'static str) {
        let svc = StreamableHttpService::new(
            || Ok(Echo),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );
        let app = axum::Router::new()
            .nest_service("/mcp", svc)
            .layer(axum::middleware::from_fn(
                move |req: axum::extract::Request, next: axum::middleware::Next| async move {
                    let ok = req
                        .headers()
                        .get("authorization")
                        .is_some_and(|v| v.as_bytes() == expected.as_bytes());
                    if ok {
                        next.run(req).await
                    } else {
                        axum::http::StatusCode::UNAUTHORIZED.into_response()
                    }
                },
            ));
        tokio::spawn(async move { axum::serve(listener, app).await });
    }

    use axum::response::IntoResponse;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Supervisor;
    use crate::tools::Registry;
    use fronde_db::master_key::Backends;
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

    /// A store with `value` sealed as `name` for `origin`, and the backends that open it.
    async fn store(
        dir: &std::path::Path,
        name: &str,
        origin: &str,
        value: &str,
    ) -> (fronde_db::DatabaseConnection, Backends) {
        let db = fronde_db::open(dir).await.unwrap();
        let backends = Backends::file(dir.join("master.key"));
        put(&db, &backends, name, origin, value).await;
        (db, backends)
    }

    async fn put(
        db: &fronde_db::DatabaseConnection,
        backends: &Backends,
        name: &str,
        origin: &str,
        value: &str,
    ) {
        fronde_db::secrets::store(
            db,
            backends,
            fronde_db::secrets::SOURCES,
            name,
            origin,
            &value.to_owned().into(),
        )
        .await
        .unwrap();
    }

    fn stored_cfg(url: &str, name: &str) -> UpstreamConfig {
        UpstreamConfig {
            name: "mcphub".into(),
            url: Some(url.parse().unwrap()),
            command: None,
            token: Some(Secret::Stored {
                stored: name.into(),
            }),
        }
    }

    #[tokio::test]
    async fn stored_token_reaches_upstream() {
        use wiremock::matchers::{header, method};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        // The handshake is not MCP: the server only has to see the header.
        Mock::given(method("POST"))
            .and(header("authorization", "Bearer stored-token"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1..)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let (db, backends) = store(dir.path(), "up", &server.uri(), "stored-token").await;
        let source = Upstream::new(stored_cfg(&format!("{}/mcp", server.uri()), "up"))
            .reading_secrets_from(db, backends);
        let e = source.start().await.err().expect("not an MCP server");
        assert!(!format!("{e:#}").contains("stored-token"));
        drop(server); // verifies the expectation
    }

    #[tokio::test]
    async fn stored_token_for_stdio_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (db, backends) = store(dir.path(), "up", "https://up.example", "x").await;
        let cfg = UpstreamConfig {
            name: "mcphub".into(),
            url: None,
            command: Some(vec!["true".into()]),
            token: Some(Secret::Stored {
                stored: "up".into(),
            }),
        };
        let e = Upstream::new(cfg)
            .reading_secrets_from(db, backends)
            .start()
            .await
            .err()
            .expect("refused");
        assert!(format!("{e:#}").contains("use { file = \"…\" }"), "{e:#}");
    }

    #[tokio::test]
    async fn stored_token_other_origin_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (db, backends) = store(dir.path(), "up", "https://other.example", "secret-x").await;
        let source = Upstream::new(stored_cfg("https://up.example/mcp", "up"))
            .reading_secrets_from(db, backends);
        let e = source.start().await.err().expect("refused");
        let shown = format!("{e:#}");
        assert!(
            shown.contains("is for https://other.example, not https://up.example")
                && shown.contains("fronde secret set up --origin https://up.example")
                && !shown.contains("secret-x"),
            "{shown}"
        );
        assert!(e.downcast_ref::<crate::source::Waiting>().is_some());
    }

    #[tokio::test]
    async fn a_missing_stored_token_waits_then_starts_once_set() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        test_server::serve_requiring(listener, "Bearer later-token");
        let dir = tempfile::tempdir().unwrap();
        let db = fronde_db::open(dir.path()).await.unwrap();
        let backends = Backends::file(dir.path().join("master.key"));
        let source = Upstream::new(stored_cfg(&format!("http://{addr}/mcp"), "up"))
            .reading_secrets_from(db.clone(), backends.clone());
        let e = source.start().await.err().expect("not stored yet");
        assert!(
            e.downcast_ref::<crate::source::Waiting>().is_some(),
            "{e:#}"
        );
        assert!(format!("{e:#}").contains("no secret named up is stored"));

        let reg = Arc::new(Registry::new());
        Supervisor::new(reg.clone()).spawn(Arc::new(source));
        put(
            &db,
            &backends,
            "up",
            &format!("http://{addr}"),
            "later-token",
        )
        .await;
        // The supervisor's first retry waits about 2 s.
        assert!(wait_for(&reg, "mcphub_echo", 15).await);
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
            calls: Arc::default(),
        };
        let Err(e) = connect(&cfg(addr), None, handler).await else {
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
            Shown(RequestId, Card, Caller, String),
            Closed(RequestId, Outcome),
        }

        struct Seeing(mpsc::UnboundedSender<Seen>);

        #[async_trait]
        impl Journal for Seeing {
            async fn announce(
                &self,
                topic: &str,
                id: RequestId,
                card: &Card,
                caller: &Caller,
            ) -> anyhow::Result<u64> {
                let _ = self
                    .0
                    .send(Seen::Shown(id, card.clone(), caller.clone(), topic.into()));
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

            /// The next card and the stream it is shown in.
            async fn card_in(&mut self) -> (RequestId, String) {
                match self.next().await {
                    Seen::Shown(id, _, _, topic) => (id, topic),
                    other => panic!("not a card: {other:?}"),
                }
            }

            async fn card(&mut self) -> (RequestId, Card, Caller) {
                match self.next().await {
                    Seen::Shown(id, card, caller, _) => (id, card, caller),
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
                    tool("flood"),
                ]))
            }

            async fn call_tool(
                &self,
                r: CallToolRequestParams,
                c: RequestContext<RoleServer>,
            ) -> Result<CallToolResponse, ErrorData> {
                let asked = match r.name.as_ref() {
                    "flood" => {
                        let ask = || c.peer.create_elicitation(form("Encore", booking()));
                        let (a, b, d) = tokio::join!(ask(), ask(), ask());
                        let answers: Vec<Value> = [a, b, d]
                            .into_iter()
                            .map(|asked| serde_json::to_value(asked.unwrap()).unwrap())
                            .collect();
                        return Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                            ContentBlock::text(Value::Array(answers).to_string()),
                        ])));
                    }
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
            // A plain reason, naming neither the field nor the rule.
            assert_eq!(
                answer["_meta"][REFUSED_META],
                fronde_requests::elicitation::SECRET_REFUSAL,
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
                caller: Caller::Thread(fronde_requests::ThreadId::MAIN),
                user: "alix".into(),
                topic: "conversation".into(),
                turn: Some(1),
                cancelled: CancellationToken::new(),
            };
            let call = tokio::spawn({
                let (reg, context) = (reg.clone(), context.clone());
                async move { reg.call_as(&context, "mcphub_book", json!({})).await }
            });
            let (id, card, _) = bench.card().await;
            // The card says whose call the server was serving.
            let Card::Form { during, .. } = card else {
                panic!("{card:?}");
            };
            assert_eq!(during, [CallBy::Assistant]);
            context.cancelled.cancel();
            let out = tokio::time::timeout(WAIT, call).await.unwrap().unwrap();
            assert!(out.is_error);
            assert!(out.to_text().contains("was cancelled"), "{}", out.to_text());
            // The last call to the server ended: its card closes, whatever the server does.
            assert_eq!(bench.closed().await, (id, Outcome::Cancelled));
        }

        fn context(caller: Caller, turn: Option<u64>) -> CallContext {
            CallContext {
                caller,
                user: "alix".into(),
                topic: "conversation".into(),
                turn,
                cancelled: CancellationToken::new(),
            }
        }

        fn call_as(
            reg: &Arc<Registry>,
            context: CallContext,
            tool: &str,
        ) -> tokio::task::JoinHandle<ToolOutput> {
            let (reg, tool) = (reg.clone(), tool.to_owned());
            tokio::spawn(async move { reg.call_as(&context, &tool, json!({})).await })
        }

        #[tokio::test]
        async fn stop_closes_the_assistants_upstream_card() {
            let mut bench = Bench::new().await;
            let reg = connected(Some(bench.asking(DEFAULT_TIMEOUT))).await;
            // The agent of another thread than the chat: the card goes to its thread's stream.
            let mut assistant = context(Caller::Thread(fronde_requests::ThreadId(2)), Some(7));
            assistant.topic = "thread-2".into();
            let book = call_as(&reg, assistant, "mcphub_book");
            // Shown in thread 2's stream, with thread 2's turn: both from its call.
            let (id, topic) = bench.card_in().await;
            assert_eq!(topic, "thread-2");
            // Stop in the main thread, at the same seq, is another turn: the card stays open.
            assert_eq!(
                bench.requests.cancel_turn("conversation", 7).await.unwrap(),
                0
            );
            assert!(bench.seen.try_recv().is_err());
            // Stop in thread 2 closes it.
            assert_eq!(bench.requests.cancel_turn("thread-2", 7).await.unwrap(), 1);
            assert_eq!(bench.closed().await, (id, Outcome::Cancelled));
            let out = tokio::time::timeout(WAIT, book).await.unwrap().unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(out.value.as_str().unwrap()).unwrap(),
                json!({"action": "cancel"})
            );

            // An outside client's call is no turn's: the turn's end leaves its card open.
            let codex = context(Caller::Mcp("Codex".into()), None);
            let book = call_as(&reg, codex.clone(), "mcphub_book");
            let (id, card, _) = bench.card().await;
            let Card::Form { during, .. } = card else {
                panic!("{card:?}");
            };
            assert_eq!(
                during,
                [CallBy::Client {
                    name: "Codex".into()
                }]
            );
            assert_eq!(
                bench.requests.cancel_turn("conversation", 7).await.unwrap(),
                0
            );
            codex.cancelled.cancel();
            assert_eq!(bench.closed().await, (id, Outcome::Cancelled));
            tokio::time::timeout(WAIT, book).await.unwrap().unwrap();
        }

        #[tokio::test]
        async fn one_card_at_a_time_per_server() {
            let mut bench = Bench::new().await;
            let reg = connected(Some(bench.asking(DEFAULT_TIMEOUT))).await;
            // The server asks three forms at once during one call: one card, two cancels.
            let flood = call(&reg, "mcphub_flood");
            let (id, ..) = bench.card().await;
            bench
                .requests
                .answer(id, fronde_protocol::Response::Decline, &admin())
                .await
                .unwrap();
            let answers = result(flood).await;
            let mut actions: Vec<&str> = answers
                .as_array()
                .unwrap()
                .iter()
                .map(|answer| answer["action"].as_str().unwrap())
                .collect();
            actions.sort_unstable();
            assert_eq!(actions, ["cancel", "cancel", "decline"]);
            assert_eq!(bench.closed().await.0, id);
            // No other card was shown.
            assert!(bench.seen.try_recv().is_err());
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

        /// A link to `Rounds`, in process, on protocol 2026-07-28.
        fn rounds(asking: Asking) -> Arc<Link> {
            let (server, client) = tokio::io::duplex(8192);
            let mut peer = ClientConfig::default();
            peer.protocol_version = ProtocolVersion::V_2026_07_28;
            tokio::spawn(async move {
                let running = serve_directly::<RoleServer, _, _, _, _>(Rounds, server, Some(peer));
                let _ = running.waiting().await;
            });
            let mut info = ServerConfig::new(ServerCapabilities::builder().enable_tools().build());
            info.protocol_version = ProtocolVersion::V_2026_07_28;
            let calls = Arc::new(Calls::default());
            let handler = UpstreamClient {
                source: "rounds".into(),
                asking: Some(asking),
                calls: calls.clone(),
            };
            Arc::new(Link {
                name: "rounds".into(),
                client: serve_directly::<RoleClient, _, _, _, _>(
                    handler,
                    client,
                    Some(info.into()),
                ),
                broken: Notify::new(),
                calls,
            })
        }

        fn book(link: &Arc<Link>) -> tokio::task::JoinHandle<ToolOutput> {
            let forward = Forward {
                link: link.clone(),
                tool: "book".into(),
            };
            tokio::spawn(async move { forward.call(json!({})).await })
        }

        #[tokio::test]
        async fn mrtr_round_uses_the_same_handler() {
            let mut bench = Bench::new().await;
            let link = rounds(bench.asking(DEFAULT_TIMEOUT));
            let call = book(&link);
            let (id, _, caller) = bench.card().await;
            assert_eq!(caller, Caller::Source("rounds".into()));
            let answer = fill(&[
                ("name", FieldValue::Text("Alix".into())),
                ("nights", FieldValue::Number(1.0)),
            ]);
            bench.requests.answer(id, answer, &admin()).await.unwrap();
            let done = tokio::time::timeout(WAIT, call).await.unwrap().unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(done.value.as_str().unwrap()).unwrap(),
                json!({"action": "accept", "content": {"name": "Alix", "nights": 1}})
            );

            // A call given up while its form is shown closes the card.
            let call = book(&link);
            let (_, _) = bench.closed().await;
            let (id, ..) = bench.card().await;
            call.abort();
            assert_eq!(bench.closed().await, (id, Outcome::Cancelled));
        }

        #[tokio::test]
        async fn a_form_with_no_call_in_flight_is_cancelled_unseen() {
            let mut bench = Bench::new().await;
            let link = rounds(bench.asking(DEFAULT_TIMEOUT));
            // Straight to the server, not through a hub tool: no call is in flight for it.
            let done = tokio::time::timeout(
                WAIT,
                link.client.call_tool(CallToolRequestParams::new("book")),
            )
            .await
            .unwrap()
            .unwrap();
            let text = &done.content[0].as_text().unwrap().text;
            assert_eq!(
                serde_json::from_str::<Value>(text).unwrap(),
                json!({"action": "cancel"})
            );
            assert!(bench.seen.try_recv().is_err());
        }
    }
}
