//! Each thread's agent has its own hub identity: the token its process was given, and nothing
//! else, names the thread to `/mcp`; it dies with the process.

use fronde_agent::acp::{AgentClient, AgentSpec};
use fronde_agent::assistant::{Assistant, Connector};
use fronde_agent::fake::{FakeScript, fake_agent, text_chunk};
use fronde_agent::log::Event;
use fronde_agent::threads::{StartedBy, ThreadId, Threads, thread_dirs};
use fronde_db::ephemeral::EphemeralSecrets;
use fronde_db::master_key::Backends;
use fronde_hub::Hub;
use fronde_hub::tools::{CallContext, ToolOutput, typed_tool_with_context};
use fronde_protocol::{ChoiceAnswer, Keep, Response, SecretValue};
use fronde_requests::{Requests, SecretStore, User};
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientConfig, Implementation};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::timeout;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const WAIT: Duration = Duration::from_secs(10);
const CANARY: &str = "canary-thread-secret-51c2aa";

type Client = RunningService<rmcp::RoleClient, ClientConfig>;

/// A hub served behind the real gate, with the threads of a data directory whose agents are fake,
/// and two test tools: `test_whoami`, which returns its call's context, and `test_take_secret`,
/// which takes an ephemeral secret for its caller.
struct Bench {
    addr: SocketAddr,
    threads: Arc<Threads>,
    requests: Arc<Requests>,
    /// Each agent start's thread and hub token, in order.
    starts: Arc<Mutex<Vec<(ThreadId, String)>>>,
    /// Releases the turns the fake agents hold open.
    gate: Arc<Notify>,
    _dir: tempfile::TempDir,
}

fn alix() -> User {
    User {
        name: "alix".into(),
        admin: true,
    }
}

