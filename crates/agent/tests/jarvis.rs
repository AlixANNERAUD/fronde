use agent_client_protocol::v1::schema::{
    RequestPermissionOutcome, SelectedPermissionOutcome, SessionUpdate, ToolCall, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields,
};
use jarvis_agent::acp::{AgentClient, AgentSpec};
use jarvis_agent::fake::{FAKE_SESSION_ID, FakeHandle, FakeScript, fake_agent, text_chunk};
use jarvis_agent::jarvis::{Connector, Jarvis};
use jarvis_agent::log::{Event, ToolState, TurnOutcome};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);
const BUSY: &str = "Jarvis répond encore ; attends la fin ou arrête-le.";
const GONE: &str = "Cette demande n'est plus active.";
const FRESH: &str = "Nouvelle conversation avec l'agent : l'ancienne n'a pas pu être reprise.";
const NO_START: &str = "L'agent ne répond pas au démarrage.";

fn spec() -> AgentSpec {
    AgentSpec {
        command: vec!["claude-agent-acp".into()],
        env: Vec::new(),
        cwd: "/srv/jarvis/agent".into(),
        hub_url: "http://127.0.0.1:7878/mcp".into(),
        hub_token: "hub-token".into(),
        system_prompt: "You are Jarvis.".into(),
    }
}

/// Fake agents started by a connector, one per connection.
#[derive(Default)]
struct Fakes {
    handles: Mutex<Vec<FakeHandle>>,
}

