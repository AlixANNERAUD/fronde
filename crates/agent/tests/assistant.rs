use agent_client_protocol::v1::schema::{
    CreateElicitationResponse, ElicitationAction, ElicitationContentValue,
    RequestPermissionOutcome, SelectedPermissionOutcome, SessionUpdate, ToolCall, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields,
};
use fronde_agent::acp::{AgentClient, AgentEvent, AgentSpec, PermissionAsk, Reply};
use fronde_agent::assistant::{Assistant, Connector, TOPIC, process_connector};
use fronde_agent::fake::{FAKE_SESSION_ID, FakeHandle, FakeScript, fake_agent, text_chunk};
use fronde_agent::log::{Event, ToolState, TurnOutcome};
use fronde_agent::threads::{ThreadId, Threads};
use fronde_protocol::{Card, Outcome, PermissionChoice, PermissionKind, RequestId, Response};
use fronde_requests::{Refusal, Requests, User};
use sea_orm::EntityTrait;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio::sync::broadcast::error::TryRecvError;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);
const BUSY: &str = "Fronde répond encore ; attends la fin ou arrête-la.";
const FRESH: &str = "Nouvelle conversation avec l'agent : l'ancienne n'a pas pu être reprise.";
const NO_START: &str = "L'agent ne répond pas au démarrage.";
const NOT_INSTALLED: &str = "Impossible de démarrer l'agent : lance `mise run agent` ou indique son chemin dans [agent] command.";
const SIGNED_OUT: &str =
    "L'agent n'est pas connecté à Claude : voir le README (claude setup-token).";
const FAILED: &str = "L'agent a rencontré une erreur.";

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
    }
}

/// Fake agents started by a connector, one per connection, and the requests the assistant
/// shows their permissions through.
#[derive(Default)]
struct Fakes {
    handles: Mutex<Vec<FakeHandle>>,
    requests: Mutex<Option<Arc<Requests>>>,
}

impl Fakes {
    fn handles(&self) -> Vec<FakeHandle> {
        self.handles.lock().unwrap().clone()
    }

    fn requests(&self) -> Arc<Requests> {
        self.requests.lock().unwrap().clone().expect("no requests")
    }

    /// Answers the request `id` as Alix, choosing the agent's option `choice`.
    async fn choose(&self, id: RequestId, choice: &str) -> Result<(), Refusal> {
        let response = Response::Permission {
            choice: choice.into(),
        };
        self.requests().answer(id, response, &alix()).await
    }

    async fn allow(&self, id: RequestId) -> Result<(), Refusal> {
        self.choose(id, "allow-once").await
    }

    async fn reject(&self, id: RequestId) -> Result<(), Refusal> {
        self.choose(id, "reject").await
    }
}

fn alix() -> User {
    User {
        name: "alix".into(),
        admin: true,
    }
}

/// A connector starting a fake agent, after `gate` is notified when one is given.
fn fake_connector(script: FakeScript, gate: Option<Arc<Notify>>) -> (Connector, Arc<Fakes>) {
    let fakes = Arc::new(Fakes::default());
    let connector: Connector = Arc::new({
        let fakes = fakes.clone();
        move |spec: &AgentSpec, resume, events| {
            let (spec, script, fakes, gate) =
                (spec.clone(), script.clone(), fakes.clone(), gate.clone());
            Box::pin(async move {
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                let (transport, handle) = fake_agent(script);
                fakes.handles.lock().unwrap().push(handle);
                AgentClient::connect(transport, &spec, resume, events).await
            })
        }
    });
    (connector, fakes)
}

async fn open(dir: &Path, script: FakeScript) -> (Arc<Assistant>, Arc<Fakes>) {
    open_for(dir, script, alix()).await
}

/// The main thread's assistant with its requests, for `owner`.
async fn open_for(dir: &Path, script: FakeScript, owner: User) -> (Arc<Assistant>, Arc<Fakes>) {
    let (connector, fakes) = fake_connector(script, None);
    let threads = open_threads(dir, spec(), connector, owner).await;
    with_requests(dir, &threads, &fakes).await;
    (threads.main(), fakes)
}

/// The threads stored in `dir`, for `owner`.
async fn open_threads(
    dir: &Path,
    spec: AgentSpec,
    connector: Connector,
    owner: User,
) -> Arc<Threads> {
    let db = fronde_db::open(dir).await.expect("database");
    Threads::open(db, dir, spec, connector, owner)
        .await
        .expect("open")
}

/// Gives the threads their requests, kept in `fakes`.
async fn with_requests(dir: &Path, threads: &Arc<Threads>, fakes: &Fakes) {
    let db = fronde_db::open(dir).await.expect("database");
    let requests = Requests::open(db, threads.clone()).await.expect("requests");
    threads.set_requests(&requests).await;
    *fakes.requests.lock().unwrap() = Some(requests);
}

/// The main thread's assistant, without requests.
async fn open_with(dir: &Path, spec: AgentSpec, connector: Connector) -> Arc<Assistant> {
    open_threads(dir, spec, connector, alix()).await.main()
}

/// The main thread's agent session id saved in the database.
async fn saved_session(dir: &Path) -> Option<String> {
    let db = fronde_db::open(dir).await.unwrap();
    fronde_entity::threads::Entity::find_by_id(ThreadId::MAIN.0)
        .one(&db)
        .await
        .unwrap()
        .expect("main thread")
        .session_id
}

fn events(assistant: &Assistant) -> Vec<Event> {
    assistant
        .log()
        .since(0)
        .into_iter()
        .map(|entry| entry.event)
        .collect()
}

/// Waits until the log satisfies `done`, then returns its events.
async fn wait_until(assistant: &Assistant, done: impl Fn(&[Event]) -> bool) -> Vec<Event> {
    let waited = timeout(WAIT, async {
        loop {
            let events = events(assistant);
            if done(&events) {
                return events;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    waited.unwrap_or_else(|_| panic!("timed out; the log has {:?}", events(assistant)))
}

async fn wait_for_turns(assistant: &Assistant, count: usize) -> Vec<Event> {
    wait_until(assistant, |events| {
        events
            .iter()
            .filter(|event| matches!(event, Event::TurnEnded { .. }))
            .count()
            >= count
    })
    .await
}

/// Waits for a permission card and returns its request's id.
async fn wait_for_permission(assistant: &Assistant) -> u64 {
    let events = wait_until(assistant, |events| events.iter().any(is_permission)).await;
    events.iter().find_map(permission_id).unwrap()
}

fn is_permission(event: &Event) -> bool {
    permission_id(event).is_some()
}

fn permission_id(event: &Event) -> Option<u64> {
    match event {
        Event::Request {
            id,
            card: Card::Permission { .. },
            ..
        } => Some(*id),
        _ => None,
    }
}

fn user(text: &str) -> Event {
    Event::User {
        text: text.into(),
        by: None,
    }
}

fn notice(text: &str) -> Event {
    Event::Notice { text: text.into() }
}

fn ended(outcome: TurnOutcome) -> Event {
    Event::TurnEnded { outcome }
}

/// The card of the fake agent's permission request, for an admin.
fn permission_card() -> Card {
    let choice = |id: &str, kind, detail: Option<&str>| PermissionChoice {
        id: id.into(),
        kind,
        detail: detail.map(str::to_owned),
    };
    Card::Permission {
        title: "Lire l'agenda".into(),
        tool: None,
        body: None,
        options: vec![
            choice("allow-once", PermissionKind::AllowOnce, None),
            choice("allow-with-updates", PermissionKind::AllowAlways, None),
            choice("reject", PermissionKind::RejectOnce, None),
        ],
        truncated: false,
        replaced: false,
    }
}

fn permission(id: u64) -> Event {
    Event::Request {
        id,
        card: permission_card(),
        from: None,
        server: false,
        woken_by: None,
        carries: 0,
    }
}

/// The permission `id` closed with `outcome`, by Alix when she answered.
fn closed(id: u64, outcome: Outcome) -> Event {
    let by = matches!(outcome, Outcome::Answered { .. }).then(|| "alix".to_owned());
    Event::RequestClosed { id, outcome, by }
}

fn answered(id: u64, allowed: bool) -> Event {
    let summary = if allowed {
        "Autoriser une fois"
    } else {
        "Refuser"
    };
    closed(
        id,
        Outcome::Answered {
            summary: summary.into(),
        },
    )
}

fn selected(id: &str) -> RequestPermissionOutcome {
    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id.to_owned()))
}

fn asking() -> FakeScript {
    FakeScript {
        ask_permission: true,
        ..FakeScript::default()
    }
}

#[tokio::test]
async fn turn_is_logged_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let script = FakeScript {
        updates: vec![
            text_chunk("Bon"),
            SessionUpdate::ToolCall(ToolCall::new("t1", "Chercher dans la mémoire")),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "t1",
                ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
            )),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "t2",
                ToolCallUpdateFields::new().status(ToolCallStatus::Failed),
            )),
            text_chunk("jour"),
        ],
        ..FakeScript::default()
    };
    let (assistant, _fakes) = open(dir.path(), script).await;

    assistant.send("Salut".into()).await;

    assert_eq!(
        wait_for_turns(&assistant, 1).await,
        vec![
            user("Salut"),
            Event::Assistant { text: "Bon".into() },
            Event::Tool {
                id: "t1".into(),
                title: "Chercher dans la mémoire".into(),
                state: ToolState::Running,
            },
            Event::Tool {
                id: "t1".into(),
                title: "Chercher dans la mémoire".into(),
                state: ToolState::Done,
            },
            Event::Tool {
                id: "t2".into(),
                title: String::new(),
                state: ToolState::Failed,
            },
            Event::Assistant {
                text: "jour".into()
            },
            ended(TurnOutcome::Done),
        ]
    );
    assert_eq!(saved_session(dir.path()).await.unwrap(), FAKE_SESSION_ID);
}

