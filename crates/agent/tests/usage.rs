//! Usage readings: captured from the agent's `usage_update`s and refused prompts, kept per
//! provider in the book, and in `fronde.db` across restarts.

use agent_client_protocol::v1::schema::{SessionUpdate, UsageUpdate};
use chrono::{DateTime, Local, Utc};
use fronde_agent::acp::{AgentClient, AgentEvent, AgentSpec, Failure, TurnEnd};
use fronde_agent::assistant::{Assistant, Connector};
use fronde_agent::fake::{FakeScript, fake_agent, rate_limit_error, text_chunk, usage_update};
use fronde_agent::log::{Event, TurnOutcome};
use fronde_agent::provider::Provider;
use fronde_agent::threads::Threads;
use fronde_agent::usage::{RateLimited, UsageBook, read_usage};
use fronde_protocol::usage::{LimitStatus, ProviderUsage, Window, WindowKind};
use fronde_requests::User;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

fn spec(usage: Option<Arc<UsageBook>>) -> AgentSpec {
    AgentSpec {
        thread: fronde_protocol::ThreadId::MAIN,
        provider: &Provider::CLAUDE,
        command: vec!["claude-agent-acp".into()],
        env: Vec::new(),
        cwd: "/srv/fronde/agent".into(),
        home: "/srv/fronde/home".into(),
        hub_url: "http://127.0.0.1:7878/mcp".into(),
        hub_token: "hub-token".into(),
        oauth_token: None,
        system_prompt: "You are Fronde.".into(),
        builtin_tools: Vec::new(),
        usage,
        owners_grants: true,
        account: "claude".into(),
    }
}

/// Unix seconds `hours` from now: readings far from their receipt are dropped.
fn hence(hours: i64) -> i64 {
    (Utc::now() + chrono::Duration::hours(hours)).timestamp()
}

fn time(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds, 0).unwrap()
}

/// A `rate_limit_info` as the CLI sends it: both windows, the five-hour one the event's own.
fn rate_limit(session: f64, session_status: &str, session_reset: i64, weekly_reset: i64) -> Value {
    json!({
        "status": session_status,
        "resetsAt": session_reset,
        "rateLimitType": "five_hour",
        "utilization": session,
        "isUsingOverage": false,
        "unifiedWindows": {
            "five_hour": {"utilization": session, "resetsAt": session_reset, "status": session_status},
            "seven_day": {"utilization": 0.18, "resetsAt": weekly_reset, "status": "allowed"},
        },
    })
}

async fn connect(
    spec: &AgentSpec,
    script: FakeScript,
) -> (AgentClient, mpsc::UnboundedReceiver<AgentEvent>) {
    let (transport, _handle) = fake_agent(script);
    let (events, rx) = mpsc::unbounded_channel();
    let client = timeout(WAIT, AgentClient::connect(transport, spec, None, events))
        .await
        .expect("connect timed out")
        .expect("connect failed");
    (client, rx)
}

async fn prompt(client: &AgentClient) -> anyhow::Result<TurnEnd> {
    timeout(WAIT, client.prompt("Salut"))
        .await
        .expect("prompt timed out")
}

fn used(usage: &ProviderUsage, kind: WindowKind) -> Option<f64> {
    usage.window(&kind).and_then(|window| window.used)
}

#[tokio::test]
async fn usage_update_with_rate_limit_reaches_the_book() {
    let book = UsageBook::in_memory();
    let (session_reset, weekly_reset) = (hence(3), hence(100));
    let script = FakeScript {
        updates: vec![
            text_chunk("Bonjour"),
            usage_update(rate_limit(0.42, "allowed", session_reset, weekly_reset)),
        ],
        ..FakeScript::default()
    };
    let (client, mut rx) = connect(&spec(Some(book.clone())), script).await;
    let before = Utc::now();
    assert_eq!(prompt(&client).await.unwrap(), TurnEnd::Done);

    let usage = book.get("claude").expect("no reading");
    assert_eq!(usage.provider, "claude");
    assert!(usage.known_since >= before && usage.known_since <= Utc::now());
    let session = usage.window(&WindowKind::Session).unwrap();
    assert_eq!(session.used, Some(0.42));
    assert_eq!(session.status, Some(LimitStatus::Allowed));
    assert_eq!(session.resets_at, Some(time(session_reset)));
    assert_eq!(session.as_of, usage.known_since);
    let weekly = usage.window(&WindowKind::Weekly).unwrap();
    assert_eq!(weekly.used, Some(0.18));
    assert_eq!(weekly.resets_at, Some(time(weekly_reset)));
    assert_eq!(usage.windows.len(), 2);
    // The reading is the book's, not the conversation's.
    let mut texts = Vec::new();
    while let Ok(event) = rx.try_recv() {
        texts.push(event);
    }
    assert_eq!(texts, [AgentEvent::Text("Bonjour".into())]);
}

