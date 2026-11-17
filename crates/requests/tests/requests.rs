use async_trait::async_trait;
use fronde_entity::requests;
use fronde_protocol::{
    Card, ChoiceAnswer, ChoiceOption, Field, Outcome, PermissionChoice, PermissionKind, RequestId,
    Response,
};
use fronde_requests::{
    Ask, Caller, Journal, Refusal, Requests, Resolution, Reused, ThreadId, User,
};
use sea_orm::{ActiveValue::Set, ConnectionTrait, EntityTrait};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

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
    /// Who asked each request announced, in order.
    callers: Mutex<Vec<Caller>>,
    refuse: Mutex<bool>,
    /// When set, showing an end waits for this notify, `holding` set meanwhile.
    hold_close: Mutex<Option<Arc<Notify>>>,
    holding: AtomicBool,
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
        caller: &Caller,
    ) -> anyhow::Result<u64> {
        assert_eq!(topic, TOPIC);
        let from = caller.from();
        if *self.refuse.lock().unwrap() {
            anyhow::bail!("disk full");
        }
        self.callers.lock().unwrap().push(caller.clone());
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
        let hold = self.hold_close.lock().unwrap().clone();
        if let Some(gate) = hold {
            self.holding.store(true, Ordering::SeqCst);
            gate.notified().await;
        }
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
            note: true,
        }],
        submit: None,
        truncated: false,
        replaced: false,
        during: Vec::new(),
    }
}

fn ask() -> Ask {
    Ask {
        topic: TOPIC.into(),
        turn: Some(1),
        caller: Caller::Thread(ThreadId::MAIN),
        client_request_id: None,
        card: question(),
        for_user: "alix".into(),
        admin_only: false,
        timeout: Duration::from_secs(600),
        tied: false,
    }
}

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
            }
            .into(),
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

/// `ask` with another question, under the same `client_request_id`.
fn other_question(mut ask: Ask) -> Ask {
    let Card::Form { fields, .. } = &mut ask.card else {
        unreachable!("a question");
    };
    let Field::Choice { question, .. } = &mut fields[0] else {
        unreachable!()
    };
    *question = "Et laquelle ensuite ?".into();
    ask
}

fn with_id(mut ask: Ask, id: &str) -> Ask {
    ask.client_request_id = Some(id.into());
    ask
}

fn reused(error: anyhow::Error) -> Reused {
    error
        .downcast::<Reused>()
        .unwrap_or_else(|error| panic!("not a reused id: {error:#}"))
}

#[tokio::test]
async fn reused_id_with_another_question_does_not_get_the_old_answer() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, journal) = open(dir.path()).await;
    let first = requests.ask(with_id(ask(), "q1")).await.unwrap();
    // Still pending: another question under its id is refused, and shows no card.
    let error = requests
        .ask(with_id(other_question(ask()), "q1"))
        .await
        .unwrap_err();
    assert_eq!(
        reused(error),
        Reused {
            client_request_id: "q1".into()
        }
    );
    assert_eq!(journal.shown().len(), 1);
    // Nor does the same question for another user join it.
    let mut sam = with_id(ask(), "q1");
    sam.for_user = "sam".into();
    reused(requests.ask(sam).await.unwrap_err());

    requests
        .answer(first.id(), pick("Mealie"), &alix())
        .await
        .unwrap();
    assert_eq!(resolved(first).await, Resolution::Answered(pick("Mealie")));
    // Answered: another question under the same id is a new request, never the old answer.
    let second = requests
        .ask(with_id(other_question(ask()), "q1"))
        .await
        .unwrap();
    assert_ne!(second.id(), 0);
    assert_eq!(row(dir.path(), second.id()).await.status, "pending");
    requests
        .answer(second.id(), pick("Seerr"), &alix())
        .await
        .unwrap();
    assert_eq!(resolved(second).await, Resolution::Answered(pick("Seerr")));
}

