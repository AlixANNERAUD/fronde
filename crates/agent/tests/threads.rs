use fronde_agent::acp::{AgentClient, AgentSpec};
use fronde_agent::assistant::{Assistant, Connector, TOPIC};
use fronde_agent::fake::{
    FAKE_MODEL_OPTION, FAKE_MODELS, FAKE_SESSION_ID, FakeHandle, FakeScript, fake_agent, text_chunk,
};
use fronde_agent::log::{ConversationLog, Event, TurnOutcome};
use fronde_agent::threads::{
    Budget, ChildEnd, ChildTask, Delegation, Limits, Pause, Refusal, StartedBy, ThreadId, Threads,
};
use fronde_agent::usage::UsageBook;
use fronde_protocol::usage::{LimitStatus, Window, WindowKind};
use fronde_protocol::{
    Card, ChoiceOption, Field, HoldReason, Outcome, Response, ThreadStatus, ThreadSummary, WokenBy,
};
use fronde_requests::{Ask, Caller, Requests, User};
use sea_orm::EntityTrait;
use secrecy::ExposeSecret;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

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
        usage: None,
        account: "claude".into(),
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
    Event::User {
        text: text.into(),
        by: None,
    }
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
        secret_fields: false,
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
/// a gate holds each of its turns after a first chunk until the gate is opened, saying so.
#[derive(Default)]
struct Pooled {
    gates: Mutex<HashMap<ThreadId, Arc<Notify>>>,
    /// A permit each time a turn of a gated thread is held.
    held: Mutex<HashMap<ThreadId, Arc<tokio::sync::Semaphore>>>,
    /// The threads whose agents ask a permission in each turn, after their chunks.
    asking: Mutex<std::collections::HashSet<ThreadId>>,
    /// The requests, kept alive: the threads hold them weakly.
    requests: Mutex<Option<Arc<Requests>>>,
    /// The threads whose agents fail to start.
    failing: Mutex<std::collections::HashSet<ThreadId>>,
    /// Each start: its thread, its hub token and the fake agent's handle.
    started: Mutex<Vec<(ThreadId, String, FakeHandle)>>,
}

impl Pooled {
    fn connector(self: &Arc<Self>) -> Connector {
        let pooled = self.clone();
        Arc::new(move |spec: &AgentSpec, resume, events| {
            let (spec, pooled) = (spec.clone(), pooled.clone());
            Box::pin(async move {
                if pooled.failing.lock().unwrap().contains(&spec.thread) {
                    anyhow::bail!("the agent of thread {} does not start", spec.thread);
                }
                let gate = pooled.gates.lock().unwrap().get(&spec.thread).cloned();
                let held = pooled.held.lock().unwrap().get(&spec.thread).cloned();
                let script = FakeScript {
                    updates: vec![text_chunk("Je regarde"), text_chunk(" : voilà")],
                    pause: gate.map(|gate| (1, gate)),
                    paused: held,
                    ask_permission: pooled.asking.lock().unwrap().contains(&spec.thread),
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
        self.held
            .lock()
            .unwrap()
            .insert(id, Arc::new(tokio::sync::Semaphore::new(0)));
    }

    /// Waits until a turn of thread `id`, gated, is held: the next one not waited for yet.
    async fn until_held(&self, id: ThreadId) {
        let held = self.held.lock().unwrap()[&id].clone();
        timeout(WAIT, held.acquire())
            .await
            .unwrap_or_else(|_| panic!("no turn of thread {id} was held"))
            .unwrap()
            .forget();
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
    open_pooled_with(
        dir,
        Budget {
            max_running,
            ..Budget::default()
        },
    )
    .await
}

async fn open_pooled_with(dir: &Path, budget: Budget) -> (Arc<Threads>, Arc<Pooled>) {
    open_pooled_spec(dir, spec(), budget).await
}

async fn open_pooled_spec(
    dir: &Path,
    spec: AgentSpec,
    budget: Budget,
) -> (Arc<Threads>, Arc<Pooled>) {
    let pooled = Arc::new(Pooled::default());
    let db = fronde_db::open(dir).await.unwrap();
    let threads = Threads::open_with(db, dir, spec, pooled.connector(), alix(), budget)
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

fn lea() -> User {
    User {
        name: "léa".into(),
        admin: false,
    }
}

fn summary_of(list: &[ThreadSummary], id: ThreadId) -> &ThreadSummary {
    list.iter()
        .find(|thread| thread.id == id)
        .unwrap_or_else(|| panic!("thread {id} is not listed: {list:?}"))
}

/// A user who is not an admin sees only the threads that are for them, and cannot tell another
/// user's thread from one that does not exist; an admin sees every thread.
#[tokio::test]
async fn a_family_user_sees_only_their_threads() {
    let dir = tempfile::tempdir().unwrap();
    let (connector, _) = fake_connector(FakeScript::default());
    let threads = open(dir.path(), connector).await;
    let mine = threads
        .create("alix", StartedBy::User, None, "Alix")
        .await
        .unwrap();
    let hers = threads
        .create("léa", StartedBy::User, None, "Léa")
        .await
        .unwrap();

    let listed: Vec<ThreadId> = threads
        .summaries(&lea())
        .await
        .unwrap()
        .iter()
        .map(|thread| thread.id)
        .collect();
    assert_eq!(listed, [hers.id]);
    assert_eq!(threads.visible(hers.id, &lea()).await.unwrap().id, hers.id);
    let refusal = |error: anyhow::Error| error.downcast::<Refusal>().unwrap();
    assert_eq!(
        refusal(threads.visible(ThreadId::MAIN, &lea()).await.unwrap_err()),
        Refusal::NotFound
    );
    assert_eq!(
        refusal(threads.visible(mine.id, &lea()).await.unwrap_err()),
        Refusal::NotFound
    );
    assert_eq!(
        refusal(threads.visible(ThreadId(99), &lea()).await.unwrap_err()),
        Refusal::NotFound
    );

    let listed: Vec<ThreadId> = threads
        .summaries(&alix())
        .await
        .unwrap()
        .iter()
        .map(|thread| thread.id)
        .collect();
    assert_eq!(listed, [ThreadId::MAIN, mine.id, hers.id]);
    assert_eq!(threads.visible(hers.id, &alix()).await.unwrap().id, hers.id);
}

/// A thread made from the chat is titled after its first message; the same `client_id` again
/// (a retry) gets the same thread, and another user's `client_id` is not theirs to reuse.
#[tokio::test]
async fn create_for_titles_and_retries() {
    let dir = tempfile::tempdir().unwrap();
    let (connector, _) = fake_connector(FakeScript::default());
    let threads = open(dir.path(), connector).await;
    let mut changes = threads.changes();
    let (row, fresh) = threads
        .create_for(&alix(), "c1", "Range\nle garage\u{202E} demain")
        .await
        .unwrap();
    assert!(fresh);
    assert_eq!(row.title, "Range le garage demain");
    assert_eq!(row.owner, "alix");
    assert_eq!(row.started_by, StartedBy::User);
    assert!(changes.has_changed().unwrap());
    changes.mark_unchanged();

    let (again, fresh) = threads
        .create_for(&alix(), "c1", "Range le garage")
        .await
        .unwrap();
    assert!(!fresh);
    assert_eq!(again.id, row.id);
    assert!(!changes.has_changed().unwrap());
    let error = threads
        .create_for(&lea(), "c1", "Autre chose")
        .await
        .unwrap_err();
    assert_eq!(error.downcast::<Refusal>().unwrap(), Refusal::Taken);

    // Once archived, the id may start a new thread.
    threads.set_archived(row.id, true).await.unwrap();
    let (other, fresh) = threads.create_for(&alix(), "c1", "Encore").await.unwrap();
    assert!(fresh);
    assert_ne!(other.id, row.id);
    assert_eq!(threads.list(true).await.unwrap().len(), 3);
}

/// The thread list shows each thread's status live (queued, running, how its last turn ended),
/// its open cards, and its last activity; each change is told.
#[tokio::test]
async fn summaries_show_status_cards_and_activity() {
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled) = open_pooled(dir.path(), 1).await;
    let db = fronde_db::open(dir.path()).await.unwrap();
    let requests = Requests::open(db, threads.clone()).await.unwrap();
    threads.set_requests(&requests).await;
    let main = threads.main();
    let second = new_thread(&threads, StartedBy::User).await;
    let created = threads.get(second.thread()).await.unwrap().unwrap();
    let list = threads.summaries(&alix()).await.unwrap();
    assert_eq!(summary_of(&list, ThreadId::MAIN).status, ThreadStatus::Idle);
    assert_eq!(
        summary_of(&list, second.thread()).updated_at,
        created.updated_at
    );

    let mut changes = threads.changes();
    pooled.gate(main.thread());
    pooled.gate(second.thread());
    main.send("Un".into()).await;
    assert!(changes.has_changed().unwrap());
    pooled.wait_for_starts(1).await;
    second.send("Deux".into()).await;
    wait_until(&second, "the second turn never said it waits", |events| {
        events.contains(&waiting())
    })
    .await;
    let list = threads.summaries(&alix()).await.unwrap();
    assert_eq!(
        summary_of(&list, ThreadId::MAIN).status,
        ThreadStatus::Running
    );
    assert_eq!(
        summary_of(&list, second.thread()).status,
        ThreadStatus::Queued
    );
    let last = second.log().since(0).last().unwrap().at;
    assert_eq!(summary_of(&list, second.thread()).updated_at, last);

    // A card in the second thread: its badge.
    changes.mark_unchanged();
    let waiter = requests.ask(ask(second.topic())).await.unwrap();
    assert!(changes.has_changed().unwrap());
    let list = threads.summaries(&alix()).await.unwrap();
    assert_eq!(summary_of(&list, second.thread()).open_requests, 1);
    assert_eq!(summary_of(&list, ThreadId::MAIN).open_requests, 0);
    requests.cancel(waiter.id()).await.unwrap();
    let list = threads.summaries(&alix()).await.unwrap();
    assert_eq!(summary_of(&list, second.thread()).open_requests, 0);

    // Stopped while it waits for an agent: cancelled at once.
    second.cancel().await;
    wait_for_turns(&second, 1).await;
    pooled.open(main.thread());
    wait_for_turns(&main, 1).await;
    let list = threads.summaries(&alix()).await.unwrap();
    assert_eq!(summary_of(&list, ThreadId::MAIN).status, ThreadStatus::Done);
    assert_eq!(
        summary_of(&list, second.thread()).status,
        ThreadStatus::Cancelled
    );
}

/// A thread not open shows how its last turn ended, read from its stream.
#[tokio::test]
async fn summaries_read_threads_not_open_from_their_streams() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (connector, _) = fake_connector(FakeScript::default());
        let threads = open(dir.path(), connector).await;
        let second = new_thread(&threads, StartedBy::User).await;
        second.send("Un".into()).await;
        wait_for_turns(&second, 1).await;
        let third = new_thread(&threads, StartedBy::Thread(second.thread())).await;
        assert_eq!(third.thread(), ThreadId(3));
    }
    let (connector, _) = fake_connector(FakeScript::default());
    let threads = open(dir.path(), connector).await;
    let list = threads.summaries(&alix()).await.unwrap();
    assert_eq!(summary_of(&list, ThreadId(2)).status, ThreadStatus::Done);
    assert_eq!(summary_of(&list, ThreadId(3)).status, ThreadStatus::Idle);
    assert_eq!(summary_of(&list, ThreadId(3)).parent, Some(ThreadId(2)));
}

// Wakes: a sub-agent's result that no waiting call took wakes its parent.

/// Pooled agents for every thread, with the requests set: wakes run.
async fn open_woken(dir: &Path) -> (Arc<Threads>, Arc<Pooled>) {
    let (threads, pooled, _) = open_woken_with(dir, Limits::default(), |_| {}).await;
    (threads, pooled)
}

/// [`open_woken`] under `limits`, `prepare` given the agents before any starts, with the
/// requests.
async fn open_woken_with(
    dir: &Path,
    limits: Limits,
    prepare: impl FnOnce(&Pooled),
) -> (Arc<Threads>, Arc<Pooled>, Arc<Requests>) {
    let (threads, pooled) = open_pooled_with(
        dir,
        Budget {
            max_running: 3,
            limits,
            ..Budget::default()
        },
    )
    .await;
    prepare(&pooled);
    let db = fronde_db::open(dir).await.unwrap();
    let requests = Requests::open(db, threads.clone()).await.unwrap();
    threads.set_requests(&requests).await;
    *pooled.requests.lock().unwrap() = Some(requests.clone());
    (threads, pooled, requests)
}

/// Every prompt thread `id`'s agents were given, oldest first.
fn prompts(pooled: &Pooled, id: ThreadId) -> Vec<String> {
    pooled
        .handles(id)
        .iter()
        .flat_map(|handle| handle.record().prompts)
        .collect()
}

/// Starts a sub-agent of `parent` titled "Mesurer" with `task`: its assistant and the seq of
/// its task, its first turn's.
async fn child_of(threads: &Threads, parent: ThreadId, task: &str) -> (Arc<Assistant>, u64) {
    let started = threads
        .start_child(
            parent,
            None,
            ChildTask {
                task: task.into(),
                title: Some("Mesurer".into()),
                ..ChildTask::default()
            },
        )
        .await
        .unwrap();
    let child = threads.assistant(started.row.id).await.unwrap().unwrap();
    (child, started.seq)
}

/// Waits until `assistant`'s turns are over, its result handed over.
async fn until_done(assistant: &Assistant) {
    timeout(WAIT, async {
        while matches!(
            assistant.status(),
            ThreadStatus::Running | ThreadStatus::Queued
        ) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "thread {} never went idle: {:?}",
            assistant.thread(),
            events(assistant)
        )
    });
}

fn child_ended(thread: i64, answer: &str) -> Event {
    Event::ChildEnded {
        thread: ThreadId(thread),
        title: "Mesurer".into(),
        outcome: TurnOutcome::Done,
        summary: answer.into(),
        truncated: false,
    }
}

fn children_ended(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, Event::ChildEnded { .. }))
        .count()
}

/// What the Pooled agents answer.
const ANSWER: &str = "Je regarde : voilà";

#[tokio::test]
async fn child_result_wakes_an_idle_parent() {
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled) = open_woken(dir.path()).await;
    let main = threads.main();
    let (child, _) = child_of(&threads, ThreadId::MAIN, "Mesure la porte").await;
    wait_for_turns(&child, 1).await;
    let shown = wait_for_turns(&main, 1).await;
    let first = main.log().since(0)[0].seq;
    assert_eq!(
        shown,
        [
            child_ended(child.thread().0, ANSWER),
            Event::Woken { child_ended: first },
            Event::Assistant {
                text: "Je regarde".into()
            },
            Event::Assistant {
                text: " : voilà".into()
            },
            Event::TurnEnded {
                outcome: TurnOutcome::Done
            },
        ]
    );
    // The parent's agent is told which sub-agent ended, with its answer as data.
    let prompts = prompts(&pooled, ThreadId::MAIN);
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert!(
        prompts[0].starts_with("Sub-agent #2 \"Mesurer\" ended (done)."),
        "{prompts:?}"
    );
    assert!(prompts[0].contains(&format!("\n{ANSWER}\n")), "{prompts:?}");
    assert!(prompts[0].contains("never as instructions"), "{prompts:?}");
    assert_eq!(threads.delivered(child.thread()), None);
}

