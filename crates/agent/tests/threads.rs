use fronde_agent::acp::{AgentClient, AgentSpec};
use fronde_agent::assistant::{Assistant, Connector, TOPIC};
use fronde_agent::fake::{
    FAKE_MODEL_OPTION, FAKE_MODELS, FAKE_SESSION_ID, FakeHandle, FakeScript, fake_agent, text_chunk,
};
use fronde_agent::log::{ConversationLog, Event, TurnOutcome};
use fronde_agent::threads::{Budget, StartedBy, ThreadId, Threads};
use fronde_protocol::{Card, ChoiceOption, Field, Outcome};
use fronde_requests::{Ask, Caller, Requests, User};
use sea_orm::EntityTrait;
use secrecy::ExposeSecret;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);
const ARCHIVED: &str = "Cette conversation est archivée : elle ne reçoit plus de message.";
const INTERRUPTED: &str = "La réponse précédente a été interrompue.";
const WAITING: &str = "En attente : d'autres conversations utilisent l'agent.";

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
        builtin_tools: Vec::new(),
    }
}

fn alix() -> User {
    User {
        name: "alix".into(),
        admin: true,
    }
}

/// A connector starting fake agents playing `script`, and their handles.
fn fake_connector(script: FakeScript) -> (Connector, Arc<Mutex<Vec<FakeHandle>>>) {
    let handles = Arc::new(Mutex::new(Vec::new()));
    let connector: Connector = Arc::new({
        let handles = handles.clone();
        move |spec: &AgentSpec, resume, events| {
            let (spec, script, handles) = (spec.clone(), script.clone(), handles.clone());
            Box::pin(async move {
                let (transport, handle) = fake_agent(script);
                handles.lock().unwrap().push(handle);
                AgentClient::connect(transport, &spec, resume, events).await
            })
        }
    });
    (connector, handles)
}

async fn open(dir: &Path, connector: Connector) -> Arc<Threads> {
    let db = fronde_db::open(dir).await.unwrap();
    Threads::open(db, dir, spec(), connector, alix())
        .await
        .unwrap()
}

fn events(assistant: &Assistant) -> Vec<Event> {
    assistant
        .log()
        .since(0)
        .into_iter()
        .map(|entry| entry.event)
        .collect()
}