#[tokio::test]
async fn second_prompt_during_turn_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (assistant, fakes) = open(dir.path(), asking()).await;

    assistant.send("Mon agenda ?".into()).await;
    assistant.send("Et demain ?".into()).await;
    let id = wait_for_permission(&assistant).await;
    fakes.allow(id).await.unwrap();
    let events = wait_for_turns(&assistant, 1).await;

    assert!(events.contains(&notice(BUSY)), "{events:?}");
    assert!(!events.contains(&user("Et demain ?")), "{events:?}");
    assert_eq!(fakes.handles()[0].record().permission_outcomes.len(), 1);

    // Once the turn is over, the next message goes through.
    assistant.send("Et demain ?".into()).await;
    let id = wait_for_permission_after(&assistant, id).await;
    fakes.reject(id).await.unwrap();
    let events = wait_for_turns(&assistant, 2).await;
    assert!(events.contains(&user("Et demain ?")), "{events:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_right_after_turn_ended_is_accepted() {
    const TURNS: usize = 200;
    let dir = tempfile::tempdir().unwrap();
    let (assistant, _fakes) = open(dir.path(), FakeScript::default()).await;
    let mut live = assistant.log().subscribe();
    let runtime = tokio::runtime::Handle::current();
    // A client that sends its next message the moment it sees the previous turn end.
    let client = std::thread::spawn({
        let assistant = assistant.clone();
        move || {
            let _runtime = runtime.enter();
            let mut sent = 1;
            while sent < TURNS {
                match live.try_recv() {
                    Ok(entry) => match entry.event {
                        Event::TurnEnded { .. } => {
                            futures::executor::block_on(assistant.send(format!("{sent}")));
                            sent += 1;
                        }
                        Event::Notice { text } => return Err(text),
                        _ => {}
                    },
                    Err(TryRecvError::Empty) => std::hint::spin_loop(),
                    Err(error) => return Err(error.to_string()),
                }
            }
            Ok(())
        }
    });

    assistant.send("0".into()).await;
    let refused = tokio::task::spawn_blocking(move || client.join().unwrap())
        .await
        .unwrap();

    assert_eq!(refused, Ok(()));
    wait_for_turns(&assistant, TURNS).await;
}

/// Waits for a permission card other than `previous`.
async fn wait_for_permission_after(assistant: &Assistant, previous: u64) -> u64 {
    let other = |event: &Event| permission_id(event).filter(|id| *id != previous);
    let events = wait_until(assistant, |events| {
        events.iter().any(|e| other(e).is_some())
    })
    .await;
    events.iter().find_map(other).unwrap()
}

#[tokio::test]
async fn prompt_error_becomes_error_event() {
    let dir = tempfile::tempdir().unwrap();
    let script = FakeScript {
        fail_prompt: true,
        ..FakeScript::default()
    };
    let (assistant, fakes) = open(dir.path(), script).await;

    assistant.send("Salut".into()).await;
    let events = wait_for_turns(&assistant, 1).await;

    assert_eq!(
        events,
        vec![user("Salut"), notice(FAILED), ended(TurnOutcome::Error)]
    );

    // The failed client was dropped: the next message reconnects.
    assistant.send("Encore".into()).await;
    wait_for_turns(&assistant, 2).await;
    assert_eq!(fakes.handles().len(), 2);
}

#[tokio::test]
async fn connect_error_becomes_error_event() {
    let dir = tempfile::tempdir().unwrap();
    let connector: Connector = Arc::new(|_: &AgentSpec, _, _| {
        Box::pin(async { Err(anyhow::anyhow!("cannot start the agent (claude-agent-acp)")) })
    });
    let assistant = open_with(dir.path(), spec(), connector).await;

    assistant.send("Salut".into()).await;

    assert_eq!(
        wait_for_turns(&assistant, 1).await,
        vec![user("Salut"), notice(FAILED), ended(TurnOutcome::Error)]
    );
}

#[tokio::test]
async fn missing_agent_says_how_to_install_it() {
    let dir = tempfile::tempdir().unwrap();
    let spec = AgentSpec {
        command: vec!["/nonexistent/claude-agent-acp".into()],
        ..spec()
    };
    let assistant = open_with(dir.path(), spec, process_connector()).await;

    assistant.send("Salut".into()).await;

    assert_eq!(
        wait_for_turns(&assistant, 1).await,
        vec![
            user("Salut"),
            notice(NOT_INSTALLED),
            ended(TurnOutcome::Error)
        ]
    );
}

#[tokio::test]
async fn signed_out_agent_points_to_the_readme() {
    let dir = tempfile::tempdir().unwrap();
    let script = FakeScript {
        signed_out: true,
        ..FakeScript::default()
    };
    let (assistant, _fakes) = open(dir.path(), script).await;

    assistant.send("Salut".into()).await;

    assert_eq!(
        wait_for_turns(&assistant, 1).await,
        vec![user("Salut"), notice(SIGNED_OUT), ended(TurnOutcome::Error)]
    );
}