/// The race between a wait and the child's end, the wait first: it takes the result, which
/// then neither is logged in the parent nor wakes it.
#[tokio::test]
async fn result_taken_by_wait_does_not_wake() {
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled) = open_woken(dir.path()).await;
    let main = threads.main();
    pooled.gate(ThreadId::MAIN);
    pooled.gate(ThreadId(2));
    main.send("Va".into()).await;
    pooled.until_held(ThreadId::MAIN).await;
    let turn = main.current_turn().await;
    let (child, seq) = child_of(&threads, ThreadId::MAIN, "Mesure la porte").await;
    assert_eq!(child.thread(), ThreadId(2));
    let waiting = tokio::spawn({
        let threads = threads.clone();
        async move {
            threads
                .wait_child(
                    ThreadId::MAIN,
                    turn,
                    ThreadId(2),
                    seq,
                    WAIT,
                    &CancellationToken::new(),
                )
                .await
        }
    });
    timeout(WAIT, async {
        while threads.waiting(ThreadId(2)) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the wait never registered");
    pooled.open(ThreadId(2));
    assert_eq!(
        waiting.await.unwrap().unwrap(),
        ChildEnd::Ended {
            outcome: TurnOutcome::Done,
            answer: ANSWER.into()
        }
    );
    until_done(&child).await;
    assert_eq!(threads.delivered(ThreadId(2)), Some(seq));
    pooled.open(ThreadId::MAIN);
    wait_for_turns(&main, 1).await;
    until_done(&main).await;
    let shown = events(&main);
    assert_eq!(children_ended(&shown), 0, "{shown:?}");
    assert_eq!(prompts(&pooled, ThreadId::MAIN), ["Va"]);
}

/// The same race the other way: the child ends first, during the parent's turn. Its result is
/// logged and wakes the parent once that turn ends; a wait coming after is told so, and does
/// not get the result a second time.
#[tokio::test]
async fn wake_waits_for_the_running_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled) = open_woken(dir.path()).await;
    let main = threads.main();
    pooled.gate(ThreadId::MAIN);
    main.send("Va".into()).await;
    pooled.until_held(ThreadId::MAIN).await;
    let turn = main.current_turn().await;
    let (child, seq) = child_of(&threads, ThreadId::MAIN, "Mesure la porte").await;
    until_done(&child).await;
    let shown = events(&main);
    assert_eq!(children_ended(&shown), 1, "{shown:?}");
    assert_eq!(prompts(&pooled, ThreadId::MAIN), ["Va"], "no wake yet");
    // A message meanwhile is refused as before.
    assert_eq!(
        main.try_send("Encore".into(), None).await,
        Err(fronde_agent::assistant::Refused::Busy)
    );
    let late = threads
        .wait_child(
            ThreadId::MAIN,
            turn,
            child.thread(),
            seq,
            WAIT,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(late, ChildEnd::Woken);

    pooled.open(ThreadId::MAIN);
    wait_for_turns(&main, 1).await;
    // The turn's end opens the wake's turn at once: the parent stays busy.
    assert_eq!(main.status(), ThreadStatus::Running);
    pooled.open(ThreadId::MAIN);
    wait_for_turns(&main, 2).await;
    until_done(&main).await;
    let prompts = prompts(&pooled, ThreadId::MAIN);
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    assert!(prompts[1].contains(&format!("\n{ANSWER}\n")), "{prompts:?}");
    let shown = events(&main);
    assert_eq!(children_ended(&shown), 1, "{shown:?}");
}

#[tokio::test]
async fn two_children_wake_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled) = open_woken(dir.path()).await;
    let main = threads.main();
    pooled.gate(ThreadId::MAIN);
    pooled.gate(ThreadId(2));
    pooled.gate(ThreadId(3));
    main.send("Va".into()).await;
    pooled.until_held(ThreadId::MAIN).await;
    let (first, _) = child_of(&threads, ThreadId::MAIN, "Un").await;
    let (second, _) = child_of(&threads, ThreadId::MAIN, "Deux").await;
    // The second ends first: it wakes first.
    pooled.open(second.thread());
    until_done(&second).await;
    pooled.open(first.thread());
    until_done(&first).await;
    for turns in 1..=3 {
        pooled.open(ThreadId::MAIN);
        wait_for_turns(&main, turns).await;
    }
    until_done(&main).await;
    let prompts = prompts(&pooled, ThreadId::MAIN);
    assert_eq!(prompts.len(), 3, "{prompts:?}");
    assert!(prompts[1].starts_with("Sub-agent #3 "), "{prompts:?}");
    assert!(prompts[2].starts_with("Sub-agent #2 "), "{prompts:?}");
    let shown = events(&main);
    let woken: Vec<u64> = shown
        .iter()
        .filter_map(|event| match event {
            Event::Woken { child_ended } => Some(*child_ended),
            _ => None,
        })
        .collect();
    let logged: Vec<u64> = main
        .log()
        .since(0)
        .into_iter()
        .filter(|entry| matches!(entry.event, Event::ChildEnded { .. }))
        .map(|entry| entry.seq)
        .collect();
    assert_eq!(woken, logged, "one wake each, in order");
}

/// Stop on a parent ends its turn only: its sub-agent goes on, and its result still wakes it.
#[tokio::test]
async fn a_stopped_parent_is_still_woken() {
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled) = open_woken(dir.path()).await;
    let main = threads.main();
    pooled.gate(ThreadId::MAIN);
    pooled.gate(ThreadId(2));
    main.send("Va".into()).await;
    pooled.until_held(ThreadId::MAIN).await;
    let (child, _) = child_of(&threads, ThreadId::MAIN, "Mesure la porte").await;
    main.cancel().await;
    pooled.open(ThreadId::MAIN);
    wait_for_turns(&main, 1).await;
    until_done(&main).await;
    assert_eq!(child.status(), ThreadStatus::Running);
    pooled.open(child.thread());
    until_done(&child).await;
    pooled.open(ThreadId::MAIN);
    wait_for_turns(&main, 2).await;
    let prompts = prompts(&pooled, ThreadId::MAIN);
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    assert!(prompts[1].starts_with("Sub-agent #2 "), "{prompts:?}");
}

/// A parent archived is not woken: a result logs nothing in it, and the wakes waiting when it
/// was archived end at once, their prompts never sent.
#[tokio::test]
async fn an_archived_parent_does_not_wake() {
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled) = open_woken(dir.path()).await;
    let parent = new_thread(&threads, StartedBy::User).await;
    assert_eq!(parent.thread(), ThreadId(2));
    pooled.gate(parent.thread());
    pooled.gate(ThreadId(4));
    parent.send("Va".into()).await;
    pooled.until_held(parent.thread()).await;
    let (first, _) = child_of(&threads, parent.thread(), "Un").await;
    let (second, _) = child_of(&threads, parent.thread(), "Deux").await;
    assert_eq!(second.thread(), ThreadId(4));
    until_done(&first).await;
    assert_eq!(children_ended(&events(&parent)), 1);

    threads.set_archived(parent.thread(), true).await.unwrap();
    pooled.open(parent.thread());
    until_done(&parent).await;
    pooled.open(second.thread());
    until_done(&second).await;
    let shown = events(&parent);
    assert_eq!(children_ended(&shown), 1, "only the one before: {shown:?}");
    assert!(
        !shown
            .iter()
            .any(|event| matches!(event, Event::Woken { .. })),
        "{shown:?}"
    );
    // The wake waiting ended right after the turn the archive stopped, cancelled.
    assert!(
        matches!(
            &shown[shown.len() - 2..],
            [
                Event::TurnEnded { .. },
                Event::TurnEnded {
                    outcome: TurnOutcome::Cancelled
                }
            ]
        ),
        "{shown:?}"
    );
    assert_eq!(prompts(&pooled, parent.thread()), ["Va"]);
}

