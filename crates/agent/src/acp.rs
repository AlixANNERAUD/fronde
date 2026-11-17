//! ACP client: starts the agent, opens Fronde's session and runs prompt turns.

use crate::confine::Confine;
use crate::scope::ScopeStop;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    CancelNotification, ClientCapabilities, ContentBlock, ContentChunk, CreateElicitationRequest,
    CreateElicitationResponse, ElicitationAction, ElicitationCapabilities,
    ElicitationFormCapabilities, ElicitationMode, HttpHeader, Implementation, InitializeRequest,
    McpServer, McpServerHttp, NewSessionRequest, PermissionOption, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    ResumeSessionRequest, SessionCapabilities, SessionId, SessionNotification, SessionUpdate,
    SetSessionModeRequest, StopReason, SystemPrompt, SystemPromptMode, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, Client, ConnectTo, ErrorCode};
use anyhow::{Context, anyhow};
use fronde_protocol::ThreadId;
use secrecy::{ExposeSecret, SecretString};
use serde_json::json;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;

/// The variable that carries the hub's token to an agent Fronde starts, and that Claude Code
/// expands in the session's MCP header: the header itself then holds no secret, since the
/// adapter passes it to Claude Code on a command line, which every user can read.
pub const HUB_TOKEN_VAR: &str = "FRONDE_HUB_TOKEN";

/// The variable Claude Code reads its sign-in token from.
pub const OAUTH_TOKEN_VAR: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// The agent's Claude sign-in token, `[agent] oauth_token`: read again at each start, so that a
/// token changed in its file or store applies at the next one.
pub type OauthToken = Arc<
    dyn Fn() -> futures::future::BoxFuture<'static, anyhow::Result<SecretString>> + Send + Sync,
>;

/// An [`OauthToken`] from an async function.
pub fn oauth_token<F, Fut>(read: F) -> OauthToken
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = anyhow::Result<SecretString>> + Send + 'static,
{
    Arc::new(move || Box::pin(read()))
}

/// How to start the agent and what its session gets.
#[derive(Clone)]
pub struct AgentSpec {
    /// The thread the agent works for: its scope unit names it.
    pub thread: ThreadId,
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    /// The agent's working directory, which its sandbox lets it write.
    pub cwd: PathBuf,
    /// The agent's `HOME`, its `tmp` the agent's `TMPDIR`, which its sandbox lets it write.
    pub home: PathBuf,
    pub hub_url: String,
    /// The agent process's own hub token: a [`Threads`](crate::threads::Threads) thread's agent
    /// gets a fresh one at each start, which dies with the process.
    pub hub_token: SecretString,
    /// Given to the agent as `CLAUDE_CODE_OAUTH_TOKEN` when set.
    pub oauth_token: Option<OauthToken>,
    pub system_prompt: String,
    /// Claude Code's own tools the session gets besides the hub's, such as `WebSearch`.
    pub builtin_tools: Vec<String>,
}

impl fmt::Debug for AgentSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentSpec")
            .field("thread", &self.thread)
            .field("command", &self.command)
            .field("env", &self.env)
            .field("cwd", &self.cwd)
            .field("home", &self.home)
            .field("hub_url", &self.hub_url)
            .field("hub_token", &"[REDACTED]")
            .field(
                "oauth_token",
                &self.oauth_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("system_prompt", &self.system_prompt)
            .field("builtin_tools", &self.builtin_tools)
            .finish()
    }
}

/// What the agent does during a turn.
#[derive(Debug, PartialEq, Eq)]
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
    /// The agent asks to use a tool and waits for [`PermissionAsk::reply`].
    Permission(Box<PermissionAsk>),
    /// The agent asks the user to fill a form (Claude's `AskUserQuestion`, say) and waits for
    /// [`ElicitationAsk::reply`].
    Elicitation(Box<ElicitationAsk>),
}

/// A permission request from the agent.
#[derive(Debug, PartialEq, Eq)]
pub struct PermissionAsk {
    /// The tool call, as the agent last described it: what its earlier updates said, with what
    /// the request says over it.
    pub tool_call: ToolCallUpdateFields,
    pub options: Vec<PermissionOption>,
    pub reply: Arc<Reply>,
}