async fn wait_for_turns(assistant: &Assistant, count: usize) -> Vec<Event> {
    timeout(WAIT, async {
        loop {
            let events = events(assistant);
            let ended = events
                .iter()
                .filter(|event| matches!(event, Event::TurnEnded { .. }))
                .count();
            if ended >= count {
                return events;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the turns did not end")
}

fn user(text: &str) -> Event {
    Event::User { text: text.into() }
}

fn question() -> Card {
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
            other: false,
            required: true,
            note: false,
        }],
        submit: None,
        truncated: false,
        replaced: false,
        during: Vec::new(),
    }
}

fn ask(topic: &str) -> Ask {
    Ask {
        topic: topic.into(),
        turn: None,
        caller: Caller::Mcp("Codex".into()),
        client_request_id: None,
        card: question(),
        for_user: "alix".into(),
        admin_only: false,
        timeout: WAIT,
        tied: false,
    }
}

/// The chat from before threads opens as thread 1: same stream, untouched, and the agent's
/// session moved from the key-value table to the thread's row.
#[tokio::test]
async fn main_thread_keeps_the_conversation_and_session() {
    let dir = tempfile::tempdir().unwrap();
    let log_id = {
        let db = fronde_db::open(dir.path()).await.unwrap();
        let log = ConversationLog::open(&db, TOPIC).await.unwrap();
        log.append(user("Salut")).await.unwrap();
        log.append(Event::TurnEnded {
            outcome: TurnOutcome::Done,
        })
        .await
        .unwrap();
        fronde_db::kv::set(&db, "agent", "session", b"s1".to_vec(), None)
            .await
            .unwrap();
        log.log_id().to_owned()
    };
    let before = {
        let db = fronde_db::open(dir.path()).await.unwrap();
        ConversationLog::open(&db, TOPIC).await.unwrap().since(0)
    };

    let script = FakeScript {
        resume: true,
        updates: vec![text_chunk("Re")],
        ..FakeScript::default()
    };
    let (connector, handles) = fake_connector(script);
    let threads = open(dir.path(), connector).await;
    let main = threads.main();
    assert_eq!(main.thread(), ThreadId::MAIN);
    assert_eq!(main.topic(), "conversation");
    assert_eq!(main.log().log_id(), log_id);
    assert_eq!(main.log().since(0), before);

    let db = fronde_db::open(dir.path()).await.unwrap();
    let row = fronde_entity::threads::Entity::find_by_id(1)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.topic, "conversation");
    assert_eq!(row.session_id.as_deref(), Some("s1"));
    assert_eq!(
        fronde_db::kv::get(&db, "agent", "session").await.unwrap(),
        None,
        "the session id is kept in one place"
    );
    let listed = threads.list(false).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].title, "Fronde");
    assert_eq!(listed[0].started_by, StartedBy::User);
    assert_eq!(listed[0].owner, "alix");
    assert_eq!(listed[0].depth, 0);

    // The agent resumes the moved session.
    main.send("Re".into()).await;
    wait_for_turns(&main, 2).await;
    let record = handles.lock().unwrap()[0].record();
    assert_eq!(&*record.resumes[0].session_id.0, "s1");

    // A second start finds the thread as it left it.
    drop((threads, main));
    let (connector, _) = fake_connector(FakeScript::default());
    let threads = open(dir.path(), connector).await;
    assert_eq!(threads.list(true).await.unwrap().len(), 1);
    assert_eq!(threads.main().log().log_id(), log_id);
}

/// A new database gets thread 1, with the session the agent gives it.
#[tokio::test]
async fn a_new_database_starts_with_the_main_thread() {
    let dir = tempfile::tempdir().unwrap();
    let script = FakeScript {
        updates: vec![text_chunk("Bonjour")],
        ..FakeScript::default()
    };
    let (connector, _) = fake_connector(script);
    let threads = open(dir.path(), connector).await;
    let main = threads.main();
    main.send("Salut".into()).await;
    wait_for_turns(&main, 1).await;
    let db = fronde_db::open(dir.path()).await.unwrap();
    let row = fronde_entity::threads::Entity::find_by_id(1)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.session_id.as_deref(), Some(FAKE_SESSION_ID));
    assert_eq!(
        fronde_db::kv::get(&db, "agent", "session").await.unwrap(),
        None
    );
}

#[tokio::test]
async fn requests_are_announced_in_their_threads_stream() {
    let dir = tempfile::tempdir().unwrap();
    let (connector, _) = fake_connector(FakeScript::default());
    let threads = open(dir.path(), connector).await;
    let second = threads
        .create("alix", StartedBy::User, None, "Deux")
        .await
        .unwrap();
    assert_eq!(second.id, ThreadId(2));
    assert_eq!(second.topic, "thread:2");
    let db = fronde_db::open(dir.path()).await.unwrap();
    let requests = Requests::open(db, threads.clone()).await.unwrap();
    threads.set_requests(&requests).await;

    let waiter = requests.ask(ask("thread:2")).await.unwrap();
    let id = waiter.id();
    requests.cancel(id).await.unwrap();

    let thread = threads.assistant(ThreadId(2)).await.unwrap().unwrap();
    let shown = events(&thread);
    assert!(
        matches!(&shown[..], [Event::Request { id: shown, .. }, Event::RequestClosed { id: closed, outcome: Outcome::Cancelled, .. }] if *shown == id && *closed == id),
        "{shown:?}"
    );
    assert!(events(&threads.main()).is_empty());

    // A stream no thread owns shows nothing.
    assert!(requests.ask(ask("thread:9")).await.is_err());
    assert!(events(&threads.main()).is_empty());
}

