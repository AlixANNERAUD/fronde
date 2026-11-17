//! ACP client: starts the agent, opens Fronde's session and runs prompt turns.

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    CancelNotification, ContentBlock, ContentChunk, HttpHeader, Implementation, InitializeRequest,
    McpServer, McpServerHttp, NewSessionRequest, PermissionOption, PermissionOptionKind,
    PromptRequest, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    ResumeSessionRequest, SelectedPermissionOutcome, SessionCapabilities, SessionId,
    SessionNotification, SessionUpdate, SetSessionModeRequest, StopReason, SystemPrompt,
    SystemPromptMode, ToolCallStatus,
};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, Client, ConnectTo, ErrorCode};
use anyhow::{Context, anyhow};
use secrecy::{ExposeSecret, SecretString};
use serde_json::json;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinError;

/// How to start the agent and what its session gets.
#[derive(Clone)]
pub struct AgentSpec {
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub hub_url: String,
    pub hub_token: SecretString,
    pub system_prompt: String,
    /// Claude Code's own tools the session gets besides the hub's, such as `WebSearch`.
    pub builtin_tools: Vec<String>,
}

impl fmt::Debug for AgentSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentSpec")
            .field("command", &self.command)
            .field("env", &self.env)
            .field("cwd", &self.cwd)
            .field("hub_url", &self.hub_url)
            .field("hub_token", &"[REDACTED]")
            .field("system_prompt", &self.system_prompt)
            .field("builtin_tools", &self.builtin_tools)
            .finish()
    }
}

/// What the agent does during a turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentEvent {
    Text(String),
    ToolStarted {
        id: String,
        title: String,
    },
    ToolFinished {
        id: String,
        ok: bool,
    },
    PermissionAsked {
        id: u64,
        title: String,
    },
    /// The request was dropped unanswered: the turn was cancelled or ended.
    PermissionClosed {
        id: u64,
    },
}

/// Why a turn ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnEnd {
    Done,
    Cancelled,
    Refused,
    Limit,
}

/// What went wrong with the agent, as far as its owner can do something about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The agent's program is not installed.
    NotInstalled,
    /// The agent is not signed in to its model provider.
    SignedOut,
    Other,
}

impl Failure {
    pub fn of(error: &anyhow::Error) -> Self {
        if error.chain().any(|cause| cause.is::<NotInstalled>()) {
            Self::NotInstalled
        } else if error.chain().any(|cause| {
            cause
                .downcast_ref::<agent_client_protocol::Error>()
                .is_some_and(|error| error.code == ErrorCode::AUTH_REQUIRED)
        }) {
            Self::SignedOut
        } else {
            Self::Other
        }
    }
}

/// The agent's program is neither a path to an executable nor found on the `PATH`.
#[derive(Debug)]
struct NotInstalled(String);

impl std::fmt::Display for NotInstalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cannot start the agent: {} is not installed or not on PATH; run `mise run agent` in the Fronde checkout and start Fronde with `mise exec -- fronde serve …`, or set [agent] command to the adapter's full path",
            self.0
        )
    }
}

impl std::error::Error for NotInstalled {}

