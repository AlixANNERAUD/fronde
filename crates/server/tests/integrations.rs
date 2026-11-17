//! `integrations_request_setup`: a card built from a shipped component's manifest, answered by
//! the admin, then the secrets sealed, a dry run, the entry written to `fronde.toml` (or shown to
//! paste) and the component started. The components come from `components/dist`, as the build
//! approves them.

#[path = "support/asks.rs"]
mod asks;

use async_trait::async_trait;
use fronde::integrations::Shipped;
use fronde_db::ephemeral::EphemeralSecrets;
use fronde_db::master_key::Backends;
use fronde_hub::Hub;
use fronde_hub::config::{Allow, ComponentConfig, SourceConfig};
use fronde_hub::integrations::Integrations;
use fronde_hub::origin::Origin;
use fronde_hub::tools::{CallContext, ToolOutput};
use fronde_protocol::{
    Card, Field, FieldValue, Outcome, RequestId, Response, SecretAnswer, SecretValue,
};
use fronde_requests::{Caller, Journal, Refusal, Requests, SecretStore, ThreadId, User};
use secrecy::ExposeSecret as _;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(30);
const CANARY: &str = "canary-setup-password-7e41";
const TOOL: &str = "integrations_request_setup";
const EVENTS: &str = "https://cloud.example/remote.php/dav/calendars/alix/personal/";
const REMINDERS: &str = "https://cloud.example/remote.php/dav/calendars/alix/tasks/";

/// A configuration with comments, which a setup must keep.
const CONFIG: &str = r#"# My Fronde.
[server]
token = { env = "UNUSED" } # never written here

[data]
dir = "../data"
timezone = "Europe/Paris"
user = "alix"

[memory]
git = false

# The key out of this directory, which may be read-only.
[secrets]
backend = "file"
master_key = { file = "../master.key" }
"#;

/// A card shown, with its stream, request and asker.
#[derive(Debug)]
struct Shown {
    topic: String,
    id: RequestId,
    card: Card,
}

/// Hands each card shown to the test, and records every end: a call to cut at the next end is
/// cut then, as its client giving it up at the moment the answer comes.
struct Cards {
    shown: mpsc::UnboundedSender<Shown>,
    closed: Arc<Mutex<Vec<(RequestId, Outcome)>>>,
    cut: Arc<Mutex<Option<CancellationToken>>>,
}

impl Cards {
    fn new(shown: mpsc::UnboundedSender<Shown>) -> Self {
        Self {
            shown,
            closed: Arc::default(),
            cut: Arc::default(),
        }
    }
}

#[async_trait]
impl Journal for Cards {
    async fn announce(
        &self,
        topic: &str,
        id: RequestId,
        card: &Card,
        _caller: &Caller,
    ) -> anyhow::Result<u64> {
        let _ = self.shown.send(Shown {
            topic: topic.to_owned(),
            id,
            card: card.clone(),
        });
        Ok(id)
    }

    async fn close(
        &self,
        _topic: &str,
        id: RequestId,
        outcome: &Outcome,
        _by: Option<&str>,
    ) -> anyhow::Result<()> {
        self.closed.lock().unwrap().push((id, outcome.clone()));
        if let Some(cut) = self.cut.lock().unwrap().take() {
            cut.cancel();
        }
        Ok(())
    }
}

struct Bench {
    hub: Arc<Hub>,
    requests: Arc<Requests>,
    integrations: Arc<Shipped>,
    cards: mpsc::UnboundedReceiver<Shown>,
    /// A call cut at the next request's end.
    cut: Arc<Mutex<Option<CancellationToken>>>,
    config: PathBuf,
    dir: tempfile::TempDir,
}

async fn bench() -> Bench {
    bench_with(|_| {}).await
}

/// A Fronde with the chat's requests, the threads (showing cards only) and the shipped
/// components to set up; `prepare` runs on the directory holding `fronde.toml` (`etc`, beside
/// `data` and the key) before the configuration is read.
async fn bench_with(prepare: impl FnOnce(&Path)) -> Bench {
    bench_writing(prepare, None).await
}

/// [`bench_with`], `fronde.toml` replaced by `write` when given.
async fn bench_writing(
    prepare: impl FnOnce(&Path),
    write: Option<fronde::config_edit::Write>,
) -> Bench {
    let dir = tempfile::tempdir().unwrap();
    let etc = dir.path().join("etc");
    std::fs::create_dir(&etc).unwrap();
    let config = etc.join("fronde.toml");
    std::fs::write(&config, CONFIG).unwrap();
    prepare(&etc);
    let loaded = fronde_hub::config::load(&config).unwrap();
    fronde_hub::private::prepare_data_dir(&loaded.data.dir).unwrap();
    let db = fronde_db::open(&loaded.data.dir).await.unwrap();
    let (send, cards) = mpsc::unbounded_channel();
    let store = SecretStore::new(
        Backends::file(dir.path().join("master.key")),
        Arc::new(EphemeralSecrets::new()),
    );
    let journal = Cards::new(send);
    let cut = journal.cut.clone();
    let requests = Requests::open_with(db.clone(), Arc::new(journal), Some(store))
        .await
        .unwrap();
    let mut shipped = Shipped::new(&loaded, db.clone()).unwrap();
    if let Some(write) = write {
        shipped = shipped.writing_with(write);
    }
    let integrations = Arc::new(shipped);
    let hub = Hub::start(
        loaded,
        db,
        vec![],
        Some(requests.clone()),
        Some(Arc::new(asks::Asks(requests.clone()))),
        Some(integrations.clone() as Arc<dyn Integrations>),
    )
    .await
    .unwrap();
    Bench {
        hub: Arc::new(hub),
        requests,
        integrations,
        cards,
        cut,
        config,
        dir,
    }
}

fn context(user: &str) -> CallContext {
    CallContext {
        caller: Caller::Thread(ThreadId::MAIN),
        user: user.into(),
        topic: "conversation".into(),
        turn: Some(3),
        cancelled: tokio_util::sync::CancellationToken::new(),
    }
}

fn alix() -> User {
    User {
        name: "alix".into(),
        admin: true,
    }
}

fn calendar_prefill() -> Value {
    json!({
        "integration": "calendar",
        "prefill": {
            "events_url": EVENTS,
            "reminders_url": REMINDERS,
            "caldav.username": "alix",
        },
    })
}

