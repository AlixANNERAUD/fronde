use async_trait::async_trait;
use fronde_hub::source::Tool;
use fronde_hub::tools::{CallContext, Registry, ToolOutput};
use fronde_hub::user::user_tools;
use fronde_protocol::{Card, ChoiceAnswer, ChoiceOption, Field, Outcome, RequestId, Response};
use fronde_requests::{Caller, Journal, Requests, User};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(10);

/// Hands each card shown to the test.
struct Shown(mpsc::UnboundedSender<(RequestId, Card, Option<String>)>);

#[async_trait]
impl Journal for Shown {
    async fn announce(
        &self,
        _topic: &str,
        id: RequestId,
        card: &Card,
        from: Option<&str>,
    ) -> anyhow::Result<u64> {
        let _ = self.0.send((id, card.clone(), from.map(str::to_owned)));
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
    shown: mpsc::UnboundedReceiver<(RequestId, Card, Option<String>)>,
    _dir: tempfile::TempDir,
}

async fn bench() -> Bench {
    let dir = tempfile::tempdir().unwrap();
    let db = fronde_db::open(dir.path()).await.unwrap();
    let (send, shown) = mpsc::unbounded_channel();
    let requests = Requests::open(db, Arc::new(Shown(send))).await.unwrap();
    let registry = Arc::new(Registry::new());
    let tools: Vec<Tool> = user_tools(requests.clone());
    assert_eq!(registry.replace_source("hub", tools), 1);
    Bench {
        registry,
        requests,
        shown,
        _dir: dir,
    }
}

fn context() -> CallContext {
    CallContext {
        caller: Caller::Assistant,
        user: "alix".into(),
        topic: "conversation".into(),
        turn: Some(3),
    }
}

fn alix() -> User {
    User {
        name: "alix".into(),
        admin: true,
    }
}

fn option(label: &str) -> Value {
    json!({"label": label, "description": format!("{label}, en bref")})
}

fn one_question() -> Value {
    json!({"questions": [{
        "question": "Quelle appli installer ?",
        "header": "Appli",
        "options": [option("Mealie"), option("Seerr")],
    }]})
}

impl Bench {
    fn ask(&self, args: Value) -> tokio::task::JoinHandle<ToolOutput> {
        let registry = self.registry.clone();
        tokio::spawn(async move { registry.call_as(&context(), "user_ask", args).await })
    }