/// Whether `program` names an executable, looked up on the agent's `PATH` when it has no
/// directory.
pub fn installed(program: &str, env: &[(String, String)]) -> bool {
    let program = Path::new(program);
    if program.components().count() > 1 {
        return executable(program);
    }
    let path = env
        .iter()
        .rev()
        .find(|(name, _)| name == "PATH")
        .map(|(_, value)| OsString::from(value))
        .or_else(|| std::env::var_os("PATH"));
    path.is_some_and(|path| std::env::split_paths(&path).any(|dir| executable(&dir.join(program))))
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// One ACP session with the agent. Dropping it closes the connection and stops a spawned agent.
pub struct AgentClient {
    connection: ConnectionTo<Agent>,
    session_id: SessionId,
    permissions: Arc<Permissions>,
    // The connection task runs until this sender is dropped.
    _stop: oneshot::Sender<()>,
}

impl AgentClient {
    /// Start the agent process and open a session (resumed when possible).
    pub async fn spawn(
        spec: &AgentSpec,
        resume: Option<String>,
        events: mpsc::UnboundedSender<AgentEvent>,
    ) -> anyhow::Result<Self> {
        let command = spec.command.join(" ");
        let (program, args) = spec
            .command
            .split_first()
            .context("the agent command is empty; set it to claude-agent-acp")?;
        // The ACP crate reports a failed spawn without its cause: look for the program first.
        if !installed(program, &spec.env) {
            return Err(NotInstalled(program.clone()).into());
        }
        let config = AcpAgentConfig::new(program)
            .args(args.iter().cloned())
            .envs(spec.env.iter().cloned());
        let connection = Connection::start(AcpAgent::new(config), events)
            .await
            .with_context(|| format!("cannot start the agent ({command})"))?;
        connection.open_session(spec, resume).await
    }

    /// Open a session (resumed when possible) with an agent reached through `transport`.
    pub async fn connect(
        transport: impl ConnectTo<Client> + 'static,
        spec: &AgentSpec,
        resume: Option<String>,
        events: mpsc::UnboundedSender<AgentEvent>,
    ) -> anyhow::Result<Self> {
        Connection::start(transport, events)
            .await?
            .open_session(spec, resume)
            .await
    }

    pub fn session_id(&self) -> &str {
        &self.session_id.0
    }

    /// Run one turn; events stream to the sender given at connection until it returns.
    pub async fn prompt(&self, text: &str) -> anyhow::Result<TurnEnd> {
        let request = PromptRequest::new(self.session_id.clone(), vec![ContentBlock::from(text)]);
        // A cancel that came after the previous turn ended is not about this one.
        self.permissions.end_turn();
        let response = self.connection.send_request(request).block_task().await;
        self.permissions.end_turn();
        let response = response.context("the agent failed the turn")?;
        Ok(match response.stop_reason {
            StopReason::Cancelled => TurnEnd::Cancelled,
            StopReason::Refusal => TurnEnd::Refused,
            StopReason::MaxTokens | StopReason::MaxTurnRequests => TurnEnd::Limit,
            _ => TurnEnd::Done,
        })
    }

    /// Ask the agent to stop the current turn; pending and later permission requests of the
    /// turn are cancelled.
    pub fn cancel(&self) -> anyhow::Result<()> {
        self.permissions.cancel();
        self.connection
            .send_notification(CancelNotification::new(self.session_id.clone()))
            .context("cannot reach the agent to cancel the turn")?;
        Ok(())
    }

    /// Answer the permission request `id` announced by `AgentEvent::PermissionAsked`.
    pub fn answer_permission(&self, id: u64, allow: bool) -> anyhow::Result<()> {
        let pending = self
            .permissions
            .state()
            .pending
            .remove(&id)
            .with_context(|| {
                format!("no pending permission request {id}: it was already answered or cancelled")
            })?;
        let kind = if allow {
            PermissionOptionKind::AllowOnce
        } else {
            PermissionOptionKind::RejectOnce
        };
        // Without a one-time option, cancelling is the answer that grants nothing lasting.
        let outcome = pending
            .options
            .iter()
            .find(|option| option.kind == kind)
            .map_or(RequestPermissionOutcome::Cancelled, |option| {
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                    option.option_id.clone(),
                ))
            });
        // The turn may have ended meanwhile; then nobody waits for the answer.
        let _ = pending.reply.send(outcome);
        Ok(())
    }
}

/// Permission requests waiting for a human answer.
struct Permissions {
    last_id: AtomicU64,
    state: Mutex<PermissionState>,
    events: mpsc::UnboundedSender<AgentEvent>,
}

#[derive(Default)]
struct PermissionState {
    /// Set from `cancel` until the turn ends: requests crossing the cancel on the wire are
    /// refused at once. Cleared again when the next turn starts.
    cancelling: bool,
    pending: HashMap<u64, PendingPermission>,
}

struct PendingPermission {
    options: Vec<PermissionOption>,
    reply: oneshot::Sender<RequestPermissionOutcome>,
}

impl Permissions {
    fn new(events: mpsc::UnboundedSender<AgentEvent>) -> Self {
        Self {
            last_id: AtomicU64::new(0),
            state: Mutex::default(),
            events,
        }
    }