/// The answer that confirms a confirmation card, which has no field.
fn confirmed() -> Response {
    Response::Form {
        values: Default::default(),
    }
}

fn text(value: &str) -> FieldValue {
    FieldValue::Text(value.into())
}

fn secret(value: &str) -> FieldValue {
    FieldValue::Secret(SecretAnswer {
        secret: SecretValue::new(value.into()),
    })
}

/// The calendar's card answered with `events` and `reminders`, the password `CANARY`, and local
/// addresses allowed or not.
fn calendar_answer(events: &str, reminders: &str, local: bool) -> Response {
    Response::Form {
        values: [
            ("events_url".to_owned(), text(events)),
            ("reminders_url".to_owned(), text(reminders)),
            ("caldav.username".to_owned(), text("alix")),
            ("caldav.secret".to_owned(), secret(CANARY)),
            ("allow.local".to_owned(), FieldValue::Toggle(local)),
        ]
        .into(),
    }
}

impl Bench {
    fn call(&self, context: CallContext, args: Value) -> tokio::task::JoinHandle<ToolOutput> {
        let hub = self.hub.clone();
        tokio::spawn(async move { hub.registry.call_as(&context, TOOL, args).await })
    }

    async fn card(&mut self) -> Shown {
        tokio::time::timeout(WAIT, self.cards.recv())
            .await
            .expect("no card shown")
            .unwrap()
    }

    fn no_card(&mut self) {
        assert!(self.cards.try_recv().is_err(), "a card was shown");
    }

    fn config_text(&self) -> String {
        std::fs::read_to_string(&self.config).unwrap()
    }

    /// Every file of the bench's directory, as text: the configuration, the database and its
    /// journal, the key.
    fn every_file(&self) -> String {
        fn walk(dir: &Path, out: &mut String) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if let Ok(bytes) = std::fs::read(&path) {
                    out.push_str(&String::from_utf8_lossy(&bytes));
                }
            }
        }
        let mut out = String::new();
        walk(self.dir.path(), &mut out);
        out
    }
}

async fn output(call: tokio::task::JoinHandle<ToolOutput>) -> ToolOutput {
    tokio::time::timeout(WAIT, call).await.unwrap().unwrap()
}

async fn result(call: tokio::task::JoinHandle<ToolOutput>) -> Value {
    serde_json::from_str(&output(call).await.to_text()).unwrap()
}

fn fields(card: &Card) -> &[Field] {
    let Card::Form { fields, .. } = card else {
        panic!("a form: {card:?}");
    };
    fields
}