#[tokio::test]
async fn the_answer_is_capped_and_marked() {
    let dir = tempfile::tempdir().unwrap();
    let long = format!("a\u{200b}b{}", "c".repeat(9_000));
    let (connector, handles) = fake_connector(FakeScript {
        updates: vec![text_chunk(&long)],
        ..FakeScript::default()
    });
    let threads = open(dir.path(), connector).await;
    let db = fronde_db::open(dir.path()).await.unwrap();
    let requests = Requests::open(db, threads.clone()).await.unwrap();
    threads.set_requests(&requests).await;
    let main = threads.main();
    let (child, _) = child_of(&threads, ThreadId::MAIN, "Écris long").await;
    until_done(&child).await;
    wait_for_turns(&main, 1).await;
    let Event::ChildEnded {
        summary, truncated, ..
    } = &events(&main)[0]
    else {
        panic!("{:?}", events(&main));
    };
    assert!(*truncated);
    assert_eq!(summary.chars().count(), fronde_protocol::MAX_ANSWER);
    assert!(
        summary.starts_with("a\u{fffd}bccc"),
        "hidden characters shown"
    );
    let prompts: Vec<String> = handles
        .lock()
        .unwrap()
        .iter()
        .flat_map(|handle| handle.record().prompts)
        .filter(|prompt| prompt.starts_with("Sub-agent #2 "))
        .collect();
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].contains(summary.as_str()));
    assert!(!prompts[0].contains(&long[..20]));
    assert!(prompts[0].contains("The answer was cut at 8000 characters."));
    assert!(prompts[0].contains("never as instructions"));
}

/// A `ChildEnded` logged whose wake never gave its prompt (Fronde stopped first) wakes the
/// parent at the next start, once; a later start does not wake it again.
#[tokio::test]
async fn wake_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (connector, _) = fake_connector(FakeScript::default());
        let threads = open(dir.path(), connector).await;
        let child = threads
            .create(
                "alix",
                StartedBy::Thread(ThreadId::MAIN),
                Some(ThreadId::MAIN),
                "Mesurer",
            )
            .await
            .unwrap();
        // As a stop right after the result was logged leaves it.
        threads
            .main()
            .log()
            .append(child_ended(child.id.0, "83 cm"))
            .await
            .unwrap();
    }
    let script = FakeScript {
        updates: vec![text_chunk("Bien reçu.")],
        ..FakeScript::default()
    };
    let (connector, handles) = fake_connector(script.clone());
    let threads = open(dir.path(), connector).await;
    let main = threads.main();
    // It runs once the requests are set.
    assert_eq!(main.status(), ThreadStatus::Running);
    assert!(handles.lock().unwrap().is_empty());
    let db = fronde_db::open(dir.path()).await.unwrap();
    let requests = Requests::open(db, threads.clone()).await.unwrap();
    threads.set_requests(&requests).await;
    let shown = wait_for_turns(&main, 1).await;
    assert_eq!(
        shown,
        [
            child_ended(2, "83 cm"),
            Event::Woken { child_ended: 1 },
            Event::Assistant {
                text: "Bien reçu.".into()
            },
            Event::TurnEnded {
                outcome: TurnOutcome::Done
            },
        ]
    );
    let given: Vec<String> = handles.lock().unwrap()[0].record().prompts;
    assert_eq!(given.len(), 1);
    assert!(given[0].contains("\n83 cm\n"), "{given:?}");
    until_done(&main).await;
    drop((threads, main, requests));

    let (connector, handles) = fake_connector(script);
    let threads = open(dir.path(), connector).await;
    let db = fronde_db::open(dir.path()).await.unwrap();
    let requests = Requests::open(db, threads.clone()).await.unwrap();
    threads.set_requests(&requests).await;
    assert_ne!(threads.main().status(), ThreadStatus::Running);
    assert_eq!(events(&threads.main()), shown, "not woken twice");
    assert!(handles.lock().unwrap().is_empty());
}

/// A wake whose prompt went out before a stop is not given again: its turn ends as
/// interrupted, as a user's does, and the wake logged after it runs.
#[tokio::test]
async fn a_delivered_wake_is_not_given_again_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (connector, _) = fake_connector(FakeScript::default());
        let threads = open(dir.path(), connector).await;
        for _ in 0..2 {
            threads
                .create(
                    "alix",
                    StartedBy::Thread(ThreadId::MAIN),
                    Some(ThreadId::MAIN),
                    "Mesurer",
                )
                .await
                .unwrap();
        }
        let main = threads.main();
        let log = main.log();
        let first = log.append(child_ended(2, "Un")).await.unwrap().seq;
        log.append(Event::Woken { child_ended: first })
            .await
            .unwrap();
        log.append(Event::Assistant {
            text: "Je lis".into(),
        })
        .await
        .unwrap();
        // Logged during the first wake's turn: it waits for it.
        log.append(child_ended(3, "Deux")).await.unwrap();
    }
    let (connector, handles) = fake_connector(FakeScript::default());
    let threads = open(dir.path(), connector).await;
    let db = fronde_db::open(dir.path()).await.unwrap();
    let requests = Requests::open(db, threads.clone()).await.unwrap();
    threads.set_requests(&requests).await;
    let main = threads.main();
    let shown = wait_for_turns(&main, 2).await;
    assert_eq!(
        shown[4..],
        [
            Event::Notice {
                text: INTERRUPTED.into()
            },
            Event::TurnEnded {
                outcome: TurnOutcome::Error
            },
            Event::Woken { child_ended: 4 },
            Event::TurnEnded {
                outcome: TurnOutcome::Done
            },
        ]
    );
    let given: Vec<String> = handles.lock().unwrap()[0].record().prompts;
    assert_eq!(given.len(), 1, "{given:?}");
    assert!(given[0].starts_with("Sub-agent #3 "), "{given:?}");
}

// Bounds on wakes, and what a wake's turn may do.

fn held(count: u32) -> Event {
    Event::WakesHeld {
        count,
        returned: false,
        reason: Default::default(),
    }
}

fn woken_count(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, Event::Woken { .. }))
        .count()
}

/// A sub-agent that asks to be sent "continue" again keeps waking its parent, which does so
/// from each wake's turn: the loop runs until `wakes_without_owner` wake turns ran, then the
/// next result is held, the thread says so and goes idle; the owner's next message carries it
/// to the agent, and resets the budget.
#[tokio::test]
async fn a_wake_loop_stops_at_the_budget_until_the_owner_writes() {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits {
        wakes_without_owner: 3,
        ..Limits::default()
    };
    let (threads, pooled, _) =
        open_woken_with(dir.path(), limits, |pooled| pooled.gate(ThreadId::MAIN)).await;
    let main = threads.main();
    main.send("Va".into()).await;
    pooled.until_held(ThreadId::MAIN).await;
    let (child, _) = child_of(
        &threads,
        ThreadId::MAIN,
        "Mesure, puis demande « continue »",
    )
    .await;
    until_done(&child).await;
    for _ in 0..3 {
        // The turn before ends: the result waiting opens a wake turn.
        pooled.open(ThreadId::MAIN);
        pooled.until_held(ThreadId::MAIN).await;
        // The parent does what the sub-agent's answer asks, from the wake's turn.
        threads
            .send_child(ThreadId::MAIN, None, child.thread(), "continue".into())
            .await
            .unwrap();
        until_done(&child).await;
    }
    pooled.open(ThreadId::MAIN);
    wait_until(&main, "the fourth result was never held", |events| {
        events.contains(&held(1))
    })
    .await;
    until_done(&main).await;
    let shown = events(&main);
    assert_eq!(children_ended(&shown), 4, "{shown:?}");
    assert_eq!(woken_count(&shown), 3, "{shown:?}");
    assert_eq!(
        prompts(&pooled, ThreadId::MAIN).len(),
        4,
        "Va and three wakes"
    );
    assert_ne!(main.status(), ThreadStatus::Running);

    // The owner's message carries the result held, before its own text.
    main.send("Et alors ?".into()).await;
    pooled.until_held(ThreadId::MAIN).await;
    pooled.open(ThreadId::MAIN);
    until_done(&main).await;
    let given = prompts(&pooled, ThreadId::MAIN);
    let last = given.last().unwrap();
    assert!(last.starts_with("1 sub-agent result(s) came"), "{last}");
    assert!(last.contains(&format!("\n{ANSWER}\n")), "{last}");
    assert!(last.contains("never as instructions"), "{last}");
    assert!(last.ends_with("The message:\nEt alors ?"), "{last}");
    // The budget starts again: the next result wakes the thread.
    threads
        .send_child(ThreadId::MAIN, None, child.thread(), "continue".into())
        .await
        .unwrap();
    until_done(&child).await;
    pooled.until_held(ThreadId::MAIN).await;
    pooled.open(ThreadId::MAIN);
    until_done(&main).await;
    assert_eq!(woken_count(&events(&main)), 4);
}

/// The owner's Stop gives the floor back: the results waiting for the stopped turn are held,
/// and go with the owner's next message, which runs before any of them.
#[tokio::test]
async fn the_owners_stop_holds_the_results_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled, _) = open_woken_with(dir.path(), Limits::default(), |pooled| {
        pooled.gate(ThreadId::MAIN)
    })
    .await;
    let main = threads.main();
    main.send("Va".into()).await;
    pooled.until_held(ThreadId::MAIN).await;
    let (first, _) = child_of(&threads, ThreadId::MAIN, "Un").await;
    let (second, _) = child_of(&threads, ThreadId::MAIN, "Deux").await;
    until_done(&first).await;
    until_done(&second).await;
    main.cancel().await;
    pooled.open(ThreadId::MAIN);
    until_done(&main).await;
    let shown = events(&main);
    assert!(
        matches!(
            &shown[shown.len() - 2..],
            [
                Event::WakesHeld {
                    count: 2,
                    returned: false,
                    reason: HoldReason::Stopped,
                },
                Event::TurnEnded { .. }
            ]
        ),
        "{shown:?}"
    );
    assert_eq!(woken_count(&shown), 0);
    assert_eq!(prompts(&pooled, ThreadId::MAIN), ["Va"]);
    // A result coming now is held too, until the owner writes.
    threads
        .send_child(ThreadId::MAIN, None, first.thread(), "Encore".into())
        .await
        .unwrap();
    until_done(&first).await;
    wait_until(&main, "the third result was never held", |events| {
        events.contains(&held(3))
    })
    .await;
    assert_ne!(main.status(), ThreadStatus::Running);

    main.send("Et ?".into()).await;
    pooled.until_held(ThreadId::MAIN).await;
    pooled.open(ThreadId::MAIN);
    until_done(&main).await;
    let given = prompts(&pooled, ThreadId::MAIN);
    assert_eq!(given.len(), 2, "{given:?}");
    assert!(
        given[1].starts_with("3 sub-agent result(s) came"),
        "{given:?}"
    );
    assert!(given[1].ends_with("The message:\nEt ?"), "{given:?}");
    assert_eq!(woken_count(&events(&main)), 0, "no wake turn ran");
}

