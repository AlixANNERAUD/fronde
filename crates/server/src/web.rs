//! The chat's web side: password sign-in kept in a session cookie, sessions kept in the database,
//! and the web interface's files.

use crate::sessions::Sessions;
use crate::ws;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use fronde_agent::threads::Threads;
use fronde_requests::Requests;
use secrecy::SecretString;
use secrecy::zeroize::Zeroize;
use serde::Deserialize;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::time::Instant;
use tower_http::services::ServeDir;
use tower_sessions::cookie::SameSite;
use tower_sessions::cookie::time;
use tower_sessions::{Expiry, Session, SessionManagerLayer};

const SIGNED_IN: &str = "signed_in";
/// How long a session survives without a signed-in request; each one renews it ([`renew`]).
const SESSION_IDLE: time::Duration = time::Duration::days(30);
/// Wrong passwords in a row before sign-in is refused for [`LOCKOUT`].
const MAX_FAILURES: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(60);
/// How long a wrong password waits before its answer.
const FAILURE_DELAY: Duration = Duration::from_secs(1);
/// Asset names carry a hash of their content: a changed asset gets a new name.
const CACHE_FOREVER: HeaderValue = HeaderValue::from_static("public, max-age=31536000, immutable");
const NOT_BUILT: &str = "Interface non construite : lance `mise run web-build`.";

pub struct Web {
    pub threads: Arc<Threads>,
    pub requests: Arc<Requests>,
    /// Who signs in with the password: the owner, the one user so far.
    pub user: String,
    /// Whether that user is an admin: the owner is.
    pub admin: bool,
    pub password: SecretString,
    pub throttle: Mutex<Throttle>,
}

/// Wrong passwords in a row, for every client at once: there is a single password to guess.
#[derive(Default)]
pub struct Throttle {
    failures: u32,
    locked_until: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
enum Attempt {
    Accepted,
    Wrong,
    TooMany,
}

/// Records a sign-in attempt; a wrong password is answered after [`FAILURE_DELAY`].
async fn attempt(throttle: &Mutex<Throttle>, right: bool) -> Attempt {
    let now = Instant::now();
    {
        let mut throttle = throttle.lock().unwrap_or_else(PoisonError::into_inner);
        if throttle.locked_until.is_some_and(|until| now < until) {
            return Attempt::TooMany;
        }
        throttle.locked_until = None;
        if right {
            throttle.failures = 0;
            return Attempt::Accepted;
        }
        throttle.failures += 1;
        if throttle.failures >= MAX_FAILURES {
            throttle.failures = 0;
            throttle.locked_until = Some(now + LOCKOUT);
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
    // The signed-in pages: the main thread, any thread, the thread list, a new thread.
    let signed_in = get(page.clone()).route_layer(middleware::from_fn(require_session));
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
        .with_state(Arc::new(web))
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
async fn require_session(session: Session, request: Request, next: Next) -> Response {
    if !is_signed_in(&session).await {
        return Redirect::to("/sign-in").into_response();
    }
    renew(&session);
    next.run(request).await
}

pub async fn is_signed_in(session: &Session) -> bool {
    matches!(session.get::<bool>(SIGNED_IN).await, Ok(Some(true)))
}

/// Starts the session's idle time again, and sends its cookie with a fresh lifetime.
pub fn renew(session: &Session) {
    session.set_expiry(Some(Expiry::OnInactivity(SESSION_IDLE)));
}

#[derive(Deserialize)]
struct SignInForm {
    password: String,
}

async fn sign_in(
    State(web): State<Arc<Web>>,
    session: Session,
    Form(mut form): Form<SignInForm>,
) -> Response {
    let ok = crate::same_secret(form.password.as_bytes(), &web.password);
    form.password.zeroize();
    match attempt(&web.throttle, ok).await {
        Attempt::Accepted => {}
        Attempt::Wrong => return Redirect::to("/sign-in?error=1").into_response(),
        Attempt::TooMany => return Redirect::to("/sign-in?error=2").into_response(),
    }
    // A new id, so an id planted in the browser before sign-in is worthless after it.
    let signed_in = async {
        session.cycle_id().await?;
        session.insert(SIGNED_IN, true).await
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
        for _ in 0..MAX_FAILURES {
            let before = Instant::now();
            assert_eq!(attempt(&throttle, false).await, Attempt::Wrong);
            assert!(before.elapsed() >= FAILURE_DELAY);
        }
        // Locked: even the right password is refused, at once.
        let before = Instant::now();
        assert_eq!(attempt(&throttle, true).await, Attempt::TooMany);
        assert_eq!(attempt(&throttle, false).await, Attempt::TooMany);
        assert!(before.elapsed() < FAILURE_DELAY);

        tokio::time::advance(LOCKOUT).await;
        assert_eq!(attempt(&throttle, true).await, Attempt::Accepted);

        // A success starts the count again.
        for _ in 0..MAX_FAILURES - 1 {
            assert_eq!(attempt(&throttle, false).await, Attempt::Wrong);
        }
        assert_eq!(attempt(&throttle, true).await, Attempt::Accepted);
        for _ in 0..MAX_FAILURES - 1 {
            assert_eq!(attempt(&throttle, false).await, Attempt::Wrong);
        }
        assert_eq!(attempt(&throttle, true).await, Attempt::Accepted);
    }
}
