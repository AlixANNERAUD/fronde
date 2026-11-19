//! The chat's web side: each person signs in with their name and password, kept in a session
//! cookie naming them, sessions kept in the database, and the web interface's files.

use crate::sessions::{Sessions, USER};
use crate::ws;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use fronde_agent::threads::Threads;
use fronde_requests::{Requests, User, Users};
use secrecy::SecretString;
use secrecy::zeroize::{Zeroize, Zeroizing};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::time::Instant;
use tower_http::services::ServeDir;
use tower_sessions::cookie::SameSite;
use tower_sessions::cookie::time;
use tower_sessions::{Expiry, Session, SessionManagerLayer};

/// How long a session survives without a signed-in request; each one renews it ([`renew`]).
const SESSION_IDLE: time::Duration = time::Duration::days(30);
/// Wrong passwords in a row for one name before sign-in under that name is refused for
/// [`LOCKOUT`].
const MAX_FAILURES: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(60);
/// How long a wrong password waits before its answer.
const FAILURE_DELAY: Duration = Duration::from_secs(1);
/// Names typed whose wrong passwords are counted at once; past it, the one whose last mistake is
/// the oldest is forgotten, so that typing made-up names cannot fill the memory.
const MAX_NAMES: usize = 4096;
/// Asset names carry a hash of their content: a changed asset gets a new name.
const CACHE_FOREVER: HeaderValue = HeaderValue::from_static("public, max-age=31536000, immutable");
const NOT_BUILT: &str = "Interface non construite : lance `mise run web-build`.";

pub struct Web {
    pub threads: Arc<Threads>,
    pub requests: Arc<Requests>,
    /// The people who sign in: a session names one of them, and is no one's once that name is
    /// no longer here.
    pub users: Users,
    pub passwords: Passwords,
    pub throttle: Mutex<Throttle>,
}

impl Web {
    /// The name a sign-in is for: the one typed, without the spaces around it; with a single
    /// user, an empty one is theirs, so the form from before names keeps working.
    fn named<'a>(&'a self, typed: &'a str) -> &'a str {
        let typed = typed.trim();
        match self.users.iter().next() {
            Some(only) if typed.is_empty() && self.users.len() == 1 => &only.name,
            _ => typed,
        }
    }
}

/// Each person's password, and a stand-in that a name nobody has is compared with.
pub struct Passwords {
    by_name: HashMap<String, SecretString>,
    /// Random, made at each start: a name that is no one's takes the same compare as anyone's,
    /// so the time an answer takes does not tell names apart.
    dummy: SecretString,
}

impl Passwords {
    pub fn new(by_name: HashMap<String, SecretString>) -> anyhow::Result<Self> {
        Ok(Self {
            by_name,
            dummy: crate::init::random_token()?.into(),
        })
    }

    /// Whether `given` is the password of `name`; false for a name that is no one's, after the
    /// same compare.
    fn check(&self, name: &str, given: &[u8]) -> bool {
        let (expected, known) = match self.by_name.get(name) {
            Some(password) => (password, true),
            None => (&self.dummy, false),
        };
        crate::same_secret(given, expected) & known
    }
}

/// Wrong passwords in a row, per name typed: one person's mistakes never lock another out, and
/// a name that is no one's is counted as anyone's is, so a lockout tells no names apart. Names are
/// kept as their SHA-256, never as typed: a password typed in the name's box is not kept.
#[derive(Default)]
pub struct Throttle {
    names: HashMap<[u8; 32], Tries>,
}

struct Tries {
    failures: u32,
    locked_until: Option<Instant>,
    /// The last wrong password.
    last: Instant,
}

