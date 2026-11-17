//! In-process fake ACP agent for tests: it replays a script and records what the client sent.

use agent_client_protocol::v1::schema::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, InitializeRequest,
    InitializeResponse, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities,
    SessionNotification, SessionResumeCapabilities, SessionSystemPromptCapabilities, SessionUpdate,
    SetSessionModeRequest, SetSessionModeResponse, StopReason, TextContent, ToolCallUpdate,
    ToolCallUpdateFields,
};
use agent_client_protocol::{Agent, Channel, Client, ConnectTo, Error};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// The session id the fake agent gives to new sessions.
pub const FAKE_SESSION_ID: &str = "fake-session-1";

/// What the fake agent does on each prompt, and which capabilities it advertises.
#[derive(Clone, Debug)]
pub struct FakeScript {
    /// Updates streamed during each prompt, before any permission request.
    pub updates: Vec<SessionUpdate>,
    /// Ask one permission (options `allow-once`, `allow-with-updates`, `reject`) during each prompt.
    pub ask_permission: bool,
    /// Stop reason of each prompt, unless the permission request was cancelled.
    pub stop: StopReason,
    /// Advertise `session/resume`.
    pub resume: bool,
    /// Answer `session/resume` with an error.
    pub refuse_resume: bool,
    /// Wait for the client's cancel notification before asking the permission.
    pub wait_for_cancel: bool,
    /// End the turn without waiting for the permission answer.
    pub leave_permission_pending: bool,
    /// Answer each prompt with an error, after the updates.
    pub fail_prompt: bool,
    /// Answer each prompt with ACP's `auth_required` error, as a signed-out Claude does.
    pub signed_out: bool,
    /// Never answer a prompt, even once it is cancelled, after the updates: a wedged agent.
    pub ignore_cancel: bool,
    /// After this many updates of each prompt, wait for the notify before sending the rest.
    pub pause: Option<(usize, Arc<Notify>)>,
}

impl Default for FakeScript {
    fn default() -> Self {
        Self {
            updates: Vec::new(),
            ask_permission: false,
            stop: StopReason::EndTurn,
            resume: false,
            refuse_resume: false,
            wait_for_cancel: false,
            leave_permission_pending: false,
            fail_prompt: false,
            signed_out: false,
            ignore_cancel: false,
            pause: None,
        }
    }
}

/// An agent message chunk carrying `text`.
pub fn text_chunk(text: &str) -> SessionUpdate {
    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
        text,
    ))))
}

/// Everything the client sent to the fake agent.
#[derive(Clone, Debug, Default)]
pub struct FakeRecord {
    pub new_sessions: Vec<NewSessionRequest>,
    pub resumes: Vec<ResumeSessionRequest>,
    pub modes: Vec<String>,
    pub permission_outcomes: Vec<RequestPermissionOutcome>,
    pub cancels: usize,
    /// The client closed the connection.
    pub closed: bool,
}

/// Reads what the fake agent recorded.
#[derive(Clone, Debug)]
pub struct FakeHandle {
    record: Arc<Mutex<FakeRecord>>,
}

impl FakeHandle {
    pub fn record(&self) -> FakeRecord {
        lock(&self.record).clone()
    }
}

fn lock(record: &Mutex<FakeRecord>) -> std::sync::MutexGuard<'_, FakeRecord> {
    record
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Start a fake agent on the current tokio runtime; connect a client to the returned transport.
pub fn fake_agent(script: FakeScript) -> (impl ConnectTo<Client>, FakeHandle) {
    let (client_end, agent_end) = Channel::duplex();
    let record = Arc::new(Mutex::new(FakeRecord::default()));
    let handle = FakeHandle {
        record: record.clone(),
    };
    let agent = scripted(script, record.clone());
    tokio::spawn(async move {
        let _ = agent.connect_to(agent_end).await;
        lock(&record).closed = true;
    });
    (client_end, handle)
}

/// Serve the fake agent on this process's standard input and output, as a real agent does,
/// until its input ends: for tests that start an agent process.
pub async fn serve_on_stdio(script: FakeScript) -> Result<(), Error> {
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
    let stdio = agent_client_protocol::ByteStreams::new(
        tokio::io::stdout().compat_write(),
        tokio::io::stdin().compat(),
    );
    scripted(script, Arc::default()).connect_to(stdio).await
}

