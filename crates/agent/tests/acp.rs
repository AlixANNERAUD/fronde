use agent_client_protocol::v1::schema::{
    ContentBlock, McpServer, RequestPermissionOutcome, SelectedPermissionOutcome, SessionUpdate,
    SystemPromptMode, ToolCall, ToolCallLocation, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::v1::schema::{
    CreateElicitationResponse, ElicitationAcceptAction, ElicitationAction, ElicitationContentValue,
    SessionConfigKind,
};
use fronde_agent::acp::{
    AgentClient, AgentEvent, AgentSpec, ElicitationAsk, Failure, PermissionAsk, TurnEnd,
};
use fronde_agent::fake::{
    FAKE_MODEL_OPTION, FAKE_MODELS, FAKE_SESSION_ID, FakeHandle, FakeScript, fake_agent, text_chunk,
};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

fn spec() -> AgentSpec {
    AgentSpec {
        thread: fronde_protocol::ThreadId::MAIN,
        provider: &fronde_agent::provider::Provider::CLAUDE,
        command: vec!["claude-agent-acp".into()],
        env: Vec::new(),
        cwd: "/srv/fronde/agent".into(),
        home: "/srv/fronde/home".into(),
        hub_url: "http://127.0.0.1:7878/mcp".into(),
        hub_token: "hub-token".into(),
        oauth_token: None,
        system_prompt: "You are Fronde.".into(),
        builtin_tools: vec!["WebSearch".into(), "WebFetch".into()],
        usage: None,
        account: "claude".into(),
    }
}

async fn connect(
    script: FakeScript,
    resume: Option<&str>,
) -> (AgentClient, FakeHandle, mpsc::UnboundedReceiver<AgentEvent>) {
    connect_with(&spec(), script, resume).await
}

async fn connect_with(
    spec: &AgentSpec,
    script: FakeScript,
    resume: Option<&str>,
) -> (AgentClient, FakeHandle, mpsc::UnboundedReceiver<AgentEvent>) {
    let (transport, handle) = fake_agent(script);
    let (events, rx) = mpsc::unbounded_channel();
    let client = timeout(
        WAIT,
        AgentClient::connect(transport, spec, resume.map(str::to_owned), events),
    )
    .await
    .expect("connect timed out")
    .expect("connect failed");
    (client, handle, rx)
}

async fn prompt(client: &AgentClient, text: &str) -> TurnEnd {
    timeout(WAIT, client.prompt(text))
        .await
        .expect("prompt timed out")
        .expect("prompt failed")
}

/// Waits for the next permission request and returns it.
async fn permission_asked(rx: &mut mpsc::UnboundedReceiver<AgentEvent>) -> PermissionAsk {
    loop {
        let event = timeout(WAIT, rx.recv())
            .await
            .expect("no permission request")
            .expect("event channel closed");
        if let AgentEvent::Permission(ask) = event {
            assert_eq!(ask.tool_call.title.as_deref(), Some("Lire l'agenda"));
            return *ask;
        }
    }
}

fn drain(rx: &mut mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

fn selected(option_id: &str) -> RequestPermissionOutcome {
    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id.to_owned()))
}

/// The hub and the web tools, allowed up front, `AskUserQuestion` enabled but never allowed up
/// front, and nothing from the owner's Claude settings: no plugins, MCP servers, hooks or allow
/// rules, and no bypass mode.
fn expected_meta() -> serde_json::Value {
    json!({
        "disableBuiltInTools": true,
        "claudeCode": {"options": {
            "tools": ["WebSearch", "WebFetch", "AskUserQuestion"],
            "allowedTools": ["mcp__fronde", "WebSearch", "WebFetch"],
            "settingSources": [],
            "strictMcpConfig": true,
            "allowDangerouslySkipPermissions": false,
        }},
    })
}

