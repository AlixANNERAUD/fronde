//! The chat's web side: password sign-in kept in a session cookie, and the pages behind it.

use crate::ws;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use jarvis_agent::jarvis::Jarvis;
use serde::Deserialize;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tower_sessions::cookie::SameSite;
use tower_sessions::{MemoryStore, Session, SessionManagerLayer};

const SIGNED_IN: &str = "signed_in";

pub struct Web {
    pub jarvis: Arc<Jarvis>,
    pub password: String,
}

pub fn router(web: Web, secure_cookie: bool) -> Router {
    let sessions = SessionManagerLayer::new(MemoryStore::default())
        .with_name("jarvis_session")
        .with_http_only(true)
        .with_same_site(SameSite::Lax)
        .with_secure(secure_cookie);
    Router::new()
        .route("/", get(home))
        .route("/sign-in", get(sign_in_page).post(sign_in))
        .route("/sign-out", post(sign_out))
        .route("/ws", get(ws::upgrade))
        .with_state(Arc::new(web))
        .layer(sessions)
}

pub async fn is_signed_in(session: &Session) -> bool {
    matches!(session.get::<bool>(SIGNED_IN).await, Ok(Some(true)))
}

// Placeholder pages: the Leptos app replaces them.
async fn home(session: Session) -> Response {
    if !is_signed_in(&session).await {
        return Redirect::to("/sign-in").into_response();
    }
    Html(
        "<!doctype html><title>Jarvis</title><h1>Jarvis</h1>\
         <form method=\"post\" action=\"/sign-out\"><button>Se déconnecter</button></form>",
    )
    .into_response()
}

#[derive(Deserialize)]
struct SignInQuery {
    error: Option<String>,
}

async fn sign_in_page(Query(query): Query<SignInQuery>) -> Html<String> {
    let error = if query.error.is_some() {
        "<p>Mot de passe incorrect.</p>"
    } else {
        ""
    };
    Html(format!(
        "<!doctype html><title>Jarvis</title>{error}\
         <form method=\"post\" action=\"/sign-in\">\
         <input type=\"password\" name=\"password\" aria-label=\"Mot de passe\" autofocus required>\
         <button>Se connecter</button></form>"
    ))
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
    if !bool::from(form.password.as_bytes().ct_eq(web.password.as_bytes())) {
        return Redirect::to("/sign-in?error=1").into_response();
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