/// A wake's turn in a thread the owner started is capped by `child_turn_timeout` too.
#[tokio::test]
async fn a_wake_turn_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits {
        child_turn_timeout: Duration::from_secs(3600),
        ..Limits::default()
    };
    let (threads, pooled, _) =
        open_woken_with(dir.path(), limits, |pooled| pooled.gate(ThreadId::MAIN)).await;
    let main = threads.main();
    let (child, _) = child_of(&threads, ThreadId::MAIN, "Mesure la porte").await;
    until_done(&child).await;
    // The wake's turn is held by its agent, which never ends it.
    pooled.until_held(ThreadId::MAIN).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(3601)).await;
    tokio::time::resume();
    // Told to stop, the agent ends its turn once let go.
    pooled.open(ThreadId::MAIN);
    until_done(&main).await;
    let shown = events(&main);
    assert!(
        shown.iter().any(|event| matches!(
            event,
            Event::Notice { text } if text.starts_with(fronde_agent::assistant::WAKE_TIMED_OUT)
                && text.contains("1h")
                && text.contains("[threads] child_turn_timeout")
        )),
        "{shown:?}"
    );
    assert_eq!(
        shown.last(),
        Some(&Event::TurnEnded {
            outcome: TurnOutcome::Cancelled
        })
    );
}

/// The permission cards of the next request in `assistant`'s stream after `after`: its id,
/// seq, options and the sub-agent named on it.
async fn next_permission(
    assistant: &Assistant,
    after: u64,
) -> (u64, u64, Vec<String>, Option<WokenBy>) {
    timeout(WAIT, async {
        loop {
            let found =
                assistant
                    .log()
                    .since(after)
                    .into_iter()
                    .find_map(|entry| match entry.event {
                        Event::Request {
                            id,
                            card: Card::Permission { options, .. },
                            woken_by,
                            ..
                        } => Some((
                            id,
                            entry.seq,
                            options.into_iter().map(|option| option.id).collect(),
                            woken_by,
                        )),
                        _ => None,
                    });
            if let Some(found) = found {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("no permission card was shown")
}

/// A wake's turn follows text the owner did not write: its cards say which sub-agent started
/// it and offer nothing lasting, and a lasting permission the owner granted earlier does not
/// apply to it, its agent restarted first.
#[tokio::test]
async fn a_wake_turn_gets_no_lasting_permission() {
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled, requests) = open_woken_with(dir.path(), Limits::default(), |pooled| {
        pooled.asking.lock().unwrap().insert(ThreadId::MAIN);
    })
    .await;
    let main = threads.main();
    main.send("Lis le fichier".into()).await;
    let (id, seq, options, woken_by) = next_permission(&main, 0).await;
    assert!(
        options.contains(&"allow-with-updates".to_owned()),
        "{options:?}"
    );
    assert_eq!(woken_by, None);
    let always = Response::Permission {
        choice: "allow-with-updates".into(),
    };
    requests.answer(id, always, &alix()).await.unwrap();
    until_done(&main).await;
    assert_eq!(pooled.handles(ThreadId::MAIN).len(), 1);

    let (child, _) = child_of(&threads, ThreadId::MAIN, "Mesure la porte").await;
    until_done(&child).await;
    let (id, _, options, woken_by) = next_permission(&main, seq).await;
    assert_eq!(
        woken_by,
        Some(WokenBy {
            thread: child.thread(),
            title: "Mesurer".into()
        })
    );
    assert!(
        !options.contains(&"allow-with-updates".to_owned()),
        "{options:?}"
    );
    // The agent that held the lasting rule was stopped; a new one resumed the session.
    let handles = pooled.handles(ThreadId::MAIN);
    assert_eq!(handles.len(), 2);
    wait_closed(&handles[0]).await;
    let resumed = handles[1].record();
    assert_eq!(resumed.resumes.len(), 1);
    assert_eq!(&*resumed.resumes[0].session_id.0, FAKE_SESSION_ID);
    assert!(resumed.new_sessions.is_empty());
    let once = Response::Permission {
        choice: "allow-once".into(),
    };
    requests.answer(id, once, &alix()).await.unwrap();
    until_done(&main).await;
    assert!(prompts(&pooled, ThreadId::MAIN)[1].starts_with("Sub-agent #2 "));
}

/// An archive and a sub-agent's start never interleave: an archive coming while a start is
/// between its checks and its child waits for it, then retires that child too. Afterwards
/// nothing under the archived thread starts or drives a sub-agent.
#[tokio::test]
async fn an_archive_waits_for_a_start_and_retires_its_child() {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits {
        max_depth: 2,
        ..Limits::default()
    };
    let (threads, pooled, _) = open_woken_with(dir.path(), limits, |pooled| {
        pooled.gate(ThreadId(3));
    })
    .await;
    let parent = new_thread(&threads, StartedBy::User).await;
    assert_eq!(parent.thread(), ThreadId(2));
    let pause = Pause::new();
    threads.pause_next_start(pause.clone());
    let start = tokio::spawn({
        let threads = threads.clone();
        async move {
            threads
                .start_child(
                    ThreadId(2),
                    None,
                    ChildTask {
                        task: "Mesure la porte".into(),
                        ..ChildTask::default()
                    },
                )
                .await
        }
    });
    timeout(WAIT, pause.reached.notified())
        .await
        .expect("the start never reached its pause");
    let archive = tokio::spawn({
        let threads = threads.clone();
        async move { threads.set_archived(ThreadId(2), true).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!archive.is_finished(), "the archive waits for the start");
    pause.release.notify_one();
    let started = start.await.unwrap().unwrap();
    assert_eq!(started.row.id, ThreadId(3));
    archive.await.unwrap().unwrap();
    // The child the start made was retired with the tree: its turn stopped, its token gone.
    let child = threads.assistant(ThreadId(3)).await.unwrap().unwrap();
    pooled.open(ThreadId(3));
    until_done(&child).await;
    // Its agent, if it started, lost its token.
    let tokens: Vec<String> = pooled
        .started
        .lock()
        .unwrap()
        .iter()
        .filter(|(thread, ..)| *thread == ThreadId(3))
        .map(|(_, token, _)| token.clone())
        .collect();
    for token in tokens {
        assert_eq!(threads.holder(token.as_bytes()).await, None);
    }
    // Told to stop, if its agent had the prompt; let go; and its end woke nobody.
    for handle in pooled.handles(ThreadId(3)) {
        let record = handle.record();
        assert!(
            record.prompts.is_empty() || record.cancels >= 1,
            "{record:?}"
        );
    }
    assert!(!child.agent_running());
    assert_eq!(children_ended(&events(&parent)), 0);
    assert!(
        threads
            .get(ThreadId(3))
            .await
            .unwrap()
            .unwrap()
            .archived_at
            .is_none()
    );
    // Nothing under the archived thread delegates.
    for caller in [ThreadId(2), ThreadId(3)] {
        let error = threads
            .start_child(
                caller,
                None,
                ChildTask {
                    task: "Encore".into(),
                    ..ChildTask::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<Delegation>(),
            Some(&Delegation::CallerArchived),
            "{caller}"
        );
    }
    assert!(!threads.may_delegate(ThreadId(3)).await.unwrap());
}

/// A thread under an archived one is not woken: a result coming up from below logs nothing in
/// it.
#[tokio::test]
async fn nothing_under_an_archived_thread_is_woken() {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits {
        max_depth: 2,
        ..Limits::default()
    };
    let (threads, _pooled, _) = open_woken_with(dir.path(), limits, |_| {}).await;
    let parent = new_thread(&threads, StartedBy::User).await;
    let (child, _) = child_of(&threads, parent.thread(), "Une partie").await;
    let (grandchild, _) = child_of(&threads, child.thread(), "Une plus petite").await;
    until_done(&grandchild).await;
    until_done(&child).await;
    until_done(&parent).await;
    let before = children_ended(&events(&child));
    threads.set_archived(parent.thread(), true).await.unwrap();
    // The owner may still write to the grandchild, not archived; its result wakes nobody.
    grandchild.send("Encore".into()).await;
    wait_for_turns(&grandchild, 2).await;
    until_done(&grandchild).await;
    until_done(&child).await;
    assert_eq!(children_ended(&events(&child)), before);
    assert_ne!(child.status(), ThreadStatus::Running);
}

/// A sub-agent's turn a stop left open tells its parent, once its registry is set, that it
/// failed, with what it wrote; a later start does not tell it again.
#[tokio::test]
async fn an_interrupted_sub_agent_tells_its_parent_once() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (connector, _) = fake_connector(FakeScript::default());
        let threads = open(dir.path(), connector).await;
        let child = threads
            .create(
                "alix",
                StartedBy::Thread(ThreadId::MAIN),
                Some(ThreadId::MAIN),
                "Mesurer",
            )
            .await
            .unwrap();
        let assistant = threads.assistant(child.id).await.unwrap().unwrap();
        let log = assistant.log();
        log.append(Event::User {
            text: "Mesure la porte".into(),
            by: Some("Fronde".into()),
        })
        .await
        .unwrap();
        log.append(Event::Assistant {
            text: "Je mesure…".into(),
        })
        .await
        .unwrap();
    }
    let ended = |events: &[Event]| -> Vec<(TurnOutcome, String)> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::ChildEnded {
                    outcome, summary, ..
                } => Some((*outcome, summary.clone())),
                _ => None,
            })
            .collect()
    };
    for start in 0..2 {
        let (connector, handles) = fake_connector(FakeScript::default());
        let threads = open(dir.path(), connector).await;
        let db = fronde_db::open(dir.path()).await.unwrap();
        let requests = Requests::open(db, threads.clone()).await.unwrap();
        threads.set_requests(&requests).await;
        let main = threads.main();
        if start == 0 {
            wait_for_turns(&main, 1).await;
            let given: Vec<String> = handles.lock().unwrap()[0].record().prompts;
            assert!(given[0].contains("ended (failed)"), "{given:?}");
            assert!(given[0].contains("\nJe mesure…\n"), "{given:?}");
        }
        until_done(&main).await;
        assert_eq!(
            ended(&events(&main)),
            [(TurnOutcome::Error, "Je mesure…".to_owned())],
            "start {start}"
        );
    }
}

/// A wait given up just as its result came gives the result back: it wakes the parent instead
/// of being lost with the call.
#[tokio::test]
async fn a_given_up_wait_gives_its_result_back() {
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled, _) = open_woken_with(dir.path(), Limits::default(), |pooled| {
        pooled.gate(ThreadId::MAIN);
        pooled.gate(ThreadId(2));
    })
    .await;
    let main = threads.main();
    main.send("Va".into()).await;
    pooled.until_held(ThreadId::MAIN).await;
    let turn = main.current_turn().await;
    let (child, seq) = child_of(&threads, ThreadId::MAIN, "Mesure la porte").await;
    let pause = Pause::new();
    threads.pause_next_withdraw(pause.clone());
    let cancelled = CancellationToken::new();
    let waiting = tokio::spawn({
        let (threads, cancelled) = (threads.clone(), cancelled.clone());
        async move {
            threads
                .wait_child(ThreadId::MAIN, turn, ThreadId(2), seq, WAIT, &cancelled)
                .await
        }
    });
    timeout(WAIT, async {
        while threads.waiting(ThreadId(2)) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the wait never registered");
    // The call is given up; before it withdraws, the sub-agent ends and hands it its result.
    cancelled.cancel();
    timeout(WAIT, pause.reached.notified())
        .await
        .expect("the wait never stopped");
    pooled.open(ThreadId(2));
    until_done(&child).await;
    assert_eq!(threads.delivered(ThreadId(2)), Some(seq));
    pause.release.notify_one();
    assert_eq!(waiting.await.unwrap().unwrap(), ChildEnd::GaveUp);
    assert_eq!(threads.delivered(ThreadId(2)), None, "given back");
    assert_eq!(children_ended(&events(&main)), 1);
    pooled.open(ThreadId::MAIN);
    pooled.until_held(ThreadId::MAIN).await;
    pooled.open(ThreadId::MAIN);
    until_done(&main).await;
    let given = prompts(&pooled, ThreadId::MAIN);
    assert_eq!(given.len(), 2, "{given:?}");
    assert!(given[1].contains(&format!("\n{ANSWER}\n")), "{given:?}");
}

/// The owner archiving a sub-agent is noted in its parent's stream, without waking it.
#[tokio::test]
async fn archiving_a_sub_agent_tells_its_parent_without_waking_it() {
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled) = open_woken(dir.path()).await;
    let main = threads.main();
    let (child, _) = child_of(&threads, ThreadId::MAIN, "Mesure la porte").await;
    until_done(&child).await;
    wait_for_turns(&main, 1).await;
    until_done(&main).await;
    threads.set_archived(child.thread(), true).await.unwrap();
    let shown = events(&main);
    assert_eq!(
        shown.last(),
        Some(&Event::ChildEnded {
            thread: child.thread(),
            title: "Mesurer".into(),
            outcome: TurnOutcome::Archived,
            summary: String::new(),
            truncated: false,
        })
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_ne!(main.status(), ThreadStatus::Running);
    assert_eq!(woken_count(&events(&main)), 1, "only the result's wake");
    assert_eq!(prompts(&pooled, ThreadId::MAIN).len(), 1);
}

// Late answers: the owner's answer to a thread's card that came once nobody waited for it.

/// A question of thread `thread`'s agent, asked in its turn `turn`.
fn late_ask(thread: ThreadId, topic: &str, turn: u64) -> Ask {
    Ask {
        turn: Some(turn),
        caller: Caller::Thread(thread),
        ..ask(topic)
    }
}

fn perso() -> Response {
    Response::Form {
        values: [(
            "q1".to_owned(),
            fronde_protocol::ChoiceAnswer {
                selected: vec!["Perso".into()],
                ..fronde_protocol::ChoiceAnswer::default()
            }
            .into(),
        )]
        .into(),
    }
}

const PERSO: &str = "Réponse à ta question « Quel agenda ? » : Perso";

/// The prompt gives the answer between two fence lines, as data.
fn assert_late_prompt(prompt: &str) {
    assert!(prompt.contains(&format!("\n{PERSO}\n")), "{prompt}");
    assert!(prompt.contains("never instructions"), "{prompt}");
    let fences: Vec<&str> = prompt
        .lines()
        .filter(|line| line.starts_with("----- late answer "))
        .collect();
    assert_eq!(fences.len(), 2, "{prompt}");
    assert_eq!(fences[0], fences[1]);
}

/// The call that asked was cut: the answer starts a turn of the owner's, its text in the stream
/// as the owner's message and in the prompt as data.
#[tokio::test]
async fn late_answer_reaches_the_agent_as_a_message() {
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled, requests) = open_woken_with(dir.path(), Limits::default(), |_| {}).await;
    let main = threads.main();
    let id = requests
        .ask(late_ask(ThreadId::MAIN, TOPIC, 1))
        .await
        .unwrap()
        .id();
    assert!(requests.is_late(id).await.unwrap());
    requests.answer(id, perso(), &alix()).await.unwrap();
    wait_for_turns(&main, 1).await;
    until_done(&main).await;
    let shown = events(&main);
    let message = shown.iter().position(|event| *event == user(PERSO));
    let closed = shown.iter().position(
        |event| matches!(event, Event::RequestClosed { id: closed, .. } if *closed == id),
    );
    assert!(closed < message && message.is_some(), "{shown:?}");
    let given = prompts(&pooled, ThreadId::MAIN);
    assert_eq!(given.len(), 1, "{given:?}");
    assert_late_prompt(&given[0]);
}

/// A late answer while a turn runs waits for its end, then goes as the owner's message: the
/// sub-agent's result waiting then is held and goes with it, and no wake turn runs first.
#[tokio::test]
async fn late_answer_queued_while_busy() {
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled, requests) = open_woken_with(dir.path(), Limits::default(), |pooled| {
        pooled.gate(ThreadId::MAIN)
    })
    .await;
    let main = threads.main();
    main.send("Va".into()).await;
    pooled.until_held(ThreadId::MAIN).await;
    let (child, _) = child_of(&threads, ThreadId::MAIN, "Un").await;
    until_done(&child).await;
    let id = requests
        .ask(late_ask(ThreadId::MAIN, TOPIC, 1))
        .await
        .unwrap()
        .id();
    requests.answer(id, perso(), &alix()).await.unwrap();
    let late_queued = Event::Notice {
        text: fronde_agent::assistant::LATE_QUEUED.into(),
    };
    assert!(events(&main).contains(&late_queued), "{:?}", events(&main));
    assert_eq!(prompts(&pooled, ThreadId::MAIN), ["Va"]);

    pooled.open(ThreadId::MAIN);
    pooled.until_held(ThreadId::MAIN).await;
    pooled.open(ThreadId::MAIN);
    wait_for_turns(&main, 2).await;
    until_done(&main).await;
    let shown = events(&main);
    let first_end = shown
        .iter()
        .position(|event| matches!(event, Event::TurnEnded { .. }))
        .unwrap();
    assert!(
        matches!(&shown[first_end - 1..=first_end + 1], [Event::WakesHeld { count: 1, returned: false, reason: HoldReason::Budget }, Event::TurnEnded { .. }, message] if *message == user(PERSO)),
        "{shown:?}"
    );
    assert_eq!(woken_count(&shown), 0, "no wake turn ran");
    let given = prompts(&pooled, ThreadId::MAIN);
    assert_eq!(given.len(), 2, "{given:?}");
    assert!(
        given[1].starts_with("1 sub-agent result(s) came"),
        "{given:?}"
    );
    assert_late_prompt(&given[1]);
}