impl Fakes {
    fn handles(&self) -> Vec<FakeHandle> {
        self.handles.lock().unwrap().clone()
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

fn open(dir: &Path, script: FakeScript) -> (Arc<Jarvis>, Arc<Fakes>) {
    let (connector, fakes) = fake_connector(script, None);
    let jarvis = Jarvis::open(spec(), dir, connector).expect("open");
    (jarvis, fakes)
}

fn events(jarvis: &Jarvis) -> Vec<Event> {
    jarvis
        .log()
        .since(0)
        .into_iter()
        .map(|entry| entry.event)
        .collect()
}

/// Waits until the log satisfies `done`, then returns its events.
async fn wait_until(jarvis: &Jarvis, done: impl Fn(&[Event]) -> bool) -> Vec<Event> {
    let waited = timeout(WAIT, async {
        loop {
            let events = events(jarvis);
            if done(&events) {
                return events;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    waited.unwrap_or_else(|_| panic!("timed out; the log has {:?}", events(jarvis)))
}

async fn wait_for_turns(jarvis: &Jarvis, count: usize) -> Vec<Event> {
    wait_until(jarvis, |events| {
        events
            .iter()
            .filter(|event| matches!(event, Event::TurnEnded { .. }))
            .count()
            >= count
    })
    .await
}

/// Waits for a permission request and returns its id.
async fn wait_for_permission(jarvis: &Jarvis) -> u64 {
    let events = wait_until(jarvis, |events| {
        events
            .iter()
            .any(|event| matches!(event, Event::Permission { .. }))
    })
    .await;
    events
        .iter()
        .find_map(|event| match event {
            Event::Permission { id, .. } => Some(id.parse().expect("numeric id")),
            _ => None,
        })
        .unwrap()
}

fn user(text: &str) -> Event {
    Event::User { text: text.into() }
}

fn notice(text: &str) -> Event {
    Event::Notice { text: text.into() }
}

fn ended(outcome: TurnOutcome) -> Event {
    Event::TurnEnded { outcome }
}

fn permission(id: &str) -> Event {
    Event::Permission {
        id: id.into(),
        title: "Lire l'agenda".into(),
    }
}

fn answered(id: &str, allowed: bool) -> Event {
    Event::PermissionAnswered {
        id: id.into(),
        allowed,
    }
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
    let (jarvis, _fakes) = open(dir.path(), script);

    jarvis.send("Salut".into()).await;

    assert_eq!(
        wait_for_turns(&jarvis, 1).await,
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
    let saved = std::fs::read_to_string(dir.path().join("jarvis.session")).unwrap();
    assert_eq!(saved, FAKE_SESSION_ID);
}

#[tokio::test]
async fn second_prompt_during_turn_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (jarvis, fakes) = open(dir.path(), asking());

    jarvis.send("Mon agenda ?".into()).await;
    jarvis.send("Et demain ?".into()).await;
    let id = wait_for_permission(&jarvis).await;
    jarvis.answer(id, true);
    let events = wait_for_turns(&jarvis, 1).await;

    assert!(events.contains(&notice(BUSY)), "{events:?}");
    assert!(!events.contains(&user("Et demain ?")), "{events:?}");
    assert_eq!(fakes.handles()[0].record().permission_outcomes.len(), 1);

    // Once the turn is over, the next message goes through.
    jarvis.send("Et demain ?".into()).await;
    let id = wait_for_permission_after(&jarvis, id).await;
    jarvis.answer(id, false);
    let events = wait_for_turns(&jarvis, 2).await;
    assert!(events.contains(&user("Et demain ?")), "{events:?}");
}

/// Waits for a permission request other than `previous`.
async fn wait_for_permission_after(jarvis: &Jarvis, previous: u64) -> u64 {
    let previous = previous.to_string();
    let events = wait_until(jarvis, |events| {
        events
            .iter()
            .any(|event| matches!(event, Event::Permission { id, .. } if *id != previous))
    })
    .await;
    events
        .iter()
        .find_map(|event| match event {
            Event::Permission { id, .. } if *id != previous => Some(id.parse().unwrap()),
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn prompt_error_becomes_error_event() {
    let dir = tempfile::tempdir().unwrap();
    let script = FakeScript {
        fail_prompt: true,
        ..FakeScript::default()
    };
    let (jarvis, fakes) = open(dir.path(), script);

    jarvis.send("Salut".into()).await;
    let events = wait_for_turns(&jarvis, 1).await;

    let [first, Event::Notice { text }, last] = events.as_slice() else {
        panic!("expected user, notice, end; got {events:?}");
    };
    assert_eq!(first, &user("Salut"));
    assert!(text.contains("the agent failed the turn"), "{text}");
    assert_eq!(last, &ended(TurnOutcome::Error));

    // The failed client was dropped: the next message reconnects.
    jarvis.send("Encore".into()).await;
    wait_for_turns(&jarvis, 2).await;
    assert_eq!(fakes.handles().len(), 2);
}

#[tokio::test]
async fn connect_error_becomes_error_event() {
    let dir = tempfile::tempdir().unwrap();
    let connector: Connector = Arc::new(|_: &AgentSpec, _, _| {
        Box::pin(async { Err(anyhow::anyhow!("cannot start the agent (claude-agent-acp)")) })
    });
    let jarvis = Jarvis::open(spec(), dir.path(), connector).expect("open");

    jarvis.send("Salut".into()).await;

    assert_eq!(
        wait_for_turns(&jarvis, 1).await,
        vec![
            user("Salut"),
            notice("cannot start the agent (claude-agent-acp)"),
            ended(TurnOutcome::Error),
        ]
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
    let (jarvis, _fakes) = open(dir.path(), resumable.clone());
    jarvis.send("Salut".into()).await;
    wait_for_turns(&jarvis, 1).await;
    drop(jarvis);
    let (jarvis, fakes) = open(dir.path(), resumable.clone());
    jarvis.send("Re".into()).await;
    let events = wait_for_turns(&jarvis, 2).await;
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

    // An agent that cannot resume it starts a new conversation, and says so.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("jarvis.session"), "earlier-session").unwrap();
    let refusing = FakeScript {
        refuse_resume: true,
        ..resumable
    };
    let (jarvis, fakes) = open(dir.path(), refusing);
    jarvis.send("Salut".into()).await;
    let events = wait_for_turns(&jarvis, 1).await;
    assert_eq!(
        &*fakes.handles()[0].record().resumes[0].session_id.0,
        "earlier-session"
    );
    assert_eq!(
        events,
        vec![user("Salut"), notice(FRESH), ended(TurnOutcome::Done)]
    );
    let saved = std::fs::read_to_string(dir.path().join("jarvis.session")).unwrap();
    assert_eq!(saved, FAKE_SESSION_ID);
}

#[tokio::test]
async fn permission_answer_is_logged() {
    let dir = tempfile::tempdir().unwrap();
    let (jarvis, fakes) = open(dir.path(), asking());

    jarvis.send("Mon agenda ?".into()).await;
    let id = wait_for_permission(&jarvis).await;
    jarvis.answer(id, true);
    wait_for_turns(&jarvis, 1).await;
    jarvis.answer(id, true);

    assert_eq!(
        events(&jarvis),
        vec![
            user("Mon agenda ?"),
            permission("1"),
            answered("1", true),
            ended(TurnOutcome::Done),
            notice(GONE),
        ]
    );
    assert_eq!(
        fakes.handles()[0].record().permission_outcomes,
        vec![RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new("allow-once")
        )]
    );
}

#[tokio::test]
async fn cancel_ends_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (jarvis, fakes) = open(dir.path(), asking());

    jarvis.send("Mon agenda ?".into()).await;
    wait_for_permission(&jarvis).await;
    jarvis.cancel();
    wait_for_turns(&jarvis, 1).await;
    // Idle: nothing to cancel.
    jarvis.cancel();

    assert_eq!(
        events(&jarvis),
        vec![
            user("Mon agenda ?"),
            permission("1"),
            answered("1", false),
            ended(TurnOutcome::Cancelled),
        ]
    );
    assert_eq!(fakes.handles()[0].record().cancels, 1);
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
    let jarvis = Jarvis::open(spec(), dir.path(), connector).expect("open");

    jarvis.send("Salut".into()).await;
    jarvis.cancel();
    gate.notify_one();

    assert_eq!(
        wait_for_turns(&jarvis, 1).await,
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
    let (jarvis, _fakes) = open(dir.path(), script);

    jarvis.send("Mon agenda ?".into()).await;
    wait_for_turns(&jarvis, 1).await;
    jarvis.answer(1, true);

    assert_eq!(
        events(&jarvis),
        vec![
            user("Mon agenda ?"),
            permission("1"),
            answered("1", false),
            ended(TurnOutcome::Done),
            notice(GONE),
        ]
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
    let jarvis = Jarvis::open(spec(), dir.path(), connector).expect("open");

    jarvis.send("Salut".into()).await;
    timeout(WAIT, async {
        while hanging.started.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the connect never started");
    jarvis.cancel();

    assert_eq!(
        wait_for_turns(&jarvis, 1).await,
        vec![user("Salut"), ended(TurnOutcome::Cancelled)]
    );
    assert_eq!(hanging.dropped.load(Ordering::SeqCst), 1);

    // Jarvis is free again.
    jarvis.send("Encore".into()).await;
    jarvis.cancel();
    let events = wait_for_turns(&jarvis, 2).await;
    assert!(!events.contains(&notice(BUSY)), "{events:?}");
    assert!(events.contains(&user("Encore")), "{events:?}");
}

#[tokio::test(start_paused = true)]
async fn connect_timeout_ends_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (connector, hanging) = hanging_connector();
    let jarvis = Jarvis::open(spec(), dir.path(), connector).expect("open");

    jarvis.send("Salut".into()).await;
    tokio::time::sleep(Duration::from_secs(59)).await;
    assert_eq!(events(&jarvis), vec![user("Salut")]);
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(
        events(&jarvis),
        vec![user("Salut"), notice(NO_START), ended(TurnOutcome::Error)]
    );
    assert_eq!(hanging.dropped.load(Ordering::SeqCst), 1);
}
