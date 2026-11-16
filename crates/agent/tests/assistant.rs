use agent_client_protocol::v1::schema::{
    RequestPermissionOutcome, SelectedPermissionOutcome, SessionUpdate, ToolCall, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields,
};
use fronde_agent::acp::{AgentClient, AgentSpec};
use fronde_agent::assistant::{Assistant, Connector, LOG_FILE, SESSION_FILE, process_connector};
use fronde_agent::fake::{FAKE_SESSION_ID, FakeHandle, FakeScript, fake_agent, text_chunk};
use fronde_agent::log::{Event, ToolState, TurnOutcome};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::broadcast::error::TryRecvError;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);
const BUSY: &str = "Fronde répond encore ; attends la fin ou arrête-la.";
const GONE: &str = "Cette demande n'est plus active.";
const FRESH: &str = "Nouvelle conversation avec l'agent : l'ancienne n'a pas pu être reprise.";
const NO_START: &str = "L'agent ne répond pas au démarrage.";
const NOT_INSTALLED: &str = "Impossible de démarrer l'agent : lance `mise run agent` ou indique son chemin dans [agent] command.";
const SIGNED_OUT: &str =
    "L'agent n'est pas connecté à Claude : voir le README (claude setup-token).";
const FAILED: &str = "L'agent a rencontré une erreur.";

