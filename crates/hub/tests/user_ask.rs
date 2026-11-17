use async_trait::async_trait;
use fronde_hub::source::Tool;
use fronde_hub::tools::{CallContext, Registry, ToolOutput};
use fronde_hub::user::user_tools;
use fronde_protocol::{Card, ChoiceAnswer, ChoiceOption, Field, Outcome, RequestId, Response};
use fronde_requests::{Caller, Journal, LateAnswer, Requests, ThreadId, User};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(10);

/// Late answers given: to which thread, in which stream.
type Late = Arc<Mutex<Vec<(ThreadId, String, LateAnswer)>>>;

/// Hands each card shown to the test, and keeps the late answers given.
struct Shown(
    mpsc::UnboundedSender<(RequestId, Card, Option<String>)>,
    Late,
);

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

    async fn answered_late(
        &self,
        thread: ThreadId,
        topic: &str,
        answer: LateAnswer,
    ) -> anyhow::Result<()> {
        self.1
            .lock()
            .unwrap()
            .push((thread, topic.to_owned(), answer));
        Ok(())
    }
}

struct Bench {
    registry: Arc<Registry>,
    requests: Arc<Requests>,
    shown: mpsc::UnboundedReceiver<(RequestId, Card, Option<String>)>,
    late: Late,
    _dir: tempfile::TempDir,
}

