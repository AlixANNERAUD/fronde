//! Where tools come from. Every source (the hub's own tools, upstream MCP servers, components)
//! goes through one supervisor, which starts it, registers its tools and, when it stops working,
//! keeps its tools listed with an explanation while it restarts the source with backoff.

use crate::tools::{Registry, ToolDef, ToolHandler, ToolOutput};
use async_trait::async_trait;
use backon::{BackoffBuilder, ExponentialBackoff, ExponentialBuilder};
use serde_json::Value;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// A tool and what runs it.
pub type Tool = (ToolDef, Arc<dyn ToolHandler>);

/// Resolves when a started source stops working.
pub type Closed = Pin<Box<dyn Future<Output = ()> + Send>>;

/// A started source: its tools, and whether it can stop on its own.
pub struct Session {
    pub tools: Vec<Tool>,
    pub closed: Option<Closed>,
}

impl Session {
    /// Tools that work for as long as the hub runs: native ones, components.
    pub fn lasting(tools: Vec<Tool>) -> Self {
        Self {
            tools,
            closed: None,
        }
    }

    /// Tools that work until `closed` resolves, such as those of a connection.
    pub fn until(tools: Vec<Tool>, closed: impl Future<Output = ()> + Send + 'static) -> Self {
        Self {
            tools,
            closed: Some(Box::pin(closed)),
        }
    }
}

#[async_trait]
pub trait ToolSource: Send + Sync {
    /// The source's name, unique in the hub; its tools carry it as their `source`.
    fn name(&self) -> &str;

    /// Start the source (connect, load) and return its tools.
    async fn start(&self) -> anyhow::Result<Session>;
}

/// Tools that need nothing started: the hub's own.
pub struct Native {
    name: String,
    tools: Vec<Tool>,
}

impl Native {
    pub fn new(name: impl Into<String>, tools: Vec<Tool>) -> Self {
        Self {
            name: name.into(),
            tools,
        }
    }
}

#[async_trait]
impl ToolSource for Native {
    fn name(&self) -> &str {
        &self.name
    }

    async fn start(&self) -> anyhow::Result<Session> {
        Ok(Session::lasting(self.tools.clone()))
    }
}

/// What the supervisor knows about a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Starting,
    /// Started, with this many tools registered.
    Up {
        tools: usize,
    },
    /// Stopped or failed to start; the supervisor will try again.
    Down {
        error: String,
    },
    /// Failed to start and was given up: it needs a configuration change and a restart.
    Failed {
        error: String,
    },
}

/// The message of a tool whose source is down.
pub fn unreachable(source: &str) -> ToolOutput {
    ToolOutput::error(format!(
        "{source} is unreachable right now; retry later or do without it"
    ))
}

/// Stands in for the tools of a source that is down.
struct Unreachable(String);

#[async_trait]
impl ToolHandler for Unreachable {
    async fn call(&self, _args: Value) -> ToolOutput {
        unreachable(&self.0)
    }
}

/// Waits after a failure: 2 s, doubling up to a minute, for as long as it takes.
fn backoff() -> ExponentialBackoff {
    ExponentialBuilder::new()
        .with_min_delay(Duration::from_secs(2))
        .with_max_delay(Duration::from_secs(60))
        .with_factor(2.0)
        .without_max_times()
        .build()
}

async fn wait(delays: &mut ExponentialBackoff) {
    let delay = delays.next().unwrap_or(Duration::from_secs(60));
    tokio::time::sleep(delay).await;
}

/// Starts sources, registers their tools and restarts the ones that stop.
#[derive(Clone)]
pub struct Supervisor {
    registry: Arc<Registry>,
    health: Arc<RwLock<BTreeMap<String, Health>>>,
}