/// A reading is kept under the account the agent signs in with, the spec's, not its provider:
/// the seam for an account per person.
#[tokio::test]
async fn readings_go_to_the_agents_account() {
    let book = UsageBook::in_memory();
    let script = FakeScript {
        updates: vec![usage_update(rate_limit(
            0.42,
            "allowed",
            hence(3),
            hence(100),
        ))],
        ..FakeScript::default()
    };
    let spec = AgentSpec {
        owners_grants: true,
        account: "claude:lea".into(),
        ..spec(Some(book.clone()))
    };
    let (client, _rx) = connect(&spec, script).await;
    assert_eq!(prompt(&client).await.unwrap(), TurnEnd::Done);
    assert_eq!(book.get("claude"), None);
    let usage = book.get("claude:lea").expect("no reading for the account");
    assert_eq!(usage.provider, "claude:lea");
    assert_eq!(used(&usage, WindowKind::Session), Some(0.42));
}

#[tokio::test]
async fn usage_update_without_rate_limit_is_ignored() {
    let book = UsageBook::in_memory();
    let changes = book.changes();
    let Value::Object(model_only) = json!({"_claude/model": "claude-opus-4-5"}) else {
        unreachable!()
    };
    let script = FakeScript {
        updates: vec![
            SessionUpdate::UsageUpdate(UsageUpdate::new(53_000, 200_000)),
            SessionUpdate::UsageUpdate(UsageUpdate::new(54_000, 200_000).meta(model_only)),
        ],
        ..FakeScript::default()
    };
    let (client, _rx) = connect(&spec(Some(book.clone())), script).await;
    assert_eq!(prompt(&client).await.unwrap(), TurnEnd::Done);
    assert_eq!(book.get("claude"), None);
    assert!(!changes.has_changed().unwrap());
}

/// Garbage where the reading goes never fails a turn, and is never read as a reading.
#[tokio::test]
async fn bad_usage_never_fails_a_turn() {
    let book = UsageBook::in_memory();
    let garbage = [
        json!(null),
        json!("rejected"),
        json!([1, 2, 3]),
        json!({"status": "allowed", "rateLimitType": "five_hour", "utilization": 42, "resetsAt": "soon"}),
        json!({"unifiedWindows": {"five_hour": {"utilization": f64::MAX, "resetsAt": -5}}}),
        json!({"rateLimitType": {"nested": true}, "status": 1, "unifiedWindows": [null]}),
    ];
    let mut updates: Vec<_> = garbage.into_iter().map(usage_update).collect();
    updates.push(text_chunk("Toujours là"));
    let script = FakeScript {
        updates,
        ..FakeScript::default()
    };
    let (client, mut rx) = connect(&spec(Some(book.clone())), script).await;
    assert_eq!(prompt(&client).await.unwrap(), TurnEnd::Done);
    assert_eq!(
        rx.try_recv().unwrap(),
        AgentEvent::Text("Toujours là".into())
    );
    // The one window with a readable status has a reset that does not read: refused whole,
    // since without its reset it could never be told from a later window.
    assert_eq!(book.get("claude"), None);
}

#[tokio::test]
async fn bare_provider_drops_rate_limits() {
    static BARE: Provider = Provider::bare("claude");
    let book = UsageBook::in_memory();
    let spec = AgentSpec {
        provider: &BARE,
        ..spec(Some(book.clone()))
    };
    let script = FakeScript {
        updates: vec![usage_update(rate_limit(
            0.5,
            "allowed",
            hence(3),
            hence(100),
        ))],
        ..FakeScript::default()
    };
    let (client, _rx) = connect(&spec, script).await;
    assert_eq!(prompt(&client).await.unwrap(), TurnEnd::Done);
    assert_eq!(book.get("claude"), None);

    // Its refused prompt is any failed turn.
    let script = FakeScript {
        fail_prompt_data: Some(rate_limit_error(rate_limit(
            1.0,
            "rejected",
            hence(1),
            hence(100),
        ))),
        ..FakeScript::default()
    };
    let (client, _rx) = connect(&spec, script).await;
    let error = prompt(&client).await.unwrap_err();
    assert!(error.downcast_ref::<RateLimited>().is_none());
    assert_eq!(error.to_string(), "the agent failed the turn");
    assert_eq!(book.get("claude"), None);
}