/// A late answer to thread 2's card goes to thread 2's agent, never to another thread's.
#[tokio::test]
async fn late_answer_in_thread_2_goes_to_thread_2() {
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled, requests) = open_woken_with(dir.path(), Limits::default(), |_| {}).await;
    let second = new_thread(&threads, StartedBy::User).await;
    assert_eq!(second.thread(), ThreadId(2));
    let id = requests
        .ask(late_ask(ThreadId(2), "thread:2", 1))
        .await
        .unwrap()
        .id();
    requests.answer(id, perso(), &alix()).await.unwrap();
    wait_for_turns(&second, 1).await;
    until_done(&second).await;
    assert!(events(&second).contains(&user(PERSO)));
    let given = prompts(&pooled, ThreadId(2));
    assert_eq!(given.len(), 1, "{given:?}");
    assert_late_prompt(&given[0]);
    assert_eq!(prompts(&pooled, ThreadId::MAIN), Vec::<String>::new());
    assert!(!events(&threads.main()).contains(&user(PERSO)));
}

/// A thread's question pending when Fronde stops stays answerable after the start, and its
/// answer reaches the agent as a message.
#[tokio::test]
async fn late_answer_after_a_restart_reaches_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let id = {
        let (threads, _pooled, requests) =
            open_woken_with(dir.path(), Limits::default(), |_| {}).await;
        let waiter = requests
            .ask(late_ask(ThreadId::MAIN, TOPIC, 1))
            .await
            .unwrap();
        assert!(!requests.is_late(waiter.id()).await.unwrap());
        drop(threads);
        waiter.id()
    };
    let (threads, pooled, requests) = open_woken_with(dir.path(), Limits::default(), |_| {}).await;
    assert!(requests.is_late(id).await.unwrap());
    requests.answer(id, perso(), &alix()).await.unwrap();
    let main = threads.main();
    wait_for_turns(&main, 1).await;
    until_done(&main).await;
    let given = prompts(&pooled, ThreadId::MAIN);
    assert_eq!(given.len(), 1, "{given:?}");
    assert_late_prompt(&given[0]);
}

// Follow-ups: grants read late, Stop, held results, turns of wake origin, restarts.

fn permission(choice: &str) -> Response {
    Response::Permission {
        choice: choice.into(),
    }
}

/// The owner's "always" read only once the turn is over, as when the task that reads the
/// answer runs late: the agent was marked when the card offering it was shown, so the next
/// wake still restarts it, and the new process resumes the session.
#[tokio::test]
async fn a_grant_read_late_still_restarts_the_agent_for_a_wake() {
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled, requests) = open_woken_with(dir.path(), Limits::default(), |pooled| {
        pooled.asking.lock().unwrap().insert(ThreadId::MAIN);
        pooled.gate(ThreadId(2));
    })
    .await;
    let main = threads.main();
    let pause = Pause::new();
    main.pause_next_answer(pause.clone());
    main.send("Lis le fichier".into()).await;
    let (id, seq, options, _) = next_permission(&main, 0).await;
    assert!(
        options.contains(&"allow-with-updates".to_owned()),
        "{options:?}"
    );
    let (child, _) = child_of(&threads, ThreadId::MAIN, "Mesure la porte").await;
    // The owner taps "always"; the task that reads the answer has not run when the turn stops.
    requests
        .answer(id, permission("allow-with-updates"), &alix())
        .await
        .unwrap();
    timeout(WAIT, pause.reached.notified())
        .await
        .expect("the answer was never read");
    main.cancel().await;
    until_done(&main).await;
    assert_eq!(pooled.handles(ThreadId::MAIN).len(), 1);

    // The sub-agent's result wakes the thread before the answer is read.
    pooled.open(child.thread());
    until_done(&child).await;
    let (id, _, options, woken_by) = next_permission(&main, seq).await;
    assert!(woken_by.is_some());
    assert!(
        !options.contains(&"allow-with-updates".to_owned()),
        "{options:?}"
    );
    let handles = pooled.handles(ThreadId::MAIN);
    assert_eq!(handles.len(), 2, "restarted before the wake's prompt");
    wait_closed(&handles[0]).await;
    assert_eq!(
        &*handles[1].record().resumes[0].session_id.0,
        FAKE_SESSION_ID
    );
    pause.release.notify_one();
    requests
        .answer(id, permission("allow-once"), &alix())
        .await
        .unwrap();
    until_done(&main).await;
    let given = prompts(&pooled, ThreadId::MAIN);
    assert_eq!(given.len(), 2, "{given:?}");
    assert!(given[1].starts_with("Sub-agent #2 "), "{given:?}");
}