/// A form the agent asks the user to fill, in form mode: other modes never get this far.
#[derive(Debug)]
pub struct ElicitationAsk {
    pub request: CreateElicitationRequest,
    pub reply: Arc<Reply<CreateElicitationResponse>>,
}

/// Two asks are equal only if they are the same.
impl PartialEq for ElicitationAsk {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.reply, &other.reply)
    }
}

impl Eq for ElicitationAsk {}

/// What the agent gets for a request nobody answers: the turn was stopped or ended, the request
/// was dropped.
pub trait Unanswered {
    fn unanswered() -> Self;
}

impl Unanswered for RequestPermissionOutcome {
    fn unanswered() -> Self {
        Self::Cancelled
    }
}

impl Unanswered for CreateElicitationResponse {
    fn unanswered() -> Self {
        Self::new(ElicitationAction::Cancel)
    }
}

/// Where the answer to a request of the agent's (a permission, an elicitation) goes: the first
/// answer is the one the agent gets. Dropped unanswered, the agent gets a cancel
/// ([`Unanswered`]); so does a request still open when its turn is stopped or ends.
pub struct Reply<T = RequestPermissionOutcome> {
    sender: Mutex<Option<oneshot::Sender<T>>>,
    answered: CancellationToken,
}

impl<T> Reply<T> {
    /// A reply, and where the agent's side receives its answer.
    pub fn new() -> (Arc<Self>, oneshot::Receiver<T>) {
        let (sender, receiver) = oneshot::channel();
        let reply = Self {
            sender: Mutex::new(Some(sender)),
            answered: CancellationToken::new(),
        };
        (Arc::new(reply), receiver)
    }

    /// Answers the agent; false if it was already answered.
    pub fn send(&self, outcome: T) -> bool {
        let sender = lock(&self.sender).take();
        // The agent may already have given up on the request: answered all the same.
        let sent = sender.is_some_and(|sender| {
            let _ = sender.send(outcome);
            true
        });
        self.answered.cancel();
        sent
    }

    pub fn is_answered(&self) -> bool {
        self.answered.is_cancelled()
    }

    /// Resolves once the agent got an answer, from whichever side: a card that outlives it
    /// must close.
    pub async fn answered(&self) {
        self.answered.cancelled().await;
    }
}

impl<T> fmt::Debug for Reply<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reply")
            .field("answered", &self.is_answered())
            .finish()
    }
}

/// Two replies are equal only if they are the same.
impl<T> PartialEq for Reply<T> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl<T> Eq for Reply<T> {}

/// A reply the turn's end or a cancel closes, whatever it answers.
trait Open: Send + Sync {
    fn close(&self);
    fn is_answered(&self) -> bool;
}

impl<T: Unanswered + Send> Open for Reply<T> {
    fn close(&self) {
        self.send(T::unanswered());
    }

    fn is_answered(&self) -> bool {
        Reply::is_answered(self)
    }
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

pub(crate) fn executable(path: &Path) -> bool {
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
    // Then the agent's scope, if it has one, is stopped.
    _scope: Option<ScopeStop>,
}

impl AgentClient {
    /// Start the agent process, inheriting Fronde's environment, and open a session (resumed
    /// when possible). Unconfined: for tests.
    #[cfg(feature = "testing")]
    pub async fn spawn(
        spec: &AgentSpec,
        resume: Option<String>,
        events: mpsc::UnboundedSender<AgentEvent>,
    ) -> anyhow::Result<Self> {
        Self::spawn_with(spec, None, resume, events).await
    }

    /// Start the agent through `fronde __confine`, which gives it only the environment and the
    /// files the policy allows, and open a session (resumed when possible).
    pub async fn spawn_confined(
        spec: &AgentSpec,
        confine: &Confine,
        resume: Option<String>,
        events: mpsc::UnboundedSender<AgentEvent>,
    ) -> anyhow::Result<Self> {
        Self::spawn_with(spec, Some(confine), resume, events).await
    }

