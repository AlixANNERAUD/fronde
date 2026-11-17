use agent_client_protocol::v1::schema::{
    ContentBlock, McpServer, RequestPermissionOutcome, SelectedPermissionOutcome, SessionUpdate,
    SystemPromptMode, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use fronde_agent::acp::{AgentClient, AgentEvent, AgentSpec, Failure, TurnEnd};
use fronde_agent::fake::{FAKE_SESSION_ID, FakeHandle, FakeScript, fake_agent, text_chunk};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

fn spec() -> AgentSpec {
    AgentSpec {
        command: vec!["claude-agent-acp".into()],
        env: Vec::new(),
        cwd: "/srv/fronde/agent".into(),
        hub_url: "http://127.0.0.1:7878/mcp".into(),
        hub_token: "hub-token".into(),
        oauth_token: None,
        system_prompt: "You are Fronde.".into(),
        builtin_tools: vec!["WebSearch".into(), "WebFetch".into()],
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

/// Waits for the next permission request and returns its id.
async fn permission_asked(rx: &mut mpsc::UnboundedReceiver<AgentEvent>) -> u64 {
    loop {
        let event = timeout(WAIT, rx.recv())
            .await
            .expect("no permission request")
            .expect("event channel closed");
        if let AgentEvent::PermissionAsked { id, title } = event {
            assert_eq!(title, "Lire l'agenda");
            return id;
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

/// The hub and the web tools, allowed up front, and nothing from the owner's Claude settings: no
/// plugins, MCP servers, hooks or allow rules, and no bypass mode.
fn expected_meta() -> serde_json::Value {
    json!({
        "disableBuiltInTools": true,
        "claudeCode": {"options": {
            "tools": ["WebSearch", "WebFetch"],
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
async fn no_builtin_tools_leaves_only_the_hub() {
    let spec = AgentSpec {
        builtin_tools: Vec::new(),
        ..spec()
    };
    let (_client, handle, _rx) = connect_with(&spec, FakeScript::default(), None).await;

    let record = handle.record();
    let meta = serde_json::Value::Object(record.new_sessions[0].meta.clone().expect("no _meta"));
    assert_eq!(
        meta,
        json!({
            "disableBuiltInTools": true,
            "claudeCode": {"options": {
                "tools": [],
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
    let id = permission_asked(&mut rx).await;
    client.answer_permission(id, true).expect("answer allow");
    let end = timeout(WAIT, turn).await.expect("turn timed out");
    assert_eq!(end.expect("turn panicked").expect("prompt"), TurnEnd::Done);

    let turn = tokio::spawn({
        let client = client.clone();
        async move { client.prompt("Encore ?").await }
    });
    let second = permission_asked(&mut rx).await;
    assert_ne!(second, id);
    client
        .answer_permission(second, false)
        .expect("answer deny");
    let end = timeout(WAIT, turn).await.expect("turn timed out");
    assert_eq!(end.expect("turn panicked").expect("prompt"), TurnEnd::Done);

    assert_eq!(
        handle.record().permission_outcomes,
        vec![selected("allow-once"), selected("reject")]
    );
    let error = client
        .answer_permission(id, true)
        .expect_err("an answered permission cannot be answered again");
    assert!(error.to_string().contains(&id.to_string()), "{error}");
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
    permission_asked(&mut rx).await;
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
    let [
        AgentEvent::PermissionAsked { id, .. },
        AgentEvent::PermissionClosed { id: closed },
    ] = events.as_slice()
    else {
        panic!("expected the permission to be asked then closed, got {events:?}");
    };
    assert_eq!(closed, id);
    client
        .answer_permission(*id, true)
        .expect_err("a closed permission cannot be answered");
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
    let id = permission_asked(&mut rx).await;
    client.answer_permission(id, true).expect("answer allow");
    let end = timeout(WAIT, turn).await.expect("turn timed out");
    assert_eq!(end.expect("turn panicked").expect("prompt"), TurnEnd::Done);
    assert_eq!(
        handle.record().permission_outcomes,
        vec![selected("allow-once")]
    );
}