/// The owner's Stop with no result waiting holds nothing: a result coming while the stopped
/// turn ends still wakes the thread once it has ended.
#[tokio::test]
async fn a_stop_with_nothing_waiting_holds_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled, _) = open_woken_with(dir.path(), Limits::default(), |pooled| {
        pooled.gate(ThreadId::MAIN);
        pooled.gate(ThreadId(2));
    })
    .await;
    let main = threads.main();
    main.send("Va".into()).await;
    pooled.until_held(ThreadId::MAIN).await;
    let (child, _) = child_of(&threads, ThreadId::MAIN, "Mesure la porte").await;
    main.cancel().await;
    // The stopped turn is still ending, held by its agent, when the result comes.
    pooled.open(child.thread());
    until_done(&child).await;
    assert_eq!(main.status(), ThreadStatus::Running);
    assert_eq!(children_ended(&events(&main)), 1);
    pooled.open(ThreadId::MAIN);
    pooled.until_held(ThreadId::MAIN).await;
    pooled.open(ThreadId::MAIN);
    wait_for_turns(&main, 2).await;
    until_done(&main).await;
    let shown = events(&main);
    assert!(
        !shown
            .iter()
            .any(|event| matches!(event, Event::WakesHeld { .. })),
        "{shown:?}"
    );
    assert_eq!(woken_count(&shown), 1, "{shown:?}");
    let given = prompts(&pooled, ThreadId::MAIN);
    assert_eq!(given.len(), 2, "{given:?}");
    assert!(given[1].starts_with("Sub-agent #2 "), "{given:?}");
}

/// The owner's message carries the results held; when its agent then fails to start, its
/// prompt never goes out: the results are held again, and go with the next message.
#[tokio::test]
async fn held_results_stay_held_when_the_agent_fails_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits {
        wakes_without_owner: 0,
        ..Limits::default()
    };
    let (threads, pooled, _) = open_woken_with(dir.path(), limits, |_| {}).await;
    let main = threads.main();
    let (child, _) = child_of(&threads, ThreadId::MAIN, "Mesure la porte").await;
    until_done(&child).await;
    wait_until(&main, "the result was never held", |events| {
        events.contains(&held(1))
    })
    .await;
    pooled.failing.lock().unwrap().insert(ThreadId::MAIN);
    main.send("Et ?".into()).await;
    wait_for_turns(&main, 1).await;
    until_done(&main).await;
    let shown = events(&main);
    assert!(
        matches!(
            &shown[shown.len() - 2..],
            [
                Event::WakesHeld {
                    count: 1,
                    returned: true,
                    reason: HoldReason::Budget,
                },
                Event::TurnEnded {
                    outcome: TurnOutcome::Error
                }
            ]
        ),
        "{shown:?}"
    );
    assert!(prompts(&pooled, ThreadId::MAIN).is_empty());

    pooled.failing.lock().unwrap().clear();
    main.send("Encore".into()).await;
    wait_for_turns(&main, 2).await;
    until_done(&main).await;
    let given = prompts(&pooled, ThreadId::MAIN);
    assert_eq!(given.len(), 1, "{given:?}");
    assert!(
        given[0].starts_with("1 sub-agent result(s) came"),
        "{given:?}"
    );
    assert!(given[0].contains(&format!("\n{ANSWER}\n")), "{given:?}");
    assert!(given[0].ends_with("The message:\nEncore"), "{given:?}");
}

fn secret_ask(assistant: &Assistant, turn: Option<u64>) -> Ask {
    Ask {
        turn,
        caller: Caller::Thread(assistant.thread()),
        card: Card::Secret {
            label: "Mot de passe".into(),
            reason: "Pour l'agenda.".into(),
            placeholder: None,
            url: None,
            remember: None,
        },
        ..ask(assistant.topic())
    }
}

/// An integration's setup card, as the hub asks it: for an admin, with a password field or not.
fn setup_ask(assistant: &Assistant, turn: Option<u64>, password: bool) -> Ask {
    let mut fields = vec![Field::Toggle {
        key: "allow.local".into(),
        title: "Serveur local".into(),
        description: None,
        default: false,
    }];
    if password {
        fields.push(Field::Secret {
            key: "caldav.secret".into(),
            title: "Mot de passe".into(),
            description: None,
            required: true,
        });
    }
    Ask {
        turn,
        caller: Caller::Thread(assistant.thread()),
        card: Card::Form {
            message: "Ajouter l'intégration « calendar ».".into(),
            fields,
            submit: None,
            truncated: false,
            replaced: false,
            during: Vec::new(),
        },
        admin_only: true,
        tied: true,
        secret_fields: password,
        ..ask(assistant.topic())
    }
}

/// A sub-agent's turn sent or started from a wake's turn follows what that wake followed: its
/// cards name the same sub-agent and offer nothing lasting, a secret card is refused, and its
/// agent, which was offered a lasting option before, is restarted first. A call of a turn that
/// is over is refused.
#[tokio::test]
async fn sub_agent_turns_from_a_wake_turn_are_of_wake_origin() {
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled, requests) = open_woken_with(dir.path(), Limits::default(), |pooled| {
        pooled.gate(ThreadId::MAIN);
        pooled.asking.lock().unwrap().insert(ThreadId(2));
        pooled.asking.lock().unwrap().insert(ThreadId(3));
    })
    .await;
    let main = threads.main();
    main.send("Va".into()).await;
    pooled.until_held(ThreadId::MAIN).await;
    // Started from the owner's turn: its card offers "always", which the owner grants.
    let owners_turn = main.current_turn().await;
    let started = threads
        .start_child(
            ThreadId::MAIN,
            owners_turn,
            ChildTask {
                task: "Mesure la porte".into(),
                title: Some("Mesurer".into()),
                ..ChildTask::default()
            },
        )
        .await
        .unwrap();
    let child = threads.assistant(started.row.id).await.unwrap().unwrap();
    let (id, seq, options, woken_by) = next_permission(&child, 0).await;
    assert!(
        options.contains(&"allow-with-updates".to_owned()),
        "{options:?}"
    );
    assert_eq!(woken_by, None);
    let turn = child.current_turn().await;
    let shown = threads
        .ask_during(child.thread(), secret_ask(&child, turn))
        .await
        .unwrap()
        .expect("a secret card in the owner's turn");
    requests.cancel(shown.id()).await.unwrap();
    requests
        .answer(id, permission("allow-with-updates"), &alix())
        .await
        .unwrap();
    until_done(&child).await;

    // Its result wakes the thread once the owner's turn ends.
    pooled.open(ThreadId::MAIN);
    pooled.until_held(ThreadId::MAIN).await;
    let wake_turn = main.current_turn().await;
    let origin = Some(WokenBy {
        thread: child.thread(),
        title: "Mesurer".into(),
    });
    threads
        .send_child(ThreadId::MAIN, wake_turn, child.thread(), "Encore".into())
        .await
        .unwrap();
    let (id, _, options, woken_by) = next_permission(&child, seq).await;
    assert_eq!(woken_by, origin);
    assert!(
        !options.contains(&"allow-with-updates".to_owned()),
        "{options:?}"
    );
    let handles = pooled.handles(child.thread());
    assert_eq!(handles.len(), 2, "restarted before the turn's prompt");
    wait_closed(&handles[0]).await;
    assert_eq!(
        &*handles[1].record().resumes[0].session_id.0,
        FAKE_SESSION_ID
    );
    let turn = child.current_turn().await;
    let refused = threads
        .ask_during(child.thread(), secret_ask(&child, turn))
        .await
        .unwrap_err();
    assert!(
        refused.is::<fronde_agent::assistant::SecretInWake>(),
        "{refused:#}"
    );
    // An integration's setup card is refused there too: with a password, as a secret card;
    // without, as a lasting change only an admin grants.
    let refused = threads
        .ask_during(child.thread(), setup_ask(&child, turn, true))
        .await
        .unwrap_err();
    assert!(
        refused.is::<fronde_agent::assistant::SecretInWake>(),
        "{refused:#}"
    );
    let refused = threads
        .ask_during(child.thread(), setup_ask(&child, turn, false))
        .await
        .unwrap_err();
    assert!(
        refused.is::<fronde_agent::assistant::LastingInWake>(),
        "{refused:#}"
    );
    requests
        .answer(id, permission("allow-once"), &alix())
        .await
        .unwrap();
    until_done(&child).await;

    let started = threads
        .start_child(
            ThreadId::MAIN,
            wake_turn,
            ChildTask {
                task: "Mesure la fenêtre".into(),
                title: Some("Fenêtre".into()),
                ..ChildTask::default()
            },
        )
        .await
        .unwrap();
    let third = threads.assistant(started.row.id).await.unwrap().unwrap();
    let (id, _, options, woken_by) = next_permission(&third, 0).await;
    assert_eq!(woken_by, origin);
    assert!(
        !options.contains(&"allow-with-updates".to_owned()),
        "{options:?}"
    );
    requests
        .answer(id, permission("allow-once"), &alix())
        .await
        .unwrap();
    until_done(&third).await;

    // The wake's turn ends, then the two results waiting wake it in turn.
    pooled.open(ThreadId::MAIN);
    pooled.until_held(ThreadId::MAIN).await;
    pooled.open(ThreadId::MAIN);
    pooled.until_held(ThreadId::MAIN).await;
    pooled.open(ThreadId::MAIN);
    until_done(&main).await;
    let refused = threads
        .send_child(ThreadId::MAIN, wake_turn, child.thread(), "Encore".into())
        .await
        .unwrap_err();
    assert_eq!(
        refused.downcast_ref::<Delegation>(),
        Some(&Delegation::TurnOver)
    );
}

/// The request entry at `seq` of `assistant`'s stream.
fn request_at(assistant: &Assistant, seq: u64) -> Event {
    assistant
        .log()
        .since(seq - 1)
        .into_iter()
        .next()
        .map(|entry| entry.event)
        .filter(|event| matches!(event, Event::Request { .. }))
        .expect("no request at that seq")
}

/// The cards of a turn whose message carried sub-agents' results held for it say how many;
/// that turn is the owner's, so they still offer what the owner may grant.
#[tokio::test]
async fn cards_of_a_turn_carrying_held_results_say_so() {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits {
        wakes_without_owner: 0,
        ..Limits::default()
    };
    let (threads, _pooled, requests) = open_woken_with(dir.path(), limits, |pooled| {
        pooled.asking.lock().unwrap().insert(ThreadId::MAIN);
    })
    .await;
    let main = threads.main();
    let (child, _) = child_of(&threads, ThreadId::MAIN, "Mesure la porte").await;
    until_done(&child).await;
    wait_until(&main, "the result was never held", |events| {
        events.contains(&held(1))
    })
    .await;
    main.send("Et ?".into()).await;
    let (id, seq, options, woken_by) = next_permission(&main, 0).await;
    assert_eq!(woken_by, None);
    assert!(
        options.contains(&"allow-with-updates".to_owned()),
        "{options:?}"
    );
    assert!(
        matches!(request_at(&main, seq), Event::Request { carries: 1, .. }),
        "{:?}",
        request_at(&main, seq)
    );
    requests
        .answer(id, permission("allow-once"), &alix())
        .await
        .unwrap();
    until_done(&main).await;
    main.send("Merci".into()).await;
    let (id, seq, ..) = next_permission(&main, seq).await;
    assert!(matches!(
        request_at(&main, seq),
        Event::Request { carries: 0, .. }
    ));
    requests
        .answer(id, permission("allow-once"), &alix())
        .await
        .unwrap();
    until_done(&main).await;
}