#[tokio::test]
async fn restart_resumes_or_starts_fresh() {
    let resumable = FakeScript {
        resume: true,
        ..FakeScript::default()
    };

    // The saved session is resumed after a restart.
    let dir = tempfile::tempdir().unwrap();
    let (assistant, _fakes) = open(dir.path(), resumable.clone()).await;
    assistant.send("Salut".into()).await;
    wait_for_turns(&assistant, 1).await;
    drop(assistant);
    let (assistant, fakes) = open(dir.path(), resumable.clone()).await;
    assistant.send("Re".into()).await;
    let events = wait_for_turns(&assistant, 2).await;
    let record = fakes.handles()[0].record();
    assert_eq!(record.resumes.len(), 1);
    assert_eq!(&*record.resumes[0].session_id.0, FAKE_SESSION_ID);
    assert!(record.new_sessions.is_empty());
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Notice { .. })),
        "{events:?}"
    );

    // An agent that cannot resume it starts a new conversation, and says so. The session was
    // saved where it was kept before threads.
    let dir = tempfile::tempdir().unwrap();
    {
        let db = fronde_db::open(dir.path()).await.unwrap();
        fronde_db::kv::set(&db, "agent", "session", b"earlier-session".to_vec(), None)
            .await
            .unwrap();
    }
    let refusing = FakeScript {
        refuse_resume: true,
        ..resumable
    };
    let (assistant, fakes) = open(dir.path(), refusing).await;
    assistant.send("Salut".into()).await;
    let events = wait_for_turns(&assistant, 1).await;
    assert_eq!(
        &*fakes.handles()[0].record().resumes[0].session_id.0,
        "earlier-session"
    );
    assert_eq!(
        events,
        vec![user("Salut"), notice(FRESH), ended(TurnOutcome::Done)]
    );
    assert_eq!(saved_session(dir.path()).await.unwrap(), FAKE_SESSION_ID);
}

#[tokio::test]
async fn permission_answer_is_logged() {
    let dir = tempfile::tempdir().unwrap();
    let (assistant, fakes) = open(dir.path(), asking()).await;

    assistant.send("Mon agenda ?".into()).await;
    let id = wait_for_permission(&assistant).await;
    fakes.allow(id).await.unwrap();
    wait_for_turns(&assistant, 1).await;
    // The first answer wins; a second one is refused, and the log does not change.
    assert_eq!(fakes.allow(id).await, Err(Refusal::Closed));

    assert_eq!(
        events(&assistant),
        vec![
            user("Mon agenda ?"),
            permission(id),
            answered(id, true),
            ended(TurnOutcome::Done),
        ]
    );
    assert_eq!(
        fakes.handles()[0].record().permission_outcomes,
        vec![selected("allow-once")]
    );
}

#[tokio::test]
async fn permission_ids_stay_unique_across_connections() {
    let dir = tempfile::tempdir().unwrap();
    let (assistant, fakes) = open(dir.path(), asking()).await;
    assistant.send("Mon agenda ?".into()).await;
    let first = wait_for_permission(&assistant).await;
    fakes.allow(first).await.unwrap();
    wait_for_turns(&assistant, 1).await;
    drop((assistant, fakes));

    // After a restart the agent numbers its requests from 1 again; the requests' ids go on.
    let (assistant, fakes) = open(dir.path(), asking()).await;
    assistant.send("Et demain ?".into()).await;
    let second = wait_for_permission_after(&assistant, first).await;
    assert!(second > first, "{second} after {first}");
    assert_eq!(fakes.allow(first).await, Err(Refusal::Closed));
    fakes.reject(second).await.unwrap();
    let events = wait_for_turns(&assistant, 2).await;

    assert!(events.contains(&answered(second, false)), "{events:?}");
    assert_eq!(
        fakes.handles()[0].record().permission_outcomes,
        vec![selected("reject")]
    );
}

#[tokio::test]
async fn cancel_ends_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (assistant, fakes) = open(dir.path(), asking()).await;

    assistant.send("Mon agenda ?".into()).await;
    let id = wait_for_permission(&assistant).await;
    assistant.cancel().await;
    wait_for_turns(&assistant, 1).await;
    // Idle: nothing to cancel.
    assistant.cancel().await;

    assert_eq!(
        events(&assistant),
        vec![
            user("Mon agenda ?"),
            permission(id),
            closed(id, Outcome::Cancelled),
            ended(TurnOutcome::Cancelled),
        ]
    );
    let record = fakes.handles()[0].record();
    assert_eq!(record.cancels, 1);
    assert_eq!(
        record.permission_outcomes,
        vec![RequestPermissionOutcome::Cancelled]
    );
    assert_eq!(fakes.allow(id).await, Err(Refusal::Closed));
}