    async fn card(&mut self) -> (RequestId, Card, Option<String>) {
        tokio::time::timeout(WAIT, self.shown.recv())
            .await
            .unwrap()
            .unwrap()
    }
}

async fn result(call: tokio::task::JoinHandle<ToolOutput>) -> Value {
    let out = tokio::time::timeout(WAIT, call).await.unwrap().unwrap();
    serde_json::from_str(&out.to_text()).unwrap()
}

#[tokio::test]
async fn user_ask_validates_its_arguments() {
    let mut bench = bench().await;
    let five: Vec<Value> = (0..5)
        .map(|n| {
            json!({"question": format!("Q{n} ?"), "header": "H", "options": [option("a"), option("b")]})
        })
        .collect();
    let cases = [
        (
            json!({"questions": []}),
            "questions has 0 items; give 1 to 4",
        ),
        (
            json!({"questions": five}),
            "questions has 5 items; give 1 to 4",
        ),
        (
            json!({"questions": [{"question": " ", "header": "Appli", "options": [option("a"), option("b")]}]}),
            "question 1: question is empty",
        ),
        (
            json!({"questions": [{"question": "Laquelle ?", "header": "", "options": [option("a"), option("b")]}]}),
            "question 1: header is empty; give a chip of at most 12 characters",
        ),
        (
            json!({"questions": [{"question": "Laquelle ?", "header": "Fournisseur choisi", "options": [option("a"), option("b")]}]}),
            "question 1: header \"Fournisseur choisi\" has 18 characters; use at most 12",
        ),
        (
            json!({"questions": [{"question": "Laquelle ?", "header": "Appli", "options": [option("a")]}]}),
            "question 1 has 1 options; give 2 to 4",
        ),
        (
            json!({"questions": [{"question": "Laquelle ?", "header": "Appli", "options": [option("a"), option("b"), option("c"), option("d"), option("e")]}]}),
            "question 1 has 5 options; give 2 to 4",
        ),
        (
            json!({"questions": [{"question": "Laquelle ?", "header": "Appli", "options": [option("a"), option(" ")]}]}),
            "question 1, option 2: label is empty",
        ),
        (
            json!({"questions": [{"question": "Laquelle ?", "header": "Appli", "options": [option("Mealie"), option("Mealie ")]}]}),
            "question 1: label \"Mealie\" appears twice",
        ),
        (
            json!({"questions": [{"question": "Laquelle ?", "header": "Appli", "options": [
                option("Mealie"),
                {"label": "Seerr", "description": "", "recommended": true},
            ]}]}),
            "question 1: option 2 \"Seerr\" is recommended but not first",
        ),
        (
            json!({"questions": [
                {"question": "Laquelle ?", "header": "A", "options": [option("a"), option("b")]},
                {"question": "Laquelle ?", "header": "B", "options": [option("a"), option("b")]},
            ]}),
            "question 2 repeats question 1",
        ),
        (
            json!({"questions": one_question()["questions"], "timeout_ms": 999}),
            "timeout_ms 999 is out of range; give 1000 to 3600000",
        ),
        (
            json!({"questions": one_question()["questions"], "timeout_ms": 3_600_001}),
            "timeout_ms 3600001 is out of range",
        ),
        (
            json!({"questions": one_question()["questions"], "client_request_id": ""}),
            "client_request_id must have 1 to 128 characters",
        ),
        (
            json!({"questions": [{"question": "Laquelle ?", "header": "Appli"}]}),
            "invalid arguments at `questions[0]`",
        ),
    ];
    for (args, expected) in cases {
        let out = result(bench.ask(args.clone())).await;
        let error = out["error"]
            .as_str()
            .unwrap_or_else(|| panic!("{args}: {out}"));
        assert!(error.contains(expected), "{args}: {error}");
    }
    // No card was shown for any of them.
    assert!(bench.shown.try_recv().is_err());

    // Without knowing who calls, it cannot know whom to ask.
    let out = bench.registry.call("user_ask", one_question()).await;
    assert!(out.is_error);
    assert!(
        out.to_text().contains("needs to know who calls"),
        "{}",
        out.to_text()
    );
}

#[tokio::test]
async fn user_ask_returns_typed_answers() {
    let mut bench = bench().await;
    let call = bench.ask(json!({"questions": [
        {
            "question": "Quelle appli installer ?",
            "header": "Appli",
            "options": [
                {"label": "Mealie", "description": "Recettes", "preview": "[ recette ]", "recommended": true},
                option("Seerr"),
            ],
        },
        {
            "question": "Quels services exposer ?",
            "header": "Services",
            "multi_select": true,
            "options": [option("Jellyfin"), option("Immich"), option("Paperless")],
        },
    ]}));
    let (id, card, from) = bench.card().await;
    assert_eq!(from, None, "the assistant asks as Fronde");
    let Card::Form { fields, .. } = &card;
    assert_eq!(fields.len(), 2);
    let Field::Choice {
        key,
        header,
        options,
        multi,
        other,
        required,
        ..
    } = &fields[0];
    assert_eq!(key, "q1");
    assert_eq!(header.as_deref(), Some("Appli"));
    assert_eq!(
        options[0],
        ChoiceOption {
            label: "Mealie".into(),
            description: Some("Recettes".into()),
            preview: Some("[ recette ]".into()),
            recommended: true,
        }
    );
    assert!(!multi && *other && *required);
    let Field::Choice { multi, .. } = &fields[1];
    assert!(multi);

    // An answer that does not fit is refused; the call keeps waiting.
    let bad = Response::Form {
        values: [(
            "q1".to_owned(),
            ChoiceAnswer {
                selected: vec!["Nextcloud".into()],
                ..ChoiceAnswer::default()
            },
        )]
        .into(),
    };
    assert!(bench.requests.answer(id, bad, &alix()).await.is_err());
    assert!(!call.is_finished());

    let answer = Response::Form {
        values: [
            (
                "q1".to_owned(),
                ChoiceAnswer {
                    selected: vec![],
                    other: Some(" Tandoor ".into()),
                    note: Some("pour la famille".into()),
                },
            ),
            (
                "q2".to_owned(),
                ChoiceAnswer {
                    selected: vec!["Jellyfin".into(), "Immich".into()],
                    other: None,
                    note: Some("  ".into()),
                },
            ),
        ]
        .into(),
    };
    bench.requests.answer(id, answer, &alix()).await.unwrap();
    assert_eq!(
        result(call).await,
        json!({"result": {"status": "answered", "answers": [
            {"question": "Quelle appli installer ?", "selected": [], "other": "Tandoor", "note": "pour la famille"},
            {"question": "Quels services exposer ?", "selected": ["Jellyfin", "Immich"], "other": null, "note": null},
        ]}})
    );
}

#[tokio::test]
async fn user_ask_decline_and_expiry() {
    let mut bench = bench().await;
    let call = bench.ask(one_question());
    let (id, ..) = bench.card().await;
    bench
        .requests
        .answer(id, Response::Decline, &alix())
        .await
        .unwrap();
    assert_eq!(
        result(call).await,
        json!({"result": {"status": "declined"}, "next": "The user declined to answer; do not ask again this turn."})
    );

    let mut short = one_question();
    short["timeout_ms"] = json!(1000);
    let call = bench.ask(short);
    bench.card().await;
    let out = result(call).await;
    assert_eq!(out["result"], json!({"status": "expired"}), "{out}");
    assert!(out["next"].as_str().unwrap().contains("No answer in time"));

    let call = bench.ask(one_question());
    let (id, ..) = bench.card().await;
    bench.requests.cancel(id).await.unwrap();
    let out = result(call).await;
    assert_eq!(out["result"], json!({"status": "cancelled"}), "{out}");
}

#[tokio::test]
async fn user_ask_retry_shows_one_card() {
    let mut bench = bench().await;
    let mut args = one_question();
    args["client_request_id"] = json!("ask-1");
    let first = bench.ask(args.clone());
    let (id, ..) = bench.card().await;
    let retry = bench.ask(args);
    let answer = Response::Form {
        values: [(
            "q1".to_owned(),
            ChoiceAnswer {
                selected: vec!["Seerr".into()],
                ..ChoiceAnswer::default()
            },
        )]
        .into(),
    };
    // Whether the retry joined before or after the answer, it gets the same one.
    bench.requests.answer(id, answer, &alix()).await.unwrap();
    let (first, retry) = (result(first).await, result(retry).await);
    assert_eq!(first, retry);
    assert_eq!(first["result"]["answers"][0]["selected"], json!(["Seerr"]));
    assert!(bench.shown.try_recv().is_err(), "one card only");
}
