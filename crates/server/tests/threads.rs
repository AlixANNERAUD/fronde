//! Each thread's agent has its own hub identity: the token its process was given, and nothing
//! else, names the thread to `/mcp`; it dies with the process.

use fronde::thread_control::ThreadBridge;
use fronde_agent::acp::{AgentClient, AgentSpec};
use fronde_agent::assistant::{Assistant, Connector};
use fronde_agent::fake::{FakeHandle, FakeScript, fake_agent, text_chunk};
use fronde_agent::log::Event;
use fronde_agent::threads::{Budget, StartedBy, ThreadId, Threads, thread_dirs};
use fronde_db::ephemeral::EphemeralSecrets;
use fronde_db::master_key::Backends;
use fronde_hub::Hub;
use fronde_hub::threads::ThreadControl;
use fronde_hub::tools::{CallContext, Registry, ToolOutput, typed_tool_with_context};
use fronde_protocol::{
    Card, ChoiceAnswer, Keep, Outcome, RequestId, Response, SecretValue, TurnOutcome,
};
use fronde_requests::{Caller, Journal, Requests, SecretStore, User};
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
    /// The requests' journal: the threads, a step held when the test asks.
    journal: Arc<HeldJournal>,
    /// The hub's tools, called as the gate would with a context the test makes.
    registry: Arc<Registry>,
    /// Each agent start's thread and hub token, in order.
    starts: Arc<Mutex<Vec<(ThreadId, String)>>>,
    /// Each agent start's fake agent, in order.
    fakes: Arc<Mutex<Vec<FakeHandle>>>,
    /// Releases the turns the fake agents hold open.
    gate: Arc<Notify>,
    /// The data directory.
    data: std::path::PathBuf,
    /// The usage readings the threads decide background work from: none until a test records
    /// one.
    usage: Arc<fronde_agent::usage::UsageBook>,
    _dir: tempfile::TempDir,
}

fn alix() -> User {
    User {
        name: "alix".into(),
        admin: true,
    }
}

async fn bench(script: FakeScript) -> Bench {
    bench_with(script, None, Budget::default()).await
}

/// [`bench`], sub-agents playing `child` when given, at most `budget.max_running` agents at
/// once.
async fn bench_with(script: FakeScript, child: Option<FakeScript>, budget: Budget) -> Bench {
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
    let fakes: Arc<Mutex<Vec<FakeHandle>>> = Arc::default();
    let connector: Connector = Arc::new({
        let (starts, fakes) = (starts.clone(), fakes.clone());
        move |spec: &AgentSpec, resume, events| {
            starts
                .lock()
                .unwrap()
                .push((spec.thread, spec.hub_token.expose_secret().to_owned()));
            // A sub-agent is told it is one in its system prompt.
            let script = match &child {
                Some(child) if spec.system_prompt.contains("sub-agent") => child.clone(),
                _ => script.clone(),
            };
            let (spec, fakes) = (spec.clone(), fakes.clone());
            Box::pin(async move {
                let (transport, handle) = fake_agent(script);
                fakes.lock().unwrap().push(handle);
                AgentClient::connect(transport, &spec, resume, events).await
            })
        }
    });
    let main = thread_dirs(&data, ThreadId::MAIN);
    let usage = fronde_agent::usage::UsageBook::in_memory();
    let spec = AgentSpec {
        thread: ThreadId::MAIN,
        provider: &fronde_agent::provider::Provider::CLAUDE,
        command: vec!["claude-agent-acp".into()],
        env: Vec::new(),
        cwd: main.work,
        home: main.home,
        hub_url: "http://127.0.0.1:1/mcp".into(),
        hub_token: SecretString::from(String::new()),
        oauth_token: None,
        system_prompt: String::new(),
        builtin_tools: Vec::new(),
        usage: Some(usage.clone()),
        account: "claude".into(),
    };
    let threads = Threads::open_with(db.clone(), &data, spec, connector, alix(), budget)
        .await
        .unwrap();
    let ephemeral = Arc::new(EphemeralSecrets::new());
    let store = SecretStore::new(
        Backends::file(dir.path().join("master.key")),
        ephemeral.clone(),
    );
    let journal = Arc::new(HeldJournal {
        threads: threads.clone(),
        hold: Mutex::default(),
        reached: Notify::new(),
    });
    let requests = Requests::open_with(db.clone(), journal.clone(), Some(store))
        .await
        .unwrap();
    threads.set_requests(&requests).await;
    let control: Arc<dyn ThreadControl> = Arc::new(ThreadBridge(threads.clone()));
    let integrations: Arc<dyn fronde_hub::integrations::Integrations> =
        Arc::new(fronde::integrations::Shipped::new(&config, db.clone()).unwrap());
    let hub = Hub::start(
        config,
        db,
        vec![],
        Some(requests.clone()),
        Some(control),
        Some(integrations),
    )
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
    let registry = hub.registry.clone();
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
        journal,
        registry,
        starts,
        fakes,
        gate: Arc::new(Notify::new()),
        data,
        usage,
        _dir: dir,
    }
}

/// A step of the requests' journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Announce,
    Close,
}

/// The threads as the requests' journal, the next step of a kind held when the test asks: it
/// sees what happens while a card is being shown or closed.
struct HeldJournal {
    threads: Arc<Threads>,
    /// The next step of this kind waits for the notify.
    hold: Mutex<Option<(Step, Arc<Notify>)>>,
    /// Told when a held step is reached.
    reached: Notify,
}

impl HeldJournal {
    /// Holds the next `step` until the returned notify is told once.
    fn hold(&self, step: Step) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        *self.hold.lock().unwrap() = Some((step, gate.clone()));
        gate
    }

    /// Returns once a held step is reached.
    async fn reached(&self) {
        timeout(WAIT, self.reached.notified())
            .await
            .expect("the held step was never reached");
    }

    async fn pass(&self, step: Step) {
        let gate = {
            let mut hold = self.hold.lock().unwrap();
            match &*hold {
                Some((held, _)) if *held == step => hold.take().map(|(_, gate)| gate),
                _ => None,
            }
        };
        if let Some(gate) = gate {
            self.reached.notify_one();
            gate.notified().await;
        }
    }
}

#[async_trait::async_trait]
impl Journal for HeldJournal {
    async fn announce(
        &self,
        topic: &str,
        id: RequestId,
        card: &Card,
        caller: &Caller,
    ) -> anyhow::Result<u64> {
        self.pass(Step::Announce).await;
        self.threads.announce(topic, id, card, caller).await
    }