impl Throttle {
    /// The count of the name hashed as `key`, made empty when it has none.
    fn tries(&mut self, key: [u8; 32], now: Instant) -> &mut Tries {
        if !self.names.contains_key(&key) && self.names.len() >= MAX_NAMES {
            // A name locked out goes last, its lockout ending first, so that typing made-up
            // names does not lift a lockout sooner.
            let locked = |tries: &Tries| tries.locked_until.filter(|until| now < *until);
            if let Some(oldest) = self
                .names
                .iter()
                .min_by_key(|(_, tries)| (locked(tries), tries.last))
                .map(|(key, _)| *key)
            {
                self.names.remove(&oldest);
            }
        }
        self.names.entry(key).or_insert(Tries {
            failures: 0,
            locked_until: None,
            last: now,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Attempt {
    Accepted,
    Wrong,
    TooMany,
}

/// Records a sign-in attempt under `name`; a wrong password is answered after
/// [`FAILURE_DELAY`], whether the name is anyone's or not.
async fn attempt(throttle: &Mutex<Throttle>, name: &str, right: bool) -> Attempt {
    let now = Instant::now();
    let key: [u8; 32] = Sha256::digest(name.as_bytes()).into();
    {
        let mut throttle = throttle.lock().unwrap_or_else(PoisonError::into_inner);
        let tries = throttle.tries(key, now);
        if tries.locked_until.is_some_and(|until| now < until) {
            return Attempt::TooMany;
        }
        tries.locked_until = None;
        if right {
            throttle.names.remove(&key);
            return Attempt::Accepted;
        }
        tries.failures += 1;
        tries.last = now;
        if tries.failures >= MAX_FAILURES {
            tries.failures = 0;
            tries.locked_until = Some(now + LOCKOUT);
        }
    }
    tokio::time::sleep(FAILURE_DELAY).await;
    Attempt::Wrong
}

/// `sessions` keeps the sessions in `fronde.db`, so a restart signs no one out; `site_root` holds
/// what `dx build` writes: `index.html`, the single page of the interface, and `assets/`.
pub fn router(web: Web, sessions: Sessions, secure_cookie: bool, site_root: &Path) -> Router {
    // Not `with_always_save`: that would store a session for any cookie a client makes up.
    // Signed-in requests renew theirs instead.
    let sessions = SessionManagerLayer::new(sessions)
        .with_name("fronde_session")
        .with_http_only(true)
        .with_same_site(SameSite::Lax)
        .with_secure(secure_cookie)
        .with_expiry(Expiry::OnInactivity(SESSION_IDLE));
    let index = Arc::new(site_root.join("index.html"));
    if !index.is_file() {
        tracing::warn!(
            "{} is missing: the web interface is not built (mise run web-build)",
            index.display()
        );
    }
    let page = move || {
        let index = index.clone();
        async move { serve_index(&index).await }
    };
    let assets = Router::new()
        .nest_service("/assets", static_files(&site_root.join("assets")))
        .route_layer(middleware::map_response(cache_forever));
    let web = Arc::new(web);
    // The signed-in pages: the main thread, any thread, the thread list, a new thread.
    let signed_in =
        get(page.clone()).route_layer(middleware::from_fn_with_state(web.clone(), require_session));
    Router::new()
        .route("/", signed_in.clone())
        .route("/t/{id}", signed_in.clone())
        .route("/threads", signed_in.clone())
        .route("/new", signed_in)
        .route("/sign-in", get(page))
        // The forms post under `/auth`, apart from the pages, so that in development dx serves the
        // pages and passes only these to the server (crates/web/Dioxus.toml).
        .route("/auth/sign-in", post(sign_in))
        .route("/auth/sign-out", post(sign_out))
        .route("/ws", get(ws::upgrade))
        .with_state(web)
        .merge(assets)
        .fallback_service(static_files(site_root))
        .layer(sessions)
        .layer(middleware::map_response(security_headers))
}

fn static_files(dir: &Path) -> ServeDir {
    ServeDir::new(dir).precompressed_br().precompressed_gzip()
}

/// No page may be framed by another site, sniffed into another type, or leak its address.
async fn security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    response
}

/// Lets browsers keep the assets they got; a missing one is asked for again.
async fn cache_forever(mut response: Response) -> Response {
    if response.status().is_success() {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, CACHE_FOREVER);
    }
    response
}

/// The interface's page, read at each request and checked again by browsers each time: a new
/// build names new assets.
async fn serve_index(index: &Path) -> Response {
    match tokio::fs::read_to_string(index).await {
        Ok(html) => ([(header::CACHE_CONTROL, "no-cache")], Html(html)).into_response(),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            (StatusCode::SERVICE_UNAVAILABLE, NOT_BUILT).into_response()
        }
        Err(error) => {
            tracing::error!("cannot read {}: {error}", index.display());
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Sends visitors without a session to the sign-in page.
async fn require_session(
    State(web): State<Arc<Web>>,
    session: Session,
    request: Request,
    next: Next,
) -> Response {
    if signed_in_as(&session, &web.users).await.is_none() {
        return Redirect::to("/sign-in").into_response();
    }
    renew(&session);
    next.run(request).await
}

/// The user `session` is signed in as: the one it names, while that name is one of `users`. A
/// session from before names (which held `signed_in` alone) is no one's, and so is one whose
/// person is no longer configured: nothing else of the request is ever read as an identity.
pub async fn signed_in_as(session: &Session, users: &Users) -> Option<User> {
    let name = session.get::<String>(USER).await.ok().flatten()?;
    users.get(&name).cloned()
}

/// Starts the session's idle time again, and sends its cookie with a fresh lifetime.
pub fn renew(session: &Session) {
    session.set_expiry(Some(Expiry::OnInactivity(SESSION_IDLE)));
}

#[derive(Deserialize)]
struct SignInForm {
    /// Absent from the form of before names, which a single user may still post.
    #[serde(default)]
    name: String,
    password: String,
}

async fn sign_in(
    State(web): State<Arc<Web>>,
    session: Session,
    Form(mut form): Form<SignInForm>,
) -> Response {
    // Wiped as the password is: a password typed in the name's box is not kept either.
    let name = Zeroizing::new(web.named(&form.name).to_owned());
    form.name.zeroize();
    // An unknown name and a wrong password take the same compare, the same count and the same
    // answer: nothing tells whether a name is someone's.
    let ok = web.passwords.check(&name, form.password.as_bytes());
    form.password.zeroize();
    match attempt(&web.throttle, &name, ok).await {
        Attempt::Accepted => {}
        Attempt::Wrong => return Redirect::to("/sign-in?error=1").into_response(),
        Attempt::TooMany => return Redirect::to("/sign-in?error=2").into_response(),
    }
    // A new id, so an id planted in the browser before sign-in is worthless after it.
    let signed_in = async {
        session.cycle_id().await?;
        session.insert(USER, name.as_str()).await
    };
    match signed_in.await {
        Ok(()) => Redirect::to("/").into_response(),
        Err(error) => {
            tracing::error!("cannot sign in: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn sign_out(session: Session) -> Response {
    if let Err(error) = session.flush().await {
        tracing::error!("cannot sign out: {error}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    Redirect::to("/sign-in").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn repeated_wrong_passwords_are_throttled() {
        let throttle = Mutex::new(Throttle::default());
        let attempt = |right| attempt(&throttle, "alix", right);
        for _ in 0..MAX_FAILURES {
            let before = Instant::now();
            assert_eq!(attempt(false).await, Attempt::Wrong);
            assert!(before.elapsed() >= FAILURE_DELAY);
        }
        // Locked: even the right password is refused, at once.
        let before = Instant::now();
        assert_eq!(attempt(true).await, Attempt::TooMany);
        assert_eq!(attempt(false).await, Attempt::TooMany);
        assert!(before.elapsed() < FAILURE_DELAY);

        tokio::time::advance(LOCKOUT).await;
        assert_eq!(attempt(true).await, Attempt::Accepted);

        // A success starts the count again.
        for _ in 0..MAX_FAILURES - 1 {
            assert_eq!(attempt(false).await, Attempt::Wrong);
        }
        assert_eq!(attempt(true).await, Attempt::Accepted);
        for _ in 0..MAX_FAILURES - 1 {
            assert_eq!(attempt(false).await, Attempt::Wrong);
        }
        assert_eq!(attempt(true).await, Attempt::Accepted);
    }

    /// One name locked out leaves every other name in, a name that is no one's included, and a
    /// name that is no one's locks out after as many mistakes, at the same pace, as a person's:
    /// the throttle tells no names apart.
    #[tokio::test(start_paused = true)]
    async fn lockout_is_per_name_and_alike_for_unknown_names() {
        let throttle = Mutex::new(Throttle::default());
        let mut paces = Vec::new();
        for name in ["marie", "nobody"] {
            let mut pace = Vec::new();
            for _ in 0..MAX_FAILURES + 1 {
                let before = Instant::now();
                pace.push((attempt(&throttle, name, false).await, before.elapsed()));
            }
            paces.push(pace);
        }
        assert_eq!(paces[0], paces[1]);
        assert_eq!(paces[0][MAX_FAILURES as usize].0, Attempt::TooMany);
        // Both locked; the others are not.
        assert_eq!(attempt(&throttle, "alix", true).await, Attempt::Accepted);
        assert_eq!(attempt(&throttle, "someone", false).await, Attempt::Wrong);
        assert_eq!(attempt(&throttle, "marie", true).await, Attempt::TooMany);
    }

    /// Made-up names cannot fill the memory, nor lift a lockout by pushing it out.
    #[tokio::test(start_paused = true)]
    async fn made_up_names_neither_grow_the_count_nor_lift_a_lockout() {
        let throttle = Mutex::new(Throttle::default());
        for _ in 0..MAX_FAILURES {
            attempt(&throttle, "marie", false).await;
        }
        for n in 0..MAX_NAMES + 10 {
            // Not awaited: the count is taken before the delay.
            let _ = std::pin::pin!(attempt(&throttle, &format!("name-{n}"), false))
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()));
        }
        assert!(throttle.lock().unwrap().names.len() <= MAX_NAMES);
        assert_eq!(attempt(&throttle, "marie", true).await, Attempt::TooMany);
    }

    fn passwords() -> Passwords {
        Passwords::new(
            [("alix".to_owned(), SecretString::from("password-of-alix"))]
                .into_iter()
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn a_password_opens_only_its_own_name() {
        let passwords = passwords();
        assert!(passwords.check("alix", b"password-of-alix"));
        assert!(!passwords.check("alix", b"password-of-marie"));
        assert!(!passwords.check("Alix", b"password-of-alix"));
        assert!(!passwords.check("marie", b"password-of-alix"));
        // The stand-in opens nothing, whatever is typed.
        let dummy = secrecy::ExposeSecret::expose_secret(&passwords.dummy).to_owned();
        assert!(!passwords.check("marie", dummy.as_bytes()));
        assert!(!passwords.check("", dummy.as_bytes()));
    }
}