#[tokio::test]
async fn new_session_sends_hub_and_meta() {
    let (client, handle, _rx) = connect(FakeScript::default(), None).await;

    assert_eq!(client.session_id(), FAKE_SESSION_ID);
    let record = handle.record();
    assert_eq!(record.new_sessions.len(), 1);
    let new = &record.new_sessions[0];
    assert_eq!(new.cwd, PathBuf::from("/srv/fronde/agent"));
    let [McpServer::Http(hub)] = new.mcp_servers.as_slice() else {
        panic!("expected only the hub, got {:?}", new.mcp_servers);
    };
    assert_eq!(hub.name, "fronde");
    assert_eq!(hub.url, "http://127.0.0.1:7878/mcp");
    assert_eq!(hub.headers.len(), 1);
    assert_eq!(hub.headers[0].name, "Authorization");
    assert_eq!(hub.headers[0].value, "Bearer hub-token");
    let meta = serde_json::Value::Object(new.meta.clone().expect("no _meta"));
    assert_eq!(meta, expected_meta());
    let system_prompt = new.system_prompt.as_ref().expect("no system prompt");
    assert_eq!(system_prompt.mode, SystemPromptMode::Append);
    assert_eq!(
        system_prompt.content,
        vec![ContentBlock::from("You are Fronde.")]
    );
    assert_eq!(record.modes, vec!["default"]);
    assert!(record.resumes.is_empty());
}

#[tokio::test]
async fn no_builtin_tools_leaves_the_hub_and_ask_user_question() {
    let spec = AgentSpec {
        builtin_tools: Vec::new(),
        ..spec()
    };
    let (_client, handle, _rx) = connect_with(&spec, FakeScript::default(), None).await;

    let record = handle.record();
    let meta = serde_json::Value::Object(record.new_sessions[0].meta.clone().expect("no _meta"));
    assert_eq!(
        meta,
        // Only AskUserQuestion besides the hub, never allowed up front.
        json!({
            "disableBuiltInTools": true,
            "claudeCode": {"options": {
                "tools": ["AskUserQuestion"],
                "allowedTools": ["mcp__fronde"],
                "settingSources": [],
                "strictMcpConfig": true,
                "allowDangerouslySkipPermissions": false,
            }},
        })
    );
}

#[tokio::test]
async fn prompt_streams_text_then_done() {
    let script = FakeScript {
        updates: vec![
            text_chunk("Bon"),
            SessionUpdate::ToolCall(ToolCall::new("t1", "Chercher dans la mémoire")),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "t1",
                ToolCallUpdateFields::new().status(ToolCallStatus::InProgress),
            )),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "t1",
                ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
            )),
            SessionUpdate::ToolCall(ToolCall::new("t2", "Lire l'agenda")),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "t2",
                ToolCallUpdateFields::new().status(ToolCallStatus::Failed),
            )),
            text_chunk("jour"),
        ],
        ..FakeScript::default()
    };
    let (client, _handle, mut rx) = connect(script, None).await;

    assert_eq!(prompt(&client, "Salut").await, TurnEnd::Done);

    assert_eq!(
        drain(&mut rx),
        vec![
            AgentEvent::Text("Bon".into()),
            AgentEvent::ToolStarted {
                id: "t1".into(),
                title: "Chercher dans la mémoire".into()
            },
            AgentEvent::ToolFinished {
                id: "t1".into(),
                ok: true
            },
            AgentEvent::ToolStarted {
                id: "t2".into(),
                title: "Lire l'agenda".into()
            },
            AgentEvent::ToolFinished {
                id: "t2".into(),
                ok: false
            },
            AgentEvent::Text("jour".into()),
        ]
    );
}

#[tokio::test]
async fn permission_round_trip() {
    let script = FakeScript {
        ask_permission: true,
        ..FakeScript::default()
    };
    let (client, handle, mut rx) = connect(script, None).await;
    let client = Arc::new(client);

    let turn = tokio::spawn({
        let client = client.clone();
        async move { client.prompt("Mon agenda ?").await }
    });
    let first = permission_asked(&mut rx).await;
    assert!(first.reply.send(selected("allow-once")), "answer allow");
    let end = timeout(WAIT, turn).await.expect("turn timed out");
    assert_eq!(end.expect("turn panicked").expect("prompt"), TurnEnd::Done);

    let turn = tokio::spawn({
        let client = client.clone();
        async move { client.prompt("Encore ?").await }
    });
    let second = permission_asked(&mut rx).await;
    assert_ne!(second.reply, first.reply);
    assert!(second.reply.send(selected("reject")), "answer deny");
    let end = timeout(WAIT, turn).await.expect("turn timed out");
    assert_eq!(end.expect("turn panicked").expect("prompt"), TurnEnd::Done);

    assert_eq!(
        handle.record().permission_outcomes,
        vec![selected("allow-once"), selected("reject")]
    );
    assert!(
        !first.reply.send(selected("allow-once")),
        "an answered permission cannot be answered again"
    );
    // The agent's own options, as it offered them.
    let ids: Vec<String> = first
        .options
        .iter()
        .map(|option| option.option_id.0.to_string())
        .collect();
    assert_eq!(ids, ["allow-once", "allow-with-updates", "reject"]);
}

