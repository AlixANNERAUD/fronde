use async_trait::async_trait;
use fronde_entity::requests;
use fronde_protocol::{Card, ChoiceAnswer, ChoiceOption, Field, Outcome, RequestId, Response};
use fronde_requests::{Ask, Caller, Journal, Refusal, Requests, Resolution, User};
use sea_orm::{ActiveValue::Set, EntityTrait};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TOPIC: &str = "conversation";
const WAIT: Duration = Duration::from_secs(10);

/// What the journal was asked to show.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Shown {
    Announced {
        id: RequestId,
        from: Option<String>,
    },
    Closed {
        id: RequestId,
        outcome: Outcome,
        by: Option<String>,
    },
}

#[derive(Default)]
struct FakeJournal {
    shown: Mutex<Vec<Shown>>,
    refuse: Mutex<bool>,
}

impl FakeJournal {
    fn shown(&self) -> Vec<Shown> {
        self.shown.lock().unwrap().clone()
    }
}

#[async_trait]
impl Journal for FakeJournal {
    async fn announce(
        &self,
        topic: &str,
        id: RequestId,
        _card: &Card,
        from: Option<&str>,
    ) -> anyhow::Result<u64> {
        assert_eq!(topic, TOPIC);
        if *self.refuse.lock().unwrap() {
            anyhow::bail!("disk full");
        }
        let mut shown = self.shown.lock().unwrap();
        shown.push(Shown::Announced {
            id,
            from: from.map(str::to_owned),
        });
        Ok(shown.len() as u64)
    }

    async fn close(
        &self,
        _topic: &str,
        id: RequestId,
        outcome: &Outcome,
        by: Option<&str>,
    ) -> anyhow::Result<()> {
        self.shown.lock().unwrap().push(Shown::Closed {
            id,
            outcome: outcome.clone(),
            by: by.map(str::to_owned),
        });
        Ok(())
    }
}

fn alix() -> User {
    User {
        name: "alix".into(),
        admin: true,
    }
}

fn option(label: &str) -> ChoiceOption {
    ChoiceOption {
        label: label.into(),
        description: None,
        preview: None,
        recommended: false,
    }
}

fn question() -> Card {
    Card::Form {
        message: String::new(),
        fields: vec![Field::Choice {
            key: "q1".into(),
            header: Some("Appli".into()),
            question: "Laquelle ?".into(),
            options: vec![option("Mealie"), option("Seerr")],
            multi: false,
            other: true,
            required: true,
        }],
        submit: None,
    }
}

fn ask() -> Ask {
    Ask {
        topic: TOPIC.into(),
        turn: Some(1),
        caller: Caller::Assistant,
        client_request_id: None,
        card: question(),
        for_user: "alix".into(),
        admin_only: false,
        timeout: Duration::from_secs(600),
    }
}

fn pick(label: &str) -> Response {
    Response::Form {
        values: [(
            "q1".to_owned(),
            ChoiceAnswer {
                selected: vec![label.into()],
                ..ChoiceAnswer::default()
            },
        )]
        .into(),
    }
}

async fn open(dir: &std::path::Path) -> (Arc<Requests>, Arc<FakeJournal>) {
    let db = fronde_db::open(dir).await.unwrap();
    let journal = Arc::new(FakeJournal::default());
    let requests = Requests::open(db, journal.clone()).await.unwrap();
    (requests, journal)
}

async fn row(dir: &std::path::Path, id: RequestId) -> requests::Model {
    let db = fronde_db::open(dir).await.unwrap();
    requests::Entity::find_by_id(id as i64)
        .one(&db)
        .await
        .unwrap()
        .unwrap()
}

async fn resolved(waiter: fronde_requests::Waiter) -> Resolution {
    tokio::time::timeout(WAIT, waiter.wait()).await.unwrap()
}

