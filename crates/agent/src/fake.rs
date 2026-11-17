//! In-process fake ACP agent for tests: it replays a script and records what the client sent.

use agent_client_protocol::v1::schema::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, CreateElicitationRequest,
    CreateElicitationResponse, ElicitationAction, InitializeRequest, InitializeResponse,
    NewSessionRequest, NewSessionResponse, PermissionOption, PermissionOptionKind, PromptRequest,
    PromptResponse, RequestPermissionOutcome, RequestPermissionRequest, ResumeSessionRequest,
    ResumeSessionResponse, SessionCapabilities, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOption, SessionNotification, SessionResumeCapabilities,
    SessionSystemPromptCapabilities, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse, StopReason,
    TextContent, ToolCallUpdate, ToolCallUpdateFields, UsageUpdate,
};
use agent_client_protocol::{Agent, Channel, Client, ConnectTo, Error};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// The session id the fake agent gives to new sessions, unless it counts them.
pub const FAKE_SESSION_ID: &str = "fake-session-1";

/// The models the fake agent offers with [`FakeScript::models`], the first one current.
pub const FAKE_MODELS: [&str; 2] = ["fake-small", "fake-large"];

/// The id of the fake agent's model option.
pub const FAKE_MODEL_OPTION: &str = "model";

/// What the fake agent does on each prompt, and which capabilities it advertises.
#[derive(Clone, Debug)]
pub struct FakeScript {
    /// Updates streamed during each prompt, before any permission request.
    pub updates: Vec<SessionUpdate>,
    /// Ask one permission to use the tool `Read` (options `allow-once`, `allow-with-updates`,
    /// `reject`, as the Claude adapter offers them) during each prompt.
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
    /// Answer each prompt with an internal error carrying this `data`, after the updates, as the
    /// Claude adapter fails a turn ([`rate_limit_error`] for one refused for a limit).
    pub fail_prompt_data: Option<serde_json::Value>,
    /// Answer each prompt with ACP's `auth_required` error, as a signed-out Claude does.
    pub signed_out: bool,
    /// Never answer a prompt, even once it is cancelled, after the updates: a wedged agent.
    pub ignore_cancel: bool,
    /// After this many updates of each prompt, wait for the notify before sending the rest.
    pub pause: Option<(usize, Arc<Notify>)>,
    /// Given a permit each time a prompt reaches `pause`, before it waits there: a test knows
    /// the turn is held.
    pub paused: Option<Arc<tokio::sync::Semaphore>>,
    /// Ask this `elicitation/create` during each prompt, after the permission if any, its
    /// `sessionId` filled in unless it has one: the JSON the agent would send, such as the Claude
    /// adapter's form for `AskUserQuestion`.
    pub elicitation: Option<serde_json::Value>,
    /// End the turn without waiting for the elicitation's answer.
    pub leave_elicitation_pending: bool,
    /// Withdraw the elicitation with `$/cancel_request` once notified, as the Claude adapter does
    /// when the tool call that asked is aborted.
    pub withdraw_elicitation: Option<Arc<Notify>>,
    /// Offer a model option ([`FAKE_MODELS`]) in the config options of `session/new` and
    /// `session/resume`, as the Claude adapter does, starting on the first model.
    pub models: bool,
    /// Number new sessions `fake-session-<n>` from this counter, shared by every agent playing
    /// the script, instead of always giving [`FAKE_SESSION_ID`].
    pub counted_sessions: Option<Arc<AtomicU64>>,
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
            fail_prompt_data: None,
            signed_out: false,
            ignore_cancel: false,
            pause: None,
            paused: None,
            elicitation: None,
            leave_elicitation_pending: false,
            withdraw_elicitation: None,
            models: false,
            counted_sessions: None,
        }
    }
}

/// An agent message chunk carrying `text`.
pub fn text_chunk(text: &str) -> SessionUpdate {
    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
        text,
    ))))
}