#[tokio::test]
async fn stop_frees_a_turn_the_agent_never_ends() {
    let dir = tempfile::tempdir().unwrap();
    let script = FakeScript {
        updates: vec![text_chunk("Je cherche")],
        ignore_cancel: true,
        ..FakeScript::default()
    };
    let (assistant, fakes) = open(dir.path(), script).await;
    let searching = Event::Assistant {
        text: "Je cherche".into(),
    };

    assistant.send("Salut".into()).await;
    wait_until(&assistant, |events| events.contains(&searching)).await;
    // A paused clock jumps to the next timer whenever the runtime waits, and the database is
    // read and written on SQLite's own thread: under load a jump during `cancel`'s read would
    // fire the grace period (and the pool's acquire timeout) at once. A blocking task in
    // flight keeps the paused clock still, so it moves only by `advance`.
    tokio::time::pause();
    let (thaw, frozen) = std::sync::mpsc::channel::<()>();
    let held = tokio::task::spawn_blocking(move || frozen.recv().ok());
    assistant.cancel().await;
    // The grace period starts when the cancel goes out, on the clock still at the pause.
    let started = Instant::now();
    while fakes.handles()[0].record().cancels == 0 {
        assert!(
            started.elapsed() < WAIT,
            "the cancel never reached the agent"
        );
        tokio::task::yield_now().await;
    }
    // The agent gets a grace period to end the turn itself.
    tokio::time::advance(Duration::from_secs(9)).await;
    assert_eq!(events(&assistant), vec![user("Salut"), searching.clone()]);
    drop(thaw);
    held.await.unwrap();
    tokio::time::resume();
    let resumed = Instant::now();

    assert_eq!(
        wait_for_turns(&assistant, 1).await,
        vec![user("Salut"), searching, ended(TurnOutcome::Cancelled)]
    );
    // About 1 s of the grace period was left. The bound is loose for a loaded test machine yet
    // well under the 10 s a grace period started over would take.
    assert!(
        resumed.elapsed() < Duration::from_secs(5),
        "{:?}",
        resumed.elapsed()
    );
    let record = fakes.handles()[0].record();
    assert_eq!(record.cancels, 1);
    // The wedged agent was let go.
    assert!(record.closed);

    // The next message is accepted, on a new connection.
    assistant.send("Encore".into()).await;
    let events = wait_until(&assistant, |events| events.contains(&user("Encore"))).await;
    assert!(!events.contains(&notice(BUSY)), "{events:?}");
    timeout(WAIT, async {
        while fakes.handles().len() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("no new connection");
}

#[tokio::test]
async fn cancel_while_connecting_skips_the_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(Notify::new());
    let script = FakeScript {
        updates: vec![text_chunk("Bonjour")],
        ..FakeScript::default()
    };
    let (connector, _fakes) = fake_connector(script, Some(gate.clone()));
    let assistant = open_with(dir.path(), spec(), connector).await;

    assistant.send("Salut".into()).await;
    assistant.cancel().await;
    gate.notify_one();

    assert_eq!(
        wait_for_turns(&assistant, 1).await,
        vec![user("Salut"), ended(TurnOutcome::Cancelled)]
    );
}

#[tokio::test]
async fn stale_permission_is_closed_in_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let script = FakeScript {
        leave_permission_pending: true,
        ..asking()
    };
    let (assistant, fakes) = open(dir.path(), script).await;

    assistant.send("Mon agenda ?".into()).await;
    let events = wait_for_turns(&assistant, 1).await;
    let id = events
        .iter()
        .find_map(permission_id)
        .expect("no permission card");
    assert_eq!(fakes.allow(id).await, Err(Refusal::Closed));

    assert_eq!(
        events,
        vec![
            user("Mon agenda ?"),
            permission(id),
            closed(id, Outcome::Cancelled),
            ended(TurnOutcome::Done),
        ]
    );
    timeout(WAIT, async {
        while fakes.handles()[0].record().permission_outcomes.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the agent never got an answer");
    assert_eq!(
        fakes.handles()[0].record().permission_outcomes,
        vec![RequestPermissionOutcome::Cancelled]
    );
}

#[tokio::test]
async fn permission_that_cannot_be_saved_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let (assistant, fakes) = open(dir.path(), asking()).await;
    let db = fronde_db::open(dir.path()).await.unwrap();
    sea_orm::ConnectionTrait::execute_unprepared(
        &db,
        "CREATE TRIGGER refuse BEFORE INSERT ON stream_entries \
         WHEN json_extract(NEW.event, '$.type') = 'request' \
         BEGIN SELECT RAISE(ABORT, 'disk full'); END",
    )
    .await
    .unwrap();

    assistant.send("Mon agenda ?".into()).await;
    let events = wait_for_turns(&assistant, 1).await;
    // The request was never shown: an answer naming it finds nothing pending.
    assert!(!events.iter().any(is_permission), "{events:?}");
    assert_eq!(fakes.allow(1).await, Err(Refusal::Closed));

    assert_eq!(events, vec![user("Mon agenda ?"), ended(TurnOutcome::Done)]);
    assert_eq!(
        fakes.handles()[0].record().permission_outcomes,
        vec![selected("reject")]
    );
}

/// Counts the connects started and the connect futures dropped.
#[derive(Default)]
struct Hanging {
    started: AtomicUsize,
    dropped: AtomicUsize,
}

struct DropCounter(Arc<Hanging>);

impl Drop for DropCounter {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

/// A connector whose agent never answers.
fn hanging_connector() -> (Connector, Arc<Hanging>) {
    let hanging = Arc::new(Hanging::default());
    let connector: Connector = Arc::new({
        let hanging = hanging.clone();
        move |_: &AgentSpec, _, _| {
            let hanging = hanging.clone();
            Box::pin(async move {
                let _counter = DropCounter(hanging.clone());
                hanging.started.fetch_add(1, Ordering::SeqCst);
                std::future::pending().await
            })
        }
    });
    (connector, hanging)
}

#[tokio::test]
async fn cancel_interrupts_a_hanging_connect() {
    let dir = tempfile::tempdir().unwrap();
    let (connector, hanging) = hanging_connector();
    let assistant = open_with(dir.path(), spec(), connector).await;

    assistant.send("Salut".into()).await;
    timeout(WAIT, async {
        while hanging.started.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the connect never started");
    assistant.cancel().await;

    assert_eq!(
        wait_for_turns(&assistant, 1).await,
        vec![user("Salut"), ended(TurnOutcome::Cancelled)]
    );
    assert_eq!(hanging.dropped.load(Ordering::SeqCst), 1);

    // Fronde is free again.
    assistant.send("Encore".into()).await;
    assistant.cancel().await;
    let events = wait_for_turns(&assistant, 2).await;
    assert!(!events.contains(&notice(BUSY)), "{events:?}");
    assert!(events.contains(&user("Encore")), "{events:?}");
}

#[tokio::test]
async fn connect_timeout_ends_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (connector, hanging) = hanging_connector();
    let assistant = open_with(dir.path(), spec(), connector).await;

    assistant.send("Salut".into()).await;
    timeout(WAIT, async {
        while hanging.started.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the connect never started");
    // Paused only while nothing is written, as in `stop_frees_a_turn_the_agent_never_ends`.
    tokio::time::pause();
    tokio::time::sleep(Duration::from_secs(59)).await;
    assert_eq!(events(&assistant), vec![user("Salut")]);
    tokio::time::resume();
    let resumed = Instant::now();

    assert_eq!(
        wait_for_turns(&assistant, 1).await,
        vec![user("Salut"), notice(NO_START), ended(TurnOutcome::Error)]
    );
    // About 1 s of the timeout was left. The bound is loose for a loaded test machine yet well
    // under the 60 s a timeout started over would take.
    assert!(
        resumed.elapsed() < Duration::from_secs(5),
        "{:?}",
        resumed.elapsed()
    );
    assert_eq!(hanging.dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn open_closes_a_turn_left_open() {
    let dir = tempfile::tempdir().unwrap();
    // The server stopped in the middle of a turn: its user message has no `turn_ended`.
    {
        let db = fronde_db::open(dir.path()).await.unwrap();
        let log = fronde_agent::log::ConversationLog::open(&db, TOPIC)
            .await
            .unwrap();
        log.append(user("Un")).await.unwrap();
        log.append(ended(TurnOutcome::Done)).await.unwrap();
        log.append(user("Deux")).await.unwrap();
        log.append(Event::Assistant {
            text: "Je regarde".into(),
        })
        .await
        .unwrap();
    }
    let (assistant, _) = open(dir.path(), FakeScript::default()).await;
    let closed = [
        user("Un"),
        ended(TurnOutcome::Done),
        user("Deux"),
        Event::Assistant {
            text: "Je regarde".into(),
        },
        notice("La réponse précédente a été interrompue."),
        ended(TurnOutcome::Error),
    ];
    assert_eq!(events(&assistant), closed);
    drop(assistant);

    // A log whose last turn ended is left alone.
    let (assistant, _) = open(dir.path(), FakeScript::default()).await;
    assert_eq!(events(&assistant), closed);
}

#[tokio::test]
async fn open_imports_the_files_from_before_fronde() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("jarvis.jsonl"),
        concat!(
            "{\"log_id\":\"jarvis-log\"}\n",
            "{\"seq\":1,\"at\":\"2026-11-16T10:00:00Z\",\"event\":{\"type\":\"user\",\"text\":\"Un\"}}\n",
            "{\"seq\":2,\"at\":\"2026-11-16T10:00:01Z\",\"event\":{\"type\":\"turn_ended\",\"outcome\":\"done\"}}\n",
        ),
    )
    .unwrap();
    std::fs::write(dir.path().join("jarvis.session"), "earlier-session").unwrap();

    let (assistant, _) = open(dir.path(), FakeScript::default()).await;

    assert_eq!(
        events(&assistant),
        vec![user("Un"), ended(TurnOutcome::Done)]
    );
    assert_eq!(assistant.log().log_id(), "jarvis-log");
    drop(assistant);
    assert_eq!(
        saved_session(dir.path()).await.as_deref(),
        Some("earlier-session")
    );
    assert!(!dir.path().join("jarvis.jsonl").exists());
    assert!(!dir.path().join("jarvis.session").exists());
    assert!(dir.path().join("jarvis.jsonl.imported").exists());
    assert!(dir.path().join("jarvis.session.imported").exists());
}

#[tokio::test]
async fn requests_are_shown_in_the_conversation() {
    use fronde_protocol::{ChoiceAnswer, ChoiceOption, Field};
    use fronde_requests::{Ask, Caller, Resolution};

    let dir = tempfile::tempdir().unwrap();
    let (assistant, fakes) = open(dir.path(), asking()).await;
    assert_eq!(assistant.current_turn().await, None);
    assistant.send("Mon agenda ?".into()).await;
    let permission = wait_for_permission(&assistant).await;
    // The turn runs while the agent waits for the permission.
    assert_eq!(assistant.current_turn().await, Some(1));

    let requests = fakes.requests();
    let option = |label: &str| ChoiceOption {
        label: label.into(),
        description: None,
        preview: None,
        recommended: false,
    };
    let card = Card::Form {
        message: String::new(),
        fields: vec![Field::Choice {
            key: "q1".into(),
            header: None,
            question: "Quel agenda ?".into(),
            options: vec![option("Perso"), option("Travail")],
            multi: false,
            other: false,
            required: true,
            note: true,
        }],
        submit: None,
        truncated: false,
        replaced: false,
        during: Vec::new(),
    };
    let waiter = requests
        .ask(Ask {
            topic: TOPIC.into(),
            turn: assistant.current_turn().await,
            caller: Caller::Mcp("Codex".into()),
            client_request_id: None,
            card: card.clone(),
            for_user: "alix".into(),
            admin_only: false,
            timeout: WAIT,
            tied: false,
        })
        .await
        .unwrap();
    let id = waiter.id();
    let response = Response::Form {
        values: [(
            "q1".to_owned(),
            ChoiceAnswer {
                selected: vec!["Perso".into()],
                ..ChoiceAnswer::default()
            }
            .into(),
        )]
        .into(),
    };
    requests
        .answer(id, response.clone(), &alix())
        .await
        .unwrap();
    assert_eq!(waiter.wait().await, Resolution::Answered(response));

    fakes.allow(permission).await.unwrap();
    let events = wait_for_turns(&assistant, 1).await;
    assert_eq!(
        events[2..4],
        [
            Event::Request {
                id,
                card,
                from: Some("Codex".into()),
                server: false,
                woken_by: None,
                carries: 0,
            },
            Event::RequestClosed {
                id,
                outcome: Outcome::Answered {
                    summary: "Perso".into()
                },
                by: Some("alix".into())
            },
        ]
    );
    assert_eq!(assistant.current_turn().await, None);
}

/// A one-question card for the assistant's requests tests.
fn question_card() -> fronde_protocol::Card {
    use fronde_protocol::{Card, ChoiceOption, Field};
    let option = |label: &str| ChoiceOption {
        label: label.into(),
        description: None,
        preview: None,
        recommended: false,
    };
    Card::Form {
        message: String::new(),
        fields: vec![Field::Choice {
            key: "q1".into(),
            header: None,
            question: "Quel agenda ?".into(),
            options: vec![option("Perso"), option("Travail")],
            multi: false,
            other: true,
            required: true,
            note: true,
        }],
        submit: None,
        truncated: false,
        replaced: false,
        during: Vec::new(),
    }
}

/// The assistant with its requests, asking one question during a turn that waits on a
/// permission; returns the waiter.
async fn ask_during_a_turn(
    dir: &Path,
) -> (Arc<Assistant>, Arc<Fakes>, fronde_requests::Waiter, u64) {
    use fronde_requests::{Ask, Caller};
    let (assistant, fakes) = open(dir, asking()).await;
    let requests = fakes.requests();
    assistant.send("Mon agenda ?".into()).await;
    let permission = wait_for_permission(&assistant).await;
    let waiter = requests
        .ask(Ask {
            topic: TOPIC.into(),
            turn: assistant.current_turn().await,
            caller: Caller::Thread(fronde_protocol::ThreadId::MAIN),
            client_request_id: None,
            card: question_card(),
            for_user: "alix".into(),
            admin_only: false,
            timeout: Duration::from_secs(600),
            tied: false,
        })
        .await
        .unwrap();
    (assistant, fakes, waiter, permission)
}

#[tokio::test]
async fn stop_cancels_the_turns_open_requests() {
    use fronde_requests::Resolution;
    let dir = tempfile::tempdir().unwrap();
    let (assistant, _fakes, waiter, _) = ask_during_a_turn(dir.path()).await;
    let id = waiter.id();
    assistant.cancel().await;
    assert_eq!(
        timeout(WAIT, waiter.wait()).await.unwrap(),
        Resolution::Cancelled
    );
    let events = wait_for_turns(&assistant, 1).await;
    let closed = Event::RequestClosed {
        id,
        outcome: Outcome::Cancelled,
        by: None,
    };
    let at = events.iter().position(|event| *event == closed);
    assert!(at.is_some(), "{events:?}");
    assert_eq!(events.last(), Some(&ended(TurnOutcome::Cancelled)));
}

#[tokio::test]
async fn turn_end_cancels_the_requests_left_open() {
    use fronde_requests::Resolution;
    let dir = tempfile::tempdir().unwrap();
    let (assistant, fakes, waiter, permission) = ask_during_a_turn(dir.path()).await;
    let id = waiter.id();
    // The agent goes on without the answer and ends its turn.
    fakes.allow(permission).await.unwrap();
    assert_eq!(
        timeout(WAIT, waiter.wait()).await.unwrap(),
        Resolution::Cancelled
    );
    let events = wait_for_turns(&assistant, 1).await;
    let closed = events
        .iter()
        .position(|event| {
            *event
                == Event::RequestClosed {
                    id,
                    outcome: Outcome::Cancelled,
                    by: None,
                }
        })
        .expect("closed");
    let ended_at = events
        .iter()
        .position(|event| matches!(event, Event::TurnEnded { .. }))
        .unwrap();
    assert!(closed < ended_at, "{events:?}");
}

/// The card of the permission `id`, once shown.
async fn shown_card(assistant: &Assistant, id: u64) -> Card {
    events(assistant)
        .into_iter()
        .find_map(|event| match event {
            Event::Request {
                id: shown, card, ..
            } if shown == id => Some(card),
            _ => None,
        })
        .expect("the card")
}

#[tokio::test]
async fn permission_options_follow_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let (assistant, fakes) = open(dir.path(), asking()).await;
    let mut previous = 0;
    for (turn, choice) in ["allow-once", "allow-with-updates", "reject"]
        .into_iter()
        .enumerate()
    {
        assistant.send(format!("Tour {turn}")).await;
        let newer = |event: &Event| permission_id(event).filter(|id| *id > previous);
        let shown = wait_until(&assistant, |events| {
            events.iter().any(|e| newer(e).is_some())
        });
        let id = shown.await.iter().find_map(newer).unwrap();
        assert_eq!(shown_card(&assistant, id).await, permission_card());
        fakes.choose(id, choice).await.unwrap();
        wait_for_turns(&assistant, turn + 1).await;
        previous = id;
    }
    // Each choice reached the agent as the option it offered.
    assert_eq!(
        fakes.handles()[0].record().permission_outcomes,
        vec![
            selected("allow-once"),
            selected("allow-with-updates"),
            selected("reject")
        ]
    );
    // Each card closed with the option chosen, by whom.
    let summaries: Vec<String> = events(&assistant)
        .into_iter()
        .filter_map(|event| match event {
            Event::RequestClosed {
                outcome: Outcome::Answered { summary },
                by: Some(by),
                ..
            } if by == "alix" => Some(summary),
            _ => None,
        })
        .collect();
    assert_eq!(
        summaries,
        ["Autoriser une fois", "Toujours autoriser", "Refuser"]
    );
}

#[tokio::test]
async fn always_hidden_for_family() {
    let dir = tempfile::tempdir().unwrap();
    let sam = User {
        name: "sam".into(),
        admin: false,
    };
    let (assistant, fakes) = open_for(dir.path(), asking(), sam.clone()).await;
    assistant.send("Mon agenda ?".into()).await;
    let id = wait_for_permission(&assistant).await;
    let Card::Permission { options, .. } = shown_card(&assistant, id).await else {
        unreachable!()
    };
    let ids: Vec<&str> = options.iter().map(|option| option.id.as_str()).collect();
    assert_eq!(ids, ["allow-once", "reject"]);
    // Not offered, so not accepted either.
    let always = Response::Permission {
        choice: "allow-with-updates".into(),
    };
    let refused = fakes.requests().answer(id, always, &sam).await;
    assert!(matches!(refused, Err(Refusal::Invalid(_))), "{refused:?}");
    let allow = Response::Permission {
        choice: "allow-once".into(),
    };
    fakes.requests().answer(id, allow, &sam).await.unwrap();
    wait_for_turns(&assistant, 1).await;
    assert_eq!(
        fakes.handles()[0].record().permission_outcomes,
        vec![selected("allow-once")]
    );
}

#[tokio::test]
async fn permission_from_another_user_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (assistant, fakes) = open(dir.path(), asking()).await;
    assistant.send("Mon agenda ?".into()).await;
    let id = wait_for_permission(&assistant).await;
    let sam = User {
        name: "sam".into(),
        admin: true,
    };
    let allow = Response::Permission {
        choice: "allow-once".into(),
    };
    assert_eq!(
        fakes.requests().answer(id, allow, &sam).await,
        Err(Refusal::NotYours)
    );
    // Still Alix's to answer, and the agent still waits.
    assert!(fakes.handles()[0].record().permission_outcomes.is_empty());
    fakes.reject(id).await.unwrap();
    // A second answer is refused: the first one won.
    assert_eq!(fakes.allow(id).await, Err(Refusal::Closed));
    wait_for_turns(&assistant, 1).await;
    assert_eq!(
        fakes.handles()[0].record().permission_outcomes,
        vec![selected("reject")]
    );
}

#[tokio::test]
async fn permission_expiry_rejects() {
    let dir = tempfile::tempdir().unwrap();
    let (assistant, fakes) = open(dir.path(), asking()).await;
    // Real time: the database's own timeouts would fire at once on a paused clock.
    assistant.set_permission_timeout(Duration::from_millis(200));
    assistant.send("Mon agenda ?".into()).await;
    let events = wait_for_turns(&assistant, 1).await;
    let id = events.iter().find_map(permission_id).unwrap();

    assert_eq!(
        events,
        vec![
            user("Mon agenda ?"),
            permission(id),
            closed(id, Outcome::Expired),
            ended(TurnOutcome::Done),
        ]
    );
    // No answer means no.
    assert_eq!(
        fakes.handles()[0].record().permission_outcomes,
        vec![selected("reject")]
    );
    assert_eq!(fakes.allow(id).await, Err(Refusal::Closed));
}

#[tokio::test]
async fn permission_expires_after_thirty_minutes_by_default() {
    assert_eq!(
        fronde_agent::permission::PERMISSION_TIMEOUT,
        Duration::from_secs(30 * 60)
    );
}

#[tokio::test]
async fn cancel_closes_the_turns_requests() {
    use fronde_requests::Resolution;
    let dir = tempfile::tempdir().unwrap();
    let (assistant, fakes, waiter, permission) = ask_during_a_turn(dir.path()).await;
    let question = waiter.id();
    assistant.cancel().await;
    assert_eq!(
        timeout(WAIT, waiter.wait()).await.unwrap(),
        Resolution::Cancelled
    );
    let events = wait_for_turns(&assistant, 1).await;
    // Both the agent's permission and the question are cancelled before the turn ends.
    let ended_at = events.len() - 1;
    for id in [permission, question] {
        let at = events
            .iter()
            .position(|event| *event == closed(id, Outcome::Cancelled))
            .unwrap_or_else(|| panic!("{id} not cancelled: {events:?}"));
        assert!(at < ended_at, "{events:?}");
    }
    assert_eq!(events[ended_at], ended(TurnOutcome::Cancelled));
    assert_eq!(
        fakes.handles()[0].record().permission_outcomes,
        vec![RequestPermissionOutcome::Cancelled]
    );
    assert_eq!(fakes.allow(permission).await, Err(Refusal::Closed));
}

#[tokio::test]
async fn plan_body_is_shown() {
    use agent_client_protocol::v1::schema::{ToolCallContent, ToolKind};
    let dir = tempfile::tempdir().unwrap();
    // The agent described the tool call before asking: its kind and its plan.
    let script = FakeScript {
        updates: vec![SessionUpdate::ToolCall(
            ToolCall::new("tool-1", "Prêt à coder ?")
                .kind(ToolKind::SwitchMode)
                .content(vec![ToolCallContent::from(
                    "1. Lire l'agenda\n2. Ajouter le rendez-vous",
                )]),
        )],
        ..asking()
    };
    let (assistant, fakes) = open(dir.path(), script).await;
    assistant.send("Planifie".into()).await;
    let id = wait_for_permission(&assistant).await;
    let Card::Permission {
        title,
        tool,
        body,
        options,
        ..
    } = shown_card(&assistant, id).await
    else {
        unreachable!()
    };
    // The request's own title, over the earlier one.
    assert_eq!(title, "Lire l'agenda");
    assert_eq!(tool, Some(fronde_protocol::ToolKind::SwitchMode));
    assert_eq!(
        body.as_deref(),
        Some("1. Lire l'agenda\n2. Ajouter le rendez-vous")
    );
    // A mode switch: only the refusal is one tap away.
    let ids: Vec<&str> = options.iter().map(|option| option.id.as_str()).collect();
    assert_eq!(ids, ["reject"]);
    assert_eq!(
        fakes.allow(id).await,
        Err(Refusal::Invalid(fronde_protocol::Invalid {
            key: None,
            message: "Ce choix n'est pas proposé.".into()
        }))
    );
    fakes.reject(id).await.unwrap();
    wait_for_turns(&assistant, 1).await;
}

#[tokio::test]
async fn permission_without_requests_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (connector, fakes) = fake_connector(asking(), None);
    let assistant = open_with(dir.path(), spec(), connector).await;
    assistant.send("Mon agenda ?".into()).await;
    let events = wait_for_turns(&assistant, 1).await;
    // Nobody could answer a card that cannot be shown.
    assert!(!events.iter().any(is_permission), "{events:?}");
    assert_eq!(
        fakes.handles()[0].record().permission_outcomes,
        vec![selected("reject")]
    );
}

/// Where a test hands the assistant agent events of its own, as the agent's connection would.
type Inject = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<AgentEvent>>>>;

/// The assistant with its requests, on a fake agent playing `script`, and the way to hand it
/// events beside the agent's.
async fn open_injecting(dir: &Path, script: FakeScript) -> (Arc<Assistant>, Arc<Fakes>, Inject) {
    let fakes = Arc::new(Fakes::default());
    let inject: Inject = Arc::default();
    let connector: Connector = Arc::new({
        let (fakes, inject) = (fakes.clone(), inject.clone());
        move |spec: &AgentSpec, resume, events| {
            let (spec, script, fakes) = (spec.clone(), script.clone(), fakes.clone());
            *inject.lock().unwrap() = Some(events.clone());
            Box::pin(async move {
                let (transport, handle) = fake_agent(script);
                fakes.handles.lock().unwrap().push(handle);
                AgentClient::connect(transport, &spec, resume, events).await
            })
        }
    });
    let threads = open_threads(dir, spec(), connector, alix()).await;
    with_requests(dir, &threads, &fakes).await;
    (threads.main(), fakes, inject)
}

/// A permission request the test answers in the agent's place.
fn injected_ask() -> (
    PermissionAsk,
    tokio::sync::oneshot::Receiver<RequestPermissionOutcome>,
) {
    use agent_client_protocol::v1::schema::{PermissionOption, PermissionOptionKind};
    let (reply, answer) = Reply::new();
    let ask = PermissionAsk {
        tool_call: ToolCallUpdateFields::new()
            .title("Lire l'agenda".to_owned())
            .name("Read".to_owned()),
        options: vec![
            PermissionOption::new("allow-once", "Yes", PermissionOptionKind::AllowOnce),
            PermissionOption::new("reject", "No", PermissionOptionKind::RejectOnce),
        ],
        reply,
    };
    (ask, answer)
}

fn inject(inject: &Inject, event: AgentEvent) {
    let events = inject.lock().unwrap().clone().expect("not connected");
    events.send(event).expect("the assistant stopped listening");
}

/// A turn that waits for `gate` after its first text, "Je regarde".
fn held(gate: &Arc<Notify>) -> FakeScript {
    FakeScript {
        updates: vec![text_chunk("Je regarde"), text_chunk(" Voilà.")],
        pause: Some((1, gate.clone())),
        ..FakeScript::default()
    }
}

fn looking() -> Event {
    Event::Assistant {
        text: "Je regarde".into(),
    }
}

/// The ids of the requests still pending in the database.
async fn pending(dir: &Path) -> Vec<i64> {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let db = fronde_db::open(dir).await.unwrap();
    fronde_entity::requests::Entity::find()
        .filter(fronde_entity::requests::Column::Status.eq("pending"))
        .all(&db)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.id)
        .collect()
}

#[tokio::test]
async fn a_permission_answered_on_the_wire_closes_its_card() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(Notify::new());
    let (assistant, fakes, injector) = open_injecting(dir.path(), held(&gate)).await;
    assistant.send("Mon agenda ?".into()).await;
    wait_until(&assistant, |events| events.contains(&looking())).await;

    // The card shows, then the agent's side cancels the request (a cancel on the wire) before
    // the turn's requests are cancelled.
    let (ask, answer) = injected_ask();
    let reply = ask.reply.clone();
    inject(&injector, AgentEvent::Permission(Box::new(ask)));
    let id = wait_for_permission(&assistant).await;
    assert!(reply.send(RequestPermissionOutcome::Cancelled));
    assert_eq!(answer.await.unwrap(), RequestPermissionOutcome::Cancelled);

    // The card closes at once, so no answer is recorded as given.
    wait_until(&assistant, |events| {
        events.contains(&closed(id, Outcome::Cancelled))
    })
    .await;
    assert_eq!(fakes.allow(id).await, Err(Refusal::Closed));
    assert_eq!(pending(dir.path()).await, Vec::<i64>::new());
    gate.notify_one();
    wait_for_turns(&assistant, 1).await;
}

