use fronde_agent::acp::{AgentClient, AgentSpec};
use fronde_agent::assistant::{Assistant, Connector, TOPIC};
use fronde_agent::fake::{FAKE_SESSION_ID, FakeHandle, FakeScript, fake_agent, text_chunk};
use fronde_agent::log::{ConversationLog, Event, TurnOutcome};
use fronde_agent::threads::{StartedBy, ThreadId, Threads};
use fronde_protocol::{Card, ChoiceOption, Field, Outcome};
use fronde_requests::{Ask, Caller, Requests, User};
use sea_orm::EntityTrait;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);
const ARCHIVED: &str = "Cette conversation est archivée : elle ne reçoit plus de message.";
const INTERRUPTED: &str = "La réponse précédente a été interrompue.";

fn spec() -> AgentSpec {
    AgentSpec {
        thread: fronde_protocol::ThreadId::MAIN,
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
