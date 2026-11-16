//! The chat's web side: password sign-in kept in a session cookie, and the pages behind it.

use crate::ws;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use jarvis_agent::jarvis::Jarvis;
use jarvis_app::{App, shell};
use leptos::config::LeptosOptions;
use leptos_axum::{LeptosRoutes, generate_route_list};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::time::Instant;
use tower_http::services::ServeDir;
use tower_sessions::cookie::SameSite;
use tower_sessions::cookie::time;
use tower_sessions::{Expiry, MemoryStore, Session, SessionManagerLayer};

const SIGNED_IN: &str = "signed_in";
/// How long a session survives without a request.
const SESSION_IDLE: time::Duration = time::Duration::days(30);
/// Wrong passwords in a row before sign-in is refused for [`LOCKOUT`].
const MAX_FAILURES: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(60);
/// How long a wrong password waits before its answer.
const FAILURE_DELAY: Duration = Duration::from_secs(1);

pub struct Web {
    pub jarvis: Arc<Jarvis>,
    pub password: String,
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

/// Compares digests, so neither the content nor the length of the password leaks through timing.
fn same_password(given: &str, expected: &str) -> bool {
    let given = Sha256::digest(given.as_bytes());
    let expected = Sha256::digest(expected.as_bytes());
    bool::from(given.as_slice().ct_eq(expected.as_slice()))
}

/// `site_root` holds what `cargo leptos build` writes: the browser bundle and the stylesheet.
pub fn router(web: Web, secure_cookie: bool, site_root: &Path) -> Router {
    let sessions = SessionManagerLayer::new(MemoryStore::default())
        .with_name("jarvis_session")
        .with_http_only(true)
        .with_same_site(SameSite::Lax)
        .with_secure(secure_cookie)
        .with_expiry(Expiry::OnInactivity(SESSION_IDLE));
    Router::new()
        .route("/sign-in", post(sign_in))
        .route("/sign-out", post(sign_out))
        .route("/ws", get(ws::upgrade))
        .with_state(Arc::new(web))
        .merge(pages(site_root))
        .layer(sessions)
        .layer(middleware::map_response(security_headers))
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

/// The Leptos pages, and the site root's files for any other path.
fn pages(site_root: &Path) -> Router {
    let options = LeptosOptions::builder()
        .output_name("jarvis")
        .site_root(site_root.to_string_lossy().into_owned())
        .site_pkg_dir("pkg")
        .build();
    let shell_options = options.clone();
    Router::new()
        .leptos_routes(&options, generate_route_list(App), move || {
            shell(shell_options.clone())
        })
        .route_layer(middleware::from_fn(require_session))
        .fallback_service(
            ServeDir::new(site_root)
                .precompressed_br()
                .precompressed_gzip(),
        )
        .with_state(options)
}

/// Sends visitors without a session to the sign-in page, the only page open to them.
async fn require_session(session: Session, request: Request, next: Next) -> Response {
    if request.uri().path() == "/sign-in" || is_signed_in(&session).await {
        next.run(request).await
    } else {
        Redirect::to("/sign-in").into_response()
    }
}

pub async fn is_signed_in(session: &Session) -> bool {
    matches!(session.get::<bool>(SIGNED_IN).await, Ok(Some(true)))
}

#[derive(Deserialize)]
struct SignInForm {
    password: String,
}

async fn sign_in(
    State(web): State<Arc<Web>>,
    session: Session,
    Form(form): Form<SignInForm>,
) -> Response {
    match attempt(&web.throttle, same_password(&form.password, &web.password)).await {
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