    async fn spawn_with(
        spec: &AgentSpec,
        confine: Option<&Confine>,
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
        // The secrets the agent gets, through its environment only: never on a command line.
        let mut secrets = vec![(
            HUB_TOKEN_VAR.to_owned(),
            spec.hub_token.expose_secret().to_owned(),
        )];
        if let Some(read) = &spec.oauth_token {
            let token = read().await.context("[agent] oauth_token")?;
            secrets.push((OAUTH_TOKEN_VAR.to_owned(), token.expose_secret().to_owned()));
        }
        let mut scope = None;
        let config = match confine {
            None => AcpAgentConfig::new(program)
                .args(args.iter().cloned())
                .envs(spec.env.iter().cloned().chain(secrets)),
            Some(confine) => {
                let confine = confine.ready().await;
                let helper = confine.command_with(spec, secrets)?;
                if let (Some(unit), Some(run)) = (helper.unit, &confine.scope) {
                    tracing::info!(
                        "agent limits: {}, in the systemd scope {unit} under {}",
                        run.limits.describe(),
                        crate::scope::SLICE
                    );
                    scope = Some(ScopeStop {
                        ctl: run.ctl(),
                        unit,
                    });
                }
                AcpAgentConfig::new(helper.program)
                    .args(helper.args)
                    .envs(helper.env)
            }
        };
        let connection = Connection::start(AcpAgent::new(config), events)
            .await
            .with_context(|| format!("cannot start the agent ({command})"))?;
        let header = format!("Bearer ${{{HUB_TOKEN_VAR}}}");
        let mut client = connection.open_session(spec, header, resume).await?;
        client._scope = scope;
        Ok(client)
    }

    /// Open a session (resumed when possible) with an agent reached through `transport`.
    pub async fn connect(
        transport: impl ConnectTo<Client> + 'static,
        spec: &AgentSpec,
        resume: Option<String>,
        events: mpsc::UnboundedSender<AgentEvent>,
    ) -> anyhow::Result<Self> {
        // In this process: the header may hold the token itself.
        let header = format!("Bearer {}", spec.hub_token.expose_secret());
        Connection::start(transport, events)
            .await?
            .open_session(spec, header, resume)
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

    /// Ask the agent to stop the current turn; pending and later requests of the turn
    /// (permissions, elicitations) are cancelled.
    pub fn cancel(&self) -> anyhow::Result<()> {
        self.permissions.cancel();
        self.connection
            .send_notification(CancelNotification::new(self.session_id.clone()))
            .context("cannot reach the agent to cancel the turn")?;
        Ok(())
    }
}

/// The ACP side of the agent's requests: the tool calls permission requests are about, and the
/// turn's requests (permissions, elicitations) not answered yet, which a cancel or the end of
/// the turn cancels. The answer itself comes from whoever handles [`AgentEvent::Permission`] and
/// [`AgentEvent::Elicitation`].
struct Permissions {
    state: Mutex<PermissionState>,
}

#[derive(Default)]
struct PermissionState {
    /// Set from `cancel` until the turn ends: requests crossing the cancel on the wire are
    /// refused at once. Cleared again when the next turn starts.
    cancelling: bool,
    /// Weak, so that a request dropped unanswered is cancelled at once.
    open: Vec<Weak<dyn Open>>,
    /// The turn's tool calls not finished yet, as their updates described them, by id: a
    /// permission request carries only part of its tool call.
    tools: HashMap<String, ToolCallUpdateFields>,
}

impl Permissions {
    fn new() -> Self {
        Self {
            state: Mutex::default(),
        }
    }

    fn state(&self) -> MutexGuard<'_, PermissionState> {
        lock(&self.state)
    }

    /// Remembers what a tool call update says of its call.
    fn track(&self, update: &SessionUpdate) {
        let update = match update {
            SessionUpdate::ToolCall(call) => ToolCallUpdate::from(call.clone()),
            SessionUpdate::ToolCallUpdate(update) => update.clone(),
            _ => return,
        };
        let mut state = self.state();
        let id = update.tool_call_id.0.to_string();
        if finished(update.fields.status).is_some() {
            state.tools.remove(&id);
            return;
        }
        merge(state.tools.entry(id).or_default(), update.fields);
    }