#[tokio::test]
async fn same_id_in_a_new_turn_is_a_new_request() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, journal) = open(dir.path()).await;
    let first = requests.ask(with_id(ask(), "q1")).await.unwrap();
    let id = first.id();
    requests.answer(id, pick("Mealie"), &alix()).await.unwrap();
    resolved(first).await;

    // The assistant asks the very same question in its next turn: the user answers again.
    let mut next_turn = with_id(ask(), "q1");
    next_turn.turn = Some(9);
    let again = requests.ask(next_turn.clone()).await.unwrap();
    assert_ne!(again.id(), id);
    assert_eq!(journal.shown().len(), 3, "a second card");
    let waiting = tokio::spawn(again.wait());
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished(), "not the old answer");

    // Outside a turn, the assistant's id names nothing to join either.
    let mut no_turn = with_id(ask(), "q2");
    no_turn.turn = None;
    let one = requests.ask(no_turn.clone()).await.unwrap();
    reused(requests.ask(no_turn).await.unwrap_err());
    requests.cancel(one.id()).await.unwrap();
    waiting.abort();
}

#[tokio::test]
async fn outside_clients_of_one_name_join_only_the_same_question() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, journal) = open(dir.path()).await;
    let codex = |ask: Ask| Ask {
        caller: Caller::Mcp("Codex".into()),
        turn: None,
        ..with_id(ask, "q1")
    };
    // Two clients both called Codex, each with its own question and the same id.
    let one = requests.ask(codex(ask())).await.unwrap();
    reused(
        requests
            .ask(codex(other_question(ask())))
            .await
            .unwrap_err(),
    );
    requests
        .answer(one.id(), pick("Mealie"), &alix())
        .await
        .unwrap();
    resolved(one).await;
    // The other one asks again once the first is answered: its own card, not that answer.
    let two = requests.ask(codex(other_question(ask()))).await.unwrap();
    assert_eq!(journal.shown().len(), 3);
    requests
        .answer(two.id(), pick("Seerr"), &alix())
        .await
        .unwrap();
    assert_eq!(resolved(two).await, Resolution::Answered(pick("Seerr")));

    // A genuine retry within the timeout gets the answer at once.
    let retry = requests.ask(codex(other_question(ask()))).await.unwrap();
    assert_eq!(resolved(retry).await, Resolution::Answered(pick("Seerr")));
    // Once the first request's time is over, the same question is asked anew.
    let mut short = codex(ask());
    short.client_request_id = Some("q3".into());
    short.timeout = Duration::from_millis(100);
    let first = requests.ask(short.clone()).await.unwrap();
    requests
        .answer(first.id(), pick("Mealie"), &alix())
        .await
        .unwrap();
    resolved(first).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let late = requests.ask(short).await.unwrap();
    let waiting = tokio::spawn(late.wait());
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished(), "not the old answer");
    waiting.abort();
}

#[tokio::test]
async fn genuine_retry_in_the_same_turn_joins() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, journal) = open(dir.path()).await;
    let one = requests.ask(with_id(ask(), "q1")).await.unwrap();
    let two = requests.ask(with_id(ask(), "q1")).await.unwrap();
    assert_eq!(one.id(), two.id());
    assert_eq!(journal.shown().len(), 1, "one card");
    requests
        .answer(one.id(), pick("Seerr"), &alix())
        .await
        .unwrap();
    assert_eq!(resolved(one).await, Resolution::Answered(pick("Seerr")));
    assert_eq!(resolved(two).await, Resolution::Answered(pick("Seerr")));
    let late = requests.ask(with_id(ask(), "q1")).await.unwrap();
    assert_eq!(resolved(late).await, Resolution::Answered(pick("Seerr")));
}

#[tokio::test]
async fn cancelled_request_is_not_joined() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, journal) = open(dir.path()).await;
    let one = requests.ask(with_id(ask(), "q1")).await.unwrap();
    requests.cancel(one.id()).await.unwrap();
    assert_eq!(resolved(one).await, Resolution::Cancelled);
    let again = requests.ask(with_id(ask(), "q1")).await.unwrap();
    assert_eq!(row(dir.path(), again.id()).await.status, "pending");
    assert_eq!(journal.shown().len(), 3);
}