#[tokio::test]
async fn permission_carries_what_the_tool_call_said_before() {
    let script = FakeScript {
        updates: vec![
            SessionUpdate::ToolCall(
                ToolCall::new("tool-1", "Lire")
                    .kind(ToolKind::Read)
                    .locations(vec![ToolCallLocation::new("/home/alix/agenda.ics")]),
            ),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "tool-1",
                ToolCallUpdateFields::new().raw_input(json!({"path": "/home/alix/agenda.ics"})),
            )),
        ],
        ask_permission: true,
        ..FakeScript::default()
    };
    let (client, _handle, mut rx) = connect(script, None).await;
    let client = Arc::new(client);
    let turn = tokio::spawn({
        let client = client.clone();
        async move { client.prompt("Mon agenda ?").await }
    });

    let ask = permission_asked(&mut rx).await;
    // The request's own title wins over the earlier one; the rest comes from the updates.
    assert_eq!(ask.tool_call.kind, Some(ToolKind::Read));
    assert_eq!(
        ask.tool_call.locations,
        Some(vec![ToolCallLocation::new("/home/alix/agenda.ics")])
    );
    assert_eq!(
        ask.tool_call.raw_input,
        Some(json!({"path": "/home/alix/agenda.ics"}))
    );
    assert!(ask.reply.send(selected("allow-once")));
    let end = timeout(WAIT, turn).await.expect("turn timed out");
    assert_eq!(end.expect("turn panicked").expect("prompt"), TurnEnd::Done);
}

#[tokio::test]
async fn dropped_permission_is_cancelled() {
    let script = FakeScript {
        ask_permission: true,
        ..FakeScript::default()
    };
    let (client, handle, mut rx) = connect(script, None).await;
    let client = Arc::new(client);
    let turn = tokio::spawn({
        let client = client.clone();
        async move { client.prompt("Mon agenda ?").await }
    });
    // Whoever handles the request gives up on it without an answer.
    drop(permission_asked(&mut rx).await);
    let end = timeout(WAIT, turn).await.expect("turn timed out");
    assert_eq!(
        end.expect("turn panicked").expect("prompt"),
        TurnEnd::Cancelled
    );
    assert_eq!(
        handle.record().permission_outcomes,
        vec![RequestPermissionOutcome::Cancelled]
    );
}

#[tokio::test]
async fn cancel_resolves_pending_permission() {
    let script = FakeScript {
        ask_permission: true,
        ..FakeScript::default()
    };
    let (client, handle, mut rx) = connect(script, None).await;
    let client = Arc::new(client);

    let turn = tokio::spawn({
        let client = client.clone();
        async move { client.prompt("Mon agenda ?").await }
    });
    let ask = permission_asked(&mut rx).await;
    client.cancel().expect("cancel");
    let end = timeout(WAIT, turn).await.expect("turn timed out");
    assert_eq!(
        end.expect("turn panicked").expect("prompt"),
        TurnEnd::Cancelled
    );

    let record = handle.record();
    assert_eq!(record.cancels, 1);
    assert_eq!(
        record.permission_outcomes,
        vec![RequestPermissionOutcome::Cancelled]
    );
    assert!(
        !ask.reply.send(selected("allow-once")),
        "a cancelled permission cannot be answered"
    );
}

#[tokio::test]
async fn resume_used_when_given() {
    let script = FakeScript {
        resume: true,
        ..FakeScript::default()
    };
    let (client, handle, _rx) = connect(script, Some("earlier-session")).await;

    assert_eq!(client.session_id(), "earlier-session");
    let record = handle.record();
    assert!(record.new_sessions.is_empty());
    assert_eq!(record.resumes.len(), 1);
    let resume = &record.resumes[0];
    assert_eq!(&*resume.session_id.0, "earlier-session");
    assert_eq!(resume.cwd, PathBuf::from("/srv/fronde/agent"));
    assert!(matches!(
        resume.mcp_servers.as_slice(),
        [McpServer::Http(hub)] if hub.name == "fronde"
    ));
    let meta = serde_json::Value::Object(resume.meta.clone().expect("no _meta"));
    assert_eq!(meta, expected_meta());
    assert_eq!(record.modes, vec!["default"]);
}