#[tokio::test]
async fn later_reading_replaces_the_earlier_one() {
    let book = UsageBook::in_memory();
    let (session_reset, weekly_reset) = (hence(3), hence(100));
    let script = FakeScript {
        updates: vec![
            usage_update(rate_limit(0.42, "allowed", session_reset, weekly_reset)),
            usage_update(rate_limit(
                0.91,
                "allowed_warning",
                session_reset,
                weekly_reset,
            )),
        ],
        ..FakeScript::default()
    };
    let (client, _rx) = connect(&spec(Some(book.clone())), script).await;
    prompt(&client).await.unwrap();
    let usage = book.get("claude").unwrap();
    assert_eq!(used(&usage, WindowKind::Session), Some(0.91));
    assert_eq!(
        usage.window(&WindowKind::Session).unwrap().status,
        Some(LimitStatus::Warning)
    );

    // The next window, from another turn: replaces it again. Within five hours of now, as a
    // 5-hour window's reset must be.
    let next = hence(4);
    let script = FakeScript {
        updates: vec![usage_update(rate_limit(
            0.05,
            "allowed",
            next,
            weekly_reset,
        ))],
        ..FakeScript::default()
    };
    let (client, _rx) = connect(&spec(Some(book.clone())), script).await;
    prompt(&client).await.unwrap();
    let usage = book.get("claude").unwrap();
    assert_eq!(used(&usage, WindowKind::Session), Some(0.05));
    assert_eq!(
        usage.window(&WindowKind::Session).unwrap().resets_at,
        Some(time(next))
    );
    assert_eq!(usage.windows.len(), 2);
}

#[tokio::test]
async fn rate_limited_prompt_fails_with_its_reset() {
    let book = UsageBook::in_memory();
    let reset = hence(2);
    let script = FakeScript {
        updates: vec![text_chunk("Je regarde")],
        fail_prompt_data: Some(rate_limit_error(rate_limit(
            1.0,
            "rejected",
            reset,
            hence(100),
        ))),
        ..FakeScript::default()
    };
    let (client, _rx) = connect(&spec(Some(book.clone())), script).await;
    let error = prompt(&client).await.unwrap_err();
    let limited = error.downcast_ref::<RateLimited>().expect("a rate limit");
    assert_eq!(limited.provider, "claude");
    assert_eq!(limited.resumes_at, Some(time(reset)));
    assert_eq!(Failure::of(&error), Failure::Other);
    // The log line names the reset, and nothing of the reading's other fields.
    let logged = format!("{error:#}");
    assert!(logged.contains("usage limit is reached"), "{logged}");
    assert!(!logged.contains("unifiedWindows"), "{logged}");
    // The reading reached the book.
    let usage = book.get("claude").unwrap();
    let session = usage.window(&WindowKind::Session).unwrap();
    assert_eq!(session.status, Some(LimitStatus::Rejected));
    assert_eq!(session.used, Some(1.0));
    assert_eq!(used(&usage, WindowKind::Weekly), Some(0.18));
}

#[tokio::test]
async fn other_prompt_errors_unchanged() {
    let book = UsageBook::in_memory();
    for data in [
        Some(json!({"errorKind": "overloaded"})),
        Some(
            json!({"errorKind": "rate_limit_ish", "rateLimit": rate_limit(1.0, "rejected", hence(1), hence(100))}),
        ),
        Some(json!("rate_limit")),
        None,
    ] {
        let script = FakeScript {
            fail_prompt: data.is_none(),
            fail_prompt_data: data.clone(),
            ..FakeScript::default()
        };
        let (client, _rx) = connect(&spec(Some(book.clone())), script).await;
        let error = prompt(&client).await.unwrap_err();
        assert!(error.downcast_ref::<RateLimited>().is_none(), "{data:?}");
        assert_eq!(error.to_string(), "the agent failed the turn", "{data:?}");
        assert_eq!(Failure::of(&error), Failure::Other, "{data:?}");
    }
    assert_eq!(book.get("claude"), None);
}

