//! HTTP front of the hub: /mcp behind a bearer token, /health.

use anyhow::{Context, bail};
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use hub::Hub;
use hub::config::Config;
use hub::mcp::HubServer;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use std::net::SocketAddr;
use std::sync::Arc;
use subtle::ConstantTimeEq;

pub fn check_token(token: &str) -> anyhow::Result<()> {
    if token.chars().count() < 32 {
        bail!("server.token must be at least 32 characters");
    }
    Ok(())
}

async fn require_token(
    State(token): State<Arc<String>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let given = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    if bool::from(given.as_bytes().ct_eq(token.as_bytes())) {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

pub fn router(hub: Arc<Hub>, token: String) -> Router {
    // Host checks are off: every /mcp request must carry the token, which a rebinding page cannot know.
    let config = StreamableHttpServerConfig::default().disable_allowed_hosts();
    let mcp = StreamableHttpService::new(
        move || Ok(HubServer::new(hub.clone())),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let protected = Router::new()
        .nest_service("/mcp", mcp)
        .layer(middleware::from_fn_with_state(
            Arc::new(token),
            require_token,
        ));
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .merge(protected)
}

/// Resolve secrets, start the hub and serve HTTP in the background.
pub async fn start(config: Config) -> anyhow::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let token = config.server.token.resolve().context("server.token")?;
    check_token(&token)?;
    let listen = config.server.listen;
    let hub = Arc::new(Hub::start(config).await?);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot listen on {listen}"))?;
    let addr = listener.local_addr()?;
    let app = router(hub, token);
    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("http server stopped: {e}");
        }
    });
    Ok((addr, handle))
}