/// The fake agent, replaying `script` and writing what it receives to `record`.
fn scripted(script: FakeScript, record: Arc<Mutex<FakeRecord>>) -> impl ConnectTo<Client> {
    let script = Arc::new(script);
    let cancelled = Arc::new(Notify::new());

    Agent
        .v1()
        .on_receive_request(
            {
                let script = script.clone();
                async move |request: InitializeRequest, responder, _cx| {
                    let mut sessions = SessionCapabilities::new()
                        .system_prompt(SessionSystemPromptCapabilities::new());
                    if script.resume {
                        sessions = sessions.resume(SessionResumeCapabilities::new());
                    }
                    responder.respond(
                        InitializeResponse::new(request.protocol_version).agent_capabilities(
                            AgentCapabilities::new().session_capabilities(sessions),
                        ),
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let record = record.clone();
                async move |request: NewSessionRequest, responder, _cx| {
                    lock(&record).new_sessions.push(request);
                    responder.respond(NewSessionResponse::new(FAKE_SESSION_ID))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let record = record.clone();
                let script = script.clone();
                async move |request: ResumeSessionRequest, responder, _cx| {
                    lock(&record).resumes.push(request);
                    if script.refuse_resume {
                        responder.respond_with_error(Error::invalid_params())
                    } else {
                        responder.respond(ResumeSessionResponse::new())
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let record = record.clone();
                async move |request: SetSessionModeRequest, responder, _cx| {
                    lock(&record).modes.push(request.mode_id.0.to_string());
                    responder.respond(SetSessionModeResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let record = record.clone();
                let cancelled = cancelled.clone();
                async move |_notification: CancelNotification, _cx| {
                    lock(&record).cancels += 1;
                    cancelled.notify_one();
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let record = record.clone();
                let script = script.clone();
                async move |request: PromptRequest, responder, cx| {
                    let record = record.clone();
                    let script = script.clone();
                    let cancelled = cancelled.clone();
                    // Waiting for the permission answer inside the handler would block the
                    // dispatch loop that delivers it.
                    cx.clone().spawn(async move {
                        let session = request.session_id;
                        for (index, update) in script.updates.iter().enumerate() {
                            if let Some((after, gate)) = &script.pause
                                && index == *after
                            {
                                gate.notified().await;
                            }
                            cx.send_notification(SessionNotification::new(
                                session.clone(),
                                update.clone(),
                            ))?;
                        }
                        if script.fail_prompt {
                            return responder.respond_with_error(Error::internal_error());
                        }
                        if script.signed_out {
                            return responder.respond_with_error(Error::auth_required());
                        }
                        if script.ignore_cancel {
                            std::future::pending::<()>().await;
                        }
                        let mut stop = script.stop;
                        if script.ask_permission {
                            if script.wait_for_cancel {
                                cancelled.notified().await;
                            }
                            let asked = cx.send_request(permission_request(session));
                            if script.leave_permission_pending {
                                cx.spawn(async move {
                                    let outcome = asked.block_task().await?.outcome;
                                    lock(&record).permission_outcomes.push(outcome);
                                    Ok(())
                                })?;
                            } else {
                                let outcome = asked.block_task().await?.outcome;
                                if outcome == RequestPermissionOutcome::Cancelled {
                                    stop = StopReason::Cancelled;
                                }
                                lock(&record).permission_outcomes.push(outcome);
                            }
                        }
                        responder.respond(PromptResponse::new(stop))
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
}

fn permission_request(
    session: agent_client_protocol::v1::schema::SessionId,
) -> RequestPermissionRequest {
    RequestPermissionRequest::new(
        session,
        ToolCallUpdate::new(
            "tool-1",
            ToolCallUpdateFields::new().title("Lire l'agenda".to_owned()),
        ),
        vec![
            PermissionOption::new("allow-once", "Allow", PermissionOptionKind::AllowOnce),
            PermissionOption::new(
                "allow-with-updates",
                "Always allow",
                PermissionOptionKind::AllowAlways,
            ),
            PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
        ],
    )
}