#[tokio::test]
async fn resume_falls_back_to_a_new_session() {
    // Refused by the agent.
    let script = FakeScript {
        resume: true,
        refuse_resume: true,
        ..FakeScript::default()
    };
    let (client, handle, _rx) = connect(script, Some("earlier-session")).await;
    assert_eq!(client.session_id(), FAKE_SESSION_ID);
    let record = handle.record();
    assert_eq!(record.resumes.len(), 1);
    assert_eq!(record.new_sessions.len(), 1);

    // Not advertised by the agent.
    let (client, handle, _rx) = connect(FakeScript::default(), Some("earlier-session")).await;
    assert_eq!(client.session_id(), FAKE_SESSION_ID);
    let record = handle.record();
    assert!(record.resumes.is_empty());
    assert_eq!(record.new_sessions.len(), 1);
}

#[tokio::test]
async fn agent_start_failure_is_reported() {
    let spec = AgentSpec {
        command: vec!["/nonexistent".into()],
        ..spec()
    };
    let (events, _rx) = mpsc::unbounded_channel();

    let result = timeout(WAIT, AgentClient::spawn(&spec, None, events))
        .await
        .expect("spawn timed out");

    let error = result.err().expect("spawn should fail");
    assert_eq!(Failure::of(&error), Failure::NotInstalled);
    let error = format!("{error:#}");
    assert!(error.contains("/nonexistent"), "{error}");
    assert!(error.contains("mise run agent"), "{error}");
}

#[tokio::test]
async fn installed_agent_that_fails_to_start_gets_no_install_hint() {
    // Found on the PATH, but not an ACP agent: it exits at once.
    let spec = AgentSpec {
        command: vec!["true".into()],
        ..spec()
    };
    let (events, _rx) = mpsc::unbounded_channel();

    let result = timeout(WAIT, AgentClient::spawn(&spec, None, events))
        .await
        .expect("spawn timed out");

    let error = result.err().expect("spawn should fail");
    assert_eq!(Failure::of(&error), Failure::Other);
    let error = format!("{error:#}");
    assert!(error.contains("cannot start the agent (true)"), "{error}");
    assert!(!error.contains("npm"), "{error}");
}

#[tokio::test]
async fn signed_out_agent_is_recognised() {
    let script = FakeScript {
        signed_out: true,
        ..FakeScript::default()
    };
    let (client, _handle, _rx) = connect(script, None).await;

    let error = timeout(WAIT, client.prompt("Salut"))
        .await
        .expect("prompt timed out")
        .expect_err("the prompt should fail");

    assert_eq!(Failure::of(&error), Failure::SignedOut);
    let failed = connect(
        FakeScript {
            fail_prompt: true,
            ..FakeScript::default()
        },
        None,
    )
    .await
    .0;
    let error = failed
        .prompt("Salut")
        .await
        .expect_err("the prompt should fail");
    assert_eq!(Failure::of(&error), Failure::Other);
}

#[tokio::test]
async fn permission_after_cancel_is_cancelled() {
    let script = FakeScript {
        updates: vec![text_chunk("Je regarde")],
        ask_permission: true,
        wait_for_cancel: true,
        ..FakeScript::default()
    };
    let (client, handle, mut rx) = connect(script, None).await;
    let client = Arc::new(client);

    let turn = tokio::spawn({
        let client = client.clone();
        async move { client.prompt("Mon agenda ?").await }
    });
    // The prompt has reached the agent once its first update is back.
    let first = timeout(WAIT, rx.recv()).await.expect("no update");
    assert_eq!(first, Some(AgentEvent::Text("Je regarde".into())));
    client.cancel().expect("cancel");
    let end = timeout(WAIT, turn).await.expect("turn timed out");
    assert_eq!(
        end.expect("turn panicked").expect("prompt"),
        TurnEnd::Cancelled
    );

    assert_eq!(
        handle.record().permission_outcomes,
        vec![RequestPermissionOutcome::Cancelled]
    );
    assert_eq!(drain(&mut rx), vec![]);
}