async fn bench(script: FakeScript) -> Bench {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let config: fronde_hub::config::Config = toml::from_str(&format!(
        "[server]\ntoken = {{ env = \"UNUSED\" }}\n[data]\ndir = {:?}\ntimezone = \"Europe/Paris\"\nuser = \"alix\"\n[memory]\ngit = false\n[secrets]\nbackend = \"file\"\nmaster_key = {{ file = {:?} }}\n",
        data.display().to_string(),
        dir.path().join("master.key").display().to_string(),
    ))
    .unwrap();
    fronde_hub::private::prepare_data_dir(&data).unwrap();
    let db = fronde_db::open(&data).await.unwrap();
    let starts: Arc<Mutex<Vec<(ThreadId, String)>>> = Arc::default();
    let connector: Connector = Arc::new({
        let starts = starts.clone();
        move |spec: &AgentSpec, resume, events| {
            starts
                .lock()
                .unwrap()
                .push((spec.thread, spec.hub_token.expose_secret().to_owned()));
            let (spec, script) = (spec.clone(), script.clone());
            Box::pin(async move {
                let (transport, _handle) = fake_agent(script);
                AgentClient::connect(transport, &spec, resume, events).await
            })
        }
    });
    let main = thread_dirs(&data, ThreadId::MAIN);
    let spec = AgentSpec {
        thread: ThreadId::MAIN,
        command: vec!["claude-agent-acp".into()],
        env: Vec::new(),
        cwd: main.work,
        home: main.home,
        hub_url: "http://127.0.0.1:1/mcp".into(),
        hub_token: SecretString::from(String::new()),
        oauth_token: None,
        system_prompt: String::new(),
        builtin_tools: Vec::new(),
    };
    let threads = Threads::open(db.clone(), &data, spec, connector, alix())
        .await
        .unwrap();
    let ephemeral = Arc::new(EphemeralSecrets::new());
    let store = SecretStore::new(
        Backends::file(dir.path().join("master.key")),
        ephemeral.clone(),
    );
    let requests = Requests::open_with(db.clone(), threads.clone(), Some(store))
        .await
        .unwrap();
    threads.set_requests(&requests).await;
    let hub = Hub::start(config, db, vec![], Some(requests.clone()))
        .await
        .unwrap();
    let (def, handler) = typed_tool_with_context(
        "test_whoami",
        "Who calls.",
        false,
        |context: CallContext, _: Value| async move {
            ToolOutput::ok(json!({
                "caller": context.caller.id(),
                "topic": context.topic,
                "turn": context.turn,
            }))
        },
    );
    hub.registry.register(def, handler).unwrap();
    let (def, handler) = typed_tool_with_context("test_take_secret", "Takes a secret.", false, {
        let ephemeral = ephemeral.clone();
        move |context: CallContext, args: Value| {
            let taken = args["secret_ref"]
                .as_str()
                .map(|reference| ephemeral.take(reference, &context.secret_scope()));
            async move {
                match taken {
                    Some(Ok(value)) => {
                        ToolOutput::ok(json!({"same": value.expose_secret() == CANARY}))
                    }
                    Some(Err(error)) => ToolOutput::error(error.to_string()),
                    None => ToolOutput::error("no secret_ref"),
                }
            }
        }
    });
    hub.registry.register(def, handler).unwrap();
    let app = fronde::router(
        Arc::new(hub),
        SecretString::from(TOKEN),
        Some(threads.clone()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Bench {
        addr,
        threads,
        requests,
        starts,
        gate: Arc::new(Notify::new()),
        _dir: dir,
    }
}

/// A fake agent that holds each turn open until `gate` is notified.
fn held(gate: &Arc<Notify>) -> FakeScript {
    FakeScript {
        updates: vec![text_chunk("Je regarde."), text_chunk(" Voilà.")],
        pause: Some((1, gate.clone())),
        ..FakeScript::default()
    }
}

impl Bench {
    async fn held() -> Self {
        let gate = Arc::new(Notify::new());
        let mut bench = bench(held(&gate)).await;
        bench.gate = gate;
        bench
    }

    async fn new_thread(&self) -> ThreadId {
        self.threads
            .create("alix", StartedBy::User, None, "Une autre")
            .await
            .unwrap()
            .id
    }

    async fn assistant(&self, id: ThreadId) -> Arc<Assistant> {
        self.threads.assistant(id).await.unwrap().unwrap()
    }

    fn tokens_of(&self, id: ThreadId) -> Vec<String> {
        self.starts
            .lock()
            .unwrap()
            .iter()
            .filter(|(thread, _)| *thread == id)
            .map(|(_, token)| token.clone())
            .collect()
    }

    /// Sends a message to thread `id` and returns the token its agent got for this turn's
    /// start, once the agent is starting.
    async fn start(&self, id: ThreadId) -> String {
        let before = self.tokens_of(id).len();
        self.assistant(id).await.send("Salut".into()).await;
        timeout(WAIT, async {
            loop {
                if let Some(token) = self.tokens_of(id).get(before) {
                    return token.clone();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the agent never started")
    }

    async fn client(
        &self,
        token: &str,
        name: &str,
        headers: &[(&'static str, &'static str)],
    ) -> Client {
        let headers: HashMap<HeaderName, HeaderValue> = headers
            .iter()
            .map(|(name, value)| {
                (
                    HeaderName::from_static(name),
                    HeaderValue::from_static(value),
                )
            })
            .collect();
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(format!("http://{}/mcp", self.addr))
                .auth_header(token)
                .custom_headers(headers),
        );
        let mut info = ClientConfig::default();
        info.client_info = Implementation::new(name, "1.0");
        timeout(WAIT, info.serve(transport)).await.unwrap().unwrap()
    }

    /// The status `/mcp` answers an `initialize` carrying `token`.
    async fn status(&self, token: &str) -> u16 {
        reqwest::Client::new()
            .post(format!("http://{}/mcp", self.addr))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }
}

/// Calls `tool` with `params` (its name filled in) and returns the tool's text, parsed.
async fn call(client: &Client, tool: &str, mut params: Value) -> Value {
    params["name"] = tool.into();
    let params: CallToolRequestParams = serde_json::from_value(params).unwrap();
    let result = timeout(WAIT, client.call_tool(params))
        .await
        .unwrap()
        .unwrap();
    match &result.content[0] {
        rmcp::model::ContentBlock::Text(text) => serde_json::from_str(&text.text).unwrap(),
        other => panic!("{other:?}"),
    }
}

/// Calls `tool` in the background.
fn call_in_background(
    client: &Client,
    tool: &'static str,
    arguments: Value,
) -> tokio::task::JoinHandle<Value> {
    let peer = client.peer().clone();
    tokio::spawn(async move {
        let result = peer
            .call_tool(
                CallToolRequestParams::new(tool)
                    .with_arguments(arguments.as_object().unwrap().clone()),
            )
            .await
            .unwrap();
        match &result.content[0] {
            rmcp::model::ContentBlock::Text(text) => serde_json::from_str(&text.text).unwrap(),
            other => panic!("{other:?}"),
        }
    })
}

/// The next request shown in `assistant`'s stream after the entry `after`: its id and seq.
async fn next_request(assistant: &Assistant, after: u64) -> (u64, u64) {
    timeout(WAIT, async {
        loop {
            let found =
                assistant
                    .log()
                    .since(after)
                    .into_iter()
                    .find_map(|entry| match entry.event {
                        Event::Request { id, .. } => Some((id, entry.seq)),
                        _ => None,
                    });
            if let Some(found) = found {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("no request was shown")
}

fn requests_in(assistant: &Assistant) -> usize {
    assistant
        .log()
        .since(0)
        .iter()
        .filter(|entry| matches!(entry.event, Event::Request { .. }))
        .count()
}

#[tokio::test]
async fn thread_identity_comes_from_the_token_only() {
    let bench = Bench::held().await;
    let second = bench.new_thread().await;
    let main_token = bench.start(ThreadId::MAIN).await;
    let token = bench.start(second).await;
    let main_turn = bench.assistant(ThreadId::MAIN).await.current_turn().await;
    let turn = bench.assistant(second).await.current_turn().await;
    assert!(turn.is_some() && main_turn.is_some());

    // Thread 2's agent, everything else in the request naming thread 1.
    let posing = bench
        .client(
            &token,
            "Fronde",
            &[("x-fronde-thread", "1"), ("x-fronde-topic", "conversation")],
        )
        .await;
    let names_thread_one = json!({
        "arguments": {"thread": 1, "thread_id": 1, "topic": "conversation", "caller": "thread:1"},
        "_meta": {"thread": 1, "fronde/thread": "1", "topic": "conversation"},
    });
    let expected = json!({"result": {"caller": "thread:2", "topic": "thread:2", "turn": turn}});
    assert_eq!(
        call(&posing, "test_whoami", names_thread_one.clone()).await,
        expected
    );
    // Through `use_tool` too: the context follows the call.
    let through = json!({
        "arguments": {"name": "test_whoami", "arguments": {"thread": 1}},
        "_meta": {"thread": 1},
    });
    assert_eq!(call(&posing, "use_tool", through).await, expected);

    // Thread 1's agent at the same time: each call is its own thread's.
    let main = bench
        .client(&main_token, "claude-code", &[("x-fronde-thread", "2")])
        .await;
    assert_eq!(
        call(&main, "test_whoami", json!({"arguments": {"thread": 2}})).await,
        json!({"result": {"caller": "thread:1", "topic": "conversation", "turn": main_turn}})
    );
    assert_eq!(
        call(&posing, "test_whoami", names_thread_one).await,
        expected
    );
    bench.gate.notify_waiters();
}

#[tokio::test]
async fn outside_token_is_no_thread() {
    let bench = Bench::held().await;
    let second = bench.new_thread().await;
    bench.start(second).await;
    let outside = bench
        .client(TOKEN, "Fronde", &[("x-fronde-thread", "2")])
        .await;
    let params = json!({
        "arguments": {"thread": 2, "topic": "thread:2"},
        "_meta": {"thread": 2},
    });
    assert_eq!(
        call(&outside, "test_whoami", params).await,
        json!({"result": {"caller": "mcp:Fronde", "topic": "conversation", "turn": null}})
    );
    bench.gate.notify_waiters();
}

#[tokio::test]
async fn each_agent_process_gets_its_own_token() {
    let bench = bench(FakeScript::default()).await;
    let second = bench.new_thread().await;
    let main_token = bench.start(ThreadId::MAIN).await;
    let first = bench.start(second).await;
    assert_ne!(main_token, first);
    assert_eq!(first.len(), 64);
    assert_ne!(first, TOKEN);
    for token in [&main_token, &first] {
        assert_eq!(bench.status(token).await, 200);
    }
    // Thread 2's agent stops and starts again: a new token, the old one revoked.
    let assistant = bench.assistant(second).await;
    until_idle(&assistant).await;
    assert!(assistant.stop_agent().await);
    let again = bench.start(second).await;
    assert_ne!(again, first);
    assert_eq!(bench.status(&first).await, 401);
    assert_eq!(bench.status(&again).await, 200);
    // Thread 1's agent never stopped.
    assert_eq!(bench.status(&main_token).await, 200);
    assert_eq!(bench.threads.tokens().len(), 2);
}

/// Waits until `assistant`'s turn is over.
async fn until_idle(assistant: &Assistant) {
    timeout(WAIT, async {
        while assistant.current_turn().await.is_some()
            || !assistant
                .log()
                .since(0)
                .last()
                .is_some_and(|entry| matches!(entry.event, Event::TurnEnded { .. }))
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the turn never ended");
}

#[tokio::test]
async fn a_stopped_agents_token_is_refused() {
    // Stopped by Fronde while idle.
    let bench = bench(FakeScript::default()).await;
    let second = bench.new_thread().await;
    let token = bench.start(second).await;
    let assistant = bench.assistant(second).await;
    until_idle(&assistant).await;
    assert_eq!(bench.status(&token).await, 200);
    let client = bench.client(&token, "claude-code", &[]).await;
    assert!(assistant.stop_agent().await);
    assert!(!assistant.agent_running());
    assert_eq!(bench.status(&token).await, 401);
    // A session opened before keeps no access either: every request carries the token.
    let refused = timeout(
        WAIT,
        client.call_tool(CallToolRequestParams::new("test_whoami")),
    )
    .await
    .unwrap();
    assert!(refused.is_err(), "{refused:?}");
    assert!(bench.threads.tokens().is_empty());

    // Dropped after a failed turn, which may leave the connection broken.
    let bench = bench_failing().await;
    let second = bench.new_thread().await;
    let token = bench.start(second).await;
    let assistant = bench.assistant(second).await;
    until_idle(&assistant).await;
    assert!(!assistant.agent_running());
    assert_eq!(bench.status(&token).await, 401);
    // Unknown and empty tokens are refused alike.
    assert_eq!(bench.status(&"0".repeat(64)).await, 401);
    assert_eq!(bench.status("").await, 401);
}

async fn bench_failing() -> Bench {
    bench(FakeScript {
        fail_prompt: true,
        ..FakeScript::default()
    })
    .await
}

#[tokio::test]
async fn cards_go_to_the_calling_thread() {
    let bench = Bench::held().await;
    let second = bench.new_thread().await;
    bench.start(ThreadId::MAIN).await;
    let token = bench.start(second).await;
    let main = bench.assistant(ThreadId::MAIN).await;
    let assistant = bench.assistant(second).await;
    let client = bench
        .client(&token, "claude-code", &[("x-fronde-thread", "1")])
        .await;
    let question = json!({"questions": [{
        "question": "Quelle appli installer ?",
        "header": "Appli",
        "options": [
            {"label": "Mealie", "description": "Recettes"},
            {"label": "Seerr", "description": "Films"},
        ],
    }]});
    let asked = call_in_background(&client, "user_ask", question);
    let (id, _) = next_request(&assistant, 0).await;
    assert_eq!(requests_in(&main), 0, "the card went to the main thread");
    let answer = Response::Form {
        values: [(
            "q1".to_owned(),
            ChoiceAnswer {
                selected: vec!["Seerr".into()],
                ..Default::default()
            }
            .into(),
        )]
        .into(),
    };
    bench.requests.answer(id, answer, &alix()).await.unwrap();
    let result = timeout(WAIT, asked).await.unwrap().unwrap();
    assert_eq!(result["result"]["status"], "answered", "{result}");
    assert_eq!(result["result"]["answers"][0]["selected"], json!(["Seerr"]));
    // The card closed in thread 2's stream, never in the main one.
    timeout(WAIT, async {
        while !assistant.log().since(0).iter().any(
            |entry| matches!(entry.event, Event::RequestClosed { id: closed, .. } if closed == id),
        ) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(requests_in(&main), 0);
    bench.gate.notify_waiters();
}

#[tokio::test]
async fn ephemeral_secret_stays_in_its_thread() {
    let bench = Bench::held().await;
    let second = bench.new_thread().await;
    let main_token = bench.start(ThreadId::MAIN).await;
    let token = bench.start(second).await;
    let main = bench.client(&main_token, "claude-code", &[]).await;
    let asked = call_in_background(
        &main,
        "user_secret",
        json!({"label": "Mot de passe Nextcloud", "reason": "Pour lire ton agenda."}),
    );
    let main_assistant = bench.assistant(ThreadId::MAIN).await;
    let (id, _) = next_request(&main_assistant, 0).await;
    let secret = Response::Secret {
        value: SecretValue::new(CANARY.into()),
        keep: Keep::Conversation,
    };
    bench.requests.answer(id, secret, &alix()).await.unwrap();
    let saved = timeout(WAIT, asked).await.unwrap().unwrap();
    let reference = saved["result"]["secret_ref"].as_str().unwrap().to_owned();
    let take = json!({"arguments": {"secret_ref": reference}});

    // Thread 2's agent, even naming thread 1, and an outside client are refused.
    let other = bench
        .client(&token, "Fronde", &[("x-fronde-thread", "1")])
        .await;
    let outside = bench.client(TOKEN, "assistant", &[]).await;
    for client in [&other, &outside] {
        let refused = call(client, "test_take_secret", take.clone()).await;
        assert!(
            refused["error"]
                .as_str()
                .is_some_and(|error| error.contains("not for this conversation")),
            "{refused}"
        );
    }
    // Thread 1's agent gets it, once.
    assert_eq!(
        call(&main, "test_take_secret", take.clone()).await,
        json!({"result": {"same": true}})
    );
    assert!(call(&main, "test_take_secret", take).await["error"].is_string());
    bench.gate.notify_waiters();
}
