use async_trait::async_trait;
use fronde_db::ephemeral::{EphemeralScope, EphemeralSecrets};
use fronde_db::master_key::Backends;
use fronde_hub::tools::{CallContext, Registry, ToolOutput};
use fronde_hub::user::user_tools;
use fronde_protocol::{Card, Keep, Outcome, Remember, RequestId, Response, SecretValue};
use fronde_requests::{Caller, Journal, Requests, SecretStore, User};
use secrecy::ExposeSecret as _;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(10);
const CANARY: &str = "canary-user-secret-3a9c0e";

/// Hands each card shown to the test.
struct Shown(mpsc::UnboundedSender<(RequestId, Card, Option<String>)>);

#[async_trait]
impl Journal for Shown {
    async fn announce(
        &self,
        _topic: &str,
        id: RequestId,
        card: &Card,
        caller: &Caller,
    ) -> anyhow::Result<u64> {
        let _ = self
            .0
            .send((id, card.clone(), caller.from().map(str::to_owned)));
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
    registry: Arc<Registry>,
    requests: Arc<Requests>,
    ephemeral: Arc<EphemeralSecrets>,
    shown: mpsc::UnboundedReceiver<(RequestId, Card, Option<String>)>,
    _dir: tempfile::TempDir,
}

async fn bench() -> Bench {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let db = fronde_db::open(&data).await.unwrap();
    let (send, shown) = mpsc::unbounded_channel();
    let ephemeral = Arc::new(EphemeralSecrets::new());
    let store = SecretStore::new(
        Backends::file(dir.path().join("master.key")),
        ephemeral.clone(),
    );
    let requests = Requests::open_with(db, Arc::new(Shown(send)), Some(store))
        .await
        .unwrap();
    let registry = Arc::new(Registry::new());
    assert_eq!(
        registry.replace_source("hub", user_tools(requests.clone(), None)),
        2
    );
    Bench {
        registry,
        requests,
        ephemeral,
        shown,
        _dir: dir,
    }
}

fn context() -> CallContext {
    CallContext {
        caller: Caller::Thread(fronde_requests::ThreadId::MAIN),
        user: "alix".into(),
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

fn args() -> Value {
    json!({
        "label": "Mot de passe Nextcloud",
        "reason": "Pour lire ton agenda.",
    })
}

fn with(extra: Value) -> Value {
    let mut args = args();
    for (key, value) in extra.as_object().unwrap() {
        args[key] = value.clone();
    }
    args
}

fn secret(value: &str, keep: Keep) -> Response {
    Response::Secret {
        value: SecretValue::new(value.into()),
        keep,
    }
}

impl Bench {
    fn call(&self, context: CallContext, args: Value) -> tokio::task::JoinHandle<ToolOutput> {
        let registry = self.registry.clone();
        tokio::spawn(async move { registry.call_as(&context, "user_secret", args).await })
    }

    async fn card(&mut self) -> (RequestId, Card, Option<String>) {
        tokio::time::timeout(WAIT, self.shown.recv())
            .await
            .unwrap()
            .unwrap()
    }
}

async fn output(call: tokio::task::JoinHandle<ToolOutput>) -> ToolOutput {
    tokio::time::timeout(WAIT, call).await.unwrap().unwrap()
}

async fn result(call: tokio::task::JoinHandle<ToolOutput>) -> Value {
    serde_json::from_str(&output(call).await.to_text()).unwrap()
}

#[tokio::test]
async fn user_secret_validates_its_arguments() {
    let mut bench = bench().await;
    let cases = [
        (json!({"label": " "}), "label is empty"),
        (json!({"reason": ""}), "reason is empty"),
        (
            json!({"label": "x".repeat(61)}),
            "label has 61 characters; use at most 60",
        ),
        (
            json!({"reason": "x".repeat(301)}),
            "reason has 301 characters; use at most 300",
        ),
        (
            json!({"placeholder": "x".repeat(61)}),
            "placeholder has 61 characters; use at most 60",
        ),
        (
            json!({"label": "Mot\u{202e}de passe"}),
            "label contains the control character U+202E",
        ),
        (
            json!({"reason": "Pour lire\nton agenda."}),
            "reason contains the control character U+000A",
        ),
        (
            json!({"url": "http://cloud.example/login"}),
            "url must be an https:// address, not http:",
        ),
        (
            json!({"url": "javascript:alert(1)"}),
            "url must be an https:// address, not javascript:",
        ),
        (
            json!({"url": "JavaScript:alert(1)"}),
            "url must be an https:// address, not javascript:",
        ),
        (
            json!({"url": "data:text/html,<script>alert(1)</script>"}),
            "url must be an https:// address, not data:",
        ),
        (json!({"url": "cloud.example/login"}), "is not a URL"),
        (
            json!({"url": "https://cloud.example@evil.example/"}),
            "url must not carry a user name or password",
        ),
        (
            json!({"url": "https://cloud.example/\u{7}"}),
            "url contains the control character U+0007",
        ),
        (
            json!({"remember": {"name": "Nextcloud", "origin": "https://cloud.example"}}),
            "remember.name \"Nextcloud\" is not allowed",
        ),
        (
            json!({"remember": {"name": "nextcloud", "origin": "https://cloud.example/login"}}),
            "remember.origin: origin \"https://cloud.example/login\" must be scheme://host[:port]",
        ),
        (
            json!({"remember": {"name": "nextcloud", "origin": "http://cloud.example"}}),
            "remember.origin http://cloud.example is plain http to another machine",
        ),
        (json!({"timeout_ms": 999}), "timeout_ms 999 is out of range"),
        (
            json!({"client_request_id": ""}),
            "client_request_id must have 1 to 128 characters",
        ),
        (json!({"label": 3}), "invalid arguments at `label`"),
    ];
    for (extra, expected) in cases {
        let out = result(bench.call(context(), with(extra.clone()))).await;
        let error = out["error"]
            .as_str()
            .unwrap_or_else(|| panic!("{extra}: {out}"));
        assert!(error.contains(expected), "{extra}: {error}");
    }
    assert!(bench.shown.try_recv().is_err(), "no card for any of them");

    // An outside client is held to the same names as with user_ask.
    let fronde = CallContext {
        caller: Caller::Mcp("Fronde".into()),
        turn: None,
        ..context()
    };
    let out = result(bench.call(fronde, args())).await;
    assert!(
        out["error"].as_str().unwrap().contains("is Fronde's own"),
        "{out}"
    );
}

#[tokio::test]
async fn user_secret_shows_a_secret_card() {
    let mut bench = bench().await;
    let call = bench.call(
        context(),
        with(json!({
            "placeholder": " xxxx-xxxx ",
            "url": "https://Cloud.Example:443/settings/security",
            "remember": {"name": "nextcloud", "origin": "HTTPS://Cloud.Example:443"},
        })),
    );
    let (id, card, from) = bench.card().await;
    assert_eq!(from, None);
    assert_eq!(
        card,
        Card::Secret {
            label: "Mot de passe Nextcloud".into(),
            reason: "Pour lire ton agenda.".into(),
            placeholder: Some("xxxx-xxxx".into()),
            url: Some("https://cloud.example/settings/security".into()),
            remember: Some(Remember {
                name: "nextcloud".into(),
                origin: "https://cloud.example".into(),
                replaces: false,
            }),
        }
    );
    bench.requests.cancel(id).await.unwrap();
    assert_eq!(result(call).await["result"]["status"], "cancelled");
}

#[tokio::test]
async fn user_secret_returns_a_reference_not_the_value() {
    let mut bench = bench().await;
    let call = bench.call(context(), args());
    let (id, _, _) = bench.card().await;
    bench
        .requests
        .answer(id, secret(CANARY, Keep::Conversation), &alix())
        .await
        .unwrap();
    let out = output(call).await;
    assert!(!out.is_error, "{}", out.to_text());
    let text = out.to_text();
    assert!(!text.contains(CANARY), "{text}");
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["result"]["status"], "saved");
    assert!(value["result"]["expires_at"].is_string(), "{value}");
    assert!(
        value["next"]
            .as_str()
            .unwrap()
            .contains("No tool accepts a secret_ref yet"),
        "{value}"
    );
    let secret_ref = value["result"]["secret_ref"].as_str().unwrap();
    let scope = EphemeralScope {
        topic: "conversation".into(),
        asker: "thread:1".into(),
    };
    assert_eq!(context().secret_scope(), scope);
    assert_eq!(
        bench
            .ephemeral
            .take(secret_ref, &scope)
            .unwrap()
            .expose_secret(),
        CANARY
    );
}

/// A reference given to one thread's agent is refused to another thread's, in its stream or in
/// the first one's: each thread is its own asker.
#[tokio::test]
async fn a_reference_stays_in_its_thread() {
    use fronde_requests::ThreadId;
    let mut bench = bench().await;
    let call = bench.call(context(), args());
    let (id, _, _) = bench.card().await;
    bench
        .requests
        .answer(id, secret(CANARY, Keep::Conversation), &alix())
        .await
        .unwrap();
    let value = result(call).await;
    let secret_ref = value["result"]["secret_ref"].as_str().unwrap();
    let second = CallContext {
        caller: Caller::Thread(ThreadId(2)),
        topic: "thread:2".into(),
        ..context()
    };
    let posing = CallContext {
        caller: Caller::Thread(ThreadId(2)),
        ..context()
    };
    for other in [&second, &posing] {
        assert!(
            bench
                .ephemeral
                .take(secret_ref, &other.secret_scope())
                .is_err()
        );
    }
    let taken = bench
        .ephemeral
        .take(secret_ref, &context().secret_scope())
        .unwrap();
    assert_eq!(taken.expose_secret(), CANARY);
}

#[tokio::test]
async fn user_secret_returns_the_stored_name() {
    let mut bench = bench().await;
    let remember = json!({"remember": {"name": "nextcloud", "origin": "https://cloud.example"}});
    let call = bench.call(context(), with(remember.clone()));
    let (id, card, _) = bench.card().await;
    assert!(
        matches!(&card, Card::Secret { remember: Some(remember), .. } if !remember.replaces),
        "nothing stored yet: {card:?}"
    );
    bench
        .requests
        .answer(id, secret(CANARY, Keep::Stored), &alix())
        .await
        .unwrap();
    let out = output(call).await;
    let text = out.to_text();
    assert!(!text.contains(CANARY), "{text}");
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        value["result"],
        json!({"status": "saved", "stored": "nextcloud", "origin": "https://cloud.example"})
    );
    assert!(
        value["next"]
            .as_str()
            .unwrap()
            .contains(r#"{ stored = "nextcloud" }"#),
        "{value}"
    );

    // The name is taken for that origin: another origin is refused before any card, without
    // learning which origin holds it.
    let elsewhere = json!({"remember": {"name": "nextcloud", "origin": "https://other.example"}});
    let out = result(bench.call(context(), with(elsewhere))).await;
    let error = out["error"].as_str().unwrap();
    assert_eq!(
        error, "remember.name \"nextcloud\" is already used; pick another name",
        "{out}"
    );
    assert!(!error.contains("cloud.example"), "{error}");
    assert!(bench.shown.try_recv().is_err());
    // The same origin replaces it: a card is shown, which says so.
    let call = bench.call(context(), with(remember));
    let (id, card, _) = bench.card().await;
    let Card::Secret {
        remember: Some(remember),
        ..
    } = card
    else {
        panic!("{card:?}");
    };
    assert_eq!(
        remember,
        Remember {
            name: "nextcloud".into(),
            origin: "https://cloud.example".into(),
            replaces: true,
        }
    );
    bench.requests.cancel(id).await.unwrap();
    output(call).await;
}

#[tokio::test]
async fn user_secret_decline_expiry_and_cancel() {
    let mut bench = bench().await;
    let call = bench.call(context(), args());
    let (id, _, _) = bench.card().await;
    bench
        .requests
        .answer(id, Response::Decline, &alix())
        .await
        .unwrap();
    let out = result(call).await;
    assert_eq!(out["result"]["status"], "declined");
    assert!(
        out["next"]
            .as_str()
            .unwrap()
            .contains("do not ask again this turn")
    );

    let call = bench.call(context(), with(json!({"timeout_ms": 1000})));
    bench.card().await;
    assert_eq!(result(call).await["result"]["status"], "expired");

    // An outside client gives up: the card closes.
    let outside = CallContext {
        caller: Caller::Mcp("Codex".into()),
        turn: None,
        ..context()
    };
    let cancelled = outside.cancelled.clone();
    let call = bench.call(outside, args());
    let (id, _, _) = bench.card().await;
    cancelled.cancel();
    assert_eq!(result(call).await["result"]["status"], "cancelled");
    assert_eq!(
        bench
            .requests
            .answer(id, secret(CANARY, Keep::Conversation), &alix())
            .await,
        Err(fronde_requests::Refusal::Closed)
    );
    assert!(bench.ephemeral.is_empty());

    // A thread's call is cut: the card stays open, late; its answer goes to the agent as a
    // message (see the requests' tests).
    let context = context();
    let cancelled = context.cancelled.clone();
    let call = bench.call(context, args());
    let (id, _, _) = bench.card().await;
    cancelled.cancel();
    assert_eq!(result(call).await["result"]["status"], "pending");
    assert!(bench.requests.is_late(id).await.unwrap());
    bench
        .requests
        .answer(id, Response::Decline, &alix())
        .await
        .unwrap();
    assert!(bench.ephemeral.is_empty());
}

#[tokio::test]
async fn user_secret_needs_a_store() {
    let dir = tempfile::tempdir().unwrap();
    let db = fronde_db::open(dir.path()).await.unwrap();
    let (send, _shown) = mpsc::unbounded_channel();
    let requests = Requests::open(db, Arc::new(Shown(send))).await.unwrap();
    let names: Vec<String> = user_tools(requests, None)
        .into_iter()
        .map(|(def, _)| def.name)
        .collect();
    assert_eq!(names, ["user_ask"]);
}