#[tokio::test]
async fn a_permission_answered_before_its_card_shows_none() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(Notify::new());
    let (assistant, _fakes, injector) = open_injecting(dir.path(), held(&gate)).await;
    assistant.send("Mon agenda ?".into()).await;
    wait_until(&assistant, |events| events.contains(&looking())).await;

    let (ask, answer) = injected_ask();
    assert!(ask.reply.send(RequestPermissionOutcome::Cancelled));
    inject(&injector, AgentEvent::Permission(Box::new(ask)));
    // Events are recorded in order: once the next one shows, the request was handled.
    inject(&injector, AgentEvent::Text("Ensuite".into()));
    let after = Event::Assistant {
        text: "Ensuite".into(),
    };
    let events = wait_until(&assistant, |events| events.contains(&after)).await;
    assert!(!events.iter().any(is_permission), "{events:?}");
    assert_eq!(answer.await.unwrap(), RequestPermissionOutcome::Cancelled);
    assert_eq!(pending(dir.path()).await, Vec::<i64>::new());
    gate.notify_one();
    wait_for_turns(&assistant, 1).await;
}

#[tokio::test]
async fn a_permission_after_its_turn_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (assistant, _fakes, injector) = open_injecting(dir.path(), FakeScript::default()).await;
    assistant.send("Mon agenda ?".into()).await;
    wait_for_turns(&assistant, 1).await;

    // A background task of the agent's asks once the turn is over: no card can belong to it.
    let (ask, answer) = injected_ask();
    inject(&injector, AgentEvent::Permission(Box::new(ask)));
    assert_eq!(
        timeout(WAIT, answer).await.unwrap().unwrap(),
        selected("reject")
    );
    let events = events(&assistant);
    assert!(!events.iter().any(is_permission), "{events:?}");
    assert_eq!(pending(dir.path()).await, Vec::<i64>::new());
}