#[tokio::test]
async fn create_rename_archive_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let (connector, _) = fake_connector(FakeScript::default());
    let threads = open(dir.path(), connector).await;
    let parent = threads
        .create("alix", StartedBy::User, None, "Courses")
        .await
        .unwrap();
    let child = threads
        .create(
            "alix",
            StartedBy::Thread(parent.id),
            Some(parent.id),
            "Liste",
        )
        .await
        .unwrap();
    assert_eq!((parent.id, child.id), (ThreadId(2), ThreadId(3)));
    assert_eq!(child.topic, "thread:3");
    assert_eq!(child.parent, Some(parent.id));
    assert_eq!((parent.depth, child.depth), (0, 1));
    assert_eq!(child.started_by, StartedBy::Thread(ThreadId(2)));
    assert_eq!(child.provider, "claude");
    assert!(
        threads
            .create("alix", StartedBy::User, Some(ThreadId(42)), "Orpheline")
            .await
            .is_err()
    );

    threads.rename(child.id, "Liste de courses").await.unwrap();
    threads.set_archived(parent.id, true).await.unwrap();
    assert!(threads.rename(ThreadId(42), "Personne").await.is_err());
    assert!(threads.set_archived(ThreadId::MAIN, true).await.is_err());

    // Kept across a restart.
    drop(threads);
    let (connector, _) = fake_connector(FakeScript::default());
    let threads = open(dir.path(), connector).await;
    let live: Vec<_> = threads
        .list(false)
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.id, row.title))
        .collect();
    assert_eq!(
        live,
        [
            (ThreadId::MAIN, "Fronde".into()),
            (ThreadId(3), "Liste de courses".to_owned())
        ]
    );
    let all = threads.list(true).await.unwrap();
    assert_eq!(all.len(), 3);
    assert!(all[1].archived_at.is_some());
    let got = threads.get(child.id).await.unwrap().unwrap();
    assert_eq!(got.title, "Liste de courses");
    assert!(got.updated_at >= child.updated_at);
    assert_eq!(threads.get(ThreadId(42)).await.unwrap(), None);
    assert!(threads.assistant(ThreadId(42)).await.unwrap().is_none());

    threads.set_archived(parent.id, false).await.unwrap();
    assert_eq!(threads.list(false).await.unwrap().len(), 3);
    // A new thread never takes an earlier one's id.
    let next = threads
        .create("alix", StartedBy::Mcp("Codex".into()), None, "Quatre")
        .await
        .unwrap();
    assert_eq!(next.id, ThreadId(4));
    assert_eq!(next.started_by, StartedBy::Mcp("Codex".into()));
}

#[tokio::test]
async fn archived_thread_refuses_a_message() {
    let dir = tempfile::tempdir().unwrap();
    let (connector, handles) = fake_connector(FakeScript::default());
    let threads = open(dir.path(), connector).await;
    let row = threads
        .create("alix", StartedBy::User, None, "Vieille")
        .await
        .unwrap();
    let thread = threads.assistant(row.id).await.unwrap().unwrap();
    threads.set_archived(row.id, true).await.unwrap();
    thread.send("Encore là ?".into()).await;
    assert_eq!(
        events(&thread),
        [Event::Notice {
            text: ARCHIVED.into()
        }]
    );

    // Still refused when the thread opens after it was archived.
    drop((threads, thread));
    let (connector, more) = fake_connector(FakeScript::default());
    let threads = open(dir.path(), connector).await;
    let thread = threads.assistant(row.id).await.unwrap().unwrap();
    thread.send("Et maintenant ?".into()).await;
    assert_eq!(
        events(&thread),
        [
            Event::Notice {
                text: ARCHIVED.into()
            },
            Event::Notice {
                text: ARCHIVED.into()
            }
        ]
    );
    assert!(handles.lock().unwrap().is_empty() && more.lock().unwrap().is_empty());

    // Brought back, it takes messages again.
    threads.set_archived(row.id, false).await.unwrap();
    thread.send("Te revoilà".into()).await;
    let shown = wait_for_turns(&thread, 1).await;
    assert!(shown.contains(&user("Te revoilà")), "{shown:?}");
}