#[tokio::test]
async fn ask_announces_then_waits() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, journal) = open(dir.path()).await;
    let mut outside = ask();
    outside.caller = Caller::Mcp("Codex".into());
    let waiter = requests.ask(outside).await.unwrap();
    let id = waiter.id();
    assert_eq!(
        journal.shown(),
        [Shown::Announced {
            id,
            from: Some("Codex".into())
        }]
    );
    let saved = row(dir.path(), id).await;
    assert_eq!(saved.status, "pending");
    assert_eq!(saved.caller, "mcp:Codex");
    assert_eq!(saved.kind, "form");
    assert_eq!(saved.turn, Some(1));
    assert_eq!(saved.stream_seq, Some(1));
    assert!(saved.expires_at.is_some());

    let waiting = tokio::spawn(waiter.wait());
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    requests.answer(id, pick("Seerr"), &alix()).await.unwrap();
    assert_eq!(
        tokio::time::timeout(WAIT, waiting).await.unwrap().unwrap(),
        Resolution::Answered(pick("Seerr"))
    );
    assert_eq!(
        journal.shown()[1],
        Shown::Closed {
            id,
            outcome: Outcome::Answered {
                summary: "Seerr".into()
            },
            by: Some("alix".into())
        }
    );
    let saved = row(dir.path(), id).await;
    assert_eq!(saved.status, "answered");
    assert_eq!(saved.answered_by.as_deref(), Some("alix"));
    assert!(saved.resolved_at.is_some());
    assert_eq!(
        serde_json::from_value::<Response>(saved.response.unwrap()).unwrap(),
        pick("Seerr")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_answer_wins() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, journal) = open(dir.path()).await;
    for _ in 0..10 {
        let waiter = requests.ask(ask()).await.unwrap();
        let id = waiter.id();
        let answers: Vec<_> = ["Mealie", "Seerr"]
            .into_iter()
            .map(|label| {
                let requests = requests.clone();
                tokio::spawn(async move { requests.answer(id, pick(label), &alix()).await })
            })
            .collect();
        let mut results = Vec::new();
        for answer in answers {
            results.push(answer.await.unwrap());
        }
        let won: Vec<_> = results.iter().filter(|r| r.is_ok()).collect();
        assert_eq!(won.len(), 1, "{results:?}");
        assert!(results.contains(&Err(Refusal::Closed)), "{results:?}");
        let Resolution::Answered(winner) = resolved(waiter).await else {
            panic!("not answered");
        };
        let stored: Response =
            serde_json::from_value(row(dir.path(), id).await.response.unwrap()).unwrap();
        assert_eq!(stored, winner);
        let closes = journal
            .shown()
            .iter()
            .filter(|shown| matches!(shown, Shown::Closed { id: closed, .. } if *closed == id))
            .count();
        assert_eq!(closes, 1);
    }
}

#[tokio::test]
async fn only_its_user_answers() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, _) = open(dir.path()).await;
    let waiter = requests.ask(ask()).await.unwrap();
    let id = waiter.id();
    let sam = User {
        name: "sam".into(),
        admin: true,
    };
    assert_eq!(
        requests.answer(id, pick("Seerr"), &sam).await,
        Err(Refusal::NotYours)
    );
    assert_eq!(
        requests.answer(id, Response::Decline, &sam).await,
        Err(Refusal::NotYours)
    );
    assert_eq!(row(dir.path(), id).await.status, "pending");
    assert_eq!(
        requests.answer(999, pick("Seerr"), &alix()).await,
        Err(Refusal::Unknown)
    );
    requests.answer(id, pick("Seerr"), &alix()).await.unwrap();
    assert_eq!(resolved(waiter).await, Resolution::Answered(pick("Seerr")));
}

#[tokio::test]
async fn admin_only_needs_the_admin() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, _) = open(dir.path()).await;
    let mut admin_only = ask();
    admin_only.for_user = "sam".into();
    admin_only.admin_only = true;
    let id = requests.ask(admin_only).await.unwrap().id();
    let sam = User {
        name: "sam".into(),
        admin: false,
    };
    assert_eq!(
        requests.answer(id, pick("Seerr"), &sam).await,
        Err(Refusal::NotYours)
    );
    // Being an admin does not make another user's card yours.
    assert_eq!(
        requests.answer(id, pick("Seerr"), &alix()).await,
        Err(Refusal::NotYours)
    );
    assert_eq!(row(dir.path(), id).await.status, "pending");
    let sam_admin = User { admin: true, ..sam };
    requests
        .answer(id, pick("Seerr"), &sam_admin)
        .await
        .unwrap();
}