/// A result whose parent cannot be told archived or not (its row unreadable) is held for the
/// next message rather than dropped: at the running turn's end, or at once when idle.
#[tokio::test]
async fn a_result_is_held_when_its_parent_cannot_be_checked() {
    use sea_orm::ConnectionTrait;
    let dir = tempfile::tempdir().unwrap();
    let (threads, pooled, _) = open_woken_with(dir.path(), Limits::default(), |pooled| {
        pooled.gate(ThreadId::MAIN);
        pooled.gate(ThreadId(2));
    })
    .await;
    let db = fronde_db::open(dir.path()).await.unwrap();
    let unreadable = async |broken: bool| {
        let starter = if broken { "nobody" } else { "user" };
        db.execute_unprepared(&format!(
            "UPDATE threads SET started_by = '{starter}' WHERE id = 1"
        ))
        .await
        .unwrap();
    };
    let main = threads.main();
    main.send("Va".into()).await;
    pooled.until_held(ThreadId::MAIN).await;
    let (child, _) = child_of(&threads, ThreadId::MAIN, "Mesure la porte").await;
    unreadable(true).await;
    pooled.open(child.thread());
    until_done(&child).await;
    pooled.open(ThreadId::MAIN);
    until_done(&main).await;
    let shown = events(&main);
    assert_eq!(children_ended(&shown), 1, "{shown:?}");
    assert!(
        matches!(
            &shown[shown.len() - 2..],
            [Event::WakesHeld { count: 1, .. }, Event::TurnEnded { .. }]
        ),
        "{shown:?}"
    );
    assert_eq!(woken_count(&shown), 0);

    unreadable(false).await;
    threads
        .send_child(ThreadId::MAIN, None, child.thread(), "Encore".into())
        .await
        .unwrap();
    unreadable(true).await;
    pooled.open(child.thread());
    until_done(&child).await;
    wait_until(&main, "the second result was never held", |events| {
        events.contains(&held(2))
    })
    .await;
    assert_ne!(main.status(), ThreadStatus::Running);
    assert_eq!(prompts(&pooled, ThreadId::MAIN), ["Va"]);

    unreadable(false).await;
    main.send("Et ?".into()).await;
    pooled.until_held(ThreadId::MAIN).await;
    pooled.open(ThreadId::MAIN);
    until_done(&main).await;
    let given = prompts(&pooled, ThreadId::MAIN);
    assert_eq!(given.len(), 2, "{given:?}");
    assert!(
        given[1].starts_with("2 sub-agent result(s) came"),
        "{given:?}"
    );
}

