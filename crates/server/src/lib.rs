//! HTTP front: /mcp behind a bearer token, /health, and the chat with Jarvis when `[web]` is set.

mod web;
mod ws;

use anyhow::{Context, bail};
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use jarvis_agent::acp::AgentSpec;
use jarvis_agent::jarvis::{Connector, Jarvis, process_connector};
use jarvis_hub::Hub;
use jarvis_hub::config::Config;
use jarvis_hub::mcp::HubServer;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
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
        // Only /mcp: unknown paths stay 404 and other routers can be merged in.
        .route_layer(middleware::from_fn_with_state(
            Arc::new(token),
            require_token,
        ));
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .merge(protected)
}

/// Where this machine reaches a server listening on `address`: a server bound to every
/// interface is reached over the loopback of the same family.
fn reachable(address: SocketAddr) -> SocketAddr {
    let ip = match address.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, address.port())
}

/// Succeeds when the server listening on `address` answers `/health` with a success.
pub async fn healthcheck(address: SocketAddr) -> anyhow::Result<()> {
    let url = format!("http://{}/health", reachable(address));
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("cannot reach {url}"))?;
    if !response.status().is_success() {
        bail!("{url} answered {}", response.status());
    }
    Ok(())
}

pub fn check_password(password: &str) -> anyhow::Result<()> {
    if password.is_empty() {
        bail!("web.password must not be empty");
    }
    Ok(())
}

fn system_prompt(user: &str) -> String {
    format!(
        "You are Jarvis, {user}'s personal assistant. Answer in the user's language, briefly and warmly. Use your jarvis tools for memory, calendar and reminders; never invent data they did not return."
    )
}

/// Where the web interface's files are: `JARVIS_SITE_ROOT`, else where `mise run web-build`
/// writes them.
fn site_root() -> PathBuf {
    std::env::var_os("JARVIS_SITE_ROOT").map_or_else(
        || PathBuf::from("target/dx/jarvis-web/release/web/public"),
        PathBuf::from,
    )
}

/// Resolve secrets, start the hub and serve HTTP in the background.
pub async fn start(config: Config) -> anyhow::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    start_with(config, process_connector(), &site_root()).await
}

/// [`start`], with Jarvis reaching its agent through `connector` and the web interface's files in
/// `site_root`.
pub async fn start_with(
    config: Config,
    connector: Connector,
    site_root: &Path,
) -> anyhow::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let token = config.server.token.resolve().context("server.token")?;
    check_token(&token)?;
    let password = match &config.web {
        Some(web) => {
            let password = web.password.resolve().context("web.password")?;
            check_password(&password)?;
            Some((password, web.secure_cookie))
        }
        None => None,
    };
    let listen = config.server.listen;
    let data_dir = config.data.dir.clone();
    let user = config.data.user.clone();
    let agent = config.agent.clone();
    let hub = Arc::new(Hub::start(config).await?);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot listen on {listen}"))?;
    let addr = listener.local_addr()?;
    let mut app = router(hub, token.clone());
    if let Some((password, secure_cookie)) = password {
        let cwd = data_dir.join("agent");
        std::fs::create_dir_all(&cwd)
            .with_context(|| format!("cannot create the agent dir {}", cwd.display()))?;
        let spec = AgentSpec {
            command: agent.command,
            env: agent.env.into_iter().collect(),
            cwd,
            hub_url: format!("http://{}/mcp", reachable(addr)),
            hub_token: token,
            system_prompt: system_prompt(&user),
        };
        let jarvis = Jarvis::open(spec, &data_dir, connector)?;
        app = app.merge(web::router(
            web::Web {
                jarvis,
                password,
                throttle: Default::default(),
            },
            secure_cookie,
            site_root,
        ));
    }
    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("http server stopped: {e}");
        }
    });
    Ok((addr, handle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reachable_maps_every_interface_to_loopback() {
        let cases = [
            ("0.0.0.0:8484", "127.0.0.1:8484"),
            ("[::]:8484", "[::1]:8484"),
            ("100.64.1.2:8484", "100.64.1.2:8484"),
            ("[::1]:8484", "[::1]:8484"),
        ];
        for (listen, expected) in cases {
            let listen: SocketAddr = listen.parse().unwrap();
            assert_eq!(reachable(listen).to_string(), expected, "{listen}");
        }
    }
}