#[tokio::test]
async fn waiter_expires_even_when_the_database_cannot_close() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, journal) = open(dir.path()).await;
    // Every expiry fails in the database, the timer's and the waiter's own attempt.
    let db = fronde_db::open(dir.path()).await.unwrap();
    db.execute_unprepared(
        "CREATE TRIGGER fail_expiry BEFORE UPDATE OF status ON requests \
         WHEN NEW.status = 'expired' BEGIN SELECT RAISE(ABORT, 'disk full'); END",
    )
    .await
    .unwrap();
    let mut short = ask();
    short.timeout = Duration::from_millis(100);
    let waiter = requests.ask(short).await.unwrap();
    let id = waiter.id();
    assert_eq!(resolved(waiter).await, Resolution::Expired);
    // The row could not change; the next start expires it.
    assert_eq!(row(dir.path(), id).await.status, "pending");
    assert_eq!(journal.shown().len(), 1);
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
async fn cancel_turn_returns_once_an_end_shown_elsewhere_is_shown() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, journal) = open(dir.path()).await;
    let id = requests.ask(ask()).await.unwrap().id();
    let gate = Arc::new(Notify::new());
    *journal.hold_close.lock().unwrap() = Some(gate.clone());

    // An answer closes the request, then is slow to show its end.
    let answering = tokio::spawn({
        let requests = requests.clone();
        async move { requests.answer(id, pick("Seerr"), &alix()).await }
    });
    tokio::time::timeout(WAIT, async {
        while !journal.holding.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the answer never showed its end");
    let cancelling = tokio::spawn({
        let (requests, journal) = (requests.clone(), journal.clone());
        async move {
            let cancelled = requests.cancel_turn(TOPIC, 1).await.unwrap();
            (cancelled, journal.shown())
        }
    });
    // Time enough for `cancel_turn` to return, were it not waiting for the end.
    tokio::time::sleep(Duration::from_millis(200)).await;
    gate.notify_one();

    let (cancelled, shown) = tokio::time::timeout(WAIT, cancelling)
        .await
        .unwrap()
        .unwrap();
    // The answer won; its end was shown before `cancel_turn` returned, so before the turn's.
    assert_eq!(cancelled, 0);
    assert!(
        matches!(shown.last(), Some(Shown::Closed { id: closed, outcome: Outcome::Answered { .. }, .. }) if *closed == id),
        "{shown:?}"
    );
    assert_eq!(answering.await.unwrap(), Ok(()));
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

fn permission() -> Card {
    Card::Permission {
        title: "Lire l'agenda".into(),
        tool: None,
        body: None,
        options: vec![
            PermissionChoice {
                id: "allow-once".into(),
                kind: PermissionKind::AllowOnce,
                detail: None,
            },
            PermissionChoice {
                id: "reject".into(),
                kind: PermissionKind::RejectOnce,
                detail: None,
            },
        ],
        truncated: false,
        replaced: false,
    }
}

#[tokio::test]
async fn restart_expires_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, _) = open(dir.path()).await;
    let question = requests.ask(ask()).await.unwrap().id();
    let mut asked = ask();
    asked.card = permission();
    let permission = requests.ask(asked).await.unwrap().id();
    drop(requests);

    // The agent's call that waited on the permission is gone with the process: no answer can
    // reach it any more. A question stays answerable.
    let (requests, journal) = open(dir.path()).await;
    assert_eq!(row(dir.path(), permission).await.status, "expired");
    assert_eq!(
        journal.shown(),
        [Shown::Closed {
            id: permission,
            outcome: Outcome::Expired,
            by: None
        }]
    );
    let allow = Response::Permission {
        choice: "allow-once".into(),
    };
    assert_eq!(
        requests.answer(permission, allow, &alix()).await,
        Err(Refusal::Closed)
    );
    assert_eq!(row(dir.path(), question).await.status, "pending");
    requests
        .answer(question, pick("Mealie"), &alix())
        .await
        .unwrap();
}

