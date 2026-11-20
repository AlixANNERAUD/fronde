//! The chat's web side: each person signs in with their name and password, kept in a session
//! cookie naming them, sessions kept in the database, and the web interface's files.

use crate::sessions::{BINDING, Sessions, USER};
use crate::ws;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use fronde_agent::threads::Threads;
use fronde_requests::{Requests, User, Users};
use hmac::{Hmac, KeyInit, Mac};
use secrecy::zeroize::{Zeroize, Zeroizing};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::time::Instant;
use tower_http::services::ServeDir;
use tower_sessions::cookie::SameSite;
use tower_sessions::cookie::time;
use tower_sessions::{Expiry, Session, SessionManagerLayer};

/// How long a session survives without a signed-in request; each one renews it ([`renew`]).
const SESSION_IDLE: time::Duration = time::Duration::days(30);
/// Wrong passwords for one name, each within [`FORGET_AFTER`] of the one before, before sign-in
/// under that name is refused for [`LOCKOUT`].
const MAX_FAILURES: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(60);
/// How long a wrong password waits before its answer.
const FAILURE_DELAY: Duration = Duration::from_secs(1);
/// Names typed whose wrong passwords are counted at once, so that typing made-up names cannot
/// fill the memory; past it, a new name is refused until some are forgotten ([`Throttle`]).
const MAX_NAMES: usize = 4096;
/// How long a name's wrong passwords are remembered after the last one: a few mistakes now and a
/// few next month never add up to a lockout.
const FORGET_AFTER: Duration = Duration::from_secs(15 * 60);
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
    /// The name a sign-in is for: the one typed, without the spaces around it; when a single
    /// user is configured (the legacy form, or `[users]` listing one), an empty one is theirs, so
    /// the form from before names keeps working.
    fn named<'a>(&'a self, typed: &'a str) -> &'a str {
        let typed = typed.trim();
        match self.users.iter().next() {
            Some(only) if typed.is_empty() && self.users.len() == 1 => &only.name,
            _ => typed,
        }
    }
}

type HmacSha256 = Hmac<Sha256>;