impl Supervisor {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            health: Arc::default(),
        }
    }

    /// Every source's state, by name.
    pub fn health(&self) -> BTreeMap<String, Health> {
        self.health.read().expect("health lock").clone()
    }

    fn set(&self, name: &str, health: Health) {
        self.health
            .write()
            .expect("health lock")
            .insert(name.to_string(), health);
    }

    /// Start `source` now and return its error if it fails, without retrying: for sources that
    /// cannot fix themselves (the hub's own tools, a component). If it starts and later stops,
    /// it is restarted with backoff.
    pub async fn start(&self, source: Arc<dyn ToolSource>) -> anyhow::Result<()> {
        let name = source.name().to_string();
        self.set(&name, Health::Starting);
        match source.start().await {
            Ok(session) => {
                if let Some(closed) = self.up(&name, session) {
                    tokio::spawn(self.clone().run(source, Some(closed)));
                }
                Ok(())
            }
            Err(e) => {
                self.set(
                    &name,
                    Health::Failed {
                        error: format!("{e:#}"),
                    },
                );
                Err(e)
            }
        }
    }

    /// Start `source` in the background, retrying with backoff until it starts, and restart it
    /// whenever it stops: for connections.
    pub fn spawn(&self, source: Arc<dyn ToolSource>) {
        self.set(source.name(), Health::Starting);
        tokio::spawn(self.clone().run(source, None));
    }

    async fn run(self, source: Arc<dyn ToolSource>, mut running: Option<Closed>) {
        let name = source.name().to_string();
        let mut delays = backoff();
        loop {
            if let Some(closed) = running.take() {
                closed.await;
                tracing::warn!("{name}: stopped; restarting");
                self.down(&name, "stopped".to_string());
                delays = backoff();
                wait(&mut delays).await;
            }
            match source.start().await {
                Ok(session) => {
                    running = self.up(&name, session);
                    if running.is_none() {
                        return;
                    }
                }
                Err(e) => {
                    tracing::warn!("{name}: cannot start: {e:#}");
                    self.set(
                        &name,
                        Health::Down {
                            error: format!("{e:#}"),
                        },
                    );
                    wait(&mut delays).await;
                }
            }
        }
    }

    /// Register a started source's tools in place of its earlier ones.
    fn up(&self, name: &str, session: Session) -> Option<Closed> {
        let count = self.registry.replace_source(name, session.tools);
        tracing::info!("{name}: started with {count} tools");
        self.set(name, Health::Up { tools: count });
        session.closed
    }

    /// Keep a stopped source's tools listed, each saying the source is unreachable.
    fn down(&self, name: &str, error: String) {
        let unreachable: Arc<dyn ToolHandler> = Arc::new(Unreachable(name.to_string()));
        let stubs: Vec<Tool> = self
            .registry
            .all()
            .into_iter()
            .filter(|d| d.source == name)
            .map(|d| (d, unreachable.clone()))
            .collect();
        self.registry.replace_source(name, stubs);
        self.set(name, Health::Down { error });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::typed_tool;
    use serde_json::json;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;

    /// Fails its first start, then starts with one tool that answers its start number; each
    /// started session runs until the test closes it.
    #[derive(Default)]
    struct Flaky {
        starts: AtomicUsize,
        closers: Mutex<Vec<oneshot::Sender<()>>>,
    }

    #[async_trait]
    impl ToolSource for Flaky {
        fn name(&self) -> &str {
            "flaky"
        }

        async fn start(&self) -> anyhow::Result<Session> {
            let n = self.starts.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                anyhow::bail!("not yet");
            }
            let tool = typed_tool(
                "flaky_n",
                "Start number.",
                true,
                move |_: Value| async move { ToolOutput::ok(n) },
            );
            let (close, closed) = oneshot::channel();
            self.closers.lock().unwrap().push(close);
            Ok(Session::until(vec![tool], async move {
                let _ = closed.await;
            }))
        }
    }

    async fn settle() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn supervisor_restarts_a_source_that_stops() {
        let registry = Arc::new(Registry::new());
        let supervisor = Supervisor::new(registry.clone());
        let source = Arc::new(Flaky::default());
        supervisor.spawn(source.clone());
        settle().await;
        assert!(matches!(
            supervisor.health()["flaky"],
            Health::Down { ref error } if error == "not yet"
        ));
        assert!(registry.get("flaky_n").is_none());

        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(supervisor.health()["flaky"], Health::Up { tools: 1 });
        assert_eq!(registry.get("flaky_n").unwrap().source, "flaky");
        assert_eq!(registry.call("flaky_n", json!({})).await.value, json!(2));

        // It stops: the tool stays listed and explains, then the source comes back.
        source.closers.lock().unwrap().remove(0).send(()).unwrap();
        settle().await;
        assert!(matches!(supervisor.health()["flaky"], Health::Down { .. }));
        let out = registry.call("flaky_n", json!({})).await;
        assert!(out.is_error);
        assert!(
            out.to_text().contains("flaky is unreachable"),
            "{}",
            out.to_text()
        );
        assert_eq!(registry.deferred_names(), ["flaky_n"]);

        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(supervisor.health()["flaky"], Health::Up { tools: 1 });
        assert_eq!(registry.call("flaky_n", json!({})).await.value, json!(3));
    }

    #[tokio::test(start_paused = true)]
    async fn start_reports_a_failure_and_gives_up() {
        let registry = Arc::new(Registry::new());
        let supervisor = Supervisor::new(registry.clone());
        let source = Arc::new(Flaky::default());
        let e = supervisor.start(source.clone()).await.unwrap_err();
        assert_eq!(e.to_string(), "not yet");
        assert!(matches!(
            supervisor.health()["flaky"],
            Health::Failed { .. }
        ));
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(source.starts.load(Ordering::SeqCst), 1, "no retry");
    }

    #[tokio::test]
    async fn native_tools_take_the_source_name_and_skip_clashes() {
        let registry = Arc::new(Registry::new());
        let supervisor = Supervisor::new(registry.clone());
        let tool = || typed_tool("same", "x", false, |_: Value| async { ToolOutput::ok(1) });
        supervisor
            .start(Arc::new(Native::new("hub", vec![tool()])))
            .await
            .unwrap();
        supervisor
            .start(Arc::new(Native::new("other", vec![tool()])))
            .await
            .unwrap();
        assert_eq!(registry.get("same").unwrap().source, "hub");
        assert_eq!(supervisor.health()["other"], Health::Up { tools: 0 });
    }
}