    async fn close(
        &self,
        topic: &str,
        id: RequestId,
        outcome: &Outcome,
        by: Option<&str>,
    ) -> anyhow::Result<()> {
        self.pass(Step::Close).await;
        self.threads.close(topic, id, outcome, by).await
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

/// The fake agent of the latest start, once it connected.
async fn latest_fake(bench: &Bench, count: usize) -> FakeHandle {
    timeout(WAIT, async {
        loop {
            if let Some(handle) = bench.fakes.lock().unwrap().get(count - 1) {
                return handle.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the fake agent never started")
}

/// An agent process that exits on its own while idle (a crash, say) loses its token: Fronde
/// sees its connection end, and `/mcp` refuses the token.
#[tokio::test]
async fn an_agent_that_exits_on_its_own_loses_its_token() {
    let bench = bench(FakeScript::default()).await;
    let second = bench.new_thread().await;
    let token = bench.start(second).await;
    let assistant = bench.assistant(second).await;
    until_idle(&assistant).await;
    assert!(assistant.agent_running());
    assert_eq!(bench.status(&token).await, 200);

    latest_fake(&bench, 1).await.exit();
    timeout(WAIT, async {
        while assistant.agent_running() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Fronde never saw the agent exit");
    assert_eq!(bench.status(&token).await, 401);
    assert!(bench.threads.tokens().is_empty());
    // The next message starts it again, with a new token.
    let again = bench.start(second).await;
    assert_ne!(again, token);
    assert_eq!(bench.status(&again).await, 200);
}

/// Archiving a thread lets its agent go: its running turn is cancelled, its token refused at
/// once, its process stopped; an idle agent goes the same way.
#[tokio::test]
async fn archiving_a_thread_stops_its_agent_and_revokes_its_token() {
    let bench = Bench::held().await;
    let second = bench.new_thread().await;
    let token = bench.start(second).await;
    let assistant = bench.assistant(second).await;
    let fake = latest_fake(&bench, 1).await;
    assert!(assistant.current_turn().await.is_some());
    assert_eq!(bench.status(&token).await, 200);

    bench.threads.set_archived(second, true).await.unwrap();
    assert_eq!(bench.status(&token).await, 401);
    assert!(!assistant.agent_running());
    bench.gate.notify_waiters();
    until_idle(&assistant).await;
    timeout(WAIT, async {
        while !fake.record().closed {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the archived thread's agent never stopped");
    assert!(fake.record().cancels >= 1);
    assert!(bench.threads.tokens().is_empty());

    // An idle agent.
    let bench = bench_idle().await;
    let third = bench.new_thread().await;
    let token = bench.start(third).await;
    let assistant = bench.assistant(third).await;
    until_idle(&assistant).await;
    let fake = latest_fake(&bench, 1).await;
    assert_eq!(bench.status(&token).await, 200);
    bench.threads.set_archived(third, true).await.unwrap();
    assert_eq!(bench.status(&token).await, 401);
    timeout(WAIT, async {
        while !fake.record().closed {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the archived thread's idle agent never stopped");
}

async fn bench_idle() -> Bench {
    bench(FakeScript::default()).await
}

/// An MCP session is used only with the token that opened it: another valid token, an agent's
/// or `server.token`, gets 403 in it.
#[tokio::test]
async fn an_mcp_session_keeps_the_token_that_opened_it() {
    let bench = bench(FakeScript::default()).await;
    let second = bench.new_thread().await;
    let agent = bench.start(second).await;
    let assistant = bench.assistant(second).await;
    until_idle(&assistant).await;
    let post = |token: &str, session: Option<&str>, body: &'static str| {
        let mut request = reqwest::Client::new()
            .post(format!("http://{}/mcp", bench.addr))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(body);
        if let Some(session) = session {
            request = request.header("mcp-session-id", session);
        }
        request.send()
    };
    const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    const INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    let opened = post(&agent, None, INITIALIZE).await.unwrap();
    assert_eq!(opened.status(), 200);
    let session = opened.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_owned();
    for other in [TOKEN.to_owned(), bench.start(ThreadId::MAIN).await] {
        let refused = post(&other, Some(&session), INITIALIZED).await.unwrap();
        assert_eq!(refused.status(), 403, "{other}");
    }
    // The token that opened it goes on.
    let went_on = post(&agent, Some(&session), INITIALIZED).await.unwrap();
    assert!(went_on.status().is_success(), "{}", went_on.status());
    // So does another session, opened by the other token.
    let outside = post(TOKEN, None, INITIALIZE).await.unwrap();
    assert_eq!(outside.status(), 200);
}

// Sub-agents: the `threads_*` tools.

const TASK: &str = "Trouve trois recettes de soupe à la courge.";

/// Enough agents at once for a parent, its sub-agents and another thread.
fn many() -> Budget {
    Budget {
        max_running: 8,
        ..Budget::default()
    }
}

/// Every agent, sub-agents included, holds its turns open until the gate is notified.
async fn held_bench() -> Bench {
    let gate = Arc::new(Notify::new());
    let mut bench = bench_with(held(&gate), None, many()).await;
    bench.gate = gate;
    bench
}

/// The thread's agents hold their turns open; sub-agents play `child`.
async fn held_parent(child: FakeScript) -> Bench {
    let gate = Arc::new(Notify::new());
    let mut bench = bench_with(held(&gate), Some(child), many()).await;
    bench.gate = gate;
    bench
}

/// Starts a sub-agent of the thread holding `client`'s token: its thread.
async fn start_child(client: &Client, task: &str) -> ThreadId {
    let started = call(
        client,
        "threads_start",
        json!({"arguments": {"task": task}}),
    )
    .await;
    ThreadId(
        started["result"]["thread"]
            .as_i64()
            .unwrap_or_else(|| panic!("{started}")),
    )
}

/// The hub token of thread `id`'s agent, once it started.
async fn token_of(bench: &Bench, id: ThreadId) -> String {
    timeout(WAIT, async {
        loop {
            if let Some(token) = bench.tokens_of(id).last() {
                return token.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the sub-agent never started")
}

/// What `client` gets from reading, sending to and stopping thread `target`.
async fn refusals(client: &Client, target: i64) -> Vec<Value> {
    let mut answers = Vec::new();
    for (tool, arguments) in [
        ("threads_read", json!({"thread": target})),
        (
            "threads_send",
            json!({"thread": target, "text": "Arrête tout."}),
        ),
        (
            "threads_send",
            json!({"thread": target, "text": "Et alors ?", "wait": true}),
        ),
        ("threads_cancel", json!({"thread": target})),
    ] {
        answers.push(call(client, tool, json!({"arguments": arguments})).await);
    }
    answers
}

fn no_such_thread() -> Value {
    json!({"error": fronde_hub::threads::NO_SUCH_THREAD})
}

async fn tool_names(client: &Client) -> Vec<String> {
    timeout(WAIT, client.list_all_tools())
        .await
        .unwrap()
        .unwrap()
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect()
}

/// The sub-agents of `parent`, oldest first.
async fn children_of(bench: &Bench, parent: ThreadId) -> Vec<ThreadId> {
    bench
        .threads
        .list(true)
        .await
        .unwrap()
        .into_iter()
        .filter(|row| row.parent == Some(parent))
        .map(|row| row.id)
        .collect()
}

#[tokio::test]
async fn child_cannot_read_parent_thread() {
    let bench = held_bench().await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let first = start_child(&main, TASK).await;
    let second = start_child(&main, "Trouve un vin pour la soupe.").await;
    let child = bench
        .client(&token_of(&bench, first).await, "claude-code", &[])
        .await;

    // Its parent, its sibling and an unknown thread get the same answer.
    let unknown = refusals(&child, 999).await;
    assert!(
        unknown.iter().all(|answer| *answer == no_such_thread()),
        "{unknown:?}"
    );
    assert_eq!(refusals(&child, ThreadId::MAIN.0).await, unknown);
    assert_eq!(refusals(&child, second.0).await, unknown);
    // Nothing reached the main thread or the sibling: the main thread's turn goes on.
    let main_assistant = bench.assistant(ThreadId::MAIN).await;
    assert!(main_assistant.current_turn().await.is_some());
    assert_eq!(
        main_assistant.status(),
        fronde_protocol::ThreadStatus::Running
    );
    assert_eq!(
        main_assistant
            .log()
            .since(0)
            .iter()
            .filter(|entry| matches!(entry.event, Event::User { .. }))
            .count(),
        1
    );
    assert!(
        !bench
            .assistant(second)
            .await
            .log()
            .since(0)
            .iter()
            .any(|entry| matches!(&entry.event, Event::User { text, .. } if text != "Trouve un vin pour la soupe."))
    );

    // The parent reads its child.
    let read = call(
        &main,
        "threads_read",
        json!({"arguments": {"thread": first.0}}),
    )
    .await;
    assert_eq!(
        read["result"]["items"][0],
        json!({"seq": 1, "kind": "user", "text": TASK}),
        "{read}"
    );
    bench.gate.notify_waiters();
}

#[tokio::test]
async fn a_thread_cannot_drive_another_threads_children() {
    let bench = held_bench().await;
    let other = bench.new_thread().await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let child = start_child(&main, TASK).await;
    let other_token = bench.start(other).await;
    // Another thread, everything but its token naming the main thread.
    let posing = bench
        .client(
            &other_token,
            "Fronde",
            &[("x-fronde-thread", "1"), ("x-fronde-topic", "conversation")],
        )
        .await;
    assert_eq!(
        refusals(&posing, child.0).await,
        refusals(&posing, 999).await
    );
    assert!(
        refusals(&posing, child.0)
            .await
            .iter()
            .all(|answer| *answer == no_such_thread())
    );
    let listed = call(
        &posing,
        "threads_list",
        json!({"arguments": {"thread": 1}, "_meta": {"thread": 1}}),
    )
    .await;
    assert_eq!(listed["result"]["children"], json!([]), "{listed}");
    // The main thread's child goes on, untouched.
    assert_eq!(
        bench.assistant(child).await.status(),
        fronde_protocol::ThreadStatus::Running
    );
    bench.gate.notify_waiters();
}

/// The parent of a sub-agent is the thread whose token started it, whatever the call names.
#[tokio::test]
async fn threads_tools_take_the_caller_from_the_token_only() {
    let bench = held_bench().await;
    let other = bench.new_thread().await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let posing = bench
        .client(
            &bench.start(other).await,
            "Fronde",
            &[("x-fronde-thread", "1")],
        )
        .await;
    let started = call(
        &posing,
        "threads_start",
        json!({
            "arguments": {
                "task": "Résume la journée.",
                "thread": 1, "parent": 1, "parent_id": 1, "caller": "thread:1", "depth": 0,
            },
            "_meta": {"thread": 1, "fronde/thread": "1"},
        }),
    )
    .await;
    let id = ThreadId(started["result"]["thread"].as_i64().unwrap());
    let row = bench.threads.get(id).await.unwrap().unwrap();
    assert_eq!(row.parent, Some(other));
    assert_eq!(row.started_by, StartedBy::Thread(other));
    assert_eq!(row.depth, 1);
    // Through use_tool too.
    let through = call(
        &posing,
        "use_tool",
        json!({"arguments": {"name": "threads_start", "arguments": {"task": "Autre chose.", "parent": 1}}}),
    )
    .await;
    let through = ThreadId(through["result"]["thread"].as_i64().unwrap());
    assert_eq!(
        bench.threads.get(through).await.unwrap().unwrap().parent,
        Some(other)
    );
    // The main thread sees neither.
    let listed = call(&main, "threads_list", json!({})).await;
    assert_eq!(listed["result"]["children"], json!([]), "{listed}");
    assert!(children_of(&bench, ThreadId::MAIN).await.is_empty());
    bench.gate.notify_waiters();
}

#[tokio::test]
async fn child_starts_with_only_its_task() {
    let bench = held_bench().await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let child = start_child(&main, TASK).await;
    token_of(&bench, child).await;
    let fake = latest_fake(&bench, 2).await;
    timeout(WAIT, async {
        while fake.record().prompts.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the sub-agent got no prompt");
    let record = fake.record();
    assert_eq!(record.new_sessions.len(), 1);
    assert!(record.resumes.is_empty(), "a new session, never a resume");
    assert_eq!(
        record.new_sessions[0].cwd,
        thread_dirs(&bench.data, child).work
    );
    assert_eq!(record.prompts, [TASK]);
    let system = serde_json::to_string(&record.new_sessions[0].system_prompt).unwrap();
    assert!(system.contains("You are a sub-agent of Fronde"), "{system}");
    assert!(!system.contains("personal assistant"), "{system}");

    let row = bench.threads.get(child).await.unwrap().unwrap();
    assert_eq!(row.parent, Some(ThreadId::MAIN));
    assert_eq!(row.depth, 1);
    assert_eq!(row.started_by, StartedBy::Thread(ThreadId::MAIN));
    assert_eq!(row.owner, "alix");
    let first = bench.assistant(child).await.log().since(0)[0].event.clone();
    assert_eq!(
        first,
        Event::User {
            text: TASK.into(),
            by: Some("Fronde".into()),
        }
    );
    bench.gate.notify_waiters();
}

#[tokio::test]
async fn wait_returns_the_final_answer() {
    let bench = held_parent(FakeScript {
        updates: vec![text_chunk("Trois "), text_chunk("recettes.")],
        ..FakeScript::default()
    })
    .await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let waited = call(
        &main,
        "threads_start",
        json!({"arguments": {"task": TASK, "wait": true}}),
    )
    .await;
    let child = waited["result"]["thread"].as_i64().unwrap();
    assert_eq!(
        waited["result"],
        json!({
            "thread": child,
            "status": "done",
            "outcome": "done",
            "answer": "Trois recettes.",
            "note": fronde_hub::threads::NOTE,
        }),
        "{waited}"
    );
    assert!(
        waited["next"].as_str().unwrap().contains("data"),
        "{waited}"
    );
    assert_eq!(bench.threads.delivered(ThreadId(child)), Some(1));

    // A new turn, waited for too.
    let again = call(
        &main,
        "threads_send",
        json!({"arguments": {"thread": child, "text": "Et une quatrième ?", "wait": true}}),
    )
    .await;
    assert_eq!(again["result"]["status"], "done", "{again}");
    assert_eq!(again["result"]["answer"], "Trois recettes.");
    let entries = bench.assistant(ThreadId(child)).await.log().since(0);
    let second = entries
        .iter()
        .rfind(|entry| matches!(entry.event, Event::User { .. }))
        .unwrap();
    assert_eq!(bench.threads.delivered(ThreadId(child)), Some(second.seq));
    assert_eq!(
        second.event,
        Event::User {
            text: "Et une quatrième ?".into(),
            by: Some("Fronde".into()),
        }
    );
    bench.gate.notify_waiters();
}

#[tokio::test]
async fn wait_ends_when_the_child_asks() {
    let bench = held_parent(FakeScript {
        ask_permission: true,
        ..FakeScript::default()
    })
    .await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let waited = call(
        &main,
        "threads_start",
        json!({"arguments": {"task": TASK, "wait": true}}),
    )
    .await;
    assert_eq!(waited["result"]["status"], "waiting_for_user", "{waited}");
    let child = ThreadId(waited["result"]["thread"].as_i64().unwrap());
    let assistant = bench.assistant(child).await;
    assert_eq!(
        requests_in(&assistant),
        1,
        "the card is in the sub-agent's thread"
    );
    assert_eq!(requests_in(&*bench.assistant(ThreadId::MAIN).await), 0);
    assert_eq!(bench.threads.delivered(child), None);
    bench.gate.notify_waiters();
}

#[tokio::test]
async fn wait_timeout_leaves_the_child_running() {
    let bench = held_bench().await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let waited = call(
        &main,
        "threads_start",
        json!({"arguments": {"task": TASK, "wait": true, "timeout_secs": 1}}),
    )
    .await;
    assert_eq!(waited["result"]["status"], "running", "{waited}");
    let child = ThreadId(waited["result"]["thread"].as_i64().unwrap());
    let assistant = bench.assistant(child).await;
    assert_eq!(assistant.status(), fronde_protocol::ThreadStatus::Running);
    assert!(assistant.current_turn().await.is_some());
    // It ends on its own, with its answer, read in the parent's next turn.
    let before = turn_of(&bench, ThreadId::MAIN).await;
    bench.gate.notify_waiters();
    until_idle(&assistant).await;
    next_turn(&bench, ThreadId::MAIN, before).await;
    let read = call(
        &main,
        "threads_read",
        json!({"arguments": {"thread": child.0}}),
    )
    .await;
    let items = read["result"]["items"].as_array().unwrap();
    assert_eq!(
        items[1],
        json!({"seq": 2, "kind": "assistant", "text": "Je regarde. Voilà."})
    );
    assert_eq!(
        items[2],
        json!({"seq": 4, "kind": "turn_ended", "outcome": "done"})
    );
    assert_eq!(read["result"]["status"], "done");
    // Nobody waited any more: its result woke the parent.
    until_woken(&bench, ThreadId::MAIN, child).await;
    bench.gate.notify_waiters();
}

/// Waits until thread `parent` was woken by a result of its sub-agent `child`: a
/// `ChildEnded` entry of it, then that entry's `Woken`. Lets the held turns go meanwhile.
async fn until_woken(bench: &Bench, parent: ThreadId, child: ThreadId) {
    let assistant = bench.assistant(parent).await;
    timeout(WAIT, async {
        loop {
            bench.gate.notify_waiters();
            let entries = assistant.log().since(0);
            let woke = entries.iter().any(|ended| {
                matches!(ended.event, Event::ChildEnded { thread, .. } if thread == child)
                    && entries.iter().any(|woken| {
                        matches!(woken.event, Event::Woken { child_ended } if child_ended == ended.seq)
                    })
            });
            if woke {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("thread {parent} was never woken by {child}: {:?}", events_of(&assistant)));
}

#[tokio::test]
async fn cancelled_call_stops_waiting_not_the_child() {
    let bench = held_bench().await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let call = main.peer().call_tool_once(
        CallToolRequestParams::new("threads_start").with_arguments(
            json!({"task": TASK, "wait": true})
                .as_object()
                .unwrap()
                .clone(),
        ),
    );
    // The client gives up: dropping the call sends `notifications/cancelled`.
    let given_up = timeout(Duration::from_millis(500), call).await;
    assert!(given_up.is_err(), "the call should still be waiting");
    let child = timeout(WAIT, async {
        loop {
            if let Some(child) = children_of(&bench, ThreadId::MAIN).await.first() {
                return *child;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("no sub-agent was started");
    let assistant = bench.assistant(child).await;
    // Given time to notice, the call stops waiting; the sub-agent goes on.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(assistant.status(), fronde_protocol::ThreadStatus::Running);
    bench.gate.notify_waiters();
    until_idle(&assistant).await;
    assert_eq!(
        assistant.status(),
        fronde_protocol::ThreadStatus::Done,
        "the sub-agent finished its turn"
    );
    assert_eq!(
        bench.threads.delivered(child),
        None,
        "nobody was waiting any more to take its result"
    );
}

#[tokio::test]
async fn retry_with_the_same_client_request_id_joins_the_child() {
    let bench = held_bench().await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let args = json!({"arguments": {"task": TASK, "client_request_id": "soupe-1"}});
    let first = call(&main, "threads_start", args.clone()).await;
    let again = call(&main, "threads_start", args.clone()).await;
    let child = first["result"]["thread"].clone();
    assert_eq!(again["result"]["thread"], child, "{again}");
    assert_eq!(again["result"]["joined"], true);
    assert!(first["result"].get("joined").is_none());
    assert_eq!(children_of(&bench, ThreadId::MAIN).await.len(), 1);
    // The same id for another task, or another title, is refused.
    for other in [
        json!({"task": "Autre chose.", "client_request_id": "soupe-1"}),
        json!({"task": TASK, "title": "Soupes", "client_request_id": "soupe-1"}),
    ] {
        let refused = call(&main, "threads_start", json!({"arguments": other})).await;
        assert!(
            refused["error"]
                .as_str()
                .is_some_and(|error| error.contains("already started sub-agent")),
            "{refused}"
        );
    }
    assert_eq!(children_of(&bench, ThreadId::MAIN).await.len(), 1);

    // Another turn of the parent (the child's wake, or a new message): the same call does not
    // join the earlier child.
    let before = turn_of(&bench, ThreadId::MAIN).await;
    bench.gate.notify_waiters();
    next_turn(&bench, ThreadId::MAIN, before).await;
    let refused = call(&main, "threads_start", args).await;
    assert!(
        refused["error"]
            .as_str()
            .is_some_and(|error| error.contains("another turn")),
        "{refused}"
    );
    let fresh = call(
        &main,
        "threads_start",
        json!({"arguments": {"task": TASK, "client_request_id": "soupe-2"}}),
    )
    .await;
    assert_ne!(fresh["result"]["thread"], child, "{fresh}");
    assert_eq!(children_of(&bench, ThreadId::MAIN).await.len(), 2);
    // Nobody waited for the first one: its result woke the parent.
    until_woken(&bench, ThreadId::MAIN, ThreadId(child.as_i64().unwrap())).await;
    bench.gate.notify_waiters();
}

#[tokio::test]
async fn threads_start_is_hidden_and_refused_past_max_depth() {
    let bench = held_bench().await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let all = [
        "threads_cancel",
        "threads_list",
        "threads_read",
        "threads_send",
        "threads_start",
    ];
    let names = tool_names(&main).await;
    for name in all {
        assert!(
            names.contains(&name.to_owned()),
            "{name} missing from {names:?}"
        );
    }
    let child = start_child(&main, TASK).await;
    let child_client = bench
        .client(&token_of(&bench, child).await, "claude-code", &[])
        .await;
    let names = tool_names(&child_client).await;
    for name in ["threads_start", "threads_send"] {
        assert!(
            !names.contains(&name.to_owned()),
            "{name} offered to a sub-agent"
        );
    }
    for name in ["threads_read", "threads_list", "threads_cancel"] {
        assert!(
            names.contains(&name.to_owned()),
            "{name} missing from {names:?}"
        );
    }
    // Refused when called anyway, directly or through use_tool: no grandchild.
    let direct = call(
        &child_client,
        "threads_start",
        json!({"arguments": {"task": TASK}}),
    )
    .await;
    let through = call(
        &child_client,
        "use_tool",
        json!({"arguments": {"name": "threads_start", "arguments": {"task": TASK}}}),
    )
    .await;
    let too_deep = fronde_agent::threads::Delegation::TooDeep { max_depth: 1 }.to_string();
    for refused in [&direct, &through] {
        assert_eq!(*refused, json!({"error": too_deep}), "{refused}");
    }
    // The instructions speak of delegating to the thread that may, not to its sub-agent.
    let says = |client: &Client| {
        client
            .peer_info()
            .and_then(|info| info.instructions.clone())
            .unwrap_or_default()
            .contains("threads_start a sub-agent")
    };
    assert!(says(&main));
    assert!(!says(&child_client));
    // The threads refuse it too, whoever asks.
    let error = bench
        .threads
        .start_child(
            child,
            None,
            fronde_agent::threads::ChildTask {
                task: TASK.into(),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<fronde_agent::threads::Delegation>(),
        Some(&fronde_agent::threads::Delegation::TooDeep { max_depth: 1 })
    );
    assert!(children_of(&bench, child).await.is_empty());
    bench.gate.notify_waiters();
}

#[tokio::test]
async fn outside_client_gets_no_threads_tools() {
    let bench = held_bench().await;
    bench.start(ThreadId::MAIN).await;
    let outside = bench
        .client(TOKEN, "Fronde", &[("x-fronde-thread", "1")])
        .await;
    let names = tool_names(&outside).await;
    assert!(
        !names.iter().any(|name| name.starts_with("threads_")),
        "{names:?}"
    );
    for (tool, arguments) in [
        ("threads_start", json!({"task": TASK, "thread": 1})),
        ("threads_list", json!({})),
        ("threads_read", json!({"thread": 1})),
    ] {
        let refused = call(
            &outside,
            tool,
            json!({"arguments": arguments, "_meta": {"thread": 1}}),
        )
        .await;
        assert!(
            refused["error"]
                .as_str()
                .is_some_and(|error| error.contains("outside MCP client")),
            "{tool}: {refused}"
        );
    }
    assert!(children_of(&bench, ThreadId::MAIN).await.is_empty());
    bench.gate.notify_waiters();
}

#[tokio::test]
async fn threads_list_shows_children_and_models() {
    let gate = Arc::new(Notify::new());
    let mut bench = bench_with(
        FakeScript {
            models: true,
            ..held(&gate)
        },
        Some(FakeScript {
            models: true,
            ..FakeScript::default()
        }),
        many(),
    )
    .await;
    bench.gate = gate;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    timeout(WAIT, async {
        while bench.threads.models("claude").await.unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the models were never learnt");

    // A model the provider does not offer, an unknown provider and a planned one are refused,
    // saying what may be chosen.
    for (arguments, expected) in [
        (
            json!({"task": TASK, "model": "gpt-9"}),
            "fake-small, fake-large",
        ),
        (
            json!({"task": TASK, "provider": "mistral"}),
            "only claude is configured",
        ),
        (json!({"task": TASK, "provider": "codex"}), "planned"),
        (
            json!({"task": TASK, "provider": "claude", "model": "fake-huge"}),
            "fake-small, fake-large",
        ),
    ] {
        let refused = call(&main, "threads_start", json!({"arguments": arguments})).await;
        assert!(
            refused["error"]
                .as_str()
                .is_some_and(|error| error.contains(expected)),
            "{refused}"
        );
    }
    assert!(children_of(&bench, ThreadId::MAIN).await.is_empty());

    let started = call(
        &main,
        "threads_start",
        json!({"arguments": {"task": TASK, "provider": "claude", "model": "fake-large", "title": "Soupes"}}),
    )
    .await;
    let child = ThreadId(started["result"]["thread"].as_i64().unwrap());
    token_of(&bench, child).await;
    let fake = latest_fake(&bench, 2).await;
    timeout(WAIT, async {
        while !fake
            .record()
            .config_sets
            .contains(&("model".to_owned(), "fake-large".to_owned()))
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the sub-agent's model was never set");

    let listed = call(&main, "threads_list", json!({})).await;
    let children = listed["result"]["children"].as_array().unwrap();
    assert_eq!(children.len(), 1, "{listed}");
    assert_eq!(children[0]["thread"], child.0);
    assert_eq!(children[0]["title"], "Soupes");
    assert_eq!(children[0]["provider"], "claude");
    assert_eq!(children[0]["model"], "fake-large");
    assert!(children[0]["status"].is_string());
    assert_eq!(
        listed["result"]["providers"],
        json!([{"id": "claude", "background": "unknown", "models": [
            {"id": "fake-small", "name": "fake small"},
            {"id": "fake-large", "name": "fake large"},
        ]}])
    );
    bench.gate.notify_waiters();
}

#[tokio::test]
async fn cancel_stops_the_child_and_keeps_what_it_wrote() {
    let bench = held_bench().await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let child = start_child(&main, TASK).await;
    let assistant = bench.assistant(child).await;
    timeout(WAIT, async {
        while !assistant
            .log()
            .since(0)
            .iter()
            .any(|entry| matches!(entry.event, Event::Assistant { .. }))
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let stopped = call(
        &main,
        "threads_cancel",
        json!({"arguments": {"thread": child.0, "reason": "Plus besoin."}}),
    )
    .await;
    assert_eq!(
        stopped["result"],
        json!({"thread": child.0, "stopped": true}),
        "{stopped}"
    );
    // The sub-agent was told to stop; its parent's own turn goes on.
    let fake = latest_fake(&bench, 2).await;
    assert!(fake.record().cancels >= 1);
    assert!(
        bench
            .assistant(ThreadId::MAIN)
            .await
            .current_turn()
            .await
            .is_some()
    );
    // The fake agent ends its turn once released; the parent reads it in its next turn.
    let before = turn_of(&bench, ThreadId::MAIN).await;
    bench.gate.notify_waiters();
    until_idle(&assistant).await;
    next_turn(&bench, ThreadId::MAIN, before).await;
    let read = call(
        &main,
        "threads_read",
        json!({"arguments": {"thread": child.0}}),
    )
    .await;
    let items = read["result"]["items"].as_array().unwrap();
    assert!(
        items.iter().any(|item| item["kind"] == "notice"
            && item["text"].as_str().unwrap().contains("Plus besoin.")),
        "{read}"
    );
    assert!(
        items.iter().any(|item| item["kind"] == "assistant"),
        "{read}"
    );
    // The parent asked for it: the stopped turn woke nobody.
    assert_eq!(
        children_ended_in(&*bench.assistant(ThreadId::MAIN).await),
        []
    );
    // Stopping it again stops nothing; its result stays.
    let again = call(
        &main,
        "threads_cancel",
        json!({"arguments": {"thread": child.0}}),
    )
    .await;
    assert_eq!(again["result"]["stopped"], false, "{again}");
    bench.gate.notify_waiters();
}

/// Thread `id`'s running turn, before its agent is released: see [`next_turn`].
async fn turn_of(bench: &Bench, id: ThreadId) -> Option<u64> {
    bench.assistant(id).await.current_turn().await
}

/// Waits for thread `id`'s next turn after `before`, once its agent is released, with the same
/// agent and token: the wake of a sub-agent whose result no call waited for, or else a new
/// message, sent once the thread is idle.
async fn next_turn(bench: &Bench, id: ThreadId, before: Option<u64>) {
    let assistant = bench.assistant(id).await;
    timeout(WAIT, async {
        loop {
            match assistant.current_turn().await {
                Some(turn) if Some(turn) != before => return,
                None if !matches!(
                    assistant.status(),
                    fronde_protocol::ThreadStatus::Running | fronde_protocol::ThreadStatus::Queued
                ) =>
                {
                    assistant.send("Encore".into()).await;
                }
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("no next turn");
}

#[tokio::test]
async fn children_running_at_once_are_capped() {
    use fronde_agent::threads::Delegation;
    let bench = held_bench().await;
    // The documented default, read from the threads as configured.
    let max_children_running = bench.threads.limits().children_running;
    assert_eq!(max_children_running, 3);
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let mut children = Vec::new();
    for n in 0..max_children_running {
        children.push(start_child(&main, &format!("Tâche {n}.")).await);
    }
    for child in &children {
        let assistant = bench.assistant(*child).await;
        assert!(matches!(
            assistant.status(),
            fronde_protocol::ThreadStatus::Running | fronde_protocol::ThreadStatus::Queued
        ));
    }
    let refused = call(
        &main,
        "threads_start",
        json!({"arguments": {"task": "Une de trop."}}),
    )
    .await;
    let limit = Delegation::TooManyRunning {
        limit: max_children_running,
    }
    .to_string();
    assert_eq!(refused, json!({"error": limit}), "{refused}");
    assert!(limit.contains("3 sub-agents running"), "{limit}");
    assert!(limit.contains("[threads] children_running"), "{limit}");
    assert_eq!(
        children_of(&bench, ThreadId::MAIN).await.len(),
        max_children_running
    );
    bench.gate.notify_waiters();
}

#[tokio::test]
async fn children_kept_are_capped() {
    use fronde_agent::threads::{Delegation, Limits};
    // The documented default is 200: a guard against clutter. A smaller one, configured, is
    // enforced the same way.
    assert_eq!(Limits::default().children_kept, 200);
    let max_children_kept = 30;
    let bench = limited(
        None,
        Limits {
            children_kept: max_children_kept,
            // Every child created here counts in the day's too.
            children_per_day: 100,
            ..Limits::default()
        },
    )
    .await;
    assert_eq!(bench.threads.limits().children_kept, max_children_kept);
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let mut kept = Vec::new();
    for n in 0..max_children_kept {
        let row = bench
            .threads
            .create(
                "alix",
                StartedBy::Thread(ThreadId::MAIN),
                Some(ThreadId::MAIN),
                &format!("Ancienne {n}"),
            )
            .await
            .unwrap();
        kept.push(row.id);
    }
    let refused = call(
        &main,
        "threads_start",
        json!({"arguments": {"task": "Une de trop."}}),
    )
    .await;
    let limit = Delegation::TooManyKept {
        limit: max_children_kept,
    }
    .to_string();
    assert_eq!(refused, json!({"error": limit}), "{refused}");
    assert!(limit.contains("30 sub-agents"), "{limit}");
    assert!(limit.contains("[threads] children_kept"), "{limit}");
    assert_eq!(
        children_of(&bench, ThreadId::MAIN).await.len(),
        max_children_kept
    );
    // An archived one no longer counts.
    bench.threads.set_archived(kept[0], true).await.unwrap();
    start_child(&main, "Une de plus.").await;
    bench.gate.notify_waiters();
}

#[tokio::test]
async fn an_archived_thread_cannot_delegate() {
    let bench = held_bench().await;
    let other = bench.new_thread().await;
    bench.threads.set_archived(other, true).await.unwrap();
    let error = bench
        .threads
        .start_child(
            other,
            Some(1),
            fronde_agent::threads::ChildTask {
                task: TASK.into(),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<fronde_agent::threads::Delegation>(),
        Some(&fronde_agent::threads::Delegation::CallerArchived)
    );
    assert!(children_of(&bench, other).await.is_empty());
}

/// A thread's token outlives its turns while its agent idles, and a process the agent started
/// holds it too: the tools that act refuse it outside a turn.
#[tokio::test]
async fn threads_tools_refuse_a_thread_out_of_its_turn() {
    let bench = bench_with(FakeScript::default(), None, many()).await;
    let token = bench.start(ThreadId::MAIN).await;
    let main_assistant = bench.assistant(ThreadId::MAIN).await;
    until_idle(&main_assistant).await;
    assert_eq!(
        bench.status(&token).await,
        200,
        "the idle agent's token lives"
    );
    let main = bench.client(&token, "claude-code", &[]).await;
    let out_of_turn = json!({"error": fronde_hub::threads::OUT_OF_TURN});
    for (tool, arguments) in [
        ("threads_start", json!({"task": TASK})),
        ("threads_start", json!({"task": TASK, "wait": true})),
        ("threads_send", json!({"thread": 2, "text": "Salut"})),
        ("threads_read", json!({"thread": 2})),
        ("threads_cancel", json!({"thread": 2})),
    ] {
        let refused = call(&main, tool, json!({"arguments": arguments})).await;
        assert_eq!(refused, out_of_turn, "{tool}");
    }
    assert!(children_of(&bench, ThreadId::MAIN).await.is_empty());
    // Listing tells nothing it could act on.
    let listed = call(&main, "threads_list", json!({})).await;
    assert_eq!(listed["result"]["children"], json!([]), "{listed}");
}

#[tokio::test]
async fn a_thread_cannot_reach_itself() {
    let bench = held_bench().await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let child = start_child(&main, TASK).await;
    let child_client = bench
        .client(&token_of(&bench, child).await, "claude-code", &[])
        .await;
    let itself = json!({"error": fronde_agent::threads::Delegation::Itself.to_string()});
    for (client, id) in [(&main, ThreadId::MAIN), (&child_client, child)] {
        let assistant = bench.assistant(id).await;
        let messages = |assistant: &Assistant| {
            assistant
                .log()
                .since(0)
                .iter()
                .filter(|entry| matches!(entry.event, Event::User { .. }))
                .count()
        };
        let before = messages(&assistant);
        let turn = assistant.current_turn().await;
        assert!(turn.is_some());
        for answer in refusals(client, id.0).await {
            assert_eq!(answer, itself, "thread {id}");
        }
        assert_eq!(messages(&assistant), before, "thread {id}");
        assert_eq!(
            assistant.current_turn().await,
            turn,
            "thread {id} was stopped"
        );
    }
    bench.gate.notify_waiters();
}

#[tokio::test]
async fn the_same_client_request_id_from_another_parent_starts_its_own_child() {
    let bench = held_bench().await;
    let other = bench.new_thread().await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let other_client = bench
        .client(&bench.start(other).await, "claude-code", &[])
        .await;
    let args = json!({"arguments": {"task": TASK, "client_request_id": "soupe-1"}});
    let first = call(&main, "threads_start", args.clone()).await;
    let second = call(&other_client, "threads_start", args).await;
    assert!(second["result"].get("joined").is_none(), "{second}");
    assert_ne!(first["result"]["thread"], second["result"]["thread"]);
    let id = ThreadId(second["result"]["thread"].as_i64().unwrap());
    assert_eq!(
        bench.threads.get(id).await.unwrap().unwrap().parent,
        Some(other)
    );
    assert_eq!(children_of(&bench, ThreadId::MAIN).await.len(), 1);
    assert_eq!(children_of(&bench, other).await, [id]);
    bench.gate.notify_waiters();
}

/// A wait ends with the turn of the thread that waits: nobody would read its answer.
#[tokio::test]
async fn a_wait_ends_with_the_callers_turn() {
    let child_gate = Arc::new(Notify::new());
    let bench = held_parent(FakeScript {
        updates: vec![text_chunk("Je cherche."), text_chunk(" Trouvé.")],
        pause: Some((1, child_gate.clone())),
        ..FakeScript::default()
    })
    .await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let waiting = call_in_background(&main, "threads_start", json!({"task": TASK, "wait": true}));
    let child = timeout(WAIT, async {
        loop {
            if let Some(child) = children_of(&bench, ThreadId::MAIN).await.first() {
                let assistant = bench.assistant(*child).await;
                if assistant
                    .log()
                    .since(0)
                    .iter()
                    .any(|entry| matches!(entry.event, Event::Assistant { .. }))
                {
                    return *child;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the sub-agent never started");
    // The parent's turn ends; its wait ends with it, long before its timeout.
    bench.gate.notify_waiters();
    let waited = timeout(WAIT, waiting).await.unwrap().unwrap();
    assert_eq!(waited["result"]["status"], "running", "{waited}");
    let assistant = bench.assistant(child).await;
    assert_eq!(assistant.status(), fronde_protocol::ThreadStatus::Running);
    child_gate.notify_waiters();
    until_idle(&assistant).await;
    assert_eq!(
        bench.threads.delivered(child),
        None,
        "nobody took its result"
    );
}

/// A sub-agent waiting for an agent slot: the wait says so at once.
#[tokio::test]
async fn a_wait_says_when_the_child_waits_for_an_agent() {
    let gate = Arc::new(Notify::new());
    let mut bench = bench_with(
        held(&gate),
        None,
        Budget {
            max_running: 2,
            ..Budget::default()
        },
    )
    .await;
    bench.gate = gate;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    // The one slot left besides the main thread's goes to the first sub-agent.
    let first = start_child(&main, TASK).await;
    token_of(&bench, first).await;
    let waited = timeout(
        WAIT,
        call(
            &main,
            "threads_start",
            json!({"arguments": {"task": "Une autre.", "wait": true}}),
        ),
    )
    .await
    .expect("the wait ran to its timeout");
    assert_eq!(waited["result"]["status"], "queued", "{waited}");
    let second = ThreadId(waited["result"]["thread"].as_i64().unwrap());
    assert_eq!(
        bench.assistant(second).await.status(),
        fronde_protocol::ThreadStatus::Queued
    );
    bench.gate.notify_waiters();
}

// Limits and cancellation: `[threads]`.

/// [`held_bench`] with `limits`, sub-agents playing `child` when given.
async fn limited(child: Option<FakeScript>, limits: fronde_agent::threads::Limits) -> Bench {
    let gate = Arc::new(Notify::new());
    let mut bench = bench_with(held(&gate), child, Budget { limits, ..many() }).await;
    bench.gate = gate;
    bench
}

/// A sub-agent that asks a permission in each turn and waits for the answer: its turn ends
/// only when stopped (or answered).
fn asking() -> FakeScript {
    FakeScript {
        updates: vec![text_chunk("Je cherche.")],
        ask_permission: true,
        ..FakeScript::default()
    }
}

fn events_of(assistant: &Assistant) -> Vec<Event> {
    assistant
        .log()
        .since(0)
        .into_iter()
        .map(|entry| entry.event)
        .collect()
}

fn children_ended_in(assistant: &Assistant) -> Vec<(ThreadId, fronde_protocol::TurnOutcome)> {
    events_of(assistant)
        .into_iter()
        .filter_map(|event| match event {
            Event::ChildEnded {
                thread, outcome, ..
            } => Some((thread, outcome)),
            _ => None,
        })
        .collect()
}

fn cancelled() -> Event {
    Event::TurnEnded {
        outcome: fronde_protocol::TurnOutcome::Cancelled,
    }
}

/// The documented defaults are the configuration's, and every `[threads]` value reaches the
/// threads.
#[test]
fn the_limits_come_from_threads_config() {
    use fronde_agent::threads::Limits;
    let config = |threads: &str| -> fronde_hub::config::Config {
        toml::from_str(&format!(
            "[server]\ntoken = {{ env = \"UNUSED\" }}\n[data]\ndir = \"/nonexistent\"\ntimezone = \"Europe/Paris\"\nuser = \"alix\"\n[memory]\ngit = false\n{threads}"
        ))
        .unwrap()
    };
    let defaults = fronde::budget(&config(""));
    assert_eq!(defaults.limits, Limits::default());
    assert_eq!(
        defaults.limits,
        Limits {
            max_depth: 1,
            children_running: 3,
            children_per_day: 30,
            children_kept: 200,
            child_turns_per_day: 100,
            child_turn_timeout: Duration::from_secs(30 * 60),
            wakes_without_owner: 10,
            reserve: 30,
        }
    );
    assert_eq!(
        fronde::children_line(&config("")),
        "your threads may start sub-agents, theirs may not; 3 running at once per thread, 200 \
         kept per thread, 30m per turn, 10 wakes in a row without you; 30% of each subscription \
         window kept for you; 30 a day in all (100 turns), daily counts apply only without a \
         usage reading"
    );
    let set = config(
        "[threads]\nmax_depth = 2\nchildren_running = 5\nchildren_per_day = 50\nchildren_kept = 80\nchild_turns_per_day = 70\nchild_turn_timeout = \"1h\"\nwakes_without_owner = 4\nreserve = \"15%\"\n",
    );
    assert_eq!(
        fronde::budget(&set).limits,
        Limits {
            max_depth: 2,
            children_running: 5,
            children_per_day: 50,
            children_kept: 80,
            child_turns_per_day: 70,
            child_turn_timeout: Duration::from_secs(3600),
            wakes_without_owner: 4,
            reserve: 15,
        }
    );
    assert!(
        fronde::children_line(&config("[threads]\nreserve = \"0%\"\n"))
            .ends_with("no reserve kept for you (reserve = \"0%\"); 30 a day in all (100 turns)"),
        "reserve off"
    );
    assert!(
        fronde::children_line(&set).starts_with("sub-agents go 2 levels deep"),
        "{}",
        fronde::children_line(&set)
    );
    assert!(
        fronde::children_line(&config("[threads]\nmax_depth = 0\n")).starts_with("off"),
        "max_depth 0"
    );
}

/// `max_depth = 0`: no thread may start sub-agents; the tools are hidden and refused, and the
/// refusal names the setting.
#[tokio::test]
async fn max_depth_zero_turns_sub_agents_off() {
    use fronde_agent::threads::{Delegation, Limits};
    let bench = limited(
        None,
        Limits {
            max_depth: 0,
            ..Limits::default()
        },
    )
    .await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let names = tool_names(&main).await;
    for name in ["threads_start", "threads_send"] {
        assert!(!names.contains(&name.to_owned()), "{name} offered");
    }
    let refused = call(
        &main,
        "use_tool",
        json!({"arguments": {"name": "threads_start", "arguments": {"task": TASK}}}),
    )
    .await;
    let off = Delegation::TooDeep { max_depth: 0 }.to_string();
    assert_eq!(refused, json!({"error": off}), "{refused}");
    assert!(off.contains("[threads] max_depth is 0"), "{off}");
    assert!(children_of(&bench, ThreadId::MAIN).await.is_empty());
    bench.gate.notify_waiters();
}

/// A configured `children_running` is the cap, named in the refusal.
#[tokio::test]
async fn children_running_limit() {
    use fronde_agent::threads::{Delegation, Limits};
    let bench = limited(
        None,
        Limits {
            children_running: 1,
            ..Limits::default()
        },
    )
    .await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    start_child(&main, TASK).await;
    let refused = call(
        &main,
        "threads_start",
        json!({"arguments": {"task": "Une de trop."}}),
    )
    .await;
    let limit = Delegation::TooManyRunning { limit: 1 }.to_string();
    assert_eq!(refused, json!({"error": limit}), "{refused}");
    assert!(limit.contains("[threads] children_running"), "{limit}");
    assert_eq!(children_of(&bench, ThreadId::MAIN).await.len(), 1);
    bench.gate.notify_waiters();
}

/// `children_per_day` counts the sub-agents every thread started in 24 hours, archived ones
/// included: archiving frees no place in it.
#[tokio::test]
async fn children_per_day_limit() {
    use fronde_agent::threads::{Delegation, Limits};
    let bench = limited(
        None,
        Limits {
            children_per_day: 3,
            ..Limits::default()
        },
    )
    .await;
    let other = bench.new_thread().await;
    for title in ["Hier", "Avant-hier"] {
        let row = bench
            .threads
            .create("alix", StartedBy::Thread(other), Some(other), title)
            .await
            .unwrap();
        bench.threads.set_archived(row.id, true).await.unwrap();
    }
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    start_child(&main, TASK).await;
    let limit = Delegation::TooManyToday { limit: 3 }.to_string();
    assert!(limit.contains("[threads] children_per_day"), "{limit}");
    let refused = call(
        &main,
        "threads_start",
        json!({"arguments": {"task": "Une de trop."}}),
    )
    .await;
    assert_eq!(refused, json!({"error": limit}), "{refused}");
    // Another thread is refused too: the count is for all of them.
    let second = bench
        .client(&bench.start(other).await, "claude-code", &[])
        .await;
    let refused = call(
        &second,
        "threads_start",
        json!({"arguments": {"task": "Une autre."}}),
    )
    .await;
    assert_eq!(refused, json!({"error": limit}), "{refused}");
    assert_eq!(children_of(&bench, ThreadId::MAIN).await.len(), 1);
    bench.gate.notify_waiters();
}

/// A sub-agent's turn past `child_turn_timeout` is stopped as Stop does: a notice, its card
/// closed, its end cancelled, and its parent told so in a `ChildEnded` entry. The parent's own
/// turn, longer, is not a sub-agent's and goes on.
#[tokio::test]
async fn child_turn_times_out() {
    use fronde_agent::threads::Limits;
    // Shorter than its card's own timeout (30 minutes), which would end the turn first.
    let bench = limited(
        Some(asking()),
        Limits {
            child_turn_timeout: Duration::from_secs(10 * 60),
            ..Limits::default()
        },
    )
    .await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let child = start_child(&main, TASK).await;
    let assistant = bench.assistant(child).await;
    // The turn has its card: only its timeout ends it now, on a clock moved by hand.
    next_request(&assistant, 0).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(10 * 60 + 1)).await;
    tokio::time::resume();
    until_idle(&assistant).await;
    let shown = events_of(&assistant);
    assert!(
        shown.iter().any(|event| matches!(
            event,
            Event::Notice { text } if text.starts_with(fronde_agent::assistant::TIMED_OUT)
                && text.contains("10m")
                && text.contains("[threads] child_turn_timeout")
        )),
        "{shown:?}"
    );
    assert_eq!(shown.last(), Some(&cancelled()), "{shown:?}");
    assert!(
        !bench
            .requests
            .pending_by_topic()
            .await
            .unwrap()
            .contains_key(&format!("thread:{child}")),
        "its card is closed"
    );
    let main_assistant = bench.assistant(ThreadId::MAIN).await;
    assert_eq!(
        children_ended_in(&main_assistant),
        [(child, fronde_protocol::TurnOutcome::Cancelled)]
    );
    assert!(main_assistant.current_turn().await.is_some());
    bench.gate.notify_waiters();
}

/// `threads_cancel` stops the whole tree under the sub-agent named, deepest first, with their
/// cards; none of the turns stopped wakes a parent. With `max_depth = 2` a sub-agent may start
/// its own, which may not.
#[tokio::test]
async fn cancel_reaches_grandchildren() {
    use fronde_agent::threads::{Delegation, Limits};
    let bench = limited(
        Some(asking()),
        Limits {
            max_depth: 2,
            ..Limits::default()
        },
    )
    .await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let child = start_child(&main, TASK).await;
    let child_assistant = bench.assistant(child).await;
    next_request(&child_assistant, 0).await;
    let child_client = bench
        .client(&token_of(&bench, child).await, "claude-code", &[])
        .await;
    let grandchild = start_child(&child_client, "Une partie de la tâche.").await;
    let grandchild_assistant = bench.assistant(grandchild).await;
    next_request(&grandchild_assistant, 0).await;
    // Two levels under the owner's thread, and no more.
    let grandchild_client = bench
        .client(&token_of(&bench, grandchild).await, "claude-code", &[])
        .await;
    let refused = call(
        &grandchild_client,
        "threads_start",
        json!({"arguments": {"task": "Encore plus bas."}}),
    )
    .await;
    let too_deep = Delegation::TooDeep { max_depth: 2 }.to_string();
    assert_eq!(refused, json!({"error": too_deep}), "{refused}");
    assert!(too_deep.contains("[threads] max_depth"), "{too_deep}");

    let stopped = call(
        &main,
        "threads_cancel",
        json!({"arguments": {"thread": child.0}}),
    )
    .await;
    assert_eq!(stopped["result"]["stopped"], true, "{stopped}");
    for assistant in [&child_assistant, &grandchild_assistant] {
        until_idle(assistant).await;
        let shown = events_of(assistant);
        assert_eq!(shown.last(), Some(&cancelled()), "{shown:?}");
    }
    let pending = bench.requests.pending_by_topic().await.unwrap();
    for id in [child, grandchild] {
        assert!(
            !pending.contains_key(&format!("thread:{id}")),
            "{pending:?}"
        );
    }
    // Nobody is woken: the one who stopped them knows.
    let main_assistant = bench.assistant(ThreadId::MAIN).await;
    assert_eq!(children_ended_in(&child_assistant), []);
    assert_eq!(children_ended_in(&main_assistant), []);
    assert!(main_assistant.current_turn().await.is_some());
    bench.gate.notify_waiters();
}

/// Archiving a thread stops its sub-agents' turns, which stay, not archived, and wake nobody;
/// the secrets given in its cards for the conversation are forgotten, another thread's kept.
#[tokio::test]
async fn archive_cancels_children_and_forgets_secrets() {
    use fronde_db::ephemeral::EphemeralScope;
    let bench = limited(Some(asking()), fronde_agent::threads::Limits::default()).await;
    let parent = bench.new_thread().await;
    let parent_client = bench
        .client(&bench.start(parent).await, "claude-code", &[])
        .await;
    let child = start_child(&parent_client, TASK).await;
    let child_assistant = bench.assistant(child).await;
    next_request(&child_assistant, 0).await;
    let ephemeral = bench.requests.secrets().unwrap().ephemeral().clone();
    let put = |topic: &str| {
        let scope = EphemeralScope {
            topic: topic.into(),
            asker: "claude-code".into(),
        };
        let (reference, _) = ephemeral
            .put(
                scope.clone(),
                SecretString::from(CANARY),
                fronde_db::ephemeral::DEFAULT_TTL,
                false,
            )
            .unwrap();
        (reference, scope)
    };
    let (parents, parent_scope) = put(&format!("thread:{parent}"));
    let (childs, child_scope) = put(&format!("thread:{child}"));
    let (mains, main_scope) = put("conversation");

    bench.threads.set_archived(parent, true).await.unwrap();
    until_idle(&child_assistant).await;
    assert_eq!(events_of(&child_assistant).last(), Some(&cancelled()));
    let child_row = bench.threads.get(child).await.unwrap().unwrap();
    assert!(child_row.archived_at.is_none(), "the sub-agent stays");
    assert_eq!(children_ended_in(&*bench.assistant(parent).await), []);
    assert!(
        ephemeral.take(parents.as_str(), &parent_scope).is_err(),
        "the archived thread's secret is forgotten"
    );
    assert!(
        ephemeral.take(childs.as_str(), &child_scope).is_err(),
        "and its sub-agent's"
    );
    assert!(ephemeral.take(mains.as_str(), &main_scope).is_ok());

    // A sub-agent archived on its own wakes its parent no more.
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let second = start_child(&main, "Une autre tâche.").await;
    let second_assistant = bench.assistant(second).await;
    next_request(&second_assistant, 0).await;
    bench.threads.set_archived(second, true).await.unwrap();
    until_idle(&second_assistant).await;
    assert_eq!(events_of(&second_assistant).last(), Some(&cancelled()));
    // Its parent is told it was archived, by an entry that wakes nothing: its stopped turn's
    // result never comes.
    let main_assistant = bench.assistant(ThreadId::MAIN).await;
    assert_eq!(
        children_ended_in(&main_assistant),
        [(second, fronde_protocol::TurnOutcome::Archived)]
    );
    assert!(
        !events_of(&main_assistant)
            .iter()
            .any(|event| matches!(event, Event::Woken { .. })),
        "not woken"
    );
    bench.gate.notify_waiters();
}

/// Stop on a parent stops its own turn only: its sub-agent goes on. Stop in a sub-agent's own
/// thread, which its parent did not ask for, ends its turn cancelled and wakes the parent.
#[tokio::test]
async fn stop_on_parent_leaves_children_running() {
    let bench = limited(Some(asking()), fronde_agent::threads::Limits::default()).await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let child = start_child(&main, TASK).await;
    let child_assistant = bench.assistant(child).await;
    next_request(&child_assistant, 0).await;
    let main_assistant = bench.assistant(ThreadId::MAIN).await;
    main_assistant.cancel().await;
    bench.gate.notify_waiters();
    until_idle(&main_assistant).await;
    assert!(child_assistant.current_turn().await.is_some());
    assert_eq!(
        child_assistant.status(),
        fronde_protocol::ThreadStatus::Running
    );

    child_assistant.cancel().await;
    until_idle(&child_assistant).await;
    assert_eq!(events_of(&child_assistant).last(), Some(&cancelled()));
    timeout(WAIT, async {
        while children_ended_in(&main_assistant).is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the parent was never told");
    assert_eq!(
        children_ended_in(&main_assistant),
        [(child, fronde_protocol::TurnOutcome::Cancelled)]
    );
    bench.gate.notify_waiters();
}

/// A call of the hub's `tool` by the main thread's agent, with the context the gate made when
/// its turn was `turn`.
fn call_in_turn(
    bench: &Bench,
    turn: Option<u64>,
    tool: &'static str,
    arguments: Value,
) -> tokio::task::JoinHandle<Value> {
    let registry = bench.registry.clone();
    tokio::spawn(async move {
        let context = CallContext {
            caller: Caller::Thread(ThreadId::MAIN),
            user: "alix".into(),
            topic: "conversation".into(),
            turn,
            cancelled: tokio_util::sync::CancellationToken::new(),
        };
        let output = registry.call_as(&context, tool, arguments).await;
        serde_json::from_str(&output.to_text()).unwrap()
    })
}

fn question() -> Value {
    json!({"questions": [{
        "question": "Quelle appli installer ?",
        "header": "Appli",
        "options": [
            {"label": "Mealie", "description": "Les recettes"},
            {"label": "Seerr", "description": "Les films"},
        ],
    }]})
}

fn secret() -> Value {
    json!({"label": "Mot de passe Nextcloud", "reason": "Pour lire ton agenda."})
}

/// The cards shown and closed in `assistant`'s stream, and the ends of its turns, in order.
fn cards_and_ends(assistant: &Assistant) -> Vec<Event> {
    assistant
        .log()
        .since(0)
        .into_iter()
        .map(|entry| entry.event)
        .filter(|event| {
            matches!(
                event,
                Event::Request { .. } | Event::RequestClosed { .. } | Event::TurnEnded { .. }
            )
        })
        .collect()
}

/// Waits until `assistant`'s stream satisfies `done`.
async fn wait_for_stream(assistant: &Assistant, done: impl Fn(&[Event]) -> bool) {
    timeout(WAIT, async {
        loop {
            let events: Vec<Event> = assistant
                .log()
                .since(0)
                .into_iter()
                .map(|entry| entry.event)
                .collect();
            if done(&events) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; the stream has {:?}", assistant.log().since(0)));
}

fn turn_ended(events: &[Event]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, Event::TurnEnded { .. }))
}

/// The main thread's agent calls `tool` as its turn ends, the call first: the card is being
/// shown when the agent ends the turn. The turn's end waits for the card, then cancels it: its
/// end comes before the turn's.
async fn hub_card_shown_as_the_turn_ends_closes_before_its_end(tool: &'static str, args: Value) {
    let bench = Bench::held().await;
    let main = bench.assistant(ThreadId::MAIN).await;
    main.send("Salut".into()).await;
    let turn = main.current_turn().await;
    assert!(turn.is_some());

    let shown = bench.journal.hold(Step::Announce);
    let call = call_in_turn(&bench, turn, tool, args);
    bench.journal.reached().await;
    // The card is being shown: the agent ends its turn meanwhile.
    bench.gate.notify_one();
    wait_for_stream(&main, |events| {
        events.contains(&Event::Assistant {
            text: " Voilà.".into(),
        })
    })
    .await;
    // Time enough for the turn's end to be logged, were it not waiting for the card.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !turn_ended(&cards_and_ends(&main)),
        "{:?}",
        cards_and_ends(&main)
    );
    shown.notify_one();

    let result = timeout(WAIT, call).await.unwrap().unwrap();
    assert_eq!(result["result"]["status"], "cancelled", "{result}");
    wait_for_stream(&main, turn_ended).await;
    let events = cards_and_ends(&main);
    let [
        Event::Request { id, .. },
        Event::RequestClosed {
            id: closed,
            outcome: Outcome::Cancelled,
            ..
        },
        Event::TurnEnded {
            outcome: TurnOutcome::Done,
        },
    ] = events.as_slice()
    else {
        panic!("{events:?}");
    };
    assert_eq!(id, closed);
    assert!(bench.requests.pending_by_topic().await.unwrap().is_empty());
}

/// The main thread's agent calls `tool` as its turn ends, the turn first: the turn is
/// cancelling its cards when the call comes, then has ended. Each time the call is refused, and
/// no card is shown: none is left open after the turn's end.
async fn hub_card_asked_as_the_turn_ends_is_refused(tool: &'static str, args: Value) {
    let bench = Bench::held().await;
    let main = bench.assistant(ThreadId::MAIN).await;
    main.send("Salut".into()).await;
    let turn = main.current_turn().await;
    assert!(turn.is_some());
    // A card of the turn, whose cancel the turn's end is slow to show.
    let first = call_in_turn(&bench, turn, tool, args.clone());
    let (first_id, _) = next_request(&main, 0).await;
    let closing = bench.journal.hold(Step::Close);
    bench.gate.notify_one();
    bench.journal.reached().await;
    assert!(!turn_ended(&cards_and_ends(&main)));

    // The turn is ending: the call made during it is refused at once.
    let refused = timeout(WAIT, call_in_turn(&bench, turn, tool, args.clone()))
        .await
        .expect("the call waits on a card its ending turn left open")
        .unwrap();
    assert!(
        refused["error"]
            .as_str()
            .is_some_and(|error| error.contains("your turn ended before it could be")),
        "{refused}"
    );
    closing.notify_one();
    let result = timeout(WAIT, first).await.unwrap().unwrap();
    assert_eq!(result["result"]["status"], "cancelled", "{result}");
    wait_for_stream(&main, turn_ended).await;

    // The turn has ended: a call made during it is refused too.
    let late = timeout(WAIT, call_in_turn(&bench, turn, tool, args))
        .await
        .expect("the call waits on a card shown after its turn ended")
        .unwrap();
    assert!(
        late["error"]
            .as_str()
            .is_some_and(|error| error.contains("your turn ended before it could be")),
        "{late}"
    );
    let events = cards_and_ends(&main);
    assert_eq!(
        events[1..],
        [
            Event::RequestClosed {
                id: first_id,
                outcome: Outcome::Cancelled,
                by: None,
            },
            Event::TurnEnded {
                outcome: TurnOutcome::Done,
            },
        ],
        "{events:?}"
    );
    assert_eq!(requests_in(&main), 1);
    assert!(bench.requests.pending_by_topic().await.unwrap().is_empty());
}

#[tokio::test]
async fn user_ask_shown_as_the_turn_ends_closes_before_its_end() {
    hub_card_shown_as_the_turn_ends_closes_before_its_end("user_ask", question()).await;
}

#[tokio::test]
async fn user_ask_asked_as_the_turn_ends_is_refused() {
    hub_card_asked_as_the_turn_ends_is_refused("user_ask", question()).await;
}

#[tokio::test]
async fn user_secret_shown_as_the_turn_ends_closes_before_its_end() {
    hub_card_shown_as_the_turn_ends_closes_before_its_end("user_secret", secret()).await;
}

#[tokio::test]
async fn user_secret_asked_as_the_turn_ends_is_refused() {
    hub_card_asked_as_the_turn_ends_is_refused("user_secret", secret()).await;
}

/// Archiving a thread retires every sub-agent under it, at any depth: each agent's token is
/// refused at once.
#[tokio::test]
async fn archive_revokes_the_tokens_under_it() {
    use fronde_agent::threads::Limits;
    let bench = limited(
        Some(asking()),
        Limits {
            max_depth: 2,
            ..Limits::default()
        },
    )
    .await;
    let parent = bench.new_thread().await;
    let parent_token = bench.start(parent).await;
    let parent_client = bench.client(&parent_token, "claude-code", &[]).await;
    let child = start_child(&parent_client, TASK).await;
    next_request(&*bench.assistant(child).await, 0).await;
    let child_token = token_of(&bench, child).await;
    let child_client = bench.client(&child_token, "claude-code", &[]).await;
    let grandchild = start_child(&child_client, "Une partie de la tâche.").await;
    let grandchild_assistant = bench.assistant(grandchild).await;
    next_request(&grandchild_assistant, 0).await;
    let grandchild_token = token_of(&bench, grandchild).await;
    for token in [&parent_token, &child_token, &grandchild_token] {
        assert_eq!(bench.status(token).await, 200);
    }
    bench.threads.set_archived(parent, true).await.unwrap();
    for token in [&parent_token, &child_token, &grandchild_token] {
        assert_eq!(bench.status(token).await, 401);
    }
    for id in [child, grandchild] {
        let row = bench.threads.get(id).await.unwrap().unwrap();
        assert!(row.archived_at.is_none(), "{id} stays, not archived");
        until_idle(&*bench.assistant(id).await).await;
    }
    bench.gate.notify_waiters();
}

/// `children_per_day` counts the last 24 hours only: sub-agents started before free their
/// place.
#[tokio::test]
async fn children_per_day_forgets_the_day_before() {
    use fronde_agent::threads::{Delegation, Limits};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, sea_query::Expr};
    let bench = limited(
        None,
        Limits {
            children_per_day: 2,
            ..Limits::default()
        },
    )
    .await;
    let other = bench.new_thread().await;
    let mut old = Vec::new();
    for title in ["Hier", "Avant-hier"] {
        let row = bench
            .threads
            .create("alix", StartedBy::Thread(other), Some(other), title)
            .await
            .unwrap();
        old.push(row.id.0);
    }
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let refused = call(&main, "threads_start", json!({"arguments": {"task": TASK}})).await;
    let limit = Delegation::TooManyToday { limit: 2 }.to_string();
    assert_eq!(refused, json!({"error": limit}), "{refused}");
    // A day and an hour ago: out of the window.
    let db = fronde_db::open(&bench.data).await.unwrap();
    fronde_entity::threads::Entity::update_many()
        .col_expr(
            fronde_entity::threads::Column::CreatedAt,
            Expr::value(sea_orm::prelude::DateTimeUtc::from(
                std::time::SystemTime::now() - Duration::from_secs(25 * 3600),
            )),
        )
        .filter(fronde_entity::threads::Column::Id.is_in(old))
        .exec(&db)
        .await
        .unwrap();
    start_child(&main, TASK).await;
    bench.gate.notify_waiters();
}

/// `child_turns_per_day` counts each turn threads start in sub-agents, a `threads_send` as
/// much as a `threads_start`, over the last 24 hours.
#[tokio::test]
async fn child_turns_per_day_counts_each_send() {
    use fronde_agent::threads::{Delegation, Limits};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, sea_query::Expr};
    let quick = FakeScript {
        updates: vec![text_chunk("Fait.")],
        ..FakeScript::default()
    };
    let bench = limited(
        Some(quick),
        Limits {
            child_turns_per_day: 2,
            ..Limits::default()
        },
    )
    .await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let child = start_child(&main, TASK).await;
    let child_assistant = bench.assistant(child).await;
    until_idle(&child_assistant).await;
    let send = json!({"arguments": {"thread": child.0, "text": "Encore."}});
    let sent = call(&main, "threads_send", send.clone()).await;
    assert_eq!(sent["result"]["status"], "running", "{sent}");
    until_idle(&child_assistant).await;
    let limit = Delegation::TooManyTurnsToday { limit: 2 }.to_string();
    assert!(limit.contains("[threads] child_turns_per_day"), "{limit}");
    let refused = call(&main, "threads_send", send.clone()).await;
    assert_eq!(refused, json!({"error": limit}), "{refused}");
    let refused = call(
        &main,
        "threads_start",
        json!({"arguments": {"task": "Autre."}}),
    )
    .await;
    assert_eq!(refused, json!({"error": limit}), "{refused}");
    // A day and an hour later, those turns no longer count.
    let db = fronde_db::open(&bench.data).await.unwrap();
    fronde_entity::stream_entries::Entity::update_many()
        .col_expr(
            fronde_entity::stream_entries::Column::At,
            Expr::value(sea_orm::prelude::DateTimeUtc::from(
                std::time::SystemTime::now() - Duration::from_secs(25 * 3600),
            )),
        )
        .filter(fronde_entity::stream_entries::Column::Topic.eq(format!("thread:{child}")))
        .exec(&db)
        .await
        .unwrap();
    let sent = call(&main, "threads_send", send).await;
    assert_eq!(sent["result"]["status"], "running", "{sent}");
    bench.gate.notify_waiters();
}

/// A turn a sub-agent's result started follows text the owner did not write: it may not ask
/// the owner for a secret, and the cards it shows through the hub say which sub-agent started
/// it.
#[tokio::test]
async fn a_wake_turn_asks_no_secret_and_names_its_sub_agent() {
    let quick = FakeScript {
        updates: vec![text_chunk("Fait.")],
        ..FakeScript::default()
    };
    let bench = limited(Some(quick), fronde_agent::threads::Limits::default()).await;
    let token = bench.start(ThreadId::MAIN).await;
    let main = bench.client(&token, "claude-code", &[]).await;
    let child = start_child(&main, TASK).await;
    until_idle(&*bench.assistant(child).await).await;
    // The owner's turn ends; the result's wake turn starts, and is held by its agent.
    let main_assistant = bench.assistant(ThreadId::MAIN).await;
    timeout(WAIT, async {
        loop {
            let events = events_of(&main_assistant);
            let woken = events
                .iter()
                .position(|event| matches!(event, Event::Woken { .. }));
            if let Some(woken) = woken {
                // Once the wake turn started, the gate is left alone: notifying it would end
                // the very turn the test needs open. Its first chunk shown, the agent waits
                // at the gate.
                if events[woken..]
                    .iter()
                    .any(|event| matches!(event, Event::Assistant { .. }))
                {
                    return;
                }
            } else {
                // Only the owner's turn is held so far: release it.
                bench.gate.notify_waiters();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the wake turn never started");
    let refused = call(&main, "user_secret", json!({"arguments": secret()})).await;
    assert_eq!(
        refused,
        json!({"error": fronde_agent::assistant::SecretInWake.to_string()}),
        "{refused}"
    );
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("started by a sub-agent's result"),
        "{refused}"
    );
    // Nor an integration's setup, which only an admin grants, for good: the threads refuse its
    // card through the bridge, before anything is shown.
    let shown = requests_in(&main_assistant);
    let refused = call(
        &main,
        "integrations_request_setup",
        json!({"arguments": {"integration": "echo"}}),
    )
    .await;
    assert_eq!(
        refused,
        json!({"error": fronde_agent::assistant::LastingInWake.to_string()}),
        "{refused}"
    );
    assert_eq!(requests_in(&main_assistant), shown, "no card was shown");
    let before = main_assistant.log().last_seq();
    let asked = call_in_background(&main, "user_ask", question());
    let (id, seq) = next_request(&main_assistant, before).await;
    let entry = main_assistant
        .log()
        .since(seq - 1)
        .into_iter()
        .next()
        .unwrap();
    let Event::Request { woken_by, .. } = entry.event else {
        panic!("{entry:?}");
    };
    assert_eq!(
        woken_by,
        Some(fronde_protocol::WokenBy {
            thread: child,
            title: fronde_protocol::first_title(TASK),
        })
    );
    bench.requests.cancel(id).await.unwrap();
    asked.await.unwrap();
    bench.gate.notify_waiters();
}

/// Both guarded windows read, the 5-hour one at `session` and the weekly one at `weekly`,
/// resetting at `resets_at`.
fn reading(
    session: f64,
    weekly: f64,
    resets_at: chrono::DateTime<chrono::Utc>,
) -> Vec<fronde_protocol::usage::Window> {
    use fronde_protocol::usage::{LimitStatus, Window, WindowKind};
    [(WindowKind::Session, session), (WindowKind::Weekly, weekly)]
        .into_iter()
        .map(|(kind, used)| Window {
            kind,
            used: Some(used),
            status: Some(LimitStatus::Allowed),
            resets_at: Some(resets_at),
            as_of: chrono::Utc::now(),
            reset_since_reading: false,
        })
        .collect()
}

/// The reserve is Fronde's: an agent calling `threads_start` or `threads_send` through the hub,
/// in a turn the owner's own message started, is refused past the line with the setting named,
/// and nothing starts.
#[tokio::test]
async fn reserve_refusal_is_server_side() {
    let bench = held_bench().await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    // Its turn held open: the reserve is checked before whether the sub-agent is busy.
    let child = start_child(&main, TASK).await;
    let resets_at = chrono::DateTime::from_timestamp(4_102_444_800, 0).unwrap();
    bench.usage.record("claude", reading(0.75, 0.2, resets_at));
    let refused = call(
        &main,
        "threads_start",
        json!({"arguments": {"task": "Une de plus."}}),
    )
    .await;
    let expected = "the owner's reserve of the 5-hour window is reached (75 % used, 30 % kept for \
                    the owner, the setting [threads] reserve); it resets at 2100-01-01 00:00 UTC; \
                    sub-agents can neither start nor get messages until then: do the task \
                    yourself or tell the user";
    assert_eq!(refused, json!({"error": expected}), "{refused}");
    let refused = call(
        &main,
        "threads_send",
        json!({"arguments": {"thread": child.0, "text": "Encore."}}),
    )
    .await;
    assert_eq!(refused, json!({"error": expected}), "{refused}");
    assert_eq!(children_of(&bench, ThreadId::MAIN).await.len(), 1);
    bench.gate.notify_waiters();
}

/// `threads_list` tells an agent whether background work may run on each provider before it
/// tries: unknown without a reading, allowed below the line, paused until the reset past it.
#[tokio::test]
async fn threads_list_says_whether_background_may_run() {
    let bench = held_bench().await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let background = |listed: &Value| listed["result"]["providers"][0]["background"].clone();
    let listed = call(&main, "threads_list", json!({})).await;
    assert_eq!(background(&listed), "unknown", "{listed}");
    let resets_at = chrono::DateTime::from_timestamp(4_102_444_800, 0).unwrap();
    bench.usage.record("claude", reading(0.1, 0.2, resets_at));
    let listed = call(&main, "threads_list", json!({})).await;
    assert_eq!(background(&listed), "allowed", "{listed}");
    bench.usage.record(
        "claude",
        reading(0.1, 0.95, resets_at + chrono::TimeDelta::hours(1)),
    );
    let listed = call(&main, "threads_list", json!({})).await;
    assert_eq!(
        background(&listed),
        "paused until 2100-01-01 01:00 UTC",
        "{listed}"
    );
    bench.gate.notify_waiters();
}

/// Only an admin's page gets the meter's usage, and only their own accounts', once read.
#[tokio::test]
async fn usage_sent_to_admins_only() {
    let bench = bench(FakeScript::default()).await;
    assert_eq!(
        fronde::usage_message(&bench.threads, &alix()),
        None,
        "no reading yet"
    );
    let resets_at = chrono::DateTime::from_timestamp(4_102_444_800, 0).unwrap();
    let windows = reading(0.42, 0.18, resets_at);
    bench.usage.record("claude", windows.clone());
    let Some(fronde_protocol::ServerMsg::Usage { reserve, usage }) =
        fronde::usage_message(&bench.threads, &alix())
    else {
        panic!("the admin gets the usage");
    };
    assert_eq!(reserve, 30);
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].provider, "claude");
    assert_eq!(usage[0].windows, windows);
    let family = User {
        name: "lea".into(),
        admin: false,
    };
    assert_eq!(fronde::usage_message(&bench.threads, &family), None);
}

/// A setup card is tied to its call: the client cutting the call cancels the card at once,
/// and the turn goes on.
#[tokio::test]
async fn a_cut_setup_call_cancels_its_card() {
    let bench = Bench::held().await;
    let token = bench.start(ThreadId::MAIN).await;
    let main = bench.client(&token, "claude-code", &[]).await;
    let assistant = bench.assistant(ThreadId::MAIN).await;
    let before = assistant.log().last_seq();
    let mut asked = Box::pin(
        main.peer().call_tool_once(
            CallToolRequestParams::new("integrations_request_setup")
                .with_arguments(json!({"integration": "echo"}).as_object().unwrap().clone()),
        ),
    );
    let (id, _) = tokio::select! {
        ended = &mut asked => panic!("the call ended before its card was shown: {ended:?}"),
        shown = next_request(&assistant, before) => shown,
    };
    // The client gives up: dropping the call sends `notifications/cancelled`.
    drop(asked);
    timeout(WAIT, async {
        while !assistant.log().since(0).iter().any(|entry| {
            matches!(
                entry.event,
                Event::RequestClosed { id: closed, outcome: Outcome::Cancelled, .. } if closed == id
            )
        }) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the card was not cancelled");
    assert_eq!(
        bench.requests.answer(id, Response::Decline, &alix()).await,
        Err(fronde_requests::Refusal::Closed)
    );
    assert!(assistant.current_turn().await.is_some(), "the turn goes on");
    bench.gate.notify_waiters();
}

/// A sub-agent follows its parent's task, not the owner's words: it cannot ask the admin to set
/// up an integration, whatever its turn.
#[tokio::test]
async fn a_sub_agent_cannot_ask_for_a_setup() {
    let bench = held_bench().await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let child = start_child(&main, TASK).await;
    let child_client = bench
        .client(&token_of(&bench, child).await, "claude-code", &[])
        .await;
    let assistant = bench.assistant(child).await;
    timeout(WAIT, async {
        while assistant.current_turn().await.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the sub-agent's turn never started");
    let refused = call(
        &child_client,
        "integrations_request_setup",
        json!({"arguments": {"integration": "echo"}}),
    )
    .await;
    assert_eq!(
        refused,
        json!({"error": fronde_agent::assistant::AdminFromSubAgent.to_string()}),
        "{refused}"
    );
    assert_eq!(requests_in(&assistant), 0, "no card was shown");
    bench.gate.notify_waiters();
}

/// Whether `found`, what `search_tools` returned, lists the integration `name`.
fn lists_integration(found: &Value, name: &str) -> bool {
    found["result"]
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item["integration"] == name))
}

/// `search_tools` tells only the agent of a thread the owner talks to of the integrations it
/// may set up: a sub-agent, which cannot, is not told.
#[tokio::test]
async fn search_tools_lists_no_integration_to_a_sub_agent() {
    let bench = held_bench().await;
    let main = bench
        .client(&bench.start(ThreadId::MAIN).await, "claude-code", &[])
        .await;
    let need = json!({"arguments": {"need": "echo some text"}});
    let found = call(&main, "search_tools", need.clone()).await;
    assert!(lists_integration(&found, "echo"), "{found}");
    let child = start_child(&main, TASK).await;
    let child_client = bench
        .client(&token_of(&bench, child).await, "claude-code", &[])
        .await;
    let found = call(&child_client, "search_tools", need).await;
    assert!(!lists_integration(&found, "echo"), "{found}");
    assert!(
        !found.to_string().contains("integrations_request_setup"),
        "{found}"
    );
    bench.gate.notify_waiters();
}

/// The confirmation a setup asks once its turn ended is refused, as any card of that turn
/// would be: nothing is kept or set up.
#[tokio::test]
async fn a_setup_confirmation_after_the_turn_ended_is_refused() {
    use fronde_protocol::{FieldValue, SecretAnswer};
    const OTHER: &str = "https://dav.example.org/calendars/alix/";
    let bench = Bench::held().await;
    let main = bench.assistant(ThreadId::MAIN).await;
    main.send("Salut".into()).await;
    let turn = main.current_turn().await;
    assert!(turn.is_some());
    let asked = call_in_turn(
        &bench,
        turn,
        "integrations_request_setup",
        json!({"integration": "calendar", "prefill": {"events_url": "https://cloud.example/dav/"}}),
    );
    let (id, _) = next_request(&main, 0).await;
    // The answer's end is held, so the call waits for it while the turn ends.
    let closing = bench.journal.hold(Step::Close);
    let answer = Response::Form {
        values: [
            ("events_url".to_owned(), FieldValue::Text(OTHER.into())),
            ("reminders_url".to_owned(), FieldValue::Text(OTHER.into())),
            (
                "caldav.username".to_owned(),
                FieldValue::Text("alix".into()),
            ),
            (
                "caldav.secret".to_owned(),
                FieldValue::Secret(SecretAnswer {
                    secret: SecretValue::new(CANARY.into()),
                }),
            ),
            ("allow.local".to_owned(), FieldValue::Toggle(false)),
        ]
        .into(),
    };
    let answering = tokio::spawn({
        let requests = bench.requests.clone();
        async move { requests.answer(id, answer, &alix()).await }
    });
    bench.journal.reached().await;
    bench.gate.notify_one();
    timeout(WAIT, async {
        while main.current_turn().await.is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the turn never ended");
    closing.notify_one();
    answering.await.unwrap().unwrap();
    let refused = timeout(WAIT, asked).await.unwrap().unwrap();
    let error = refused["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("confirmation could not be asked") && error.contains("turn ended"),
        "{refused}"
    );
    wait_for_stream(&main, turn_ended).await;
    assert_eq!(requests_in(&main), 1, "no confirmation was shown");
    assert_eq!(
        bench
            .requests
            .stored_origin("calendar_caldav")
            .await
            .unwrap(),
        None
    );
}