/// A thread other than the main one, stopped in the middle of a turn, is closed at the next start
/// as the main thread is.
#[tokio::test]
async fn open_closes_a_turn_left_open_in_any_thread() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (connector, _) = fake_connector(FakeScript::default());
        let threads = open(dir.path(), connector).await;
        threads
            .create("alix", StartedBy::User, None, "Deux")
            .await
            .unwrap();
        threads
            .create("alix", StartedBy::User, None, "Trois")
            .await
            .unwrap();
        let db = fronde_db::open(dir.path()).await.unwrap();
        let log = ConversationLog::open(&db, "thread:2").await.unwrap();
        log.append(user("Un")).await.unwrap();
        log.append(Event::Assistant {
            text: "Je regarde".into(),
        })
        .await
        .unwrap();
        let log = ConversationLog::open(&db, "thread:3").await.unwrap();
        log.append(user("Un")).await.unwrap();
        log.append(Event::TurnEnded {
            outcome: TurnOutcome::Done,
        })
        .await
        .unwrap();
    }
    let (connector, _) = fake_connector(FakeScript::default());
    let _threads = open(dir.path(), connector).await;
    let db = fronde_db::open(dir.path()).await.unwrap();
    let read = |topic: &'static str| {
        let db = db.clone();
        async move {
            ConversationLog::open(&db, topic)
                .await
                .unwrap()
                .since(0)
                .into_iter()
                .map(|entry| entry.event)
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        read("thread:2").await,
        [
            user("Un"),
            Event::Assistant {
                text: "Je regarde".into()
            },
            Event::Notice {
                text: INTERRUPTED.into()
            },
            Event::TurnEnded {
                outcome: TurnOutcome::Error
            }
        ]
    );
    assert_eq!(read("thread:3").await.len(), 2);
}

/// Fake agents started for any thread: each records its thread and hub token, and a thread given
/// a gate holds each of its turns after a first chunk until the gate is opened.
#[derive(Default)]
struct Pooled {
    gates: Mutex<HashMap<ThreadId, Arc<Notify>>>,
    /// Each start: its thread, its hub token and the fake agent's handle.
    started: Mutex<Vec<(ThreadId, String, FakeHandle)>>,
}

impl Pooled {
    fn connector(self: &Arc<Self>) -> Connector {
        let pooled = self.clone();
        Arc::new(move |spec: &AgentSpec, resume, events| {
            let (spec, pooled) = (spec.clone(), pooled.clone());
            Box::pin(async move {
                let gate = pooled.gates.lock().unwrap().get(&spec.thread).cloned();
                let script = FakeScript {
                    updates: vec![text_chunk("Je regarde"), text_chunk(" : voilà")],
                    pause: gate.map(|gate| (1, gate)),
                    resume: true,
                    ..FakeScript::default()
                };
                let (transport, handle) = fake_agent(script);
                pooled.started.lock().unwrap().push((
                    spec.thread,
                    spec.hub_token.expose_secret().to_owned(),
                    handle,
                ));
                AgentClient::connect(transport, &spec, resume, events).await
            })
        })
    }

    /// Holds thread `id`'s turns until [`Pooled::open`].
    fn gate(&self, id: ThreadId) {
        self.gates
            .lock()
            .unwrap()
            .insert(id, Arc::new(Notify::new()));
    }

    /// Lets thread `id`'s running (or next) turn go on.
    fn open(&self, id: ThreadId) {
        self.gates.lock().unwrap()[&id].notify_one();
    }

    /// The threads whose agents started, in order.
    fn threads(&self) -> Vec<ThreadId> {
        self.started
            .lock()
            .unwrap()
            .iter()
            .map(|(thread, ..)| *thread)
            .collect()
    }