/// A `usage_update` carrying `rate_limit` (a CLI `rate_limit_info`) as the Claude adapter sends
/// it after a `rate_limit_event`: in `_meta["_claude/rateLimit"]`, beside the model.
pub fn usage_update(rate_limit: serde_json::Value) -> SessionUpdate {
    let serde_json::Value::Object(meta) = serde_json::json!({
        "_claude/rateLimit": rate_limit,
        "_claude/model": "claude-opus-4-5",
    }) else {
        unreachable!("the literal is an object")
    };
    SessionUpdate::UsageUpdate(UsageUpdate::new(53_000, 200_000).meta(meta))
}

/// The `data` of the Claude adapter's error for a turn refused for a limit (`turnErrorData`):
/// its kind and the turn's latest `rate_limit_info`.
pub fn rate_limit_error(rate_limit: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"errorKind": "rate_limit", "rateLimit": rate_limit})
}

/// Everything the client sent to the fake agent.
#[derive(Clone, Debug, Default)]
pub struct FakeRecord {
    pub new_sessions: Vec<NewSessionRequest>,
    pub resumes: Vec<ResumeSessionRequest>,
    pub modes: Vec<String>,
    /// Each `session/set_config_option`: the option's id and the value asked.
    pub config_sets: Vec<(String, String)>,
    /// The text of each prompt, its text blocks joined.
    pub prompts: Vec<String>,
    pub permission_outcomes: Vec<RequestPermissionOutcome>,
    /// What each elicitation got: the response, or the error.
    pub elicitation_responses: Vec<Result<CreateElicitationResponse, String>>,
    /// The client's `initialize` requests, with its capabilities.
    pub initializes: Vec<InitializeRequest>,
    pub cancels: usize,
    /// The client closed the connection.
    pub closed: bool,
}

/// Reads what the fake agent recorded.
#[derive(Clone, Debug)]
pub struct FakeHandle {
    record: Arc<Mutex<FakeRecord>>,
    exit: tokio_util::sync::CancellationToken,
}

impl FakeHandle {
    pub fn record(&self) -> FakeRecord {
        lock(&self.record).clone()
    }