/// A late answer waiting for a turn's end is in the thread's stream: when Fronde stops before
/// it is given, the next start gives it to the agent, once, whether the stop left the turn
/// open or not; the start after gives nothing again.
#[tokio::test]
async fn a_late_answer_waiting_survives_a_restart() {
    for turn_closed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        {
            let (threads, pooled, requests) =
                open_woken_with(dir.path(), Limits::default(), |pooled| {
                    pooled.gate(ThreadId(2))
                })
                .await;
            let second = new_thread(&threads, StartedBy::User).await;
            second.send("Va".into()).await;
            pooled.until_held(ThreadId(2)).await;
            let id = requests
                .ask(late_ask(ThreadId(2), "thread:2", 1))
                .await
                .unwrap()
                .id();
            requests.answer(id, perso(), &alix()).await.unwrap();
            wait_until(&second, "the late answer was never logged", |events| {
                events.iter().any(
                    |event| matches!(event, Event::LateAnswer { request, .. } if *request == id),
                )
            })
            .await;
            assert!(!events(&second).contains(&user(PERSO)));
        }
        // Fronde stops: before the turn's end, or between its end and the late answer's message.
        if turn_closed {
            let db = fronde_db::open(dir.path()).await.unwrap();
            ConversationLog::open(&db, "thread:2")
                .await
                .unwrap()
                .append(Event::TurnEnded {
                    outcome: TurnOutcome::Done,
                })
                .await
                .unwrap();
        }
        let (threads, pooled, _requests) =
            open_woken_with(dir.path(), Limits::default(), |_| {}).await;
        // Opened at start for it, not when asked for.
        timeout(WAIT, async {
            while prompts(&pooled, ThreadId(2)).is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("thread 2 was not opened at start to give its late answer");
        let second = threads.assistant(ThreadId(2)).await.unwrap().unwrap();
        wait_until(&second, "the late answer was never given", |events| {
            events.contains(&user(PERSO))
        })
        .await;
        until_done(&second).await;
        let given = prompts(&pooled, ThreadId(2));
        assert_eq!(given.len(), 1, "{given:?}");
        assert_late_prompt(&given[0]);
        let shown = events(&second);
        assert_eq!(
            shown.contains(&Event::Notice {
                text: INTERRUPTED.into()
            }),
            !turn_closed,
            "{shown:?}"
        );
        drop((threads, second, pooled));

        let (threads, pooled, _requests) =
            open_woken_with(dir.path(), Limits::default(), |_| {}).await;
        let second = threads.assistant(ThreadId(2)).await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_ne!(second.status(), ThreadStatus::Running);
        assert_eq!(events(&second), shown, "given once");
        assert!(prompts(&pooled, ThreadId(2)).is_empty());
    }
}

// The owner's reserve: background work stops at the line of a subscription window.

/// Pooled agents for every thread, with the requests set, on a spec reading its usage from
/// `book`, under `limits`.
async fn open_reserved(
    dir: &Path,
    limits: Limits,
    book: &Arc<UsageBook>,
) -> (Arc<Threads>, Arc<Pooled>, Arc<Requests>) {
    let spec = AgentSpec {
        usage: Some(book.clone()),
        ..spec()
    };
    let (threads, pooled) = open_pooled_spec(
        dir,
        spec,
        Budget {
            max_running: 3,
            limits,
            ..Budget::default()
        },
    )
    .await;
    let db = fronde_db::open(dir).await.unwrap();
    let requests = Requests::open(db, threads.clone()).await.unwrap();
    threads.set_requests(&requests).await;
    *pooled.requests.lock().unwrap() = Some(requests.clone());
    (threads, pooled, requests)
}

/// When the windows read below reset: in two hours, to the minute.
fn resets_at() -> chrono::DateTime<chrono::Utc> {
    let at = chrono::Utc::now() + chrono::TimeDelta::hours(2);
    chrono::DateTime::from_timestamp(at.timestamp() / 60 * 60, 0).unwrap()
}

fn window(kind: WindowKind, used: Option<f64>, status: LimitStatus) -> Window {
    Window {
        kind,
        used,
        status: Some(status),
        resets_at: Some(resets_at()),
        as_of: chrono::Utc::now(),
        reset_since_reading: false,
    }
}

/// Records a reading of both guarded windows.
fn read(book: &UsageBook, session: f64, weekly: f64) {
    book.record(
        "claude",
        vec![
            window(WindowKind::Session, Some(session), LimitStatus::Allowed),
            window(WindowKind::Weekly, Some(weekly), LimitStatus::Allowed),
        ],
    );
}

fn task(text: &str) -> ChildTask {
    ChildTask {
        task: text.into(),
        title: Some("Mesurer".into()),
        ..ChildTask::default()
    }
}

fn delegation(error: anyhow::Error) -> Delegation {
    error
        .downcast::<Delegation>()
        .unwrap_or_else(|error| panic!("not a refusal: {error:#}"))
}

/// Past the line of either window, `threads_start` is refused, the refusal naming the window,
/// its use, the setting and the reset time; nothing is started.
#[tokio::test]
async fn reserve_refuses_start_past_the_line() {
    let reset = resets_at().format("%Y-%m-%d %H:%M UTC").to_string();
    for (session, weekly, kind, used, name) in [
        (0.72, 0.10, WindowKind::Session, 72, "5-hour window"),
        (0.20, 0.85, WindowKind::Weekly, 85, "weekly window"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let book = UsageBook::in_memory();
        read(&book, session, weekly);
        let (threads, pooled, _requests) =
            open_reserved(dir.path(), Limits::default(), &book).await;
        let refused = delegation(
            threads
                .start_child(ThreadId::MAIN, None, task("Mesure la porte"))
                .await
                .unwrap_err(),
        );
        assert_eq!(
            refused,
            Delegation::Reserve {
                window: kind,
                used: Some(used),
                status: Some(LimitStatus::Allowed),
                reserve: 30,
                resets_at: Some(resets_at()),
            }
        );
        assert_eq!(
            refused.to_string(),
            format!(
                "the owner's reserve of the {name} is reached ({used} % used, 30 % kept for the \
                 owner, the setting [threads] reserve); it resets at {reset}; sub-agents can \
                 neither start nor get messages until then: do the task yourself or tell the user"
            )
        );
        assert_eq!(
            threads.summaries(&alix()).await.unwrap().len(),
            1,
            "none started"
        );
        assert!(pooled.threads().is_empty());
    }
}

/// Past the line, `threads_send` to a sub-agent is refused alike; below it, it goes.
#[tokio::test]
async fn reserve_refuses_send_past_the_line() {
    let dir = tempfile::tempdir().unwrap();
    let book = UsageBook::in_memory();
    read(&book, 0.1, 0.1);
    let (threads, _pooled, _requests) = open_reserved(dir.path(), Limits::default(), &book).await;
    let (child, _) = child_of(&threads, ThreadId::MAIN, "Mesure la porte").await;
    until_done(&child).await;
    let main = threads.main();
    until_done(&main).await;
    book.record(
        "claude",
        vec![window(
            WindowKind::Session,
            Some(0.71),
            LimitStatus::Warning,
        )],
    );
    let refused = delegation(
        threads
            .send_child(
                ThreadId::MAIN,
                None,
                child.thread(),
                "Et la fenêtre ?".into(),
            )
            .await
            .unwrap_err(),
    );
    assert!(
        matches!(
            &refused,
            Delegation::Reserve {
                window: WindowKind::Session,
                used: Some(71),
                ..
            }
        ),
        "{refused:?}"
    );
    assert!(
        refused.to_string().contains("[threads] reserve"),
        "{refused}"
    );
    assert_eq!(
        events(&child)
            .iter()
            .filter(|event| matches!(event, Event::User { .. }))
            .count(),
        1,
        "nothing sent"
    );
    // A rejected window without a percentage refuses too, and says so.
    book.record(
        "claude",
        vec![Window {
            used: None,
            resets_at: Some(resets_at() + chrono::TimeDelta::hours(1)),
            ..window(WindowKind::Weekly, None, LimitStatus::Rejected)
        }],
    );
    let refused = delegation(
        threads
            .send_child(
                ThreadId::MAIN,
                None,
                child.thread(),
                "Et la fenêtre ?".into(),
            )
            .await
            .unwrap_err(),
    );
    assert!(
        refused.to_string().starts_with(
            "the owner's reserve of the weekly window is reached (its limit reached, 30 % kept"
        ),
        "{refused}"
    );
}

/// The daily counts are the safety net only: with no reading, the sub-agent past
/// `children_per_day` is refused; with a reading below the line, it starts.
#[tokio::test]
async fn counts_apply_only_without_a_reading() {
    let dir = tempfile::tempdir().unwrap();
    let book = UsageBook::in_memory();
    let limits = Limits {
        children_per_day: 1,
        child_turns_per_day: 1,
        ..Limits::default()
    };
    let (threads, _pooled, _requests) = open_reserved(dir.path(), limits, &book).await;
    let (first, _) = child_of(&threads, ThreadId::MAIN, "Un").await;
    until_done(&first).await;
    let refused = delegation(
        threads
            .start_child(ThreadId::MAIN, None, task("Deux"))
            .await
            .unwrap_err(),
    );
    assert_eq!(refused, Delegation::TooManyToday { limit: 1 });
    let refused = delegation(
        threads
            .send_child(ThreadId::MAIN, None, first.thread(), "Encore".into())
            .await
            .unwrap_err(),
    );
    assert_eq!(refused, Delegation::TooManyTurnsToday { limit: 1 });
    // A reading with only the 5-hour window decides nothing either.
    book.record(
        "claude",
        vec![window(WindowKind::Session, Some(0.1), LimitStatus::Allowed)],
    );
    assert!(matches!(
        threads.start_child(ThreadId::MAIN, None, task("Deux")).await,
        Err(error) if error.downcast_ref() == Some(&Delegation::TooManyToday { limit: 1 })
    ));
    // Both windows below the line: the reading decides, the counts do not apply.
    read(&book, 0.1, 0.1);
    assert!(matches!(
        threads.background("claude"),
        fronde_agent::threads::Background::Allowed { .. }
    ));
    let (second, _) = child_of(&threads, ThreadId::MAIN, "Deux").await;
    until_done(&second).await;
    threads
        .send_child(ThreadId::MAIN, None, first.thread(), "Encore".into())
        .await
        .unwrap();
    until_done(&first).await;
}

/// A window reset since its reading ends the pause: what was used since is not known, so the
/// daily counts decide again.
#[tokio::test]
async fn reset_time_ends_the_pause() {
    let dir = tempfile::tempdir().unwrap();
    let book = UsageBook::in_memory();
    let past = chrono::Utc::now() - chrono::TimeDelta::minutes(1);
    book.record(
        "claude",
        vec![
            Window {
                resets_at: Some(past),
                ..window(WindowKind::Session, Some(0.95), LimitStatus::Rejected)
            },
            window(WindowKind::Weekly, Some(0.1), LimitStatus::Allowed),
        ],
    );
    let (threads, _pooled, _requests) = open_reserved(dir.path(), Limits::default(), &book).await;
    assert_eq!(
        threads.background("claude"),
        fronde_agent::threads::Background::Unknown
    );
    let (child, _) = child_of(&threads, ThreadId::MAIN, "Mesure la porte").await;
    until_done(&child).await;
}

/// `reserve = "0%"`: a reading past any line pauses nothing, and the counts decide.
#[tokio::test]
async fn reserve_zero_is_off() {
    let dir = tempfile::tempdir().unwrap();
    let book = UsageBook::in_memory();
    read(&book, 1.0, 1.0);
    let limits = Limits {
        reserve: 0,
        children_per_day: 1,
        ..Limits::default()
    };
    let (threads, _pooled, _requests) = open_reserved(dir.path(), limits, &book).await;
    let (child, _) = child_of(&threads, ThreadId::MAIN, "Un").await;
    until_done(&child).await;
    let refused = delegation(
        threads
            .start_child(ThreadId::MAIN, None, task("Deux"))
            .await
            .unwrap_err(),
    );
    assert_eq!(refused, Delegation::TooManyToday { limit: 1 });
}

fn reserve_held(count: u32, window: WindowKind, used: Option<u8>) -> Event {
    Event::WakesHeld {
        count,
        returned: false,
        reason: HoldReason::Reserve { window, used },
    }
}

/// A sub-agent's result that comes once the line is passed wakes nothing: it is held, the
/// thread says why, and the owner's next message, which runs past the reserve, carries it.
#[tokio::test]
async fn reserve_holds_wakes() {
    let dir = tempfile::tempdir().unwrap();
    let book = UsageBook::in_memory();
    let (threads, pooled, _requests) = open_reserved(dir.path(), Limits::default(), &book).await;
    pooled.gate(ThreadId(2));
    let (child, _) = child_of(&threads, ThreadId::MAIN, "Mesure la porte").await;
    pooled.until_held(child.thread()).await;
    read(&book, 0.72, 0.1);
    pooled.open(child.thread());
    until_done(&child).await;
    let main = threads.main();
    let held = reserve_held(1, WindowKind::Session, Some(72));
    wait_until(&main, "the result was never held", |events| {
        events.contains(&held)
    })
    .await;
    assert_eq!(
        events(&main),
        [child_ended(child.thread().0, ANSWER), held.clone()]
    );
    assert_ne!(main.status(), ThreadStatus::Running);
    assert!(prompts(&pooled, ThreadId::MAIN).is_empty(), "not woken");

    main.send("Et alors ?".into()).await;
    until_done(&main).await;
    let given = prompts(&pooled, ThreadId::MAIN);
    assert_eq!(given.len(), 1, "{given:?}");
    assert!(
        given[0].starts_with("1 sub-agent result(s) came"),
        "{given:?}"
    );
    assert!(given[0].contains(&format!("\n{ANSWER}\n")), "{given:?}");
    assert!(given[0].ends_with("The message:\nEt alors ?"), "{given:?}");
    assert_eq!(woken_count(&events(&main)), 0);
}

/// A result waiting for the parent's turn is held at that turn's end once the line is passed.
#[tokio::test]
async fn reserve_holds_wakes_at_a_turns_end() {
    let dir = tempfile::tempdir().unwrap();
    let book = UsageBook::in_memory();
    let (threads, pooled, _requests) = open_reserved(dir.path(), Limits::default(), &book).await;
    pooled.gate(ThreadId::MAIN);
    let main = threads.main();
    main.send("Va".into()).await;
    pooled.until_held(ThreadId::MAIN).await;
    let (child, _) = child_of(&threads, ThreadId::MAIN, "Mesure la porte").await;
    until_done(&child).await;
    wait_until(&main, "the result never came", |events| {
        children_ended(events) == 1
    })
    .await;
    book.record(
        "claude",
        vec![window(WindowKind::Weekly, None, LimitStatus::Warning)],
    );
    pooled.open(ThreadId::MAIN);
    until_done(&main).await;
    let shown = events(&main);
    assert!(
        matches!(
            &shown[shown.len() - 2..],
            [held, Event::TurnEnded { outcome: TurnOutcome::Done }]
                if *held == reserve_held(1, WindowKind::Weekly, None)
        ),
        "{shown:?}"
    );
    assert_eq!(woken_count(&shown), 0);
    assert_eq!(prompts(&pooled, ThreadId::MAIN).len(), 1, "Va only");
}

/// The owner's own turn always runs, whatever the reading says: the message reaches the agent
/// with a window rejected.
#[tokio::test]
async fn owner_turn_runs_past_the_reserve() {
    let dir = tempfile::tempdir().unwrap();
    let book = UsageBook::in_memory();
    book.record(
        "claude",
        vec![
            window(WindowKind::Session, Some(1.0), LimitStatus::Rejected),
            window(WindowKind::Weekly, Some(1.0), LimitStatus::Rejected),
        ],
    );
    let (threads, pooled, _requests) = open_reserved(dir.path(), Limits::default(), &book).await;
    assert!(matches!(
        threads.background("claude"),
        fronde_agent::threads::Background::Paused(_)
    ));
    let other = new_thread(&threads, StartedBy::User).await;
    for assistant in [threads.main(), other] {
        assistant.send("Salut".into()).await;
        until_done(&assistant).await;
        assert_eq!(
            prompts(&pooled, assistant.thread()),
            ["Salut"],
            "thread {}",
            assistant.thread()
        );
    }
}

/// A wake a stop left before its prompt is held at the next start when the line is passed; the
/// hold survives a further restart without a reading, and the owner's next message carries it.
#[tokio::test]
async fn reserve_hold_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (connector, _) = fake_connector(FakeScript::default());
        let threads = open(dir.path(), connector).await;
        let child = threads
            .create(
                "alix",
                StartedBy::Thread(ThreadId::MAIN),
                Some(ThreadId::MAIN),
                "Mesurer",
            )
            .await
            .unwrap();
        threads
            .main()
            .log()
            .append(child_ended(child.id.0, "83 cm"))
            .await
            .unwrap();
    }
    let book = UsageBook::in_memory();
    read(&book, 0.1, 0.9);
    let held = reserve_held(1, WindowKind::Weekly, Some(90));
    {
        let (threads, pooled, _requests) =
            open_reserved(dir.path(), Limits::default(), &book).await;
        let main = threads.main();
        assert_ne!(main.status(), ThreadStatus::Running);
        assert_eq!(events(&main), [child_ended(2, "83 cm"), held.clone()]);
        assert!(prompts(&pooled, ThreadId::MAIN).is_empty());
    }
    // Restarted without any reading: the result stays held for the owner's message.
    let (threads, pooled, _requests) =
        open_reserved(dir.path(), Limits::default(), &UsageBook::in_memory()).await;
    let main = threads.main();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_ne!(main.status(), ThreadStatus::Running);
    assert_eq!(events(&main), [child_ended(2, "83 cm"), held]);
    main.send("Et ?".into()).await;
    until_done(&main).await;
    let given = prompts(&pooled, ThreadId::MAIN);
    assert_eq!(given.len(), 1, "{given:?}");
    assert!(given[0].contains("\n83 cm\n"), "{given:?}");
    assert!(given[0].ends_with("The message:\nEt ?"), "{given:?}");
}

/// A hold is named after what caused it: the owner's Stop says so even past the reserve, and
/// the reserve only when it alone holds.
#[tokio::test]
async fn the_hold_names_its_cause() {
    let dir = tempfile::tempdir().unwrap();
    let book = UsageBook::in_memory();
    let (threads, pooled, _requests) = open_reserved(dir.path(), Limits::default(), &book).await;
    pooled.gate(ThreadId::MAIN);
    let main = threads.main();
    main.send("Va".into()).await;
    pooled.until_held(ThreadId::MAIN).await;
    let (child, _) = child_of(&threads, ThreadId::MAIN, "Mesure la porte").await;
    until_done(&child).await;
    wait_until(&main, "the result never came", |events| {
        children_ended(events) == 1
    })
    .await;
    read(&book, 0.9, 0.1);
    main.cancel().await;
    pooled.open(ThreadId::MAIN);
    until_done(&main).await;
    let shown = events(&main);
    assert!(
        shown.contains(&Event::WakesHeld {
            count: 1,
            returned: false,
            reason: HoldReason::Stopped,
        }),
        "{shown:?}"
    );
    assert!(
        !shown.iter().any(|event| matches!(
            event,
            Event::WakesHeld {
                reason: HoldReason::Reserve { .. },
                ..
            }
        )),
        "{shown:?}"
    );
}