#[tokio::test]
async fn answer_is_validated() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, journal) = open(dir.path()).await;
    let id = requests.ask(ask()).await.unwrap().id();
    let refused = requests.answer(id, pick("Nextcloud"), &alix()).await;
    let Err(Refusal::Invalid(invalid)) = refused else {
        panic!("{refused:?}");
    };
    assert_eq!(invalid.key.as_deref(), Some("q1"));
    assert_eq!(
        Refusal::Invalid(invalid).to_string(),
        "« Nextcloud » n'est pas une des réponses proposées."
    );
    let two = Response::Form {
        values: [(
            "q1".to_owned(),
            ChoiceAnswer {
                selected: vec!["Mealie".into(), "Seerr".into()],
                ..ChoiceAnswer::default()
            },
        )]
        .into(),
    };
    assert!(matches!(
        requests.answer(id, two, &alix()).await,
        Err(Refusal::Invalid(_))
    ));
    let none = Response::Form {
        values: Default::default(),
    };
    assert!(matches!(
        requests.answer(id, none, &alix()).await,
        Err(Refusal::Invalid(_))
    ));
    let wrong_kind = Response::Permission {
        choice: "allow_once".into(),
    };
    assert!(matches!(
        requests.answer(id, wrong_kind, &alix()).await,
        Err(Refusal::Invalid(_))
    ));
    // Each refusal left the request as it was.
    assert_eq!(row(dir.path(), id).await.status, "pending");
    assert_eq!(journal.shown().len(), 1);
    requests
        .answer(id, Response::Decline, &alix())
        .await
        .unwrap();
    assert_eq!(row(dir.path(), id).await.status, "declined");
}

#[tokio::test]
async fn decline_resolves_as_declined() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, journal) = open(dir.path()).await;
    let waiter = requests.ask(ask()).await.unwrap();
    let id = waiter.id();
    requests
        .answer(id, Response::Decline, &alix())
        .await
        .unwrap();
    assert_eq!(resolved(waiter).await, Resolution::Declined);
    assert_eq!(
        journal.shown()[1],
        Shown::Closed {
            id,
            outcome: Outcome::Declined,
            by: Some("alix".into())
        }
    );
    assert_eq!(row(dir.path(), id).await.response, None);
}

#[tokio::test]
async fn retry_with_client_request_id_reuses_the_request() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, journal) = open(dir.path()).await;
    let mut first = ask();
    first.client_request_id = Some("retry-1".into());
    let one = requests.ask(first.clone()).await.unwrap();
    let two = requests.ask(first.clone()).await.unwrap();
    assert_eq!(one.id(), two.id());
    assert_eq!(journal.shown().len(), 1, "one card");

    // Another caller with the same id gets its own request.
    let mut other = first.clone();
    other.caller = Caller::Mcp("Codex".into());
    let three = requests.ask(other).await.unwrap();
    assert_ne!(three.id(), one.id());

    requests
        .answer(one.id(), pick("Mealie"), &alix())
        .await
        .unwrap();
    assert_eq!(resolved(one).await, Resolution::Answered(pick("Mealie")));
    assert_eq!(resolved(two).await, Resolution::Answered(pick("Mealie")));
    // A retry after the answer gets the answer at once.
    let late = requests.ask(first).await.unwrap();
    assert_eq!(resolved(late).await, Resolution::Answered(pick("Mealie")));
}

#[tokio::test]
async fn timeouts_expire_and_wake_the_waiter() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, journal) = open(dir.path()).await;
    let mut short = ask();
    // Real time: the database pool's own timeouts would fire at once on a paused clock.
    short.timeout = Duration::from_millis(200);
    let waiter = requests.ask(short).await.unwrap();
    let id = waiter.id();
    assert_eq!(resolved(waiter).await, Resolution::Expired);
    assert_eq!(row(dir.path(), id).await.status, "expired");
    assert_eq!(
        journal.shown()[1],
        Shown::Closed {
            id,
            outcome: Outcome::Expired,
            by: None
        }
    );
    assert_eq!(
        requests.answer(id, pick("Seerr"), &alix()).await,
        Err(Refusal::Closed)
    );
}

#[tokio::test]
async fn cancel_turn_cancels_only_that_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, _) = open(dir.path()).await;
    let first = requests.ask(ask()).await.unwrap();
    let mut later = ask();
    later.turn = Some(5);
    let second = requests.ask(later).await.unwrap();
    let mut outside = ask();
    outside.turn = None;
    outside.caller = Caller::Mcp("Codex".into());
    let third = requests.ask(outside).await.unwrap();

    assert_eq!(requests.cancel_turn(TOPIC, 1).await.unwrap(), 1);
    assert_eq!(resolved(first).await, Resolution::Cancelled);
    assert_eq!(row(dir.path(), second.id()).await.status, "pending");
    assert_eq!(row(dir.path(), third.id()).await.status, "pending");
    assert!(requests.cancel(second.id()).await.unwrap());
    assert!(
        !requests.cancel(second.id()).await.unwrap(),
        "already closed"
    );
    assert_eq!(resolved(second).await, Resolution::Cancelled);
}