/// The HMAC-SHA256 of `parts` under `key`, each part preceded by its length so that no two
/// lists of parts read alike.
fn keyed(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac =
        <HmacSha256 as KeyInit>::new_from_slice(key).expect("HMAC takes keys of any length");
    for part in parts {
        mac.update(&(part.len() as u64).to_be_bytes());
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}

/// The key sessions are bound with, derived from `server.token`: a secret that is never in
/// `fronde.db` and that the owner already keeps, so that a copy of the database gives no way to
/// test password guesses against the bindings, and nothing new has to be made, kept or backed
/// up. Changing `server.token` signs everyone out of the chat.
fn binding_key(token: &SecretString) -> Zeroizing<[u8; 32]> {
    Zeroizing::new(keyed(
        token.expose_secret().as_bytes(),
        &[b"fronde session binding v1"],
    ))
}

/// What a session signed in as `name` with `password` carries under `server.token` `token`, as
/// [`Passwords`] makes it: for tests that write a session themselves.
pub fn session_binding(token: &SecretString, name: &str, password: &SecretString) -> String {
    Person::of(&binding_key(token), name, password).binding
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What is kept of a person's password: its digest, which a typed password is compared with,
/// and the binding a session signed in with it carries.
struct Person {
    digest: [u8; 32],
    binding: String,
}

impl Person {
    fn of(key: &[u8; 32], name: &str, password: &SecretString) -> Self {
        let password = password.expose_secret().as_bytes();
        Self {
            digest: Sha256::digest(password).into(),
            binding: hex(&keyed(key, &[name.as_bytes(), password])),
        }
    }
}

/// Each person's password, as its digest and session binding only, and a stand-in that a name
/// nobody has is compared with.
pub struct Passwords {
    by_name: HashMap<String, Person>,
    /// Random, made at each start: a name that is no one's takes the same compare as anyone's,
    /// so the time an answer takes does not tell names apart.
    dummy: [u8; 32],
}

impl Passwords {
    /// Reads each password once, then drops it: only its digest and binding are kept, the
    /// bindings under a key derived from `server.token`, `token`.
    pub fn new(
        by_name: HashMap<String, SecretString>,
        token: &SecretString,
    ) -> anyhow::Result<Self> {
        let key = binding_key(token);
        let by_name = by_name
            .into_iter()
            .map(|(name, password)| {
                let person = Person::of(&key, &name, &password);
                (name, person)
            })
            .collect();
        let mut dummy = [0; 32];
        getrandom::fill(&mut dummy).map_err(|e| anyhow::anyhow!("no randomness available: {e}"))?;
        Ok(Self { by_name, dummy })
    }

    /// Whether `given` is the password of `name`; false for a name that is no one's, after the
    /// same compare. Digests are compared, so neither the content nor the length of a password
    /// leaks through timing.
    fn check(&self, name: &str, given: &[u8]) -> bool {
        let (expected, known) = match self.by_name.get(name) {
            Some(person) => (&person.digest, true),
            None => (&self.dummy, false),
        };
        let given: [u8; 32] = Sha256::digest(given).into();
        bool::from(given.as_slice().ct_eq(expected.as_slice())) & known
    }

    /// The binding a session signed in as `name` carries.
    fn binding(&self, name: &str) -> Option<&str> {
        self.by_name.get(name).map(|person| person.binding.as_str())
    }

    /// Whether `binding` is the one `name`'s current password makes: false for a name that is
    /// no one's, a password changed since, or another `server.token`.
    pub fn binds(&self, name: &str, binding: &str) -> bool {
        self.by_name
            .get(name)
            .is_some_and(|person| bool::from(person.binding.as_bytes().ct_eq(binding.as_bytes())))
    }
}

/// Wrong passwords per name typed: one person's mistakes never lock another out, and a name that
/// is no one's is counted as anyone's is, so a lockout tells no names apart. Names are kept as
/// their HMAC under a key made at each start, never as typed: a password typed in the name's box
/// is not kept, and cannot be tested against the table either.
pub struct Throttle {
    key: Zeroizing<[u8; 32]>,
    names: HashMap<[u8; 32], Tries>,
}

struct Tries {
    failures: u32,
    locked_until: Option<Instant>,
    /// The last wrong password.
    last: Instant,
}

// A lockout ends before its name may be forgotten: an entry forgotten is never locked.
const _: () = assert!(LOCKOUT.as_secs() < FORGET_AFTER.as_secs());

impl Tries {
    fn forgotten(&self, now: Instant) -> bool {
        now.duration_since(self.last) >= FORGET_AFTER
    }
}

impl Throttle {
    pub fn new() -> anyhow::Result<Self> {
        let mut key = Zeroizing::new([0; 32]);
        getrandom::fill(key.as_mut())
            .map_err(|e| anyhow::anyhow!("no randomness available: {e}"))?;
        Ok(Self {
            key,
            names: HashMap::new(),
        })
    }

    /// What `name` is counted under. It is the name exactly as passwords are looked up
    /// (`Web::named`: trimmed, not otherwise normalised), so that one person's count is one
    /// entry, and two names that a lookup tells apart are two.
    fn key_of(&self, name: &str) -> [u8; 32] {
        keyed(self.key.as_slice(), &[name.as_bytes()])
    }

    /// The count of the name under `key`, made empty when it has none or forgot it; `None`
    /// while the table is full of names whose last mistake is recent. Those are never pushed
    /// out: typing made-up names would then reset a real name's count, or lift its lockout.
    /// Refusing the new name instead tells nothing of other names, only of how many were typed.
    fn tries(&mut self, key: [u8; 32], now: Instant) -> Option<&mut Tries> {
        if self
            .names
            .get(&key)
            .is_some_and(|tries| tries.forgotten(now))
        {
            self.names.remove(&key);
        }
        if !self.names.contains_key(&key) && self.names.len() >= MAX_NAMES {
            self.names.retain(|_, tries| !tries.forgotten(now));
            if self.names.len() >= MAX_NAMES {
                return None;
            }
        }
        Some(self.names.entry(key).or_insert(Tries {
            failures: 0,
            locked_until: None,
            last: now,
        }))
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
    {
        let mut throttle = throttle.lock().unwrap_or_else(PoisonError::into_inner);
        let key = throttle.key_of(name);
        // A full table refuses a new name whether its password is right or not: answering a
        // right one would let guesses through uncounted.
        let Some(tries) = throttle.tries(key, now) else {
            return Attempt::TooMany;
        };
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
    if signed_in_as(&session, &web).await.is_none() {
        return Redirect::to("/sign-in").into_response();
    }
    renew(&session);
    next.run(request).await
}

/// The user `session` is signed in as: the one it names, while that name is one of the users
/// and the session's binding is the one their current password makes. A session from before
/// names (which held `signed_in` alone) is no one's, and so is one whose person is no longer
/// configured, whose password changed, or made under another `server.token`: nothing else of the
/// request is ever read as an identity.
pub async fn signed_in_as(session: &Session, web: &Web) -> Option<User> {
    let name = session.get::<String>(USER).await.ok().flatten()?;
    let binding = session.get::<String>(BINDING).await.ok().flatten()?;
    let user = web.users.get(&name)?;
    web.passwords.binds(&name, &binding).then(|| user.clone())
}

/// True when `Origin` names the host the request was sent to. Browsers send it with every
/// WebSocket handshake and every form they post, so a request without one is not from a page
/// of Fronde's.
pub fn same_origin(headers: &HeaderMap) -> bool {
    let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
    let origin_host = header(header::ORIGIN).and_then(|o| o.split_once("://").map(|(_, h)| h));
    match (origin_host, header(header::HOST)) {
        (Some(origin), Some(host)) => origin.eq_ignore_ascii_case(host),
        _ => false,
    }
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
    headers: HeaderMap,
    Form(mut form): Form<SignInForm>,
) -> Response {
    // Another site's form would sign the browser in as someone else, or lock a name out from
    // its visitors' browsers: refused before anything is checked or counted.
    if !same_origin(&headers) {
        form.name.zeroize();
        form.password.zeroize();
        return StatusCode::FORBIDDEN.into_response();
    }
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
    let Some(binding) = web.passwords.binding(&name) else {
        tracing::error!("cannot sign in: a password was accepted for no one's name");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let signed_in = async {
        session.cycle_id().await?;
        session.insert(USER, name.as_str()).await?;
        session.insert(BINDING, binding).await
    };
    match signed_in.await {
        Ok(()) => Redirect::to("/").into_response(),
        Err(error) => {
            tracing::error!("cannot sign in: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn sign_out(session: Session, headers: HeaderMap) -> Response {
    if !same_origin(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
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
        let throttle = Mutex::new(Throttle::new().unwrap());
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
        let throttle = Mutex::new(Throttle::new().unwrap());
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
        let throttle = Mutex::new(Throttle::new().unwrap());
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

    /// Wrong passwords for `count` made-up names, each counted at once and none awaited, as
    /// parallel requests would be.
    fn flood(throttle: &Mutex<Throttle>, from: usize, count: usize) {
        for n in from..from + count {
            let _ = std::pin::pin!(attempt(throttle, &format!("name-{n}"), false))
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()));
        }
    }

    /// Made-up names typed after a few mistakes do not push those mistakes out: the next one
    /// still locks the name.
    #[tokio::test(start_paused = true)]
    async fn made_up_names_do_not_wipe_a_count() {
        let throttle = Mutex::new(Throttle::new().unwrap());
        for _ in 0..MAX_FAILURES - 1 {
            assert_eq!(attempt(&throttle, "marie", false).await, Attempt::Wrong);
        }
        flood(&throttle, 0, MAX_NAMES + 10);
        assert_eq!(attempt(&throttle, "marie", false).await, Attempt::Wrong);
        assert_eq!(attempt(&throttle, "marie", true).await, Attempt::TooMany);
    }

    /// While names with recent mistakes fill the table, a name not in it is refused, its
    /// password right or not, so that no answer tells a right password apart without counting
    /// it; once those mistakes are forgotten, it is counted again.
    #[tokio::test(start_paused = true)]
    async fn a_full_table_refuses_new_names_until_it_forgets() {
        let throttle = Mutex::new(Throttle::new().unwrap());
        flood(&throttle, 0, MAX_NAMES);
        assert_eq!(attempt(&throttle, "alix", true).await, Attempt::TooMany);
        assert_eq!(attempt(&throttle, "alix", false).await, Attempt::TooMany);
        tokio::time::advance(FORGET_AFTER).await;
        assert_eq!(attempt(&throttle, "alix", false).await, Attempt::Wrong);
        assert_eq!(attempt(&throttle, "alix", true).await, Attempt::Accepted);
        assert!(throttle.lock().unwrap().names.len() <= MAX_NAMES);
    }

    /// Mistakes are forgotten [`FORGET_AFTER`] after the last one, not sooner.
    #[tokio::test(start_paused = true)]
    async fn mistakes_are_forgotten_after_a_while() {
        let throttle = Mutex::new(Throttle::new().unwrap());
        let attempt = |right| attempt(&throttle, "marie", right);
        for _ in 0..MAX_FAILURES - 1 {
            assert_eq!(attempt(false).await, Attempt::Wrong);
        }
        tokio::time::advance(FORGET_AFTER).await;
        for _ in 0..MAX_FAILURES - 1 {
            assert_eq!(attempt(false).await, Attempt::Wrong);
        }
        assert_eq!(attempt(true).await, Attempt::Accepted);

        for _ in 0..MAX_FAILURES - 1 {
            assert_eq!(attempt(false).await, Attempt::Wrong);
        }
        tokio::time::advance(FORGET_AFTER - Duration::from_secs(2)).await;
        assert_eq!(attempt(false).await, Attempt::Wrong);
        assert_eq!(attempt(true).await, Attempt::TooMany);
    }

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn passwords() -> Passwords {
        Passwords::new(
            [("alix".to_owned(), SecretString::from("password-of-alix"))]
                .into_iter()
                .collect(),
            &TOKEN.to_owned().into(),
        )
        .unwrap()
    }

    #[test]
    fn a_password_opens_only_its_own_name() {
        let mut passwords = passwords();
        assert!(passwords.check("alix", b"password-of-alix"));
        assert!(!passwords.check("alix", b"password-of-marie"));
        assert!(!passwords.check("Alix", b"password-of-alix"));
        assert!(!passwords.check("marie", b"password-of-alix"));
        // The stand-in opens nothing, even typed exactly.
        passwords.dummy = Sha256::digest(b"stand-in").into();
        assert!(!passwords.check("marie", b"stand-in"));
        assert!(!passwords.check("", b"stand-in"));
    }

    /// A session's binding is its person's for this name, this password and this
    /// `server.token` only.
    #[test]
    fn a_binding_holds_for_one_name_password_and_token() {
        let passwords = passwords();
        let binding = |token: &str, name: &str, password: &str| {
            session_binding(&token.to_owned().into(), name, &password.to_owned().into())
        };
        let alix = binding(TOKEN, "alix", "password-of-alix");
        assert_eq!(passwords.binding("alix"), Some(alix.as_str()));
        assert!(passwords.binds("alix", &alix));
        for other in [
            binding(TOKEN, "alix", "another password"),
            binding(
                "another token, at least 32 characters",
                "alix",
                "password-of-alix",
            ),
            // The name and the password are not run together.
            binding(TOKEN, "alixp", "assword-of-alix"),
            String::new(),
        ] {
            assert!(!passwords.binds("alix", &other), "{other}");
        }
        assert!(!passwords.binds("marie", &alix));
        assert_eq!(passwords.binding("marie"), None);
    }
}