fn message(card: &Card) -> &str {
    let Card::Form { message, .. } = card else {
        panic!("a form: {card:?}");
    };
    message
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// The card shows the manifest's settings with the prefill, each value the agent suggested
/// marked so, a user name and a password for its credential, the origins that follow from the
/// URLs, and the toggle for local addresses, off. It is the admin's, and a secret card: refused
/// unless the hub built it.
#[tokio::test]
async fn setup_card_from_manifest() {
    let mut bench = bench().await;
    let call = bench.call(context("alix"), calendar_prefill());
    let shown = bench.card().await;
    assert_eq!(shown.topic, "conversation");
    let fields = fields(&shown.card);
    let keys: Vec<&str> = fields.iter().map(Field::key).collect();
    assert_eq!(
        keys,
        [
            "events_url",
            "reminders_url",
            "timezone",
            "caldav.username",
            "caldav.secret",
            "allow.local"
        ]
    );
    assert!(matches!(
        &fields[0],
        Field::Text { default: Some(url), required: true, format: Some(fronde_protocol::TextFormat::Uri), .. } if url == EVENTS
    ));
    // What the agent suggested is marked so, a URL above all; what it did not suggest is not.
    for suggested in [&fields[0], &fields[1], &fields[3]] {
        assert!(
            matches!(suggested, Field::Text { description: Some(d), .. } if d.starts_with("Suggéré par l'agent")),
            "{suggested:?}"
        );
    }
    assert!(
        matches!(&fields[0], Field::Text { description: Some(d), .. } if d.contains("ton serveur")),
        "{:?}",
        fields[0]
    );
    assert!(
        !matches!(&fields[2], Field::Text { description: Some(d), .. } if d.contains("Suggéré")),
        "{:?}",
        fields[2]
    );
    assert!(matches!(
        &fields[2],
        Field::Text {
            required: false,
            ..
        }
    ));
    assert!(matches!(
        &fields[3],
        Field::Text { default: Some(user), .. } if user == "alix"
    ));
    assert!(matches!(
        &fields[4],
        Field::Secret { description: Some(d), required: true, .. } if d.contains("https://cloud.example")
    ));
    assert!(matches!(&fields[5], Field::Toggle { default: false, .. }));
    let message = message(&shown.card);
    assert!(
        message.contains("Elle pourra joindre : https://cloud.example."),
        "{message}"
    );
    assert!(
        message.contains("envoyé seulement à : https://cloud.example."),
        "{message}"
    );
    assert!(
        message.contains("URL suggérées par l'agent : vérifie-les"),
        "{message}"
    );
    // Only the admin answers it: neither another user, nor the admin's own name without the
    // admin's rights.
    for not_admin in [
        User {
            name: "lea".into(),
            admin: false,
        },
        User {
            name: "alix".into(),
            admin: false,
        },
    ] {
        assert_eq!(
            bench
                .requests
                .answer(shown.id, Response::Decline, &not_admin)
                .await,
            Err(Refusal::NotYours),
            "{not_admin:?}"
        );
    }
    bench
        .requests
        .answer(shown.id, Response::Decline, &alix())
        .await
        .unwrap();
    assert_eq!(result(call).await["result"]["status"], "declined");
    assert_eq!(bench.config_text(), CONFIG);
}

/// Answered, the entry goes to the end of `fronde.toml`, every comment and the file's mode
/// kept, its password named as a stored secret; the file reads back with it.
#[cfg(unix)]
#[tokio::test]
async fn setup_writes_toml_keeping_comments() {
    use std::os::unix::fs::PermissionsExt as _;
    let mut bench = bench_with(|dir| {
        std::fs::set_permissions(
            dir.join("fronde.toml"),
            std::fs::Permissions::from_mode(0o640),
        )
        .unwrap();
    })
    .await;
    let call = bench.call(context("alix"), calendar_prefill());
    let shown = bench.card().await;
    bench
        .requests
        .answer(shown.id, calendar_answer(EVENTS, REMINDERS, false), &alix())
        .await
        .unwrap();
    let out = result(call).await;
    assert_eq!(out["result"]["status"], "set_up", "{out}");
    assert_eq!(out["result"]["config"], "written", "{out}");
    let text = bench.config_text();
    assert!(text.starts_with(CONFIG), "{text}");
    assert!(text.contains("# never written here"), "{text}");
    assert!(
        text.contains("# Added by Fronde from a setup card in the chat."),
        "{text}"
    );
    assert!(
        text.contains(r#"password = { stored = "calendar_caldav" }"#),
        "{text}"
    );
    assert!(
        text.contains(r#"allow = ["https://cloud.example"]"#),
        "{text}"
    );
    assert!(!text.contains(CANARY));
    assert_eq!(mode(&bench.config), 0o640, "the mode is kept");
    let read = fronde_hub::config::load(&bench.config).unwrap();
    let Some(SourceConfig::Component(entry)) = read.sources.first() else {
        panic!("the entry reads back: {:?}", read.sources);
    };
    assert_eq!(entry.name, "calendar");
    assert_eq!(entry.settings["events_url"], EVENTS);
    // Set up, it is offered no more.
    assert!(
        bench
            .integrations
            .available()
            .iter()
            .all(|i| i.name != "calendar")
    );
}

/// Writes nothing, as a read-only mount refuses it, whoever runs the test (root writes through
/// a mode).
fn read_only(_: &Path, _: &str, _: u32) -> std::io::Result<()> {
    Err(std::io::Error::from(std::io::ErrorKind::ReadOnlyFilesystem))
}

/// A configuration Fronde cannot write (a read-only mount in Docker) is left as it is: the
/// owner gets the entry to paste on a card of its own, with the stored secret's name, never its
/// value, and the integration runs until the next start.
#[tokio::test]
async fn setup_read_only_config_gives_snippet() {
    let mut bench = bench_writing(|_| {}, Some(read_only)).await;
    let call = bench.call(context("alix"), calendar_prefill());
    let shown = bench.card().await;
    bench
        .requests
        .answer(shown.id, calendar_answer(EVENTS, REMINDERS, false), &alix())
        .await
        .unwrap();
    let out = result(call).await;
    assert_eq!(out["result"]["config"], "not_written", "{out}");
    let snippet = out["result"]["snippet"].as_str().unwrap();
    assert!(snippet.starts_with("# Added by Fronde"), "{snippet}");
    assert!(
        snippet.contains(r#"password = { stored = "calendar_caldav" }"#),
        "{snippet}"
    );
    assert!(!snippet.contains(CANARY));
    assert_eq!(bench.config_text(), CONFIG);
    let paste = bench.card().await;
    assert!(fields(&paste.card).is_empty());
    assert!(message(&paste.card).contains(snippet), "{:?}", paste.card);
    let tools = out["result"]["tools"].as_array().unwrap();
    assert!(!tools.is_empty(), "{out}");
}

/// The password goes from the card to the store, sealed for the origin of the URL settings, and
/// nowhere else: not the tool's output, the configuration, the request's row or any file.
#[tokio::test]
async fn setup_secret_fields_reach_the_store_only() {
    let mut bench = bench().await;
    let call = bench.call(context("alix"), calendar_prefill());
    let shown = bench.card().await;
    bench
        .requests
        .answer(shown.id, calendar_answer(EVENTS, REMINDERS, false), &alix())
        .await
        .unwrap();
    let out = output(call).await;
    assert!(!out.is_error, "{}", out.to_text());
    assert!(!out.to_text().contains(CANARY));
    assert_eq!(
        serde_json::from_str::<Value>(&out.to_text()).unwrap()["result"]["stored_secrets"],
        json!(["calendar_caldav"])
    );
    let db = fronde_db::open(&bench.dir.path().join("data"))
        .await
        .unwrap();
    let stored =
        fronde_wasm::StoredSecrets::load(&db, &Backends::file(bench.dir.path().join("master.key")))
            .await
            .unwrap();
    let origin = Origin::parse("https://cloud.example").unwrap();
    assert_eq!(
        stored
            .open("calendar_caldav", &origin)
            .unwrap()
            .expose_secret(),
        CANARY
    );
    assert!(
        stored
            .open(
                "calendar_caldav",
                &Origin::parse("https://other.example").unwrap()
            )
            .is_err(),
        "sealed for its origin only"
    );
    assert!(
        !bench.every_file().contains(CANARY),
        "the value is in a file"
    );
}

/// A family member's agent asks: the card goes to the admin, says so, and only the admin
/// answers it.
#[tokio::test]
async fn family_setup_goes_to_admin() {
    let mut bench = bench().await;
    let call = bench.call(context("lea"), json!({"integration": "echo"}));
    let shown = bench.card().await;
    assert!(
        message(&shown.card).contains("Demande envoyée à l'administrateur"),
        "{:?}",
        shown.card
    );
    for lea in [
        User {
            name: "lea".into(),
            admin: false,
        },
        User {
            name: "lea".into(),
            admin: true,
        },
    ] {
        assert_eq!(
            bench
                .requests
                .answer(shown.id, Response::Decline, &lea)
                .await,
            Err(Refusal::NotYours)
        );
    }
    bench
        .requests
        .answer(shown.id, Response::Decline, &alix())
        .await
        .unwrap();
    assert_eq!(result(call).await["result"]["status"], "declined");
}

/// Once answered, the component runs and its tools are in the registry, which the tool returns;
/// `search_tools` listed it as available before, and no longer does.
#[tokio::test]
async fn setup_tools_appear_after_answer() {
    let mut bench = bench().await;
    let found: Value = serde_json::from_str(
        &bench
            .hub
            .registry
            .call_as(
                &context("alix"),
                "search_tools",
                json!({"need": "echo some text"}),
            )
            .await
            .to_text(),
    )
    .unwrap();
    let listed = found["result"].as_array().unwrap();
    assert!(
        listed.iter().any(|item| item["integration"] == "echo"
            && item["status"] == fronde_hub::integrations::AVAILABLE),
        "{found}"
    );
    assert!(bench.hub.registry.get("echo_text").is_none());
    let call = bench.call(context("alix"), json!({"integration": "echo"}));
    let shown = bench.card().await;
    assert!(!shown.card.asks_secret());
    bench
        .requests
        .answer(
            shown.id,
            Response::Form {
                values: Default::default(),
            },
            &alix(),
        )
        .await
        .unwrap();
    let out = result(call).await;
    let tools: Vec<&str> = out["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(!tools.is_empty(), "{out}");
    assert!(tools.iter().all(|tool| tool.starts_with("echo_")), "{out}");
    for tool in &tools {
        assert_eq!(bench.hub.registry.get(tool).unwrap().source, "echo");
    }
    let found = bench
        .hub
        .registry
        .call_as(
            &context("alix"),
            "search_tools",
            json!({"need": "echo some text"}),
        )
        .await
        .to_text();
    assert!(!found.contains(r#""integration":"echo""#), "{found}");
    // Asked again, it is set up already.
    let again = output(bench.call(context("alix"), json!({"integration": "echo"}))).await;
    assert!(
        again.is_error && again.to_text().contains("no integration"),
        "{}",
        again.to_text()
    );
}

/// `prefill` fills non-secret fields only, each checked against the manifest's schema: a
/// secret's key, an unknown setting, a value out of the schema and a URL holding a password
/// are refused before any card.
#[tokio::test]
async fn prefill_sets_no_secret_and_no_value_outside_the_schema() {
    let mut bench = bench().await;
    let cases = [
        (json!({"caldav.secret": "x"}), "caldav.secret"),
        (json!({"caldav.password": "x"}), "never a password"),
        (json!({"password": "x"}), "never a password"),
        (json!({"events_url": "not a url"}), "not a URL"),
        (
            json!({"events_url": "https://alix:hunter2@cloud.example/dav/"}),
            "user name or password",
        ),
        (json!({"caldav.username": "a:b"}), "':'"),
        (
            json!({"events_url": "https://cloud.example/\u{202e}x"}),
            "plain text",
        ),
    ];
    for (prefill, why) in cases {
        let out = output(bench.call(
            context("alix"),
            json!({"integration": "calendar", "prefill": prefill}),
        ))
        .await;
        assert!(out.is_error, "{prefill}: {}", out.to_text());
        assert!(out.to_text().contains(why), "{prefill}: {}", out.to_text());
        assert!(!out.to_text().contains("hunter2"), "{}", out.to_text());
    }
    bench.no_card();
    // Only Fronde's agents, during their turn, may ask.
    for context in [
        CallContext {
            caller: Caller::Mcp("Codex".into()),
            ..context("alix")
        },
        CallContext {
            turn: None,
            ..context("alix")
        },
    ] {
        let out = output(bench.call(context, calendar_prefill())).await;
        assert!(out.is_error, "{}", out.to_text());
    }
    let unknown = output(bench.call(context("alix"), json!({"integration": "github"}))).await;
    assert!(
        unknown
            .to_text()
            .contains("no integration \\\"github\\\" is available"),
        "{}",
        unknown.to_text()
    );
    bench.no_card();
}

/// The dry run refuses an entry that allows an origin its manifest does not ask for, and a
/// local address not allowed as local.
#[tokio::test]
async fn dry_run_refuses_an_origin_outside_the_manifest() {
    let bench = bench().await;
    let entry = |allow: &str, local: bool| ComponentConfig {
        name: "calendar".into(),
        path: fronde_hub::config::shipped_component("calendar").unwrap(),
        manifest: None,
        sha256: None,
        manifest_sha256: None,
        allow: vec![Allow {
            origin: Origin::parse(allow).unwrap(),
            local,
        }],
        settings: [
            ("events_url".to_owned(), EVENTS.to_owned()),
            ("reminders_url".to_owned(), REMINDERS.to_owned()),
        ]
        .into(),
        credentials: Vec::new(),
    };
    bench
        .integrations
        .dry_run(&entry("https://cloud.example", false))
        .await
        .unwrap();
    let error = bench
        .integrations
        .dry_run(&entry("https://evil.example", false))
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("which the manifest does not ask for"),
        "{error:#}"
    );
}

/// A server on the local network needs the card's toggle: unticked, nothing is kept or written;
/// ticked, a confirmation card lists the origins allowed as local, and once confirmed the entry
/// allows its origin as local.
#[tokio::test]
async fn a_local_server_needs_the_toggle() {
    const LOCAL: &str = "https://192.168.1.10/dav/personal/";
    let mut bench = bench().await;
    let prefill = json!({
        "integration": "calendar",
        "prefill": {"events_url": LOCAL, "reminders_url": LOCAL},
    });
    let call = bench.call(context("alix"), prefill.clone());
    let shown = bench.card().await;
    bench
        .requests
        .answer(shown.id, calendar_answer(LOCAL, LOCAL, false), &alix())
        .await
        .unwrap();
    let out = output(call).await;
    assert!(
        out.is_error && out.to_text().contains("local = true"),
        "{}",
        out.to_text()
    );
    assert_eq!(bench.config_text(), CONFIG);
    assert!(
        bench
            .requests
            .stored_origin("calendar_caldav")
            .await
            .unwrap()
            .is_none(),
        "nothing kept"
    );
    let call = bench.call(context("alix"), prefill);
    let shown = bench.card().await;
    bench
        .requests
        .answer(shown.id, calendar_answer(LOCAL, LOCAL, true), &alix())
        .await
        .unwrap();
    let confirm = bench.card().await;
    let text = message(&confirm.card);
    assert!(
        text.contains(
            "Adresses locales permises (cette machine ou son réseau) pour : https://192.168.1.10."
        ),
        "{text}"
    );
    assert!(
        bench
            .requests
            .stored_origin("calendar_caldav")
            .await
            .unwrap()
            .is_none(),
        "nothing kept before the confirmation"
    );
    bench
        .requests
        .answer(confirm.id, confirmed(), &alix())
        .await
        .unwrap();
    let out = result(call).await;
    assert_eq!(out["result"]["status"], "set_up", "{out}");
    let text = bench.config_text();
    assert!(
        text.contains(r#"allow = [{ origin = "https://192.168.1.10", local = true }]"#),
        "{text}"
    );
}

/// When the answer sends the password elsewhere than the card said (the admin typed another
/// server), the admin confirms the new origins on a second card before anything is kept.
#[tokio::test]
async fn another_origin_than_shown_is_confirmed() {
    const OTHER: &str = "https://dav.example.org/calendars/alix/";
    let mut bench = bench().await;
    let call = bench.call(context("alix"), calendar_prefill());
    let shown = bench.card().await;
    bench
        .requests
        .answer(shown.id, calendar_answer(OTHER, OTHER, false), &alix())
        .await
        .unwrap();
    let confirm = bench.card().await;
    assert!(fields(&confirm.card).is_empty());
    let text = message(&confirm.card);
    assert!(
        text.contains("ira seulement à : https://dav.example.org."),
        "{text}"
    );
    assert!(
        bench
            .requests
            .stored_origin("calendar_caldav")
            .await
            .unwrap()
            .is_none(),
        "nothing kept before the confirmation"
    );
    bench
        .requests
        .answer(confirm.id, Response::Decline, &alix())
        .await
        .unwrap();
    let out = output(call).await;
    assert!(
        out.is_error && out.to_text().contains("did not confirm"),
        "{}",
        out.to_text()
    );
    assert_eq!(bench.config_text(), CONFIG);
}

/// The calendar's answer with the password `password`.
fn calendar_answer_with(password: &str) -> Response {
    let Response::Form { mut values } = calendar_answer(EVENTS, REMINDERS, false) else {
        unreachable!()
    };
    values.insert("caldav.secret".to_owned(), secret(password));
    Response::Form { values }
}

/// The stored secret `name`, opened for `origin`.
async fn stored(bench: &Bench, name: &str, origin: &str) -> Option<String> {
    let db = fronde_db::open(&bench.dir.path().join("data"))
        .await
        .unwrap();
    let stored =
        fronde_wasm::StoredSecrets::load(&db, &Backends::file(bench.dir.path().join("master.key")))
            .await
            .unwrap();
    stored
        .open(name, &Origin::parse(origin).unwrap())
        .ok()
        .map(|value| value.expose_secret().to_owned())
}

/// The toggle for local addresses applies only to an origin that looks local: ticked for a
/// server on the Internet, the setup is refused, saying why, before any card or secret.
#[tokio::test]
async fn the_local_toggle_is_refused_for_a_public_host() {
    let mut bench = bench().await;
    let call = bench.call(context("alix"), calendar_prefill());
    let shown = bench.card().await;
    bench
        .requests
        .answer(shown.id, calendar_answer(EVENTS, REMINDERS, true), &alix())
        .await
        .unwrap();
    let out = output(call).await;
    assert!(out.is_error, "{}", out.to_text());
    assert!(
        out.to_text()
            .contains("https://cloud.example does not look local"),
        "{}",
        out.to_text()
    );
    bench.no_card();
    assert_eq!(bench.config_text(), CONFIG);
    assert!(
        bench
            .requests
            .stored_origin("calendar_caldav")
            .await
            .unwrap()
            .is_none()
    );
}

/// An entry of the same name added by hand to `fronde.toml` while the card was open is
/// refused: Fronde would not start with two. The file stays as the owner wrote it, and the
/// integration is no longer offered.
#[tokio::test]
async fn a_hand_added_entry_of_the_same_name_is_refused() {
    let mut bench = bench().await;
    let call = bench.call(context("alix"), json!({"integration": "echo"}));
    let shown = bench.card().await;
    let by_hand = format!(
        "{CONFIG}\n[[sources]]\nkind = \"component\"\nname = \"echo\"\npath = {:?}\n",
        fronde_hub::config::shipped_component("echo")
            .unwrap()
            .display()
            .to_string()
    );
    std::fs::write(&bench.config, &by_hand).unwrap();
    bench
        .requests
        .answer(shown.id, confirmed(), &alix())
        .await
        .unwrap();
    let out = output(call).await;
    assert!(
        out.is_error
            && out
                .to_text()
                .contains("already has a [[sources]] entry named echo"),
        "{}",
        out.to_text()
    );
    assert_eq!(bench.config_text(), by_hand);
    fronde_hub::config::load(&bench.config).unwrap();
    assert!(
        bench
            .integrations
            .available()
            .iter()
            .all(|i| i.name != "echo")
    );
    assert!(bench.hub.registry.get("echo_text").is_none());
}

/// A configuration whose entry would not pass `config::load`'s checks is never written: here
/// the file itself no longer loads (an edit by hand meanwhile), so nothing is added to it.
#[tokio::test]
async fn an_entry_is_written_only_to_a_file_that_still_loads() {
    let mut bench = bench().await;
    let call = bench.call(context("alix"), json!({"integration": "echo"}));
    let shown = bench.card().await;
    let broken =
        format!("{CONFIG}\n[[sources]]\nkind = \"mcp\"\nname = \"hub\"\ncommand = [\"x\"]\n");
    std::fs::write(&bench.config, &broken).unwrap();
    bench
        .requests
        .answer(shown.id, confirmed(), &alix())
        .await
        .unwrap();
    let out = output(call).await;
    assert!(
        out.is_error && out.to_text().contains("no longer loads"),
        "{}",
        out.to_text()
    );
    assert!(out.to_text().contains("fronde check"), "{}", out.to_text());
    assert!(!out.to_text().contains("command"), "{}", out.to_text());
    assert_eq!(bench.config_text(), broken);
}

/// `fronde.toml` as a link is never written through: the entry goes to the owner to paste, and
/// the file the link names is unchanged.
#[cfg(unix)]
#[tokio::test]
async fn setup_refuses_to_write_through_a_link() {
    let mut bench = bench_with(|dir| {
        std::fs::rename(dir.join("fronde.toml"), dir.join("real.toml")).unwrap();
        std::os::unix::fs::symlink("real.toml", dir.join("fronde.toml")).unwrap();
    })
    .await;
    let call = bench.call(context("alix"), json!({"integration": "echo"}));
    let shown = bench.card().await;
    bench
        .requests
        .answer(shown.id, confirmed(), &alix())
        .await
        .unwrap();
    let out = result(call).await;
    assert_eq!(out["result"]["config"], "not_written", "{out}");
    let real = bench.config.parent().unwrap().join("real.toml");
    assert_eq!(std::fs::read_to_string(real).unwrap(), CONFIG);
    assert!(
        std::fs::symlink_metadata(&bench.config)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let paste = bench.card().await;
    assert!(
        message(&paste.card).contains("[[sources]]"),
        "{:?}",
        paste.card
    );
}

/// Two setups of one integration answered at once: one sets it up, the other is refused before
/// it stores anything, so the stored secret is the one the entry was set up with.
#[tokio::test]
async fn concurrent_setups_never_store_over_each_other() {
    let mut bench = bench().await;
    let first = bench.call(context("alix"), calendar_prefill());
    let first_card = bench.card().await;
    let second = bench.call(context("alix"), calendar_prefill());
    let second_card = bench.card().await;
    // The later one may see the first one's secret stored before its own turn, and ask to
    // confirm replacing it: confirmed, it still finds the integration set up meanwhile.
    let mut cards = std::mem::replace(&mut bench.cards, mpsc::unbounded_channel().1);
    let confirming = tokio::spawn({
        let requests = bench.requests.clone();
        async move {
            while let Some(card) = cards.recv().await {
                let _ = requests.answer(card.id, confirmed(), &alix()).await;
            }
        }
    });
    let admin = alix();
    let (answered_first, answered_second) = tokio::join!(
        bench.requests.answer(
            first_card.id,
            calendar_answer_with("first-password"),
            &admin
        ),
        bench.requests.answer(
            second_card.id,
            calendar_answer_with("second-password"),
            &admin
        ),
    );
    answered_first.unwrap();
    answered_second.unwrap();
    let (first, second) = (output(first).await, output(second).await);
    let winner = match (first.is_error, second.is_error) {
        (false, true) => {
            assert!(
                second.to_text().contains("set up meanwhile"),
                "{}",
                second.to_text()
            );
            "first-password"
        }
        (true, false) => {
            assert!(
                first.to_text().contains("set up meanwhile"),
                "{}",
                first.to_text()
            );
            "second-password"
        }
        _ => panic!("{} / {}", first.to_text(), second.to_text()),
    };
    confirming.abort();
    assert_eq!(
        stored(&bench, "calendar_caldav", "https://cloud.example")
            .await
            .as_deref(),
        Some(winner)
    );
    assert_eq!(bench.config_text().matches("[[sources]]").count(), 1);
}

/// A secret already stored under the name the setup keeps its secret under, for the same
/// origin, is replaced: the card says so before anything is kept.
#[tokio::test]
async fn the_card_says_which_stored_secret_it_replaces() {
    let mut bench = bench().await;
    bench
        .requests
        .keep_secret(
            "calendar_caldav",
            "https://cloud.example",
            &secrecy::SecretString::from("old-password"),
            false,
        )
        .await
        .unwrap();
    let call = bench.call(context("alix"), calendar_prefill());
    let shown = bench.card().await;
    let text = message(&shown.card);
    assert!(
        text.contains(
            "remplace le secret « calendar_caldav » déjà gardé pour https://cloud.example."
        ),
        "{text}"
    );
    bench
        .requests
        .answer(shown.id, calendar_answer(EVENTS, REMINDERS, false), &alix())
        .await
        .unwrap();
    let out = result(call).await;
    assert_eq!(out["result"]["status"], "set_up", "{out}");
    assert_eq!(
        stored(&bench, "calendar_caldav", "https://cloud.example")
            .await
            .as_deref(),
        Some(CANARY)
    );
}

/// A user name typed on the card is checked as `prefill`'s are: nothing is kept for one that
/// holds `:` or a hidden character.
#[tokio::test]
async fn a_typed_user_name_is_checked() {
    let mut bench = bench().await;
    for (username, why) in [("al:ix", "':'"), ("al\nix", "one line")] {
        let call = bench.call(context("alix"), calendar_prefill());
        let shown = bench.card().await;
        let Response::Form { mut values } = calendar_answer(EVENTS, REMINDERS, false) else {
            unreachable!()
        };
        values.insert("caldav.username".to_owned(), text(username));
        bench
            .requests
            .answer(shown.id, Response::Form { values }, &alix())
            .await
            .unwrap();
        let out = output(call).await;
        assert!(
            out.is_error && out.to_text().contains(why),
            "{username:?}: {}",
            out.to_text()
        );
    }
    assert_eq!(bench.config_text(), CONFIG);
    assert!(
        bench
            .requests
            .stored_origin("calendar_caldav")
            .await
            .unwrap()
            .is_none()
    );
}

/// Confirmed, another origin than the card showed gets the secret, sealed for it alone.
#[tokio::test]
async fn a_confirmed_new_origin_stores_the_secret_for_it_only() {
    const OTHER: &str = "https://dav.example.org/calendars/alix/";
    let mut bench = bench().await;
    let call = bench.call(context("alix"), calendar_prefill());
    let shown = bench.card().await;
    bench
        .requests
        .answer(shown.id, calendar_answer(OTHER, OTHER, false), &alix())
        .await
        .unwrap();
    let confirm = bench.card().await;
    bench
        .requests
        .answer(confirm.id, confirmed(), &alix())
        .await
        .unwrap();
    let out = result(call).await;
    assert_eq!(out["result"]["status"], "set_up", "{out}");
    assert_eq!(
        stored(&bench, "calendar_caldav", "https://dav.example.org")
            .await
            .as_deref(),
        Some(CANARY)
    );
    assert_eq!(
        stored(&bench, "calendar_caldav", "https://cloud.example").await,
        None,
        "not for the origin the card first showed"
    );
    let text = bench.config_text();
    assert!(
        text.contains(r#"allow = ["https://dav.example.org"]"#),
        "{text}"
    );
    assert!(!text.contains("cloud.example"), "{text}");
}

/// A call cut as the answer comes keeps nothing: no secret stored, nothing written.
#[tokio::test]
async fn a_cut_call_stores_and_writes_nothing() {
    let mut bench = bench().await;
    let context = context("alix");
    let token = context.cancelled.clone();
    let call = bench.call(context, calendar_prefill());
    let shown = bench.card().await;
    *bench.cut.lock().unwrap() = Some(token);
    bench
        .requests
        .answer(shown.id, calendar_answer(EVENTS, REMINDERS, false), &alix())
        .await
        .unwrap();
    let out = output(call).await;
    assert!(
        out.is_error && out.to_text().contains("cut"),
        "{}",
        out.to_text()
    );
    bench.no_card();
    assert_eq!(bench.config_text(), CONFIG);
    assert!(
        bench
            .requests
            .stored_origin("calendar_caldav")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        bench
            .integrations
            .available()
            .iter()
            .any(|i| i.name == "calendar")
    );
}

/// The setup card is tied to its call: after a restart nobody would act on its answer, so the
/// requests opened again expire it, and an answer is refused.
#[tokio::test]
async fn a_restart_expires_the_setup_card() {
    let mut bench = bench().await;
    let call = bench.call(context("alix"), calendar_prefill());
    let shown = bench.card().await;
    // Fronde starts again on the same database.
    let db = fronde_db::open(&bench.dir.path().join("data"))
        .await
        .unwrap();
    let (send, _cards) = mpsc::unbounded_channel();
    let journal = Cards::new(send);
    let closed = journal.closed.clone();
    let store = SecretStore::new(
        Backends::file(bench.dir.path().join("master.key")),
        Arc::new(EphemeralSecrets::new()),
    );
    let restarted = Requests::open_with(db, Arc::new(journal), Some(store))
        .await
        .unwrap();
    assert!(
        matches!(closed.lock().unwrap().as_slice(), [(id, Outcome::Expired)] if *id == shown.id),
        "{:?}",
        closed.lock().unwrap()
    );
    assert_eq!(
        restarted
            .answer(shown.id, calendar_answer(EVENTS, REMINDERS, false), &alix())
            .await,
        Err(Refusal::Closed)
    );
    call.abort();
    assert_eq!(bench.config_text(), CONFIG);
    assert!(
        restarted
            .stored_origin("calendar_caldav")
            .await
            .unwrap()
            .is_none()
    );
}

/// The dry run takes only the files shipped with this build: changed files on disk are refused,
/// even pinned by their hashes in the entry or approved by the owner for that name.
#[tokio::test]
async fn dry_run_takes_only_the_shipped_files() {
    let bench = bench().await;
    let shipped = fronde_hub::config::shipped_component("echo").unwrap();
    let copy = bench.dir.path().join("changed");
    std::fs::create_dir(&copy).unwrap();
    let wasm = copy.join("echo.wasm");
    std::fs::copy(&shipped, &wasm).unwrap();
    let mut manifest = std::fs::read_to_string(shipped.with_extension("toml")).unwrap();
    let entry = |sha256: Option<String>, manifest_sha256: Option<String>| ComponentConfig {
        name: "echo".into(),
        path: wasm.clone(),
        manifest: None,
        sha256,
        manifest_sha256,
        allow: Vec::new(),
        settings: Default::default(),
        credentials: Vec::new(),
    };
    // The same files elsewhere are the shipped ones.
    std::fs::write(copy.join("echo.toml"), &manifest).unwrap();
    bench
        .integrations
        .dry_run(&entry(None, None))
        .await
        .unwrap();
    // Changed, they are refused, however else they would be approved.
    manifest.push_str("\n# changed on disk\n");
    std::fs::write(copy.join("echo.toml"), &manifest).unwrap();
    let hashes = fronde_wasm::Hashes::of(&entry(None, None)).unwrap();
    let pinned = entry(
        Some(hashes.sha256.clone()),
        Some(hashes.manifest_sha256.clone()),
    );
    let error = bench.integrations.dry_run(&pinned).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("not the ones shipped with this build"),
        "{error:#}"
    );
    let db = fronde_db::open(&bench.dir.path().join("data"))
        .await
        .unwrap();
    fronde_db::approvals::add(&db, "echo", &hashes.sha256, &hashes.manifest_sha256)
        .await
        .unwrap();
    let error = bench
        .integrations
        .dry_run(&entry(None, None))
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("not the ones shipped with this build"),
        "{error:#}"
    );
}

/// An outside MCP client is not told of the integrations it cannot set up; a thread's agent is.
#[tokio::test]
async fn search_tools_lists_integrations_to_threads_only() {
    let bench = bench().await;
    let search = |context: CallContext| {
        let hub = bench.hub.clone();
        async move {
            hub.registry
                .call_as(&context, "search_tools", json!({"need": "echo some text"}))
                .await
                .to_text()
        }
    };
    assert!(
        search(context("alix"))
            .await
            .contains(r#""integration":"echo""#),
        "a thread's agent is told"
    );
    let outside = search(CallContext {
        caller: Caller::Mcp("Codex".into()),
        ..context("alix")
    })
    .await;
    assert!(!outside.contains(r#""integration":"#), "{outside}");
    let anonymous = bench
        .hub
        .registry
        .call("search_tools", json!({"need": "echo some text"}))
        .await
        .to_text();
    assert!(!anonymous.contains(r#""integration":"#), "{anonymous}");
}

/// Without the threads, nothing would check the turn that asks: the setup is not served, and
/// `search_tools` lists no integration.
#[tokio::test]
async fn no_setup_without_the_threads() {
    let dir = tempfile::tempdir().unwrap();
    let etc = dir.path().join("etc");
    std::fs::create_dir(&etc).unwrap();
    let config = etc.join("fronde.toml");
    std::fs::write(&config, CONFIG).unwrap();
    let loaded = fronde_hub::config::load(&config).unwrap();
    fronde_hub::private::prepare_data_dir(&loaded.data.dir).unwrap();
    let db = fronde_db::open(&loaded.data.dir).await.unwrap();
    let (send, _cards) = mpsc::unbounded_channel();
    let requests = Requests::open_with(db.clone(), Arc::new(Cards::new(send)), None)
        .await
        .unwrap();
    let integrations = Arc::new(Shipped::new(&loaded, db.clone()).unwrap());
    let hub = Hub::start(
        loaded,
        db,
        vec![],
        Some(requests),
        None,
        Some(integrations as Arc<dyn Integrations>),
    )
    .await
    .unwrap();
    assert!(hub.registry.get(TOOL).is_none());
    let found = hub
        .registry
        .call_as(
            &context("alix"),
            "search_tools",
            json!({"need": "echo some text"}),
        )
        .await
        .to_text();
    assert!(!found.contains(r#""integration":"#), "{found}");
}

/// Fails the first write only, as a mount read-only at first would.
fn read_only_once(path: &Path, text: &str, mode: u32) -> std::io::Result<()> {
    static FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !FAILED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return read_only(path, text, mode);
    }
    fronde::config_edit::write_atomic(path, text, mode)
}

/// An integration running from an entry the owner was given to paste stays set up when
/// another setup writes `fronde.toml` without it: it is never offered, nor started, twice.
#[tokio::test]
async fn an_entry_running_from_memory_is_never_set_up_twice() {
    let mut bench = bench_writing(|_| {}, Some(read_only_once)).await;
    let call = bench.call(context("alix"), json!({"integration": "echo"}));
    let shown = bench.card().await;
    bench
        .requests
        .answer(shown.id, confirmed(), &alix())
        .await
        .unwrap();
    let out = result(call).await;
    assert_eq!(out["result"]["config"], "not_written", "{out}");
    let paste = bench.card().await;
    assert!(fields(&paste.card).is_empty());
    // The next setup writes the file, which has no echo entry.
    let call = bench.call(context("alix"), calendar_prefill());
    let shown = bench.card().await;
    bench
        .requests
        .answer(shown.id, calendar_answer(EVENTS, REMINDERS, false), &alix())
        .await
        .unwrap();
    let out = result(call).await;
    assert_eq!(out["result"]["config"], "written", "{out}");
    assert!(!bench.config_text().contains("name = \"echo\""));
    // Echo still runs, and is offered no more.
    assert!(
        bench
            .integrations
            .available()
            .iter()
            .all(|i| i.name != "echo")
    );
    let again = output(bench.call(context("alix"), json!({"integration": "echo"}))).await;
    assert!(
        again.is_error && again.to_text().contains("no integration"),
        "{}",
        again.to_text()
    );
    // Nor started again under its name, whatever asks.
    let entry = ComponentConfig {
        name: "echo".into(),
        path: fronde_hub::config::shipped_component("echo").unwrap(),
        manifest: None,
        sha256: None,
        manifest_sha256: None,
        allow: Vec::new(),
        settings: Default::default(),
        credentials: Vec::new(),
    };
    let error = bench
        .integrations
        .add(&entry, &bench.hub.sources)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("echo"), "{error:#}");
}

/// Holds every write until released, as a slow disk would: the setup writing holds its lock.
fn held_write(path: &Path, text: &str, mode: u32) -> std::io::Result<()> {
    let (lock, released) = &HELD;
    let mut state = lock.lock().unwrap();
    state.0 = true;
    released.notify_all();
    while !state.1 {
        state = released.wait(state).unwrap();
    }
    drop(state);
    fronde::config_edit::write_atomic(path, text, mode)
}

/// For [`held_write`]: (a write is held, the writes are released).
static HELD: (std::sync::Mutex<(bool, bool)>, std::sync::Condvar) = (
    std::sync::Mutex::new((false, false)),
    std::sync::Condvar::new(),
);

/// A call cut while it waits for another setup of the same integration stops at once,
/// keeping nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_cut_while_waiting_for_the_lock_stops() {
    let mut bench = bench_writing(|_| {}, Some(held_write)).await;
    let first = bench.call(context("alix"), json!({"integration": "echo"}));
    let first_card = bench.card().await;
    let second_context = context("alix");
    let cut = second_context.cancelled.clone();
    let second = bench.call(second_context, json!({"integration": "echo"}));
    let second_card = bench.card().await;
    bench
        .requests
        .answer(first_card.id, confirmed(), &alix())
        .await
        .unwrap();
    // The first setup holds the lock, writing.
    tokio::task::spawn_blocking(|| {
        let (lock, held) = &HELD;
        let mut state = lock.lock().unwrap();
        while !state.0 {
            state = held.wait(state).unwrap();
        }
    })
    .await
    .unwrap();
    bench
        .requests
        .answer(second_card.id, confirmed(), &alix())
        .await
        .unwrap();
    // Time to reach the lock, then the cut.
    tokio::time::sleep(Duration::from_millis(300)).await;
    cut.cancel();
    let out = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .expect("the cut call still waits for the lock")
        .unwrap();
    assert!(
        out.is_error && out.to_text().contains("cut"),
        "{}",
        out.to_text()
    );
    {
        let (lock, released) = &HELD;
        lock.lock().unwrap().1 = true;
        released.notify_all();
    }
    let out = result(first).await;
    assert_eq!(out["result"]["status"], "set_up", "{out}");
    assert_eq!(bench.config_text().matches("[[sources]]").count(), 1);
}

/// An entry of the same name added by hand while the card is open is found before any secret
/// is stored: the secret stored under the name the card said it replaces keeps its value.
#[tokio::test]
async fn a_hand_added_entry_leaves_the_stored_secrets_alone() {
    let mut bench = bench().await;
    bench
        .requests
        .keep_secret(
            "calendar_caldav",
            "https://cloud.example",
            &secrecy::SecretString::from("old-password"),
            false,
        )
        .await
        .unwrap();
    let call = bench.call(context("alix"), calendar_prefill());
    let shown = bench.card().await;
    assert!(message(&shown.card).contains("remplace le secret"));
    let by_hand = format!(
        "{CONFIG}\n[[sources]]\nkind = \"mcp\"\nname = \"calendar\"\nurl = \"https://mcp.example\"\n"
    );
    std::fs::write(&bench.config, &by_hand).unwrap();
    bench
        .requests
        .answer(shown.id, calendar_answer(EVENTS, REMINDERS, false), &alix())
        .await
        .unwrap();
    let out = output(call).await;
    assert!(
        out.is_error
            && out
                .to_text()
                .contains("already has a [[sources]] entry named calendar"),
        "{}",
        out.to_text()
    );
    assert_eq!(
        stored(&bench, "calendar_caldav", "https://cloud.example")
            .await
            .as_deref(),
        Some("old-password")
    );
    assert_eq!(bench.config_text(), by_hand);
}