    /// Ends the fake agent on its own, as a process that exits or crashes: its end of the
    /// connection closes.
    pub fn exit(&self) {
        self.exit.cancel();
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
    let exit = tokio_util::sync::CancellationToken::new();
    let handle = FakeHandle {
        record: record.clone(),
        exit: exit.clone(),
    };
    let agent = scripted(script, record.clone());
    tokio::spawn(async move {
        tokio::select! {
            _ = agent.connect_to(agent_end) => {}
            () = exit.cancelled() => {}
        }
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
                let record = record.clone();
                async move |request: InitializeRequest, responder, _cx| {
                    lock(&record).initializes.push(request.clone());
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
                let script = script.clone();
                async move |request: NewSessionRequest, responder, _cx| {
                    lock(&record).new_sessions.push(request);
                    let id = match &script.counted_sessions {
                        Some(counter) => {
                            format!(
                                "fake-session-{}",
                                counter.fetch_add(1, Ordering::Relaxed) + 1
                            )
                        }
                        None => FAKE_SESSION_ID.to_owned(),
                    };
                    responder.respond(
                        NewSessionResponse::new(id).config_options(config_options(&script, None)),
                    )
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
                        // A resumed session starts again on the first model.
                        responder.respond(
                            ResumeSessionResponse::new()
                                .config_options(config_options(&script, None)),
                        )
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let record = record.clone();
                let script = script.clone();
                async move |request: SetSessionConfigOptionRequest, responder, _cx| {
                    let value = request
                        .value
                        .as_value_id()
                        .map(|value| value.0.to_string())
                        .unwrap_or_default();
                    lock(&record)
                        .config_sets
                        .push((request.config_id.0.to_string(), value.clone()));
                    if !script.models
                        || &*request.config_id.0 != FAKE_MODEL_OPTION
                        || !FAKE_MODELS.contains(&value.as_str())
                    {
                        return responder.respond_with_error(Error::invalid_params());
                    }
                    let options = config_options(&script, Some(&value)).unwrap_or_default();
                    responder.respond(SetSessionConfigOptionResponse::new(options))
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
                    let text: String = request
                        .prompt
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect();
                    lock(&record).prompts.push(text);
                    // Waiting for the permission answer inside the handler would block the
                    // dispatch loop that delivers it.
                    cx.clone().spawn(async move {
                        let session = request.session_id;
                        for (index, update) in script.updates.iter().enumerate() {
                            if let Some((after, gate)) = &script.pause
                                && index == *after
                            {
                                // Listening first: a release right after the signal is not
                                // missed.
                                let released = gate.notified();
                                tokio::pin!(released);
                                released.as_mut().enable();
                                if let Some(paused) = &script.paused {
                                    paused.add_permits(1);
                                }
                                released.await;
                            }
                            cx.send_notification(SessionNotification::new(
                                session.clone(),
                                update.clone(),
                            ))?;
                        }
                        if script.fail_prompt {
                            return responder.respond_with_error(Error::internal_error());
                        }
                        if let Some(data) = &script.fail_prompt_data {
                            return responder
                                .respond_with_error(Error::internal_error().data(data.clone()));
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
                            let asked = cx.send_request(permission_request(session.clone()));
                            if script.leave_permission_pending {
                                let record = record.clone();
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
                        if let Some(form) = &script.elicitation {
                            let mut form = form.clone();
                            if form.get("sessionId").is_none() {
                                form["sessionId"] =
                                    serde_json::Value::String(session.0.to_string());
                            }
                            let request: CreateElicitationRequest = serde_json::from_value(form)
                                .map_err(|error| Error::invalid_params().data(error.to_string()))?;
                            let asked = cx.send_request(request);
                            if let Some(gate) = &script.withdraw_elicitation {
                                gate.notified().await;
                                asked.cancel()?;
                            }
                            if script.leave_elicitation_pending {
                                let record = record.clone();
                                cx.spawn(async move {
                                    let response = asked.block_task().await;
                                    lock(&record)
                                        .elicitation_responses
                                        .push(response.map_err(|error| error.to_string()));
                                    Ok(())
                                })?;
                            } else {
                                let response = asked.block_task().await;
                                if response.as_ref().is_ok_and(|response| {
                                    response.action == ElicitationAction::Cancel
                                }) {
                                    stop = StopReason::Cancelled;
                                }
                                lock(&record)
                                    .elicitation_responses
                                    .push(response.map_err(|error| error.to_string()));
                            }
                        }
                        responder.respond(PromptResponse::new(stop))
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
}

/// The fake agent's config options with `current` as the model (the first when `None`), or none
/// when the script offers no model.
fn config_options(script: &FakeScript, current: Option<&str>) -> Option<Vec<SessionConfigOption>> {
    if !script.models {
        return None;
    }
    let options: Vec<SessionConfigSelectOption> = FAKE_MODELS
        .iter()
        .map(|model| SessionConfigSelectOption::new(*model, model.replace('-', " ")))
        .collect();
    let current = current.unwrap_or(FAKE_MODELS[0]).to_owned();
    Some(vec![
        SessionConfigOption::select(FAKE_MODEL_OPTION, "Model", current, options)
            .category(SessionConfigOptionCategory::Model),
    ])
}

fn permission_request(
    session: agent_client_protocol::v1::schema::SessionId,
) -> RequestPermissionRequest {
    RequestPermissionRequest::new(
        session,
        ToolCallUpdate::new(
            "tool-1",
            ToolCallUpdateFields::new()
                .title("Lire l'agenda".to_owned())
                .name("Read".to_owned()),
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