async fn bench() -> Bench {
    let dir = tempfile::tempdir().unwrap();
    let db = fronde_db::open(dir.path()).await.unwrap();
    let (send, shown) = mpsc::unbounded_channel();
    let late = Late::default();
    let requests = Requests::open(db, Arc::new(Shown(send, late.clone())))
        .await
        .unwrap();
    let registry = Arc::new(Registry::new());
    let tools: Vec<Tool> = user_tools(requests.clone(), None);
    assert_eq!(registry.replace_source("hub", tools), 1);
    Bench {
        registry,
        requests,
        shown,
        late,
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
        (
            json!({"questions": [{"question": "x".repeat(301), "header": "Appli", "options": [option("a"), option("b")]}]}),
            "question 1: question has 301 characters; use at most 300, and put the details in the options' descriptions",
        ),
        (
            json!({"questions": [{"question": "Laquelle ?", "header": "Appli", "options": [option(&"a".repeat(61)), option("b")]}]}),
            "question 1, option 1: label has 61 characters; use at most 60, and put the details in description",
        ),
        (
            json!({"questions": [{"question": "Laquelle ?", "header": "Appli", "options": [
                {"label": "a", "description": "d".repeat(201)},
                option("b"),
            ]}]}),
            "question 1, option 1: description has 201 characters; use at most 200",
        ),
        (
            json!({"questions": [{"question": "Laquelle ?", "header": "Appli", "options": [
                option("a"),
                {"label": "b", "description": "B", "preview": "p".repeat(4001)},
            ]}]}),
            "question 1, option 2: preview has 4001 characters; use at most 4000",
        ),
        (
            json!({"questions": [{"question": "Laquelle\u{7}?", "header": "Appli", "options": [option("a"), option("b")]}]}),
            "question 1: question contains the control character U+0007; remove it (write it on one line",
        ),
        (
            json!({"questions": [{"question": "Laquelle ?\nVite.", "header": "Appli", "options": [option("a"), option("b")]}]}),
            "question 1: question contains the control character U+000A",
        ),
        (
            json!({"questions": [{"question": "Laquelle ?", "header": "Ap\u{1b}pli", "options": [option("a"), option("b")]}]}),
            "question 1: header contains the control character U+001B",
        ),
        (
            // The card's own value for "Autre…" cannot be a label.
            json!({"questions": [{"question": "Laquelle ?", "header": "Appli", "options": [option("\u{1f}autre"), option("b")]}]}),
            "question 1, option 1: label contains the control character U+001F",
        ),
        (
            json!({"questions": [{"question": "Laquelle ?", "header": "Appli", "options": [
                {"label": "a", "description": "A\u{202e}", "preview": "ok"},
                option("b"),
            ]}]}),
            "question 1, option 1: description contains the control character",
        ),
        (
            json!({"questions": [{"question": "Laquelle ?", "header": "Appli", "options": [
                {"label": "a", "description": "A", "preview": "ligne 1\n\tligne 2\u{0}"},
                option("b"),
            ]}]}),
            "question 1, option 1: preview contains the control character U+0000; remove it (only line breaks and tabs are allowed)",
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
    let Card::Form { fields, .. } = &card else {
        panic!("not a question card: {card:?}");
    };
    assert_eq!(fields.len(), 2);
    let Field::Choice {
        key,
        header,
        options,
        multi,
        other,
        required,
        ..
    } = &fields[0]
    else {
        unreachable!()
    };
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
    let Field::Choice { multi, .. } = &fields[1] else {
        unreachable!()
    };
    assert!(multi);

    // An answer that does not fit is refused; the call keeps waiting.
    let bad = Response::Form {
        values: [(
            "q1".to_owned(),
            ChoiceAnswer {
                selected: vec!["Nextcloud".into()],
                ..ChoiceAnswer::default()
            }
            .into(),
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
                }
                .into(),
            ),
            (
                "q2".to_owned(),
                ChoiceAnswer {
                    selected: vec!["Jellyfin".into(), "Immich".into()],
                    other: None,
                    note: Some("  ".into()),
                }
                .into(),
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
            }
            .into(),
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

#[tokio::test]
async fn user_ask_reused_id_gets_no_old_answer() {
    let mut bench = bench().await;
    let mut args = one_question();
    args["client_request_id"] = json!("q1");
    let first = bench.ask(args.clone());
    let (id, ..) = bench.card().await;
    // Another question under the same id, while the first is pending: refused, no card.
    let mut other = args.clone();
    other["questions"][0]["question"] = json!("Et ensuite ?");
    let out = result(bench.ask(other.clone())).await;
    let error = out["error"].as_str().unwrap_or_else(|| panic!("{out}"));
    assert!(
        error.contains("client_request_id \"q1\" already names another request")
            && error.contains("give a new client_request_id"),
        "{error}"
    );
    assert!(bench.shown.try_recv().is_err());
    let answer = Response::Form {
        values: [(
            "q1".to_owned(),
            ChoiceAnswer {
                selected: vec!["Seerr".into()],
                ..ChoiceAnswer::default()
            }
            .into(),
        )]
        .into(),
    };
    bench.requests.answer(id, answer, &alix()).await.unwrap();
    result(first).await;
    // Once that one is answered, the other question gets its own card, not the old answer.
    let second = bench.ask(other);
    let (second_id, ..) = bench.card().await;
    assert_ne!(second_id, id);
    bench.requests.cancel(second_id).await.unwrap();
    assert_eq!(
        result(second).await["result"],
        json!({"status": "cancelled"})
    );
}

#[tokio::test]
async fn user_ask_preview_keeps_its_lines() {
    let mut bench = bench().await;
    let call = bench.ask(json!({"questions": [{
        "question": "Quelle mise en page ?",
        "header": "Page",
        "options": [
            {"label": "Deux colonnes", "description": "A et B", "preview": "| A | B |\n|\tC |"},
            option("Une colonne"),
        ],
    }]}));
    let (id, card, _) = bench.card().await;
    let Card::Form { fields, .. } = &card else {
        panic!("not a question card: {card:?}");
    };
    let Field::Choice { options, .. } = &fields[0] else {
        unreachable!()
    };
    assert_eq!(options[0].preview.as_deref(), Some("| A | B |\n|\tC |"));
    bench.requests.cancel(id).await.unwrap();
    result(call).await;
}

#[tokio::test]
async fn outside_client_names_are_checked() {
    let mut bench = bench().await;
    for (name, expected) in [
        (
            "Fronde",
            "your MCP client's name \"Fronde\" is Fronde's own",
        ),
        ("fronde ", "is Fronde's own"),
        ("ASSISTANT", "is Fronde's own"),
        ("assistant", "is Fronde's own"),
        (
            "Un client au nom vraiment beaucoup trop long",
            "your MCP client's name has 44 characters, too long for the card",
        ),
        ("Co\u{1f}dex", "control character U+001F"),
    ] {
        let context = CallContext {
            caller: Caller::Mcp(name.into()),
            turn: None,
            ..context()
        };
        let out = bench
            .registry
            .call_as(&context, "user_ask", one_question())
            .await;
        assert!(out.is_error, "{name}: {}", out.to_text());
        let text = out.value.as_str().unwrap();
        assert!(text.contains(expected), "{name}: {text}");
        assert!(
            text.contains("connect with a clientInfo.name of 1 to 40 characters"),
            "{text}"
        );
    }
    assert!(bench.shown.try_recv().is_err(), "no card for any of them");

    // A fair name asks, under that name.
    let context = CallContext {
        caller: Caller::Mcp("Codex".into()),
        turn: None,
        ..context()
    };
    let registry = bench.registry.clone();
    let call =
        tokio::spawn(async move { registry.call_as(&context, "user_ask", one_question()).await });
    let (id, _, from) = bench.card().await;
    assert_eq!(from.as_deref(), Some("Codex"));
    bench.requests.cancel(id).await.unwrap();
    result(call).await;
}

/// The MCP client cuts a thread's call while its question waits: the card stays open, late, and
/// its answer goes to that thread's agent as a message. An outside client's cut call closes its
/// card: nothing could bring it a late answer.
#[tokio::test]
async fn cut_tool_call_answer_is_delivered_late() {
    let mut bench = bench().await;
    let second = CallContext {
        caller: Caller::Thread(ThreadId(2)),
        topic: "thread:2".into(),
        ..context()
    };
    let cut = second.cancelled.clone();
    let call = {
        let registry = bench.registry.clone();
        tokio::spawn(async move { registry.call_as(&second, "user_ask", one_question()).await })
    };
    let (id, _, _) = bench.card().await;
    cut.cancel();
    let out = result(call).await;
    assert_eq!(out["result"]["status"], "pending", "{out}");
    assert!(bench.requests.is_late(id).await.unwrap());
    assert!(bench.late.lock().unwrap().is_empty());
    bench
        .requests
        .answer(id, pick("Seerr"), &alix())
        .await
        .unwrap();
    let late = bench.late.lock().unwrap().clone();
    assert_eq!(late.len(), 1, "{late:?}");
    let (thread, topic, answer) = &late[0];
    assert_eq!((*thread, topic.as_str()), (ThreadId(2), "thread:2"));
    assert_eq!(
        answer.text,
        "Réponse à ta question « Quelle appli installer ? » : Seerr"
    );

    // An outside client's cut call: closed, nothing given.
    let outside = CallContext {
        caller: Caller::Mcp("Codex".into()),
        turn: None,
        ..context()
    };
    let cut = outside.cancelled.clone();
    let call = {
        let registry = bench.registry.clone();
        tokio::spawn(async move { registry.call_as(&outside, "user_ask", one_question()).await })
    };
    let (id, _, _) = bench.card().await;
    cut.cancel();
    assert_eq!(result(call).await["result"]["status"], "cancelled");
    assert_eq!(
        bench.requests.answer(id, pick("Seerr"), &alix()).await,
        Err(fronde_requests::Refusal::Closed)
    );
    assert_eq!(bench.late.lock().unwrap().len(), 1);
}

/// The first question answered with `label`.
fn pick(label: &str) -> Response {
    Response::Form {
        values: [(
            "q1".to_owned(),
            ChoiceAnswer {
                selected: vec![label.into()],
                ..ChoiceAnswer::default()
            }
            .into(),
        )]
        .into(),
    }
}