    /// The request as the assistant handles it, and where its answer arrives; `None` when the
    /// turn is being cancelled.
    fn ask(
        &self,
        request: RequestPermissionRequest,
    ) -> Option<(PermissionAsk, oneshot::Receiver<RequestPermissionOutcome>)> {
        let mut state = self.state();
        if state.cancelling {
            return None;
        }
        let id = request.tool_call.tool_call_id.0.to_string();
        let mut tool_call = state.tools.get(&id).cloned().unwrap_or_default();
        merge(&mut tool_call, request.tool_call.fields);
        let (reply, answer) = Reply::new();
        open(&mut state, &reply);
        let ask = PermissionAsk {
            tool_call,
            options: request.options,
            reply,
        };
        Some((ask, answer))
    }

    /// A reply for an elicitation of the turn, and where its answer arrives; `None` when the
    /// turn is being cancelled.
    fn elicit(
        &self,
    ) -> Option<(
        Arc<Reply<CreateElicitationResponse>>,
        oneshot::Receiver<CreateElicitationResponse>,
    )> {
        let mut state = self.state();
        if state.cancelling {
            return None;
        }
        let (reply, answer) = Reply::new();
        open(&mut state, &reply);
        Some((reply, answer))
    }

    fn cancel(&self) {
        let mut state = self.state();
        state.cancelling = true;
        close_open(&mut state);
    }

    fn end_turn(&self) {
        let mut state = self.state();
        state.cancelling = false;
        state.tools.clear();
        close_open(&mut state);
    }
}

/// Keeps `reply` among the turn's open ones, forgetting those answered or dropped.
fn open<T: Unanswered + Send + 'static>(state: &mut PermissionState, reply: &Arc<Reply<T>>) {
    state
        .open
        .retain(|open| open.upgrade().is_some_and(|open| !open.is_answered()));
    let reply: Arc<dyn Open> = reply.clone();
    state.open.push(Arc::downgrade(&reply));
}

fn close_open(state: &mut PermissionState) {
    for reply in state.open.drain(..).filter_map(|reply| reply.upgrade()) {
        reply.close();
    }
}

