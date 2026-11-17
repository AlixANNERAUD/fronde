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

/// A start error that something other than the configuration fixes while Fronde runs, such as
/// a stored secret not set yet: [`Supervisor::start`] then keeps trying the source with backoff
/// instead of giving it up. It reads as its message, which says what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting(pub String);

impl std::fmt::Display for Waiting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Waiting {}

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

/// Which failed starts a supervised source is tried again after.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Retry {
    /// Every one: a connection may come back by itself.
    Every,
    /// Only a [`Waiting`] one; any other error gives the source up as [`Health::Failed`].
    Waiting,
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
    /// cannot fix themselves (the hub's own tools, a component). A source [`Waiting`] for
    /// something outside the configuration is the exception: it is tried again with backoff
    /// for as long as it is waiting, and given up at the first other error. If it starts and
    /// later stops, it is restarted with backoff, on the same terms.
    pub async fn start(&self, source: Arc<dyn ToolSource>) -> anyhow::Result<()> {
        let name = source.name().to_string();
        self.set(&name, Health::Starting);
        match source.start().await {
            Ok(session) => {
                if let Some(closed) = self.up(&name, session) {
                    tokio::spawn(self.clone().run(source, Some(closed), Retry::Waiting));
                }
                Ok(())
            }
            Err(e) if e.downcast_ref::<Waiting>().is_some() => {
                self.set(
                    &name,
                    Health::Down {
                        error: format!("{e:#}"),
                    },
                );
                tokio::spawn(self.clone().retry(source, Retry::Waiting));
                Err(e)
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
        tokio::spawn(self.clone().run(source, None, Retry::Every));
    }

    /// Start `source` again after it failed to start, until it starts.
    async fn retry(self, source: Arc<dyn ToolSource>, retry: Retry) {
        let mut delays = backoff();
        wait(&mut delays).await;
        self.keep_running(source, None, retry, delays).await;
    }

    async fn run(self, source: Arc<dyn ToolSource>, running: Option<Closed>, retry: Retry) {
        self.keep_running(source, running, retry, backoff()).await;
    }

    /// Start `source` (unless `running` is its session), and restart it whenever it stops.
    /// `delays` goes on from the waits already made, so that they keep growing.
    async fn keep_running(
        self,
        source: Arc<dyn ToolSource>,
        mut running: Option<Closed>,
        retry: Retry,
        mut delays: ExponentialBackoff,
    ) {
        let name = source.name().to_string();
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
                Err(e) if retry == Retry::Waiting && e.downcast_ref::<Waiting>().is_none() => {
                    tracing::error!("{name}: cannot start, given up: {e:#}");
                    self.set(
                        &name,
                        Health::Failed {
                            error: format!("{e:#}"),
                        },
                    );
                    return;
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

    /// Fails its first start as [`Waiting`], then starts with one tool.
    #[derive(Default)]
    struct Patient {
        starts: AtomicUsize,
    }

    #[async_trait]
    impl ToolSource for Patient {
        fn name(&self) -> &str {
            "patient"
        }

        async fn start(&self) -> anyhow::Result<Session> {
            if self.starts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(
                    anyhow::Error::new(Waiting("set the secret".into())).context("credential")
                );
            }
            let tool = typed_tool("patient_ok", "Ok.", true, |_: Value| async {
                ToolOutput::ok(true)
            });
            Ok(Session::lasting(vec![tool]))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn start_retries_a_waiting_source() {
        let registry = Arc::new(Registry::new());
        let supervisor = Supervisor::new(registry.clone());
        let source = Arc::new(Patient::default());
        let e = supervisor.start(source.clone()).await.unwrap_err();
        assert_eq!(format!("{e:#}"), "credential: set the secret");
        assert!(matches!(
            supervisor.health()["patient"],
            Health::Down { ref error } if error == "credential: set the secret"
        ));
        assert!(registry.get("patient_ok").is_none());

        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(supervisor.health()["patient"], Health::Up { tools: 1 });
        assert_eq!(source.starts.load(Ordering::SeqCst), 2);
        assert_eq!(
            registry.call("patient_ok", json!({})).await.value,
            json!(true)
        );
    }

    /// Waits at its first start, then fails with a plain error, then would start.
    #[derive(Default)]
    struct WaitsThenBreaks {
        starts: AtomicUsize,
    }

    #[async_trait]
    impl ToolSource for WaitsThenBreaks {
        fn name(&self) -> &str {
            "breaks"
        }

        async fn start(&self) -> anyhow::Result<Session> {
            match self.starts.fetch_add(1, Ordering::SeqCst) {
                0 => Err(Waiting("set the secret".into()).into()),
                1 => anyhow::bail!("the master key does not open the stored secrets"),
                _ => Ok(Session::lasting(vec![])),
            }
        }
    }

    /// A source that waited and then fails for another reason is given up, as one that failed
    /// at once would be: it is not tried again.
    #[tokio::test(start_paused = true)]
    async fn a_waiting_source_that_then_fails_is_given_up() {
        let registry = Arc::new(Registry::new());
        let supervisor = Supervisor::new(registry.clone());
        let source = Arc::new(WaitsThenBreaks::default());
        supervisor.start(source.clone()).await.unwrap_err();
        assert!(matches!(supervisor.health()["breaks"], Health::Down { .. }));

        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(matches!(
            supervisor.health()["breaks"],
            Health::Failed { ref error } if error.contains("does not open")
        ));
        tokio::time::sleep(Duration::from_secs(600)).await;
        assert_eq!(source.starts.load(Ordering::SeqCst), 2, "tried again");
    }

    /// Waits for as long as the test lets it.
    #[derive(Default)]
    struct AlwaysWaiting {
        starts: AtomicUsize,
    }

    #[async_trait]
    impl ToolSource for AlwaysWaiting {
        fn name(&self) -> &str {
            "waiting"
        }

        async fn start(&self) -> anyhow::Result<Session> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Err(Waiting("set the secret".into()).into())
        }
    }

    /// A waiting source is tried again after 2 s, then 4, then 8: the waits keep growing from
    /// the first one.
    #[tokio::test(start_paused = true)]
    async fn a_waiting_source_waits_longer_each_time() {
        let supervisor = Supervisor::new(Arc::new(Registry::new()));
        let source = Arc::new(AlwaysWaiting::default());
        supervisor.start(source.clone()).await.unwrap_err();
        let starts = || source.starts.load(Ordering::SeqCst);
        assert_eq!(starts(), 1);
        // Tries are due at 2, 2 + 4 and 2 + 4 + 8 seconds: look just before and after each.
        let begin = tokio::time::Instant::now();
        for (at, expected) in [(1, 1), (3, 2), (5, 2), (7, 3), (13, 3), (15, 4)] {
            tokio::time::sleep_until(begin + Duration::from_secs(at)).await;
            settle().await;
            assert_eq!(starts(), expected, "at {at} s");
        }
        assert!(matches!(
            supervisor.health()["waiting"],
            Health::Down { .. }
        ));
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