    fn state(&self) -> MutexGuard<'_, PermissionState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers a request and announces it; `None` when the turn is being cancelled.
    fn ask(
        &self,
        options: Vec<PermissionOption>,
        title: String,
    ) -> Option<oneshot::Receiver<RequestPermissionOutcome>> {
        let mut state = self.state();
        if state.cancelling {
            return None;
        }
        let (reply, answer) = oneshot::channel();
        let id = self.last_id.fetch_add(1, Ordering::Relaxed) + 1;
        state
            .pending
            .insert(id, PendingPermission { options, reply });
        // Announced under the lock so a close of this id can never be sent before it.
        let _ = self.events.send(AgentEvent::PermissionAsked { id, title });
        Some(answer)
    }

    fn cancel(&self) {
        let mut state = self.state();
        state.cancelling = true;
        self.close_pending(&mut state);
    }

    fn end_turn(&self) {
        let mut state = self.state();
        state.cancelling = false;
        self.close_pending(&mut state);
    }

    fn close_pending(&self, state: &mut PermissionState) {
        for (id, pending) in state.pending.drain() {
            // The agent may already have given up on this request.
            let _ = pending.reply.send(RequestPermissionOutcome::Cancelled);
            let _ = self.events.send(AgentEvent::PermissionClosed { id });
        }
    }
}

/// An initialized connection without a session yet.
struct Connection {
    connection: ConnectionTo<Agent>,
    capabilities: SessionCapabilities,
    permissions: Arc<Permissions>,
    stop: oneshot::Sender<()>,
}