#[tokio::test]
async fn restart_expires_elicitations_and_servers_requests() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, _) = open(dir.path()).await;
    let question = requests.ask(ask()).await.unwrap().id();
    // The agent's form: only its call waits on it.
    let agent_form = requests
        .ask(Ask {
            tied: true,
            ..ask()
        })
        .await
        .unwrap()
        .id();
    // An upstream server's form, even one asked without the marker.
    let server_form = requests
        .ask(Ask {
            caller: Caller::Source("github".into()),
            turn: None,
            admin_only: true,
            ..ask()
        })
        .await
        .unwrap()
        .id();
    drop(requests);

    let (requests, journal) = open(dir.path()).await;
    for id in [agent_form, server_form] {
        assert_eq!(row(dir.path(), id).await.status, "expired", "{id}");
        assert_eq!(
            requests.answer(id, pick("Mealie"), &alix()).await,
            Err(Refusal::Closed)
        );
    }
    assert_eq!(
        journal.shown(),
        [
            Shown::Closed {
                id: agent_form,
                outcome: Outcome::Expired,
                by: None
            },
            Shown::Closed {
                id: server_form,
                outcome: Outcome::Expired,
                by: None
            },
        ]
    );
    // A thread's own question, asked through a tool, stays answerable.
    assert_eq!(row(dir.path(), question).await.status, "pending");
    requests
        .answer(question, pick("Mealie"), &alix())
        .await
        .unwrap();
}

#[tokio::test]
async fn only_a_threads_agent_asks_for_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, journal) = open(dir.path()).await;
    for caller in [Caller::Mcp("Codex".into()), Caller::Source("github".into())] {
        let mut asked = ask();
        asked.card = permission();
        asked.caller = caller;
        let error = requests.ask(asked).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("only a thread's agent"),
            "{error:#}"
        );
    }
    // Nothing was written or shown.
    assert_eq!(journal.shown(), []);
    let db = fronde_db::open(dir.path()).await.unwrap();
    assert!(requests::Entity::find().all(&db).await.unwrap().is_empty());
    // Any thread's agent may.
    let mut asked = ask();
    asked.card = permission();
    asked.caller = Caller::Thread(ThreadId(2));
    requests.ask(asked).await.unwrap();
}

#[tokio::test]
async fn every_card_is_checked_before_it_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, journal) = open(dir.path()).await;
    let mut hidden = ask();
    hidden.card = Card::Form {
        message: String::new(),
        fields: vec![Field::Choice {
            key: "q1".into(),
            header: None,
            question: "Laquelle\u{200b} ?".into(),
            options: vec![option("Mealie"), option("Seerr")],
            multi: false,
            other: false,
            required: true,
            note: true,
        }],
        submit: None,
        truncated: false,
        replaced: false,
        during: Vec::new(),
    };
    let error = requests.ask(hidden).await.unwrap_err();
    assert!(format!("{error:#}").contains("U+200B"), "{error:#}");
    let mut long = ask();
    let mut card = permission();
    if let Card::Permission { title, .. } = &mut card {
        *title = "t".repeat(201);
    }
    long.card = card;
    let error = requests.ask(long).await.unwrap_err();
    assert!(format!("{error:#}").contains("title has 201"), "{error:#}");
    assert_eq!(journal.shown(), []);
}

#[tokio::test]
async fn a_card_never_shown_shows_no_end() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, journal) = open(dir.path()).await;
    *journal.refuse.lock().unwrap() = true;
    requests.ask(ask()).await.unwrap_err();
    // Cancelled, without a close for a card nobody saw.
    assert_eq!(journal.shown(), []);
    assert_eq!(row(dir.path(), 1).await.status, "cancelled");
}