#[tokio::test]
async fn stale_permissions_are_closed_at_turn_end() {
    let script = FakeScript {
        ask_permission: true,
        leave_permission_pending: true,
        ..FakeScript::default()
    };
    let (client, handle, mut rx) = connect(script, None).await;

    assert_eq!(prompt(&client, "Mon agenda ?").await, TurnEnd::Done);

    let events = drain(&mut rx);
    let [AgentEvent::Permission(ask)] = events.as_slice() else {
        panic!("expected the permission to be asked, got {events:?}");
    };
    // Closed with its turn.
    assert!(ask.reply.is_answered());
    assert!(
        !ask.reply.send(selected("allow-once")),
        "a closed permission cannot be answered"
    );
    timeout(WAIT, async {
        while handle.record().permission_outcomes.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the agent never got an answer");
    assert_eq!(
        handle.record().permission_outcomes,
        vec![RequestPermissionOutcome::Cancelled]
    );
}

#[tokio::test]
async fn terminal_tool_call_emits_finished() {
    let script = FakeScript {
        updates: vec![
            SessionUpdate::ToolCall(
                ToolCall::new("t1", "Chercher").status(ToolCallStatus::Completed),
            ),
            SessionUpdate::ToolCall(ToolCall::new("t2", "Lire").status(ToolCallStatus::Failed)),
        ],
        ..FakeScript::default()
    };
    let (client, _handle, mut rx) = connect(script, None).await;

    assert_eq!(prompt(&client, "Salut").await, TurnEnd::Done);

    assert_eq!(
        drain(&mut rx),
        vec![
            AgentEvent::ToolStarted {
                id: "t1".into(),
                title: "Chercher".into()
            },
            AgentEvent::ToolFinished {
                id: "t1".into(),
                ok: true
            },
            AgentEvent::ToolStarted {
                id: "t2".into(),
                title: "Lire".into()
            },
            AgentEvent::ToolFinished {
                id: "t2".into(),
                ok: false
            },
        ]
    );
}

#[tokio::test]
async fn cancel_between_turns_does_not_reach_the_next_turn() {
    let script = FakeScript {
        ask_permission: true,
        ..FakeScript::default()
    };
    let (client, handle, mut rx) = connect(script, None).await;
    let client = Arc::new(client);

    // A cancel arriving after the turn ended.
    client.cancel().expect("cancel");
    let turn = tokio::spawn({
        let client = client.clone();
        async move { client.prompt("Mon agenda ?").await }
    });
    let ask = permission_asked(&mut rx).await;
    assert!(ask.reply.send(selected("allow-once")), "answer allow");
    let end = timeout(WAIT, turn).await.expect("turn timed out");
    assert_eq!(end.expect("turn panicked").expect("prompt"), TurnEnd::Done);
    assert_eq!(
        handle.record().permission_outcomes,
        vec![selected("allow-once")]
    );
}

#[tokio::test]
async fn ask_user_question_is_never_allowed_up_front() {
    let spec = AgentSpec {
        builtin_tools: vec!["AskUserQuestion".into(), "WebSearch".into()],
        ..spec()
    };
    let (_client, handle, _rx) = connect_with(&spec, FakeScript::default(), None).await;
    let record = handle.record();
    let meta = record.new_sessions[0].meta.clone().expect("no _meta");
    let options = &meta["claudeCode"]["options"];
    assert_eq!(options["tools"], json!(["WebSearch", "AskUserQuestion"]));
    assert_eq!(options["allowedTools"], json!(["mcp__fronde", "WebSearch"]));
}

#[tokio::test]
async fn initialize_advertises_forms_only() {
    let (_client, handle, _rx) = connect(FakeScript::default(), None).await;
    let record = handle.record();
    let elicitation = record.initializes[0]
        .client_capabilities
        .elicitation
        .clone()
        .expect("no elicitation capability");
    assert!(elicitation.supports_form());
    assert!(!elicitation.supports_url());
}

/// The Claude adapter's form for one question with its free-text field.
fn question_form() -> serde_json::Value {
    json!({
        "mode": "form",
        "toolCallId": "toolu_1",
        "message": "Quelle appli installer ?",
        "requestedSchema": {"type": "object", "properties": {
            "question_0": {"type": "string", "title": "Appli", "oneOf": [
                {"const": "Mealie", "title": "Mealie"},
                {"const": "Seerr", "title": "Seerr"},
            ]},
            "question_0_custom": {"type": "string", "title": "Other"},
        }},
    })
}

fn eliciting(form: serde_json::Value) -> FakeScript {
    FakeScript {
        elicitation: Some(form),
        ..FakeScript::default()
    }
}

async fn elicitation_asked(rx: &mut mpsc::UnboundedReceiver<AgentEvent>) -> ElicitationAsk {
    loop {
        let event = timeout(WAIT, rx.recv())
            .await
            .expect("no elicitation")
            .expect("event channel closed");
        if let AgentEvent::Elicitation(ask) = event {
            return *ask;
        }
    }
}

#[tokio::test]
async fn elicitation_round_trip() {
    let (client, handle, mut rx) = connect(eliciting(question_form()), None).await;
    let client = Arc::new(client);
    let turn = tokio::spawn({
        let client = client.clone();
        async move { client.prompt("Installe une appli").await }
    });
    let ask = elicitation_asked(&mut rx).await;
    assert_eq!(ask.request.message, "Quelle appli installer ?");
    let content: std::collections::BTreeMap<_, _> = [(
        "question_0".to_owned(),
        ElicitationContentValue::String("Seerr".into()),
    )]
    .into();
    let accept = CreateElicitationResponse::new(ElicitationAction::Accept(
        ElicitationAcceptAction::new().content(content),
    ));
    assert!(ask.reply.send(accept.clone()));
    let end = timeout(WAIT, turn).await.expect("turn timed out");
    assert_eq!(end.expect("turn panicked").expect("prompt"), TurnEnd::Done);
    assert_eq!(handle.record().elicitation_responses, vec![Ok(accept)]);
}

#[tokio::test]
async fn url_elicitation_is_cancelled() {
    let url = json!({
        "mode": "url", "message": "Connecte-toi", "url": "https://example.com/login",
        "elicitationId": "e1",
    });
    let (client, handle, mut rx) = connect(eliciting(url), None).await;
    assert_eq!(prompt(&client, "Installe").await, TurnEnd::Cancelled);
    assert_eq!(
        handle.record().elicitation_responses,
        vec![Ok(CreateElicitationResponse::new(
            ElicitationAction::Cancel
        ))]
    );
    // Nobody was asked.
    assert!(
        !drain(&mut rx)
            .iter()
            .any(|event| matches!(event, AgentEvent::Elicitation(_)))
    );
}

#[tokio::test]
async fn cancel_and_drop_resolve_pending_elicitation() {
    let (client, handle, mut rx) = connect(eliciting(question_form()), None).await;
    let client = Arc::new(client);
    let turn = tokio::spawn({
        let client = client.clone();
        async move { client.prompt("Installe").await }
    });
    let ask = elicitation_asked(&mut rx).await;
    client.cancel().expect("cancel");
    let end = timeout(WAIT, turn).await.expect("turn timed out");
    assert_eq!(
        end.expect("turn panicked").expect("prompt"),
        TurnEnd::Cancelled
    );
    let cancelled = CreateElicitationResponse::new(ElicitationAction::Cancel);
    assert_eq!(
        handle.record().elicitation_responses,
        vec![Ok(cancelled.clone())]
    );
    assert!(
        !ask.reply.send(cancelled),
        "a cancelled form cannot be answered"
    );

    // Dropped unanswered: cancelled too.
    let (client, handle, mut rx) = connect(eliciting(question_form()), None).await;
    let client = Arc::new(client);
    let turn = tokio::spawn({
        let client = client.clone();
        async move { client.prompt("Installe").await }
    });
    drop(elicitation_asked(&mut rx).await);
    timeout(WAIT, turn)
        .await
        .expect("turn timed out")
        .unwrap()
        .unwrap();
    assert_eq!(
        handle.record().elicitation_responses,
        vec![Ok(CreateElicitationResponse::new(
            ElicitationAction::Cancel
        ))]
    );
}

#[tokio::test]
async fn withdrawn_elicitation_closes_its_reply() {
    let withdraw = Arc::new(tokio::sync::Notify::new());
    let script = FakeScript {
        withdraw_elicitation: Some(withdraw.clone()),
        ..eliciting(question_form())
    };
    let (client, handle, mut rx) = connect(script, None).await;
    let client = Arc::new(client);
    let turn = tokio::spawn({
        let client = client.clone();
        async move { client.prompt("Installe").await }
    });
    let ask = elicitation_asked(&mut rx).await;
    withdraw.notify_one();
    timeout(WAIT, ask.reply.answered())
        .await
        .expect("the withdrawn request's reply stays open");
    timeout(WAIT, turn)
        .await
        .expect("turn timed out")
        .unwrap()
        .unwrap();
    // The agent withdrew it and gets the cancel it asked for.
    assert_eq!(
        handle.record().elicitation_responses,
        vec![Ok(CreateElicitationResponse::new(
            ElicitationAction::Cancel
        ))]
    );
}

#[tokio::test]
async fn a_form_for_another_session_is_cancelled() {
    let mut form = question_form();
    form["sessionId"] = json!("someone-elses");
    let (client, handle, mut rx) = connect(eliciting(form), None).await;
    assert_eq!(prompt(&client, "Installe").await, TurnEnd::Cancelled);
    assert_eq!(
        handle.record().elicitation_responses,
        vec![Ok(CreateElicitationResponse::new(
            ElicitationAction::Cancel
        ))]
    );
    assert!(
        !drain(&mut rx)
            .iter()
            .any(|event| matches!(event, AgentEvent::Elicitation(_)))
    );
}

fn with_models() -> FakeScript {
    FakeScript {
        models: true,
        ..FakeScript::default()
    }
}

/// The models are the values of the agent's config option of category `model`, and choosing one
/// is a `session/set_config_option` on that option.
#[tokio::test]
async fn model_is_set_through_config_options() {
    let (client, handle, _rx) = connect(with_models(), None).await;
    let ids: Vec<String> = client.models().into_iter().map(|model| model.id).collect();
    assert_eq!(ids, FAKE_MODELS);
    assert_eq!(client.config_options().len(), 1);

    timeout(WAIT, client.set_model("fake-large"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        handle.record().config_sets,
        [(FAKE_MODEL_OPTION.to_owned(), "fake-large".to_owned())]
    );
    // The options as the agent answered them: the new model is current.
    let [option] = client.config_options().try_into().unwrap();
    let SessionConfigKind::Select(select) = option.kind else {
        panic!("not a select: {option:?}");
    };
    assert_eq!(&*select.current_value.0, "fake-large");
    // The session's mode is still set as before, once.
    assert_eq!(handle.record().modes, ["default"]);
}

#[tokio::test]
async fn unknown_model_is_refused_with_the_choices() {
    let (client, handle, _rx) = connect(with_models(), None).await;
    let error = timeout(WAIT, client.set_model("fake-huge"))
        .await
        .unwrap()
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "unknown model \"fake-huge\"; choose one of: fake-small, fake-large"
    );
    // Refused before the agent is asked.
    assert!(handle.record().config_sets.is_empty());
}

#[tokio::test]
async fn agent_without_model_option_refuses_a_model() {
    let (client, handle, _rx) = connect(FakeScript::default(), None).await;
    assert!(client.models().is_empty());
    assert!(client.config_options().is_empty());
    let error = timeout(WAIT, client.set_model("fake-small"))
        .await
        .unwrap()
        .unwrap_err()
        .to_string();
    assert_eq!(error, "this agent offers no choice of model");
    assert!(handle.record().config_sets.is_empty());
}

/// A resumed session's config options are kept as `session/resume` gives them.
#[tokio::test]
async fn resumed_session_keeps_its_config_options() {
    let script = FakeScript {
        resume: true,
        ..with_models()
    };
    let (client, handle, _rx) = connect(script, Some("earlier-session")).await;
    assert_eq!(handle.record().resumes.len(), 1);
    assert_eq!(client.models().len(), 2);
    timeout(WAIT, client.set_model("fake-small"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        handle.record().config_sets,
        [(FAKE_MODEL_OPTION.to_owned(), "fake-small".to_owned())]
    );
}

/// With a shared counter, each agent playing the script gives its new session its own id.
#[tokio::test]
async fn counted_sessions_get_distinct_ids() {
    let script = FakeScript {
        counted_sessions: Some(Arc::default()),
        ..FakeScript::default()
    };
    let (first, ..) = connect(script.clone(), None).await;
    let (second, ..) = connect(script, None).await;
    assert_eq!(first.session_id(), "fake-session-1");
    assert_eq!(second.session_id(), "fake-session-2");
}