#[tokio::test]
async fn no_card_outlives_its_turn() {
    // The agent's requests race the end of the turn: whatever the order, the agent is answered
    // and no card is left open.
    for _ in 0..20 {
        let dir = tempfile::tempdir().unwrap();
        let gate = Arc::new(Notify::new());
        let (assistant, _fakes, injector) = open_injecting(dir.path(), held(&gate)).await;
        assistant.send("Mon agenda ?".into()).await;
        wait_until(&assistant, |events| events.contains(&looking())).await;
        let (ask, answer) = injected_ask();
        gate.notify_one();
        inject(&injector, AgentEvent::Permission(Box::new(ask)));
        wait_for_turns(&assistant, 1).await;
        let answered = timeout(WAIT, answer).await.unwrap().unwrap();
        assert!(
            answered == RequestPermissionOutcome::Cancelled || answered == selected("reject"),
            "{answered:?}"
        );
        timeout(WAIT, async {
            while !pending(dir.path()).await.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a card outlived its turn");
    }
}

/// The form the Claude adapter 0.107.0 sends for two questions of `AskUserQuestion`
/// (`askUserQuestionsToCreateRequest` in `dist/elicitation.js`), without its session id.
fn ask_user_question_form() -> serde_json::Value {
    serde_json::json!({
        "mode": "form",
        "toolCallId": "toolu_01",
        "message": "Please answer the following questions.",
        "requestedSchema": {
            "type": "object",
            "properties": {
                "question_0": {
                    "type": "string",
                    "title": "Appli",
                    "description": "Quelle appli installer ?",
                    "oneOf": [
                        {"const": "Mealie", "title": "Mealie", "description": "Recettes",
                         "_meta": {"_claude/askUserQuestionOption": {"preview": "[ Mealie ]"}}},
                        {"const": "Seerr", "title": "Seerr", "description": "Films"},
                    ],
                },
                "question_0_custom": {
                    "type": "string",
                    "title": "Other",
                    "description": "Type your own answer, or add a note to the option you chose above (optional).",
                },
                "question_1": {
                    "type": "array",
                    "title": "Pièces",
                    "description": "Quelles pièces ?",
                    "items": {"anyOf": [
                        {"const": "Salon", "title": "Salon"},
                        {"const": "Cuisine", "title": "Cuisine"},
                    ]},
                },
                "question_1_custom": {
                    "type": "string",
                    "title": "Other",
                    "description": "Type your own answer to add to your selection above (optional).",
                },
            },
        },
    })
}

fn eliciting(form: serde_json::Value) -> FakeScript {
    FakeScript {
        elicitation: Some(form),
        ..FakeScript::default()
    }
}

/// Waits for a form card and returns its request's id and card.
async fn wait_for_form(assistant: &Assistant) -> (u64, Card) {
    let events = wait_until(assistant, |events| {
        events.iter().any(|event| {
            matches!(
                event,
                Event::Request {
                    card: Card::Form { .. },
                    ..
                }
            )
        })
    })
    .await;
    events
        .into_iter()
        .find_map(|event| match event {
            Event::Request {
                id,
                card: card @ Card::Form { .. },
                from: None,
                server: false,
                woken_by: None,
                carries: 0,
            } => Some((id, card)),
            _ => None,
        })
        .expect("a form from the assistant")
}

fn elicited(fakes: &Fakes) -> Vec<Result<CreateElicitationResponse, String>> {
    fakes.handles()[0].record().elicitation_responses
}

fn pick(selected: &[&str], other: Option<&str>, note: Option<&str>) -> fronde_protocol::FieldValue {
    fronde_protocol::ChoiceAnswer {
        selected: selected.iter().map(|s| (*s).to_owned()).collect(),
        other: other.map(str::to_owned),
        note: note.map(str::to_owned),
    }
    .into()
}

#[tokio::test]
async fn ask_user_question_becomes_one_card_per_question() {
    let dir = tempfile::tempdir().unwrap();
    let (assistant, fakes) = open(dir.path(), eliciting(ask_user_question_form())).await;
    assistant.send("Installe une appli".into()).await;
    let (id, card) = wait_for_form(&assistant).await;
    let Card::Form {
        message, fields, ..
    } = &card
    else {
        unreachable!()
    };
    assert_eq!(message, "Please answer the following questions.");
    let questions: Vec<_> = fields
        .iter()
        .map(|field| match field {
            fronde_protocol::Field::Choice {
                key,
                header,
                question,
                multi,
                other,
                ..
            } => (
                key.as_str(),
                header.as_deref(),
                question.as_str(),
                *multi,
                *other,
            ),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        questions,
        [
            (
                "question_0",
                Some("Appli"),
                "Quelle appli installer ?",
                false,
                true
            ),
            ("question_1", Some("Pièces"), "Quelles pièces ?", true, true),
        ]
    );

    let values = [
        (
            "question_0".to_owned(),
            pick(&["Seerr"], None, Some("le soir")),
        ),
        (
            "question_1".to_owned(),
            pick(&["Salon"], Some("Garage"), None),
        ),
    ]
    .into();
    fakes
        .requests()
        .answer(id, Response::Form { values }, &alix())
        .await
        .unwrap();
    let events = wait_for_turns(&assistant, 1).await;
    assert!(
        events.contains(&closed(
            id,
            Outcome::Answered {
                summary: "Seerr ; Salon, Garage".into()
            }
        )),
        "{events:?}"
    );
    let [Ok(response)] = <[_; 1]>::try_from(elicited(&fakes)).unwrap() else {
        panic!("{:?}", elicited(&fakes));
    };
    let ElicitationAction::Accept(accept) = response.action else {
        panic!("{response:?}");
    };
    let content = accept.content.unwrap();
    let text = |s: &str| ElicitationContentValue::String(s.into());
    assert_eq!(
        content,
        [
            ("question_0".to_owned(), text("Seerr")),
            ("question_0_custom".to_owned(), text("le soir")),
            (
                "question_1".to_owned(),
                ElicitationContentValue::StringArray(vec!["Salon".into()])
            ),
            ("question_1_custom".to_owned(), text("Garage")),
        ]
        .into()
    );
}

#[tokio::test]
async fn elicitation_decline_and_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let (assistant, fakes) = open(dir.path(), eliciting(ask_user_question_form())).await;
    assistant.send("Installe".into()).await;
    let (id, _) = wait_for_form(&assistant).await;
    fakes
        .requests()
        .answer(id, Response::Decline, &alix())
        .await
        .unwrap();
    let events = wait_for_turns(&assistant, 1).await;
    let declined = Event::RequestClosed {
        id,
        outcome: Outcome::Declined,
        by: Some("alix".into()),
    };
    assert!(events.contains(&declined), "{events:?}");
    assert_eq!(
        elicited(&fakes),
        [Ok(CreateElicitationResponse::new(
            ElicitationAction::Decline
        ))]
    );
}

#[tokio::test]
async fn cancel_cancels_the_elicitation() {
    let dir = tempfile::tempdir().unwrap();
    let (assistant, fakes) = open(dir.path(), eliciting(ask_user_question_form())).await;
    assistant.send("Installe".into()).await;
    let (id, _) = wait_for_form(&assistant).await;
    assistant.cancel().await;
    let events = wait_for_turns(&assistant, 1).await;
    assert!(
        events.contains(&closed(id, Outcome::Cancelled)),
        "{events:?}"
    );
    assert_eq!(events.last(), Some(&ended(TurnOutcome::Cancelled)));
    assert_eq!(
        elicited(&fakes),
        [Ok(CreateElicitationResponse::new(
            ElicitationAction::Cancel
        ))]
    );
    let late = Response::Form {
        values: [("question_0".to_owned(), pick(&["Seerr"], None, None))].into(),
    };
    assert_eq!(
        fakes.requests().answer(id, late, &alix()).await,
        Err(Refusal::Closed)
    );
}

#[tokio::test]
async fn elicitation_expires_as_a_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let (assistant, fakes) = open(dir.path(), eliciting(ask_user_question_form())).await;
    assistant.set_elicitation_timeout(Duration::from_millis(200));
    assistant.send("Installe".into()).await;
    let (id, _) = wait_for_form(&assistant).await;
    let events = wait_for_turns(&assistant, 1).await;
    assert!(events.contains(&closed(id, Outcome::Expired)), "{events:?}");
    assert_eq!(
        elicited(&fakes),
        [Ok(CreateElicitationResponse::new(
            ElicitationAction::Cancel
        ))]
    );
}

#[tokio::test]
async fn withdrawn_elicitation_closes_its_card() {
    let dir = tempfile::tempdir().unwrap();
    let withdraw = Arc::new(Notify::new());
    let script = FakeScript {
        withdraw_elicitation: Some(withdraw.clone()),
        ..eliciting(ask_user_question_form())
    };
    let (assistant, fakes) = open(dir.path(), script).await;
    assistant.send("Installe".into()).await;
    let (id, _) = wait_for_form(&assistant).await;
    withdraw.notify_one();
    let events = wait_until(&assistant, |events| {
        events.contains(&closed(id, Outcome::Cancelled))
    })
    .await;
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::RequestClosed {
            outcome: Outcome::Answered { .. },
            ..
        }
    )));
    wait_for_turns(&assistant, 1).await;
    // The agent withdrew it: it got the cancel it asked for, never an answer.
    assert_eq!(
        elicited(&fakes),
        [Ok(CreateElicitationResponse::new(
            ElicitationAction::Cancel
        ))]
    );
}

