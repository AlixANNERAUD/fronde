//! `integrations_request_setup`: a card built from a shipped component's manifest, answered by
//! the admin, then the secrets sealed, a dry run, the entry written to `fronde.toml` (or shown to
//! paste) and the component started. The components come from `components/dist`, as the build
//! approves them.

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
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

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

/// Hands each card shown to the test, and records every end.
struct Cards(mpsc::UnboundedSender<Shown>);

#[async_trait]
impl Journal for Cards {
    async fn announce(
        &self,
        topic: &str,
        id: RequestId,
        card: &Card,
        _caller: &Caller,
    ) -> anyhow::Result<u64> {
        let _ = self.0.send(Shown {
            topic: topic.to_owned(),
            id,
            card: card.clone(),
        });
        Ok(id)
    }

    async fn close(
        &self,
        _topic: &str,
        _id: RequestId,
        _outcome: &Outcome,
        _by: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

struct Bench {
    hub: Arc<Hub>,
    requests: Arc<Requests>,
    integrations: Arc<Shipped>,
    cards: mpsc::UnboundedReceiver<Shown>,
    config: PathBuf,
    dir: tempfile::TempDir,
}

async fn bench() -> Bench {
    bench_with(|_| {}).await
}

/// A Fronde with the chat's requests and the shipped components to set up; `prepare` runs on
/// the directory holding `fronde.toml` (`etc`, beside `data` and the key) before the configuration is read.
async fn bench_with(prepare: impl FnOnce(&Path)) -> Bench {
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
    let requests = Requests::open_with(db.clone(), Arc::new(Cards(send)), Some(store))
        .await
        .unwrap();
    let integrations = Arc::new(Shipped::new(&loaded, db.clone()).unwrap());
    let hub = Hub::start(
        loaded,
        db,
        vec![],
        Some(requests.clone()),
        None,
        Some(integrations.clone() as Arc<dyn Integrations>),
    )
    .await
    .unwrap();
    Bench {
        hub: Arc::new(hub),
        requests,
        integrations,
        cards,
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

/// The card shows the manifest's settings with the prefill, a user name and a password for its
/// credential, the origins that follow from the URLs, and the toggle for local addresses, off.
/// It is the admin's, and a secret card: refused unless the hub built it.
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
    // Only the admin answers it.
    let lea = User {
        name: "alix".into(),
        admin: false,
    };
    assert_eq!(
        bench
            .requests
            .answer(shown.id, Response::Decline, &lea)
            .await,
        Err(Refusal::NotYours)
    );
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

/// A configuration Fronde cannot write (a read-only mount in Docker) is left as it is: the
/// owner gets the entry to paste on a card of its own, with the stored secret's name, never its
/// value, and the integration runs until the next start.
#[cfg(unix)]
#[tokio::test]
async fn setup_read_only_config_gives_snippet() {
    use std::os::unix::fs::PermissionsExt as _;
    let mut bench = bench().await;
    let config_dir = bench.config.parent().unwrap().to_owned();
    // Neither the file nor its directory can be written, as with a read-only mount.
    std::fs::set_permissions(&bench.config, std::fs::Permissions::from_mode(0o400)).unwrap();
    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let call = bench.call(context("alix"), calendar_prefill());
    let shown = bench.card().await;
    bench
        .requests
        .answer(shown.id, calendar_answer(EVENTS, REMINDERS, false), &alix())
        .await
        .unwrap();
    let out = result(call).await;
    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
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
/// ticked, the entry allows its origin as local.
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