#[tokio::test]
async fn permission_answer_is_one_of_its_options() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, _) = open(dir.path()).await;
    let mut asked = ask();
    asked.card = permission();
    let waiter = requests.ask(asked).await.unwrap();
    let id = waiter.id();
    let refused = requests
        .answer(
            id,
            Response::Permission {
                choice: "allow_always".into(),
            },
            &alix(),
        )
        .await;
    assert!(matches!(refused, Err(Refusal::Invalid(_))), "{refused:?}");
    assert_eq!(row(dir.path(), id).await.status, "pending");
    let allow = Response::Permission {
        choice: "allow-once".into(),
    };
    requests.answer(id, allow.clone(), &alix()).await.unwrap();
    assert_eq!(resolved(waiter).await, Resolution::Answered(allow));
    let saved = row(dir.path(), id).await;
    assert_eq!(saved.kind, "permission");
    assert_eq!(saved.answered_by.as_deref(), Some("alix"));
    assert_eq!(
        saved.response,
        Some(serde_json::json!({"type": "permission", "choice": "allow-once"}))
    );
}

#[tokio::test]
async fn one_unreadable_row_does_not_stop_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, _) = open(dir.path()).await;
    let good = requests.ask(ask()).await.unwrap().id();
    drop(requests);
    // A row a later version wrote, never shown, with a card this version cannot read.
    let db = fronde_db::open(dir.path()).await.unwrap();
    let unreadable = requests::ActiveModel {
        topic: Set(TOPIC.into()),
        turn: Set(None),
        caller: Set("mcp:Codex".into()),
        client_request_id: Set(None),
        kind: Set("hologram".into()),
        card: Set(serde_json::json!({"kind": "hologram"})),
        for_user: Set("alix".into()),
        admin_only: Set(false),
        status: Set("pending".into()),
        created_at: Set(chrono::Utc::now()),
        expires_at: Set(Some(chrono::Utc::now() + chrono::Duration::minutes(10))),
        ..Default::default()
    };
    let unreadable = requests::Entity::insert(unreadable)
        .exec(&db)
        .await
        .unwrap()
        .last_insert_id as RequestId;

    let (requests, _) = open(dir.path()).await;
    assert_eq!(row(dir.path(), unreadable).await.status, "cancelled");
    requests.answer(good, pick("Seerr"), &alix()).await.unwrap();
}