fn spec() -> AgentSpec {
    AgentSpec {
        command: vec!["claude-agent-acp".into()],
        env: Vec::new(),
        cwd: "/srv/fronde/agent".into(),
        hub_url: "http://127.0.0.1:7878/mcp".into(),
        hub_token: "hub-token".into(),
        system_prompt: "You are Fronde.".into(),
        builtin_tools: vec!["WebSearch".into(), "WebFetch".into()],
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

fn open(dir: &Path, script: FakeScript) -> (Arc<Assistant>, Arc<Fakes>) {
    let (connector, fakes) = fake_connector(script, None);
    let assistant = Assistant::open(spec(), dir, connector).expect("open");
    (assistant, fakes)
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

/// Waits for a permission request and returns its id.
async fn wait_for_permission(assistant: &Assistant) -> u64 {
    let events = wait_until(assistant, |events| {
        events
            .iter()
            .any(|event| matches!(event, Event::Permission { .. }))
    })
    .await;
    events
        .iter()
        .find_map(|event| match event {
            Event::Permission { id, .. } => Some(*id),
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

fn permission(id: u64) -> Event {
    Event::Permission {
        id,
        title: "Lire l'agenda".into(),
    }
}

fn answered(id: u64, allowed: bool) -> Event {
    Event::PermissionAnswered { id, allowed }
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
    let (assistant, _fakes) = open(dir.path(), script);

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
    let saved = std::fs::read_to_string(dir.path().join(SESSION_FILE)).unwrap();
    assert_eq!(saved, FAKE_SESSION_ID);
}

#[tokio::test]
async fn second_prompt_during_turn_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (assistant, fakes) = open(dir.path(), asking());

    assistant.send("Mon agenda ?".into()).await;
    assistant.send("Et demain ?".into()).await;
    let id = wait_for_permission(&assistant).await;
    assistant.answer(id, true);
    let events = wait_for_turns(&assistant, 1).await;

    assert!(events.contains(&notice(BUSY)), "{events:?}");
    assert!(!events.contains(&user("Et demain ?")), "{events:?}");
    assert_eq!(fakes.handles()[0].record().permission_outcomes.len(), 1);

    // Once the turn is over, the next message goes through.
    assistant.send("Et demain ?".into()).await;
    let id = wait_for_permission_after(&assistant, id).await;
    assistant.answer(id, false);
    let events = wait_for_turns(&assistant, 2).await;
    assert!(events.contains(&user("Et demain ?")), "{events:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_right_after_turn_ended_is_accepted() {
    const TURNS: usize = 200;
    let dir = tempfile::tempdir().unwrap();
    let (assistant, _fakes) = open(dir.path(), FakeScript::default());
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

/// Waits for a permission request other than `previous`.
async fn wait_for_permission_after(assistant: &Assistant, previous: u64) -> u64 {
    let events = wait_until(assistant, |events| {
        events
            .iter()
            .any(|event| matches!(event, Event::Permission { id, .. } if *id != previous))
    })
    .await;
    events
        .iter()
        .find_map(|event| match event {
            Event::Permission { id, .. } if *id != previous => Some(*id),
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
    let (assistant, fakes) = open(dir.path(), script);

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
    let assistant = Assistant::open(spec(), dir.path(), connector).expect("open");

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
    let assistant = Assistant::open(spec, dir.path(), process_connector()).expect("open");

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
    let (assistant, _fakes) = open(dir.path(), script);

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
    let (assistant, _fakes) = open(dir.path(), resumable.clone());
    assistant.send("Salut".into()).await;
    wait_for_turns(&assistant, 1).await;
    drop(assistant);
    let (assistant, fakes) = open(dir.path(), resumable.clone());
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

    // An agent that cannot resume it starts a new conversation, and says so.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(SESSION_FILE), "earlier-session").unwrap();
    let refusing = FakeScript {
        refuse_resume: true,
        ..resumable
    };
    let (assistant, fakes) = open(dir.path(), refusing);
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
    let saved = std::fs::read_to_string(dir.path().join(SESSION_FILE)).unwrap();
    assert_eq!(saved, FAKE_SESSION_ID);
}

#[tokio::test]
async fn permission_answer_is_logged() {
    let dir = tempfile::tempdir().unwrap();
    let (assistant, fakes) = open(dir.path(), asking());

    assistant.send("Mon agenda ?".into()).await;
    let id = wait_for_permission(&assistant).await;
    assistant.answer(id, true);
    wait_for_turns(&assistant, 1).await;
    assistant.answer(id, true);

    assert_eq!(
        events(&assistant),
        vec![
            user("Mon agenda ?"),
            permission(2),
            answered(2, true),
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
async fn permission_ids_stay_unique_across_connections() {
    let dir = tempfile::tempdir().unwrap();
    let (assistant, _fakes) = open(dir.path(), asking());
    assistant.send("Mon agenda ?".into()).await;
    let first = wait_for_permission(&assistant).await;
    assistant.answer(first, true);
    wait_for_turns(&assistant, 1).await;
    drop(assistant);

    // After a restart the agent numbers its requests from 1 again; the log's ids go on.
    let (assistant, fakes) = open(dir.path(), asking());
    assistant.send("Et demain ?".into()).await;
    let second = wait_for_permission_after(&assistant, first).await;
    assert!(second > first, "{second} after {first}");
    assistant.answer(first, true);
    assistant.answer(second, false);
    let events = wait_for_turns(&assistant, 2).await;

    assert_eq!(events.iter().filter(|e| **e == notice(GONE)).count(), 1);
    assert!(events.contains(&answered(second, false)), "{events:?}");
    assert_eq!(
        fakes.handles()[0].record().permission_outcomes,
        vec![RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new("reject")
        )]
    );
}

#[tokio::test]
async fn cancel_ends_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (assistant, fakes) = open(dir.path(), asking());

    assistant.send("Mon agenda ?".into()).await;
    wait_for_permission(&assistant).await;
    assistant.cancel();
    wait_for_turns(&assistant, 1).await;
    // Idle: nothing to cancel.
    assistant.cancel();

    assert_eq!(
        events(&assistant),
        vec![
            user("Mon agenda ?"),
            permission(2),
            answered(2, false),
            ended(TurnOutcome::Cancelled),
        ]
    );
    assert_eq!(fakes.handles()[0].record().cancels, 1);
}

#[tokio::test(start_paused = true)]
async fn stop_frees_a_turn_the_agent_never_ends() {
    let dir = tempfile::tempdir().unwrap();
    let script = FakeScript {
        updates: vec![text_chunk("Je cherche")],
        ignore_cancel: true,
        ..FakeScript::default()
    };
    let (assistant, fakes) = open(dir.path(), script);
    let searching = Event::Assistant {
        text: "Je cherche".into(),
    };

    assistant.send("Salut".into()).await;
    wait_until(&assistant, |events| events.contains(&searching)).await;
    assistant.cancel();
    // The agent gets a grace period to end the turn itself.
    tokio::time::sleep(Duration::from_secs(9)).await;
    assert_eq!(events(&assistant), vec![user("Salut"), searching.clone()]);
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(
        events(&assistant),
        vec![user("Salut"), searching, ended(TurnOutcome::Cancelled)]
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
    let assistant = Assistant::open(spec(), dir.path(), connector).expect("open");

    assistant.send("Salut".into()).await;
    assistant.cancel();
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
    let (assistant, _fakes) = open(dir.path(), script);

    assistant.send("Mon agenda ?".into()).await;
    wait_for_turns(&assistant, 1).await;
    assistant.answer(2, true);

    assert_eq!(
        events(&assistant),
        vec![
            user("Mon agenda ?"),
            permission(2),
            answered(2, false),
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
    let assistant = Assistant::open(spec(), dir.path(), connector).expect("open");

    assistant.send("Salut".into()).await;
    timeout(WAIT, async {
        while hanging.started.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the connect never started");
    assistant.cancel();

    assert_eq!(
        wait_for_turns(&assistant, 1).await,
        vec![user("Salut"), ended(TurnOutcome::Cancelled)]
    );
    assert_eq!(hanging.dropped.load(Ordering::SeqCst), 1);

    // Fronde is free again.
    assistant.send("Encore".into()).await;
    assistant.cancel();
    let events = wait_for_turns(&assistant, 2).await;
    assert!(!events.contains(&notice(BUSY)), "{events:?}");
    assert!(events.contains(&user("Encore")), "{events:?}");
}

#[tokio::test(start_paused = true)]
async fn connect_timeout_ends_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (connector, hanging) = hanging_connector();
    let assistant = Assistant::open(spec(), dir.path(), connector).expect("open");

    assistant.send("Salut".into()).await;
    tokio::time::sleep(Duration::from_secs(59)).await;
    assert_eq!(events(&assistant), vec![user("Salut")]);
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(
        events(&assistant),
        vec![user("Salut"), notice(NO_START), ended(TurnOutcome::Error)]
    );
    assert_eq!(hanging.dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn open_closes_a_turn_left_open() {
    let dir = tempfile::tempdir().unwrap();
    // The server stopped in the middle of a turn: its user message has no `turn_ended`.
    {
        let log = fronde_agent::log::ConversationLog::open(&dir.path().join(LOG_FILE)).unwrap();
        log.append(user("Un")).unwrap();
        log.append(ended(TurnOutcome::Done)).unwrap();
        log.append(user("Deux")).unwrap();
        log.append(Event::Assistant {
            text: "Je regarde".into(),
        })
        .unwrap();
    }
    let (assistant, _) = open(dir.path(), FakeScript::default());
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
    let (assistant, _) = open(dir.path(), FakeScript::default());
    assert_eq!(events(&assistant), closed);
}

#[tokio::test]
async fn open_renames_the_files_from_before_fronde() {
    let dir = tempfile::tempdir().unwrap();
    {
        let log =
            fronde_agent::log::ConversationLog::open(&dir.path().join("jarvis.jsonl")).unwrap();
        log.append(user("Un")).unwrap();
        log.append(ended(TurnOutcome::Done)).unwrap();
    }
    std::fs::write(dir.path().join("jarvis.session"), "earlier-session").unwrap();

    let (assistant, _) = open(dir.path(), FakeScript::default());

    assert_eq!(
        events(&assistant),
        vec![user("Un"), ended(TurnOutcome::Done)]
    );
    let saved = std::fs::read_to_string(dir.path().join(SESSION_FILE)).unwrap();
    assert_eq!(saved, "earlier-session");
    assert!(!dir.path().join("jarvis.jsonl").exists());
    assert!(!dir.path().join("jarvis.session").exists());
}