impl Connection {
    async fn start(
        transport: impl ConnectTo<Client> + 'static,
        events: mpsc::UnboundedSender<AgentEvent>,
    ) -> anyhow::Result<Self> {
        let permissions = Arc::new(Permissions::new(events.clone()));
        let builder = Client
            .v1()
            .on_receive_notification(
                async move |notification: SessionNotification, _cx| {
                    for event in events_from(notification.update) {
                        // A closed receiver only means nobody is listening any more.
                        let _ = events.send(event);
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                {
                    let permissions = permissions.clone();
                    async move |request: RequestPermissionRequest, responder, cx| {
                        let title = request
                            .tool_call
                            .fields
                            .title
                            .unwrap_or_else(|| "permission".to_owned());
                        let Some(answer) = permissions.ask(request.options, title) else {
                            return responder.respond(RequestPermissionResponse::new(
                                RequestPermissionOutcome::Cancelled,
                            ));
                        };
                        // Waiting for the human inside the handler would block the dispatch
                        // loop, and with it every other message from the agent.
                        cx.spawn(async move {
                            let outcome =
                                answer.await.unwrap_or(RequestPermissionOutcome::Cancelled);
                            responder.respond(RequestPermissionResponse::new(outcome))
                        })
                    }
                },
                agent_client_protocol::on_receive_request!(),
            );

        let (connected, connection) = oneshot::channel();
        let (stop, stopped) = oneshot::channel::<()>();
        let mut task = tokio::spawn(async move {
            let result = builder
                .connect_with(transport, async move |cx| {
                    let _ = connected.send(cx);
                    let _ = stopped.await;
                    Ok(())
                })
                .await;
            if let Err(error) = &result {
                tracing::warn!("the agent connection ended: {error}");
            }
            result
        });

        let initialize = async {
            let connection = connection.await.context("the agent connection closed")?;
            let response = connection
                .send_request(
                    InitializeRequest::new(ProtocolVersion::V1)
                        .client_info(Implementation::new("fronde", env!("CARGO_PKG_VERSION"))),
                )
                .block_task()
                .await?;
            anyhow::Ok((connection, response))
        };
        // Polling the task first reports why the connection ended (a missing program, say)
        // rather than the request it failed.
        let (connection, response) = tokio::select! {
            biased;
            ended = &mut task => return Err(connection_end_error(ended)),
            initialized = initialize => initialized?,
        };
        Ok(Self {
            connection,
            capabilities: response.agent_capabilities.session_capabilities,
            permissions,
            stop,
        })
    }

    async fn open_session(
        self,
        spec: &AgentSpec,
        resume: Option<String>,
    ) -> anyhow::Result<AgentClient> {
        let Self {
            connection,
            capabilities,
            permissions,
            stop,
        } = self;
        let hub = McpServer::Http(McpServerHttp::new("fronde", &spec.hub_url).headers(vec![
            HttpHeader::new(
                "Authorization",
                format!("Bearer {}", spec.hub_token.expose_secret()),
            ),
        ]));
        let system_prompt =
            (capabilities.system_prompt.is_some() && !spec.system_prompt.is_empty()).then(|| {
                SystemPrompt::new(vec![ContentBlock::from(spec.system_prompt.as_str())])
                    .mode(SystemPromptMode::Append)
            });

        let mut resumed = None;
        if let Some(id) = resume.filter(|_| capabilities.resume.is_some()) {
            let request = ResumeSessionRequest::new(id.clone(), spec.cwd.clone())
                .mcp_servers(vec![hub.clone()])
                .system_prompt(system_prompt.clone())
                .meta(session_meta(&spec.builtin_tools));
            match connection.send_request(request).block_task().await {
                Ok(_) => resumed = Some(SessionId::from(id)),
                Err(error) => {
                    tracing::warn!("cannot resume agent session {id}, starting a new one: {error}");
                }
            }
        }
        let session_id = match resumed {
            Some(id) => id,
            None => {
                let request = NewSessionRequest::new(spec.cwd.clone())
                    .mcp_servers(vec![hub])
                    .system_prompt(system_prompt)
                    .meta(session_meta(&spec.builtin_tools));
                connection
                    .send_request(request)
                    .block_task()
                    .await
                    .context("the agent refused to open a session")?
                    .session_id
            }
        };

        // The Claude Code adapter starts sessions in `auto`; `default` asks before acting.
        let mode = SetSessionModeRequest::new(session_id.clone(), "default");
        if let Err(error) = connection.send_request(mode).block_task().await {
            tracing::debug!("the agent has no default mode: {error}");
        }

        Ok(AgentClient {
            connection,
            session_id,
            permissions,
            _stop: stop,
        })
    }
}

/// Fronde's agent gets the hub's tools and the listed built-in ones, all allowed up front. The
/// Claude Code adapter loads the user's, project's and local settings unless told otherwise, and
/// with them the owner's plugins, MCP servers, hooks and allow rules: none of them apply. The
/// sign-in is not a setting and still works.
fn session_meta(builtin_tools: &[String]) -> serde_json::Map<String, serde_json::Value> {
    let allowed: Vec<&str> = std::iter::once("mcp__fronde")
        .chain(builtin_tools.iter().map(String::as_str))
        .collect();
    // The adapter prefers `tools` over `disableBuiltInTools`; the flag stays so that an adapter
    // ignoring `tools` falls back to no built-in tool rather than all of them.
    let serde_json::Value::Object(meta) = json!({
        "disableBuiltInTools": true,
        "claudeCode": {"options": {
            "tools": builtin_tools,
            "allowedTools": allowed,
            "settingSources": [],
            "strictMcpConfig": true,
            "allowDangerouslySkipPermissions": false,
        }},
    }) else {
        unreachable!("the literal is an object")
    };
    meta
}

fn connection_end_error(
    ended: Result<agent_client_protocol::Result<()>, JoinError>,
) -> anyhow::Error {
    match ended {
        Ok(Ok(())) => anyhow!("the agent closed the connection"),
        Ok(Err(error)) => error.into(),
        Err(error) => error.into(),
    }
}

fn events_from(update: SessionUpdate) -> Vec<AgentEvent> {
    match update {
        SessionUpdate::AgentMessageChunk(ContentChunk {
            content: ContentBlock::Text(text),
            ..
        }) => vec![AgentEvent::Text(text.text)],
        SessionUpdate::ToolCall(call) => {
            let id = call.tool_call_id.0.to_string();
            let finished = finished(Some(call.status))
                .map(|ok| AgentEvent::ToolFinished { id: id.clone(), ok });
            let started = AgentEvent::ToolStarted {
                id,
                title: call.title,
            };
            std::iter::once(started).chain(finished).collect()
        }
        SessionUpdate::ToolCallUpdate(update) => finished(update.fields.status)
            .map(|ok| AgentEvent::ToolFinished {
                id: update.tool_call_id.0.to_string(),
                ok,
            })
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether a tool call succeeded, once it is over.
fn finished(status: Option<ToolCallStatus>) -> Option<bool> {
    match status? {
        ToolCallStatus::Completed => Some(true),
        ToolCallStatus::Failed => Some(false),
        _ => None,
    }
}