#[tokio::test]
async fn secret_form_is_declined_without_a_card() {
    let dir = tempfile::tempdir().unwrap();
    let form = serde_json::json!({
        "mode": "form",
        "message": "Ta clé GitHub",
        "requestedSchema": {"type": "object", "properties": {"github_token": {"type": "string"}}},
    });
    let (assistant, fakes) = open(dir.path(), eliciting(form)).await;
    assistant.send("Configure GitHub".into()).await;
    let events = wait_for_turns(&assistant, 1).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Request { .. })),
        "{events:?}"
    );
    let [Ok(response)] = <[_; 1]>::try_from(elicited(&fakes)).unwrap() else {
        panic!("{:?}", elicited(&fakes));
    };
    assert_eq!(response.action, ElicitationAction::Decline);
    let reason = response.meta.unwrap()["fronde/refused"].clone();
    // A plain reason, naming neither the field nor the rule.
    assert_eq!(
        reason,
        fronde_requests::elicitation::SECRET_REFUSAL,
        "{reason}"
    );
}

#[tokio::test]
async fn turn_end_closes_the_agents_form() {
    let dir = tempfile::tempdir().unwrap();
    let script = FakeScript {
        leave_elicitation_pending: true,
        ..eliciting(ask_user_question_form())
    };
    let (assistant, fakes) = open(dir.path(), script).await;
    assistant.send("Installe".into()).await;
    let events = wait_for_turns(&assistant, 1).await;
    let (id, _) = wait_for_form(&assistant).await;
    // The turn ended on its own: its open form closed before it, and the agent got a cancel.
    let closed_at = events
        .iter()
        .position(|event| *event == closed(id, Outcome::Cancelled))
        .expect("the form was not closed");
    assert_eq!(events.len(), closed_at + 2, "{events:?}");
    assert_eq!(events.last(), Some(&ended(TurnOutcome::Done)));
    timeout(WAIT, async {
        while elicited(&fakes).is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        elicited(&fakes),
        [Ok(CreateElicitationResponse::new(
            ElicitationAction::Cancel
        ))]
    );
}