// Through the threads: what the owner sees, and what is kept.

fn alix() -> User {
    User {
        name: "alix".into(),
        admin: true,
    }
}

fn connector(script: FakeScript) -> Connector {
    Arc::new(move |spec: &AgentSpec, resume, events| {
        let (spec, script) = (spec.clone(), script.clone());
        Box::pin(async move {
            let (transport, _handle) = fake_agent(script);
            AgentClient::connect(transport, &spec, resume, events).await
        })
    })
}

/// The threads of `dir`, with the book stored there, as the server opens them.
async fn open(dir: &Path, script: FakeScript) -> (Arc<Threads>, Arc<UsageBook>) {
    let db = fronde_db::open(dir).await.unwrap();
    let book = UsageBook::load(db.clone(), &["claude".to_owned()]).await;
    let threads = Threads::open(db, dir, spec(Some(book.clone())), connector(script), alix())
        .await
        .unwrap();
    (threads, book)
}

async fn turn(assistant: &Arc<Assistant>, count: usize) -> Vec<Event> {
    assistant.send("Salut".into()).await;
    timeout(WAIT, async {
        loop {
            let events: Vec<Event> = assistant
                .log()
                .since(0)
                .into_iter()
                .map(|entry| entry.event)
                .collect();
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
    .expect("the turn did not end")
}

#[tokio::test]
async fn rate_limited_turn_says_when() {
    let dir = tempfile::tempdir().unwrap();
    let reset = hence(2);
    let script = FakeScript {
        fail_prompt_data: Some(rate_limit_error(rate_limit(
            1.0,
            "rejected",
            reset,
            hence(100),
        ))),
        ..FakeScript::default()
    };
    let (threads, book) = open(dir.path(), script).await;
    let events = turn(&threads.main(), 1).await;

    let when = time(reset).with_timezone(&Local).format("%H:%M");
    assert_eq!(
        events,
        [
            Event::User {
                text: "Salut".into(),
                by: None
            },
            Event::Notice {
                text: format!("Limite d'usage atteinte, reprise vers {when}.")
            },
            Event::TurnEnded {
                outcome: TurnOutcome::Error
            },
        ]
    );
    let usage = book.get("claude").expect("the reading reached the book");
    assert_eq!(
        usage.window(&WindowKind::Session).unwrap().status,
        Some(LimitStatus::Rejected)
    );
    book.flush().await;
    let db = fronde_db::open(dir.path()).await.unwrap();
    assert_eq!(read_usage(&db, "claude").await.unwrap(), Some(usage));
}

#[tokio::test]
async fn usage_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (session_reset, weekly_reset) = (hence(3), hence(100));
    let script = FakeScript {
        updates: vec![
            usage_update(rate_limit(0.42, "allowed", session_reset, weekly_reset)),
            text_chunk("Bonjour"),
        ],
        ..FakeScript::default()
    };
    let (threads, book) = open(dir.path(), script.clone()).await;
    turn(&threads.main(), 1).await;
    let recorded = book.get("claude").expect("no reading");
    book.flush().await;
    drop(threads);
    drop(book);

    // A fresh start reads the same book, before any turn.
    let (threads, book) = open(dir.path(), FakeScript::default()).await;
    assert_eq!(book.get("claude"), Some(recorded.clone()));
    assert_eq!(book.all(), std::slice::from_ref(&recorded));
    let db = fronde_db::open(dir.path()).await.unwrap();
    assert_eq!(
        read_usage(&db, "claude").await.unwrap(),
        Some(recorded.clone())
    );
    assert_eq!(read_usage(&db, "codex").await.unwrap(), None);

    // A later reading of the same window replaces it, and is kept in turn.
    drop(threads);
    let script = FakeScript {
        updates: vec![usage_update(rate_limit(
            0.6,
            "allowed",
            session_reset,
            weekly_reset,
        ))],
        ..FakeScript::default()
    };
    let (threads, book) = open(dir.path(), script).await;
    turn(&threads.main(), 2).await;
    let later = book.get("claude").unwrap();
    assert_eq!(used(&later, WindowKind::Session), Some(0.6));
    assert!(later.known_since > recorded.known_since);
    book.flush().await;
    assert_eq!(read_usage(&db, "claude").await.unwrap(), Some(later));
}

fn window(kind: WindowKind, used: f64, resets_at: i64, as_of: DateTime<Utc>) -> Window {
    Window {
        kind,
        used: Some(used),
        status: Some(LimitStatus::Allowed),
        resets_at: Some(time(resets_at)),
        as_of,
        reset_since_reading: false,
    }
}

/// Two agent processes report, the older window arriving last: the newest window stays, in
/// memory and stored.
#[tokio::test]
async fn newest_window_wins() {
    let dir = tempfile::tempdir().unwrap();
    let db = fronde_db::open(dir.path()).await.unwrap();
    let book = UsageBook::load(db.clone(), &["claude".to_owned()]).await;
    let (old_reset, new_reset) = (hence(1), hence(6));
    let now = Utc::now();
    // The second agent saw the new window first.
    assert!(book.record(
        "claude",
        vec![window(WindowKind::Session, 0.1, new_reset, now)]
    ));
    // The first agent's held reading of the window before arrives later.
    let late = now + chrono::Duration::seconds(5);
    assert!(!book.record(
        "claude",
        vec![window(WindowKind::Session, 0.95, old_reset, late)]
    ));
    assert!(book.record(
        "claude",
        vec![
            window(WindowKind::Session, 0.95, old_reset, late),
            window(WindowKind::Weekly, 0.3, hence(100), late),
        ]
    ));
    book.flush().await;
    let expected = book.get("claude").unwrap();
    assert_eq!(used(&expected, WindowKind::Session), Some(0.1));
    assert_eq!(used(&expected, WindowKind::Weekly), Some(0.3));
    assert_eq!(expected.known_since, late);
    drop(book);
    let book = UsageBook::load(db.clone(), &["claude".to_owned()]).await;
    assert_eq!(book.get("claude"), Some(expected));
}

#[tokio::test]
async fn unchanged_reading_moves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = fronde_db::open(dir.path()).await.unwrap();
    let book = UsageBook::load(db.clone(), &["claude".to_owned()]).await;
    let mut changes = book.changes();
    let reset = hence(3);
    let first = Utc::now();
    assert!(book.record(
        "claude",
        vec![window(WindowKind::Session, 0.4, reset, first)]
    ));
    assert!(changes.has_changed().unwrap());
    changes.mark_unchanged();
    book.flush().await;
    // Removed behind the book's back: a write would put it back.
    assert!(
        fronde_db::kv::delete(&db, "provider/claude", "usage")
            .await
            .unwrap()
    );

    let again = first + chrono::Duration::minutes(5);
    assert!(!book.record(
        "claude",
        vec![window(WindowKind::Session, 0.4, reset, again)]
    ));
    book.flush().await;
    assert!(!changes.has_changed().unwrap());
    assert_eq!(read_usage(&db, "claude").await.unwrap(), None);
    // Still known since the first reading.
    let usage = book.get("claude").unwrap();
    assert_eq!(usage.known_since, first);
    assert_eq!(usage.windows[0].as_of, first);
}

#[tokio::test]
async fn bad_usage_row_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let db = fronde_db::open(dir.path()).await.unwrap();
    for bad in [
        b"not json".to_vec(),
        br#"{"provider": "claude", "windows": "none"}"#.to_vec(),
        br#"{"provider": "claude", "known_since": "2026-11-17T12:00:00Z", "windows": [{"kind": "hourly", "as_of": "2026-11-17T12:00:00Z"}]}"#.to_vec(),
        // A row naming another provider is not this one's.
        br#"{"provider": "codex", "known_since": "2026-11-17T12:00:00Z", "windows": []}"#.to_vec(),
    ] {
        fronde_db::kv::set(&db, "provider/claude", "usage", bad.clone(), None)
            .await
            .unwrap();
        // Read alone, it is an error (`fronde check` says so); the book leaves it out.
        assert!(read_usage(&db, "claude").await.is_err());
        let book = UsageBook::load(db.clone(), &["claude".to_owned()]).await;
        assert_eq!(book.get("claude"), None);
        // The book still records, and replaces the bad row.
        assert!(book.record("claude", vec![window(WindowKind::Session, 0.2, hence(3), Utc::now())]));
        book.flush().await;
        assert_eq!(read_usage(&db, "claude").await.unwrap(), book.get("claude"));
    }
}