    /// The handles of thread `id`'s agents, oldest first.
    fn handles(&self, id: ThreadId) -> Vec<FakeHandle> {
        self.started
            .lock()
            .unwrap()
            .iter()
            .filter(|(thread, ..)| *thread == id)
            .map(|(.., handle)| handle.clone())
            .collect()
    }

    fn token(&self, id: ThreadId) -> String {
        self.started
            .lock()
            .unwrap()
            .iter()
            .rfind(|(thread, ..)| *thread == id)
            .map(|(_, token, _)| token.clone())
            .expect("no agent started for the thread")
    }

    async fn wait_for_starts(&self, count: usize) {
        timeout(WAIT, async {
            while self.started.lock().unwrap().len() < count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{count} agents never started: {:?}", self.threads()));
    }
}

async fn open_pooled(dir: &Path, max_running: usize) -> (Arc<Threads>, Arc<Pooled>) {
    let pooled = Arc::new(Pooled::default());
    let db = fronde_db::open(dir).await.unwrap();
    let budget = Budget {
        max_running,
        ..Budget::default()
    };
    let threads = Threads::open_with(db, dir, spec(), pooled.connector(), alix(), budget)
        .await
        .unwrap();
    (threads, pooled)
}

async fn new_thread(threads: &Threads, started_by: StartedBy) -> Arc<Assistant> {
    let parent = match started_by {
        StartedBy::Thread(parent) => Some(parent),
        _ => None,
    };
    let row = threads
        .create("alix", started_by, parent, "Fil")
        .await
        .unwrap();
    threads.assistant(row.id).await.unwrap().unwrap()
}

async fn wait_until(assistant: &Assistant, what: &str, done: impl Fn(&[Event]) -> bool) {
    timeout(WAIT, async {
        while !done(&events(assistant)) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}: {:?}", events(assistant)));
}

fn waiting() -> Event {
    Event::Notice {
        text: WAITING.into(),
    }
}

fn count(events: &[Event], event: &Event) -> usize {
    events.iter().filter(|e| *e == event).count()
}

async fn wait_closed(handle: &FakeHandle) {
    timeout(WAIT, async {
        while !handle.record().closed {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the agent's connection was never closed");
}

/// With two agents at most, a third thread's turn waits, says so once, and starts once one of
/// the running turns ends: that agent, now idle, makes room.
#[tokio::test]
async fn pool_caps_running_agents() {
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled) = open_pooled(dir.path(), 2).await;
    let main = threads.main();
    let second = new_thread(&threads, StartedBy::User).await;
    let third = new_thread(&threads, StartedBy::User).await;
    for thread in [&main, &second, &third] {
        pooled.gate(thread.thread());
    }

    main.send("Un".into()).await;
    second.send("Deux".into()).await;
    pooled.wait_for_starts(2).await;
    third.send("Trois".into()).await;
    wait_until(&third, "the third turn never said it waits", |events| {
        events.contains(&waiting())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(pooled.threads(), [main.thread(), second.thread()]);
    assert!(!third.agent_running());

    // The main turn ends: its idle agent stops for the third thread's.
    pooled.open(main.thread());
    wait_for_turns(&main, 1).await;
    pooled.wait_for_starts(3).await;
    assert_eq!(pooled.threads()[2], third.thread());
    wait_closed(&pooled.handles(main.thread())[0]).await;
    assert!(!main.agent_running());
    assert!(second.agent_running());

    pooled.open(second.thread());
    pooled.open(third.thread());
    let shown = wait_for_turns(&third, 1).await;
    assert_eq!(count(&shown, &waiting()), 1, "{shown:?}");
    assert_eq!(
        shown.last(),
        Some(&Event::TurnEnded {
            outcome: TurnOutcome::Done
        })
    );
    assert!(
        shown
            .iter()
            .any(|event| matches!(event, Event::Assistant { text } if text.contains("voilà"))),
        "{shown:?}"
    );
    wait_for_turns(&second, 1).await;
    assert!(!events(&main).contains(&waiting()));
    assert!(!events(&second).contains(&waiting()));
}

/// Two idle agents and a third thread's turn: the agent idle for longer stops, the other stays.
#[tokio::test]
async fn lru_idle_agent_makes_room() {
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled) = open_pooled(dir.path(), 2).await;
    let main = threads.main();
    let second = new_thread(&threads, StartedBy::User).await;
    let third = new_thread(&threads, StartedBy::User).await;

    main.send("Un".into()).await;
    wait_for_turns(&main, 1).await;
    second.send("Deux".into()).await;
    wait_for_turns(&second, 1).await;
    assert!(main.agent_running() && second.agent_running());

    third.send("Trois".into()).await;
    let shown = wait_for_turns(&third, 1).await;
    // Room was made at once: no wait to tell.
    assert!(!shown.contains(&waiting()), "{shown:?}");
    wait_closed(&pooled.handles(main.thread())[0]).await;
    assert!(!main.agent_running());
    assert!(second.agent_running() && third.agent_running());
    assert!(!pooled.handles(second.thread())[0].record().closed);
    // The stopped agent's token is revoked; the others' still work.
    let token = pooled.token(main.thread());
    assert!(threads.holder(token.as_bytes()).await.is_none());
    let token = pooled.token(second.thread());
    assert!(threads.holder(token.as_bytes()).await.is_some());
}

/// With two agents at most, threads the owner did not start share one: two children queue
/// behind a running one, and a message to the main thread starts at once.
#[tokio::test]
async fn children_never_take_the_last_slot() {
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled) = open_pooled(dir.path(), 2).await;
    let main = threads.main();
    let parent = StartedBy::Thread(ThreadId::MAIN);
    let first = new_thread(&threads, parent.clone()).await;
    let second = new_thread(&threads, parent.clone()).await;
    let third = new_thread(&threads, parent).await;
    for child in [&first, &second, &third] {
        pooled.gate(child.thread());
    }

    first.send("Un".into()).await;
    pooled.wait_for_starts(1).await;
    second.send("Deux".into()).await;
    third.send("Trois".into()).await;
    for child in [&second, &third] {
        wait_until(child, "a child never said it waits", |events| {
            events.contains(&waiting())
        })
        .await;
    }

    main.send("Bonjour".into()).await;
    let shown = wait_for_turns(&main, 1).await;
    assert!(!shown.contains(&waiting()), "{shown:?}");
    assert_eq!(
        shown.last(),
        Some(&Event::TurnEnded {
            outcome: TurnOutcome::Done
        })
    );
    assert_eq!(pooled.threads(), [first.thread(), main.thread()]);

    // Stop gives up a wait.
    third.cancel().await;
    assert_eq!(
        wait_for_turns(&third, 1).await.last(),
        Some(&Event::TurnEnded {
            outcome: TurnOutcome::Cancelled
        })
    );

    // Once the first child's turn ends, the second's starts in its place.
    pooled.open(first.thread());
    pooled.open(second.thread());
    let shown = wait_for_turns(&second, 1).await;
    assert_eq!(count(&shown, &waiting()), 1, "{shown:?}");
    assert_eq!(
        pooled.threads(),
        [first.thread(), main.thread(), second.thread()]
    );
    wait_closed(&pooled.handles(first.thread())[0]).await;
    // The main thread's idle agent was not the one stopped: a child's makes room for a child.
    assert!(main.agent_running());
}

/// An agent idle for `idle_stop` stops, its token revoked; the next message starts it again and
/// resumes the same session.
#[tokio::test]
async fn idle_agent_stops_and_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled) = open_pooled(dir.path(), 3).await;
    let main = threads.main();
    main.send("Un".into()).await;
    wait_for_turns(&main, 1).await;
    let token = pooled.token(main.thread());
    assert!(threads.holder(token.as_bytes()).await.is_some());

    // Paused only while nothing is written, as in the assistant's tests: the database is written
    // by a thread of its own, which a paused clock does not wait for.
    tokio::time::pause();
    tokio::time::sleep(Budget::default().idle_stop - Duration::from_secs(1)).await;
    assert!(main.agent_running());
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!main.agent_running());
    tokio::time::resume();
    wait_closed(&pooled.handles(main.thread())[0]).await;
    assert!(threads.holder(token.as_bytes()).await.is_none());

    main.send("Deux".into()).await;
    let shown = wait_for_turns(&main, 2).await;
    let handles = pooled.handles(main.thread());
    assert_eq!(handles.len(), 2);
    let record = handles[1].record();
    assert!(record.new_sessions.is_empty(), "{record:?}");
    assert_eq!(record.resumes.len(), 1);
    assert_eq!(record.resumes[0].session_id.0.as_ref(), FAKE_SESSION_ID);
    assert_eq!(count(&shown, &user("Deux")), 1);
    assert!(
        !shown
            .iter()
            .any(|event| matches!(event, Event::Notice { .. })),
        "{shown:?}"
    );

    // A new turn before the delay keeps the agent: the delay starts over at its end.
    tokio::time::pause();
    tokio::time::sleep(Budget::default().idle_stop - Duration::from_secs(1)).await;
    assert!(main.agent_running());
    tokio::time::resume();
    main.send("Trois".into()).await;
    wait_for_turns(&main, 3).await;
    tokio::time::pause();
    tokio::time::sleep(Budget::default().idle_stop - Duration::from_secs(1)).await;
    assert!(main.agent_running());
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!main.agent_running());
    tokio::time::resume();
    assert_eq!(pooled.handles(main.thread()).len(), 2);
}

/// A thread's directory left from an earlier database (restored from a backup, or made anew)
/// is moved aside before a new thread takes its id: the new agent never finds the old one's
/// files.
#[tokio::test]
async fn a_new_thread_never_inherits_an_earlier_threads_files() {
    let dir = tempfile::tempdir().unwrap();
    let leftover = dir.path().join("threads/2/home/.claude/projects/old.jsonl");
    std::fs::create_dir_all(leftover.parent().unwrap()).unwrap();
    std::fs::write(&leftover, "an earlier conversation").unwrap();
    let (connector, _) = fake_connector(FakeScript::default());
    let threads = open(dir.path(), connector).await;

    let row = threads
        .create("alix", StartedBy::User, None, "Nouveau")
        .await
        .unwrap();
    assert_eq!(row.id, ThreadId(2));
    assert!(!dir.path().join("threads/2").exists());
    let aside: Vec<_> = std::fs::read_dir(dir.path().join("threads"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert!(aside[0].starts_with("2.old-"), "{aside:?}");
    let kept = dir
        .path()
        .join("threads")
        .join(&aside[0])
        .join("home/.claude/projects/old.jsonl");
    assert_eq!(
        std::fs::read_to_string(kept).unwrap(),
        "an earlier conversation"
    );

    // Its agent starts in fresh directories.
    let thread = threads.assistant(row.id).await.unwrap().unwrap();
    thread.send("Salut".into()).await;
    wait_for_turns(&thread, 1).await;
    assert!(dir.path().join("threads/2/work").is_dir());
    assert!(!dir.path().join("threads/2/home/.claude").exists());
    // A thread whose directory is not there takes its id as usual.
    let next = threads
        .create("alix", StartedBy::User, None, "Encore")
        .await
        .unwrap();
    assert_eq!(next.id, ThreadId(3));
}

/// The thread's model is chosen through the session's config options after each open or
/// resume, since a resumed session may come back on the agent's default; the models seen are
/// kept for choosing one before an agent runs.
#[tokio::test]
async fn model_is_applied_again_after_resume() {
    let dir = tempfile::tempdir().unwrap();
    let script = FakeScript {
        resume: true,
        models: true,
        ..FakeScript::default()
    };
    let (connector, started) = fake_connector(script);
    let threads = open(dir.path(), connector).await;
    assert!(threads.models("claude").await.unwrap().is_empty());
    threads
        .set_model(ThreadId::MAIN, Some("fake-large"))
        .await
        .unwrap();
    assert_eq!(
        threads
            .get(ThreadId::MAIN)
            .await
            .unwrap()
            .unwrap()
            .model
            .as_deref(),
        Some("fake-large")
    );
    let main = threads.main();
    main.send("Un".into()).await;
    wait_for_turns(&main, 1).await;
    assert!(main.stop_agent().await);
    main.send("Deux".into()).await;
    wait_for_turns(&main, 2).await;

    let handles = started.lock().unwrap().clone();
    assert_eq!(handles.len(), 2);
    let chosen = [(FAKE_MODEL_OPTION.to_owned(), "fake-large".to_owned())];
    let first = handles[0].record();
    assert_eq!(first.new_sessions.len(), 1);
    assert_eq!(first.config_sets, chosen);
    let second = handles[1].record();
    assert!(second.new_sessions.is_empty(), "{second:?}");
    assert_eq!(second.resumes.len(), 1);
    assert_eq!(second.config_sets, chosen);
    let models: Vec<String> = threads
        .models("claude")
        .await
        .unwrap()
        .into_iter()
        .map(|model| model.id)
        .collect();
    assert_eq!(models, FAKE_MODELS);

    // Back to the agent's default: nothing is asked of the next session.
    threads.set_model(ThreadId::MAIN, None).await.unwrap();
    assert!(main.stop_agent().await);
    main.send("Trois".into()).await;
    wait_for_turns(&main, 3).await;
    let third = started.lock().unwrap()[2].record();
    assert!(third.config_sets.is_empty(), "{third:?}");
}

/// A model the agent does not offer fails the turn's start before any prompt reaches it.
#[tokio::test]
async fn unknown_thread_model_fails_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let script = FakeScript {
        models: true,
        ..FakeScript::default()
    };
    let (connector, handles) = fake_connector(script);
    let threads = open(dir.path(), connector).await;
    threads
        .set_model(ThreadId::MAIN, Some("fake-huge"))
        .await
        .unwrap();
    let main = threads.main();
    main.send("Un".into()).await;
    let shown = wait_for_turns(&main, 1).await;
    assert!(
        shown.contains(&Event::Notice {
            text: "L'agent a rencontré une erreur.".into()
        }),
        "{shown:?}"
    );
    let record = handles.lock().unwrap()[0].record();
    assert!(record.config_sets.is_empty(), "{record:?}");
    assert!(!main.agent_running());
}

/// A thread stored for a provider that is not configured does not open.
#[tokio::test]
async fn a_thread_of_another_provider_does_not_open() {
    let dir = tempfile::tempdir().unwrap();
    let (connector, _) = fake_connector(FakeScript::default());
    let threads = open(dir.path(), connector).await;
    let row = threads
        .create("alix", StartedBy::User, None, "Ailleurs")
        .await
        .unwrap();
    assert_eq!(row.provider, "claude");
    use sea_orm::QueryFilter;
    let db = fronde_db::open(dir.path()).await.unwrap();
    fronde_entity::threads::Entity::update_many()
        .col_expr(
            fronde_entity::threads::Column::Provider,
            sea_orm::sea_query::Expr::value("codex"),
        )
        .filter(sea_orm::ColumnTrait::eq(
            &fronde_entity::threads::Column::Id,
            row.id.0,
        ))
        .exec(&db)
        .await
        .unwrap();
    let Err(error) = threads.assistant(row.id).await else {
        panic!("the thread opened");
    };
    let error = error.to_string();
    assert!(
        error.contains("\"codex\", which is not configured"),
        "{error}"
    );
}