/// A request asked before threads was stored as the assistant's: it reads as the main thread's,
/// whose agent the assistant was, and is shown and answered as such.
#[tokio::test]
async fn old_assistant_rows_read_as_thread_one() {
    let dir = tempfile::tempdir().unwrap();
    drop(open(dir.path()).await);
    let db = fronde_db::open(dir.path()).await.unwrap();
    let old = requests::ActiveModel {
        topic: Set(TOPIC.into()),
        turn: Set(Some(4)),
        caller: Set("assistant".into()),
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
    let old = requests::Entity::insert(old)
        .exec(&db)
        .await
        .unwrap()
        .last_insert_id as RequestId;

    let (requests, journal) = open(dir.path()).await;
    assert_eq!(
        journal.shown(),
        [Shown::Announced {
            id: old,
            from: None
        }]
    );
    assert_eq!(
        *journal.callers.lock().unwrap(),
        [Caller::Thread(ThreadId::MAIN)]
    );
    requests.answer(old, pick("Seerr"), &alix()).await.unwrap();
    assert_eq!(row(dir.path(), old).await.status, "answered");
    // New rows name the thread.
    let waiter = requests
        .ask(Ask {
            caller: Caller::Thread(ThreadId(2)),
            ..ask()
        })
        .await
        .unwrap();
    assert_eq!(row(dir.path(), waiter.id()).await.caller, "thread:2");
}

/// A caller this version does not know is never read as a thread: its request is cancelled at
/// start, never shown again, and cannot be answered.
#[tokio::test]
async fn unknown_caller_rows_are_cancelled_at_open() {
    let dir = tempfile::tempdir().unwrap();
    drop(open(dir.path()).await);
    let db = fronde_db::open(dir.path()).await.unwrap();
    let unknown = requests::ActiveModel {
        topic: Set(TOPIC.into()),
        turn: Set(Some(4)),
        caller: Set("robot:42".into()),
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
    let unknown = requests::Entity::insert(unknown)
        .exec(&db)
        .await
        .unwrap()
        .last_insert_id as RequestId;

    let (requests, journal) = open(dir.path()).await;
    assert_eq!(row(dir.path(), unknown).await.status, "cancelled");
    assert!(journal.callers.lock().unwrap().is_empty());
    assert!(
        !journal
            .shown()
            .iter()
            .any(|shown| matches!(shown, Shown::Announced { .. })),
        "{:?}",
        journal.shown()
    );
    assert_eq!(
        requests.answer(unknown, pick("Seerr"), &alix()).await,
        Err(Refusal::Closed)
    );
    assert_eq!(row(dir.path(), unknown).await.status, "cancelled");
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

mod secrets {
    use super::*;
    use fronde_db::ephemeral::{EphemeralScope, EphemeralSecrets};
    use fronde_db::master_key::Backends;
    use fronde_protocol::{Keep, Remember, SecretValue};
    use fronde_requests::{Saved, SecretStore};
    use secrecy::ExposeSecret as _;

    const CANARY: &str = "canary-card-secret-7d1e9b";

    fn card(remember: bool) -> Card {
        Card::Secret {
            label: "Mot de passe Nextcloud".into(),
            reason: "Pour lire ton agenda.".into(),
            placeholder: None,
            url: None,
            remember: remember.then(|| Remember {
                name: "nextcloud".into(),
                origin: "https://cloud.example".into(),
                replaces: false,
            }),
        }
    }

    fn ask_secret(remember: bool) -> Ask {
        Ask {
            card: card(remember),
            ..ask()
        }
    }

    fn secret(value: &str, keep: Keep) -> Response {
        Response::Secret {
            value: SecretValue::new(value.into()),
            keep,
        }
    }

    struct Bench {
        requests: Arc<Requests>,
        journal: Arc<FakeJournal>,
        ephemeral: Arc<EphemeralSecrets>,
        key_file: std::path::PathBuf,
        dir: tempfile::TempDir,
    }

    async fn bench() -> Bench {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir(&data).unwrap();
        let key_file = dir.path().join("master.key");
        let db = fronde_db::open(&data).await.unwrap();
        let journal = Arc::new(FakeJournal::default());
        let ephemeral = Arc::new(EphemeralSecrets::new());
        let store = SecretStore::new(Backends::file(&key_file), ephemeral.clone());
        let requests = Requests::open_with(db, journal.clone(), Some(store))
            .await
            .unwrap();
        Bench {
            requests,
            journal,
            ephemeral,
            key_file,
            dir,
        }
    }

    impl Bench {
        fn data(&self) -> std::path::PathBuf {
            self.dir.path().join("data")
        }

        /// No file of the data directory holds `text`: the database, its WAL and shared memory.
        fn assert_on_no_disk(&self, text: &str) {
            let mut scanned = 0;
            for entry in std::fs::read_dir(self.data()).unwrap() {
                let path = entry.unwrap().path();
                let bytes = std::fs::read(&path).unwrap();
                scanned += 1;
                assert!(
                    !bytes
                        .windows(text.len())
                        .any(|window| window == text.as_bytes()),
                    "{} holds the secret",
                    path.display()
                );
            }
            assert!(scanned >= 2, "the database files are missing");
        }
    }

    fn assistant_scope() -> EphemeralScope {
        EphemeralScope {
            topic: TOPIC.into(),
            asker: "thread:1".into(),
        }
    }

    #[tokio::test]
    async fn secret_for_the_conversation_never_leaves_the_store() {
        let bench = bench().await;
        let waiter = bench.requests.ask(ask_secret(false)).await.unwrap();
        let id = waiter.id();
        assert_eq!(row(&bench.data(), id).await.kind, "secret");
        bench
            .requests
            .answer(id, secret(CANARY, Keep::Conversation), &alix())
            .await
            .unwrap();
        let Resolution::Saved(Saved::Conversation {
            secret_ref,
            expires_at,
        }) = resolved(waiter).await
        else {
            panic!("not saved for the conversation");
        };
        assert!(secret_ref.starts_with("sr_"), "{secret_ref}");
        assert!(expires_at > chrono::Utc::now() + chrono::Duration::minutes(59));
        assert_eq!(
            bench.journal.shown().last(),
            Some(&Shown::Closed {
                id,
                outcome: Outcome::Saved {
                    kept: Keep::Conversation,
                    new_key: None,
                },
                by: Some("alix".into()),
            })
        );
        let saved = row(&bench.data(), id).await;
        assert_eq!(saved.status, "answered");
        let response = saved.response.unwrap().to_string();
        assert!(response.contains(r#""kept":"conversation""#), "{response}");
        assert!(!response.contains(CANARY), "{response}");
        bench.assert_on_no_disk(CANARY);

        // Only for the asker in this conversation, once.
        let mut codex = assistant_scope();
        codex.asker = "mcp:Codex".into();
        assert!(bench.ephemeral.take(&secret_ref, &codex).is_err());
        let value = bench
            .ephemeral
            .take(&secret_ref, &assistant_scope())
            .unwrap();
        assert_eq!(value.expose_secret(), CANARY);
        assert!(
            bench
                .ephemeral
                .take(&secret_ref, &assistant_scope())
                .is_err()
        );
        // Nothing sealed.
        assert!(!bench.key_file.exists());
    }

    #[tokio::test]
    async fn a_secret_from_the_wrong_user_is_refused() {
        let bench = bench().await;
        let waiter = bench.requests.ask(ask_secret(true)).await.unwrap();
        let id = waiter.id();
        let sam = User {
            name: "sam".into(),
            admin: true,
        };
        for keep in [Keep::Conversation, Keep::Stored] {
            assert_eq!(
                bench.requests.answer(id, secret(CANARY, keep), &sam).await,
                Err(Refusal::NotYours)
            );
        }
        assert!(bench.ephemeral.is_empty());
        assert_eq!(row(&bench.data(), id).await.status, "pending");
        assert!(!bench.key_file.exists());
        bench.assert_on_no_disk(CANARY);
    }

    #[tokio::test]
    async fn secret_kept_for_origin_needs_admin() {
        let bench = bench().await;
        let waiter = bench.requests.ask(ask_secret(true)).await.unwrap();
        let id = waiter.id();
        let family = User {
            name: "alix".into(),
            admin: false,
        };
        let refusal = bench
            .requests
            .answer(id, secret(CANARY, Keep::Stored), &family)
            .await
            .unwrap_err();
        assert_eq!(
            refusal.to_string(),
            "Seul un administrateur peut garder un secret."
        );
        assert_eq!(row(&bench.data(), id).await.status, "pending");
        assert!(!bench.key_file.exists());

        bench
            .requests
            .answer(id, secret(CANARY, Keep::Stored), &alix())
            .await
            .unwrap();
        assert_eq!(
            resolved(waiter).await,
            Resolution::Saved(Saved::Stored {
                name: "nextcloud".into(),
                origin: "https://cloud.example".into(),
            })
        );
        assert!(bench.ephemeral.is_empty());
        bench.assert_on_no_disk(CANARY);
        // Sealed, bound to its origin, and opened by the key made for it.
        let db = fronde_db::open(&bench.data()).await.unwrap();
        let sealed = fronde_db::secrets::load_sealed(&db, fronde_db::secrets::SOURCES)
            .await
            .unwrap();
        assert_eq!(sealed.len(), 1);
        assert_eq!(sealed[0].name, "nextcloud");
        assert_eq!(sealed[0].origin, "https://cloud.example");
        let key = Backends::file(&bench.key_file)
            .load(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sealed[0].open(&key).unwrap().expose_secret(), CANARY);
        let response = row(&bench.data(), id).await.response.unwrap().to_string();
        assert!(response.contains(r#""kept":"stored""#), "{response}");
        assert!(!response.contains(CANARY), "{response}");
        // The card says where the key it made lives; the next secret kept makes none.
        let stored = |new_key| Outcome::Saved {
            kept: Keep::Stored,
            new_key,
        };
        assert!(
            matches!(bench.journal.shown().last(), Some(Shown::Closed { outcome, .. }) if *outcome == stored(Some(fronde_protocol::KeyPlace::File)))
        );
        let again = bench.requests.ask(ask_secret(true)).await.unwrap();
        bench
            .requests
            .answer(again.id(), secret("rotated", Keep::Stored), &alix())
            .await
            .unwrap();
        assert!(
            matches!(bench.journal.shown().last(), Some(Shown::Closed { outcome, .. }) if *outcome == stored(None))
        );
    }

    #[tokio::test]
    async fn a_stored_name_never_moves_to_another_origin() {
        let bench = bench().await;
        let mut elsewhere = ask_secret(true);
        elsewhere.card = Card::Secret {
            label: "Mot de passe".into(),
            reason: "Pour un autre site.".into(),
            placeholder: None,
            url: None,
            remember: Some(Remember {
                name: "nextcloud".into(),
                origin: "https://other.example".into(),
                replaces: false,
            }),
        };
        let first = bench.requests.ask(ask_secret(true)).await.unwrap();
        bench
            .requests
            .answer(first.id(), secret("first-value", Keep::Stored), &alix())
            .await
            .unwrap();
        let second = bench.requests.ask(elsewhere).await.unwrap();
        let refusal = bench
            .requests
            .answer(second.id(), secret(CANARY, Keep::Stored), &alix())
            .await
            .unwrap_err();
        assert!(
            refusal
                .to_string()
                .contains("est déjà gardé pour https://cloud.example"),
            "{refusal}"
        );
        assert_eq!(
            bench.requests.stored_origin("nextcloud").await.unwrap(),
            Some("https://cloud.example".into())
        );
        assert_eq!(row(&bench.data(), second.id()).await.status, "pending");
    }

    #[tokio::test]
    async fn a_secret_is_kept_once_whatever_answers_twice() {
        let bench = bench().await;
        let mut first = ask_secret(false);
        first.client_request_id = Some("token".into());
        let waiter = bench.requests.ask(first.clone()).await.unwrap();
        let id = waiter.id();
        let alix = alix();
        let (a, b) = tokio::join!(
            bench
                .requests
                .answer(id, secret(CANARY, Keep::Conversation), &alix),
            bench
                .requests
                .answer(id, secret("other", Keep::Conversation), &alix),
        );
        assert!(a.is_ok() != b.is_ok(), "{a:?} {b:?}");
        assert_eq!(a.err().or(b.err()), Some(Refusal::Closed));
        assert_eq!(bench.ephemeral.len(), 1);
        let saved = resolved(waiter).await;
        // A retry in the same turn learns where it was kept, from the row.
        let retry = bench.requests.ask(first).await.unwrap();
        assert_eq!(retry.id(), id);
        assert_eq!(resolved(retry).await, saved);
    }

    #[tokio::test]
    async fn a_secret_answer_without_a_store_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (requests, _journal) = open(dir.path()).await;
        let waiter = requests.ask(ask_secret(false)).await.unwrap();
        assert_eq!(
            requests
                .answer(waiter.id(), secret(CANARY, Keep::Conversation), &alix())
                .await,
            Err(Refusal::Failed)
        );
        assert_eq!(row(dir.path(), waiter.id()).await.status, "pending");
    }
}