#[tokio::test]
async fn closed_request_never_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, journal) = open(dir.path()).await;
    let id = requests.ask(ask()).await.unwrap().id();
    assert!(requests.cancel(id).await.unwrap());
    assert_eq!(
        requests.answer(id, pick("Seerr"), &alix()).await,
        Err(Refusal::Closed)
    );
    assert_eq!(
        requests.answer(id, Response::Decline, &alix()).await,
        Err(Refusal::Closed)
    );
    assert!(!requests.cancel(id).await.unwrap());
    assert_eq!(requests.cancel_turn(TOPIC, 1).await.unwrap(), 0);
    assert_eq!(requests.expire_due().await.unwrap(), 0);
    let saved = row(dir.path(), id).await;
    assert_eq!(saved.status, "cancelled");
    assert_eq!(saved.response, None);
    // One close, the first.
    assert_eq!(journal.shown().len(), 2);
}

#[tokio::test]
async fn pending_request_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, _) = open(dir.path()).await;
    let id = requests.ask(ask()).await.unwrap().id();
    drop(requests);

    let (requests, journal) = open(dir.path()).await;
    // Already shown before the restart: not announced again.
    assert_eq!(journal.shown(), []);
    requests.answer(id, pick("Mealie"), &alix()).await.unwrap();
    assert_eq!(row(dir.path(), id).await.status, "answered");
    assert!(matches!(journal.shown()[0], Shown::Closed { .. }));
}

#[tokio::test]
async fn unannounced_row_is_announced_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, journal) = open(dir.path()).await;
    *journal.refuse.lock().unwrap() = true;
    let error = requests.ask(ask()).await.unwrap_err();
    assert!(format!("{error:#}").contains("disk full"), "{error:#}");
    drop(requests);

    // A crash between the row and its entry: the row is pending and was never shown.
    let db = fronde_db::open(dir.path()).await.unwrap();
    let crashed = requests::ActiveModel {
        topic: Set(TOPIC.into()),
        turn: Set(None),
        caller: Set("mcp:Codex".into()),
        client_request_id: Set(None),
        kind: Set("form".into()),
        card: Set(serde_json::to_value(question()).unwrap()),
        for_user: Set("alix".into()),
        admin_only: Set(false),
        status: Set("pending".into()),
        created_at: Set(chrono::Utc::now()),
        expires_at: Set(Some(chrono::Utc::now() + chrono::Duration::minutes(10))),
        ..Default::default()
    };
    let crashed = requests::Entity::insert(crashed)
        .exec(&db)
        .await
        .unwrap()
        .last_insert_id as RequestId;
    // And one whose time ran out while Fronde was stopped.
    let mut overdue = requests::ActiveModel {
        expires_at: Set(Some(chrono::Utc::now() - chrono::Duration::minutes(1))),
        ..Default::default()
    };
    overdue.topic = Set(TOPIC.into());
    overdue.caller = Set("assistant".into());
    overdue.kind = Set("form".into());
    overdue.card = Set(serde_json::to_value(question()).unwrap());
    overdue.for_user = Set("alix".into());
    overdue.admin_only = Set(false);
    overdue.status = Set("pending".into());
    overdue.created_at = Set(chrono::Utc::now());
    overdue.stream_seq = Set(Some(3));
    let overdue = requests::Entity::insert(overdue)
        .exec(&db)
        .await
        .unwrap()
        .last_insert_id as RequestId;

    let (requests, journal) = open(dir.path()).await;
    let shown = journal.shown();
    assert!(
        shown.contains(&Shown::Announced {
            id: crashed,
            from: Some("Codex".into())
        }),
        "{shown:?}"
    );
    assert!(
        shown.contains(&Shown::Closed {
            id: overdue,
            outcome: Outcome::Expired,
            by: None
        }),
        "{shown:?}"
    );
    assert_eq!(row(dir.path(), overdue).await.status, "expired");
    assert!(row(dir.path(), crashed).await.stream_seq.is_some());
    requests
        .answer(crashed, pick("Seerr"), &alix())
        .await
        .unwrap();
}