/// Applies the fields an update sets over those known: ACP updates replace what they name.
fn merge(known: &mut ToolCallUpdateFields, update: ToolCallUpdateFields) {
    let ToolCallUpdateFields {
        kind,
        status,
        title,
        name,
        content,
        locations,
        raw_input,
        raw_output,
        ..
    } = update;
    known.kind = kind.or(known.kind);
    known.status = status.or(known.status);
    known.title = title.or(known.title.take());
    known.name = name.or(known.name.take());
    known.content = content.or(known.content.take());
    known.locations = locations.or(known.locations.take());
    known.raw_input = raw_input.or(known.raw_input.take());
    known.raw_output = raw_output.or(known.raw_output.take());
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
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
        let permissions = Arc::new(Permissions::new());
        let builder = Client
            .v1()
            .on_receive_notification(
                {
                    let (permissions, events) = (permissions.clone(), events.clone());
                    async move |notification: SessionNotification, _cx| {
                        permissions.track(&notification.update);
                        for event in events_from(notification.update) {
                            // A closed receiver only means nobody is listening any more.
                            let _ = events.send(event);
                        }
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                {
                    let (permissions, events) = (permissions.clone(), events.clone());
                    async move |request: RequestPermissionRequest, responder, cx| {
                        let Some((ask, answer)) = permissions.ask(request) else {
                            return responder.respond(RequestPermissionResponse::new(
                                RequestPermissionOutcome::Cancelled,
                            ));
                        };
                        // Sent in order with the agent's updates, so that the card follows the
                        // tool call it is about. Nobody listening drops the reply: cancelled.
                        let _ = events.send(AgentEvent::Permission(Box::new(ask)));
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
            )
            .on_receive_request(
                {
                    let permissions = permissions.clone();
                    async move |request: CreateElicitationRequest, responder, cx| {
                        let cancel = CreateElicitationResponse::unanswered;
                        // Fronde shows forms only, and says so: a URL or another mode the agent
                        // sends anyway is cancelled, the user never saw it.
                        if !matches!(request.mode, ElicitationMode::Form(_)) {
                            tracing::warn!(
                                "the agent sent an elicitation that is not a form: cancelled"
                            );
                            return responder.respond(cancel());
                        }
                        let Some((reply, answer)) = permissions.elicit() else {
                            return responder.respond(cancel());
                        };
                        // The agent withdraws a request with `$/cancel_request`, as the Claude
                        // adapter does when its tool call is aborted.
                        let withdrawn = responder.cancellation();
                        let ask = ElicitationAsk {
                            request,
                            reply: reply.clone(),
                        };
                        // Weak, so that a request dropped unanswered is cancelled at once.
                        let reply = Arc::downgrade(&reply);
                        let _ = events.send(AgentEvent::Elicitation(Box::new(ask)));
                        cx.spawn(async move {
                            let response = tokio::select! {
                                answer = answer => answer.unwrap_or_else(|_| cancel()),
                                () = withdrawn.cancelled() => {
                                    // The card closes; a later answer goes nowhere.
                                    if let Some(reply) = reply.upgrade() {
                                        reply.send(cancel());
                                    }
                                    cancel()
                                }
                            };
                            responder.respond(response)
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
                        .client_info(Implementation::new("fronde", env!("CARGO_PKG_VERSION")))
                        .client_capabilities(client_capabilities()),
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

    /// `authorization`: the value of the session's MCP header towards the hub.
    async fn open_session(
        self,
        spec: &AgentSpec,
        authorization: String,
        resume: Option<String>,
    ) -> anyhow::Result<AgentClient> {
        let Self {
            connection,
            capabilities,
            permissions,
            stop,
        } = self;
        let hub = McpServer::Http(
            McpServerHttp::new("fronde", &spec.hub_url)
                .headers(vec![HttpHeader::new("Authorization", authorization)]),
        );
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
            _scope: None,
        })
    }
}

/// What Fronde shows of the agent's requests besides permissions: forms. Advertising them makes
/// the Claude adapter turn `AskUserQuestion` into a form instead of disallowing it. Not URLs:
/// nothing needs one yet (an MCP server's sign-in), and the adapter forwards those only to a
/// client that advertises them.
fn client_capabilities() -> ClientCapabilities {
    ClientCapabilities::new()
        .elicitation(ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()))
}

/// Claude Code's own tool that asks the user questions: the adapter turns it into a form
/// elicitation, which becomes a card. Enabled, never allowed up front, so that every use goes
/// through the adapter's permission check, which is where it asks.
pub const ASK_USER_QUESTION: &str = "AskUserQuestion";

/// Fronde's agent gets the hub's tools and the listed built-in ones, all allowed up front, and
/// [`ASK_USER_QUESTION`]. The Claude Code adapter loads the user's, project's and local settings
/// unless told otherwise, and with them the owner's plugins, MCP servers, hooks and allow rules:
/// none of them apply. The sign-in is not a setting and still works.
fn session_meta(builtin_tools: &[String]) -> serde_json::Map<String, serde_json::Value> {
    let allowed: Vec<&str> = std::iter::once("mcp__fronde")
        .chain(builtin_tools.iter().map(String::as_str))
        .filter(|tool| *tool != ASK_USER_QUESTION)
        .collect();
    let tools: Vec<&str> = builtin_tools
        .iter()
        .map(String::as_str)
        .filter(|tool| *tool != ASK_USER_QUESTION)
        .chain([ASK_USER_QUESTION])
        .collect();
    // The adapter prefers `tools` over `disableBuiltInTools`; the flag stays so that an adapter
    // ignoring `tools` falls back to no built-in tool rather than all of them.
    let serde_json::Value::Object(meta) = json!({
        "disableBuiltInTools": true,
        "claudeCode": {"options": {
            "tools": tools,
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
