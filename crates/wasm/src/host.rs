//! The host's runtime. Every world shares the engine and its epoch ticker, and the CPU metering and
//! timeout of a call ([`meter`], [`bounded`]); each world has its own linker and its own limits.
//! The tool world's are here: its component linker (WASI without filesystem or sockets, gated
//! HTTP, and each served version's own interfaces) and its per-call stores.

use crate::ComponentSpec;
use crate::egress::{Egress, Grants};
use crate::guest_log::GuestLog;
use anyhow::{Context, bail};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder, UpdateDeadline};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

/// How often the engine's epoch advances: the unit of `Limits::cpu_ticks`.
pub(crate) const TICK: Duration = Duration::from_millis(100);

/// Per store: enough for a Rust component (a few core instances, one memory, small tables).
const MAX_INSTANCES: usize = 32;
const MAX_MEMORIES: usize = 4;
const MAX_TABLES: usize = 8;
const MAX_TABLE_ELEMENTS: usize = 20_000;
/// Live handles (requests, streams, pollables) at once.
const MAX_RESOURCES: usize = 1_000;

/// Interfaces a tool component may not import, checked when it loads.
const DENIED_IMPORTS: [&str; 2] = ["wasi:filesystem/", "wasi:sockets/"];

/// What one call may use. Each sandbox has its own.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Wall-clock time, waiting on HTTP included.
    pub call_timeout: Duration,
    /// Running time, in 100 ms ticks during which guest code ran; time spent waiting is free.
    pub cpu_ticks: u64,
    /// For each linear memory.
    pub memory_bytes: usize,
    /// For each HTTP request, from sending it to the end of the response body.
    pub request_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            call_timeout: Duration::from_secs(60),
            cpu_ticks: 100,
            memory_bytes: 64 << 20,
            request_timeout: Duration::from_secs(30),
        }
    }
}

/// The engine shared by every world and the thread that ticks its epoch, with the tool world's
/// linker and its default limits.
#[derive(Clone)]
pub struct ComponentHost {
    inner: Arc<HostInner>,
}

struct HostInner {
    engine: Engine,
    /// The tool world's: components only.
    linker: Linker<HostState>,
    /// Given to each tool component's sandbox.
    limits: Limits,
    /// Dropping it stops the ticker.
    _stop_ticker: mpsc::Sender<()>,
}

impl ComponentHost {
    pub fn new() -> anyhow::Result<Self> {
        Self::with_limits(Limits::default())
    }

    /// `limits` are those of tool components; other worlds bring their own.
    pub fn with_limits(limits: Limits) -> anyhow::Result<Self> {
        let mut config = Config::new();
        config.epoch_interruption(true);
        let engine = Engine::new(&config).map_err(anyhow::Error::from)?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
        // Every version served, side by side: their interface names differ by version.
        crate::v0_1::add_to_linker(&mut linker)?;
        // A thread rather than a task: a guest spinning on the runtime's only thread would
        // otherwise stop the clock that interrupts it.
        let (stop, stopped) = mpsc::channel::<()>();
        let ticking = engine.clone();
        std::thread::Builder::new()
            .name("wasm-epoch".into())
            .spawn(move || {
                while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(TICK) {
                    ticking.increment_epoch();
                }
            })
            .context("cannot start the epoch ticker")?;
        Ok(Self {
            inner: Arc::new(HostInner {
                engine,
                linker,
                limits,
                _stop_ticker: stop,
            }),
        })
    }

    pub(crate) fn engine(&self) -> &Engine {
        &self.inner.engine
    }

    pub(crate) fn linker(&self) -> &Linker<HostState> {
        &self.inner.linker
    }

    /// Compile a component, refuse it if its file is no longer the `approved` build (`None` only
    /// in tests) or if it asks for the filesystem or sockets.
    pub(crate) async fn compile(
        &self,
        spec: &ComponentSpec,
        approved: Option<&str>,
    ) -> anyhow::Result<Component> {
        let engine = self.engine().clone();
        let (name, path) = (spec.name.clone(), spec.path.clone());
        let approved = approved.map(str::to_string);
        // Compiling takes CPU for a while: keep it off the runtime's worker threads.
        let compiled = tokio::task::spawn_blocking(move || {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("cannot read component {}", path.display()))?;
            // The bytes compiled are the bytes checked, whatever happens to the file meanwhile.
            if let Some(approved) = approved {
                let sha256 = crate::source::sha256(&bytes);
                if sha256 != approved {
                    bail!(
                        "{} changed since it was approved (its sha256 is now {sha256})",
                        path.display()
                    );
                }
            }
            Component::new(&engine, &bytes)
                .map(|component| (component, engine))
                .map_err(|e| anyhow::anyhow!("cannot load component {}: {e:#}", path.display()))
        });
        let (component, engine) = compiled.await.context("compiling the component failed")??;
        for (import, _) in component.component_type().imports(&engine) {
            if DENIED_IMPORTS.iter().any(|d| import.starts_with(d)) {
                bail!("component {name} imports {import}: components get no filesystem or sockets");
            }
        }
        Ok(component)
    }
}

/// The store's data: WASI with nothing granted, HTTP through the component's grants, settings.
pub(crate) struct HostState {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
    egress: Egress,
    settings: Arc<BTreeMap<String, String>>,
    limits: StoreLimits,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for HostState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.egress,
        }
    }
}

impl HostState {
    /// What the guest reads through `settings.get`, whatever the interface version.
    pub(crate) fn setting(&self, key: &str) -> Option<String> {
        self.settings.get(key).cloned()
    }
}

/// One component's runtime rights: what it may reach and the settings it reads.
pub(crate) struct Sandbox {
    host: ComponentHost,
    pub(crate) name: String,
    limits: Limits,
    grants: Grants,
    settings: Arc<BTreeMap<String, String>>,
}

impl Sandbox {
    pub(crate) fn new(host: &ComponentHost, spec: ComponentSpec) -> anyhow::Result<Self> {
        Ok(Self {
            host: host.clone(),
            limits: host.inner.limits.clone(),
            grants: Grants::new(&spec.allow, &spec.credentials)?,
            name: spec.name,
            settings: Arc::new(spec.settings),
        })
    }

    /// A fresh store for one call, with the call's limits, and whether it ran out of CPU.
    pub(crate) fn store(&self) -> (Store<HostState>, Arc<AtomicBool>) {
        let limits = &self.limits;
        let mut table = ResourceTable::new();
        table.set_max_capacity(MAX_RESOURCES);
        let mut store = Store::new(
            self.host.engine(),
            HostState {
                wasi: WasiCtx::builder().stderr(GuestLog::new(&self.name)).build(),
                http: WasiHttpCtx::new(),
                table,
                egress: Egress {
                    grants: self.grants.clone(),
                    component: self.name.as_str().into(),
                    request_timeout: limits.request_timeout,
                },
                settings: self.settings.clone(),
                limits: StoreLimitsBuilder::new()
                    .memory_size(limits.memory_bytes)
                    .memories(MAX_MEMORIES)
                    .tables(MAX_TABLES)
                    .table_elements(MAX_TABLE_ELEMENTS)
                    .instances(MAX_INSTANCES)
                    .build(),
            },
        );
        store.limiter(|s| &mut s.limits);
        let out_of_cpu = meter(&mut store, limits);
        (store, out_of_cpu)
    }

    /// Run one export within the call timeout; any failure becomes a message for the caller.
    pub(crate) async fn run<R>(
        &self,
        out_of_cpu: &AtomicBool,
        call: impl Future<Output = wasmtime::Result<R>>,
    ) -> Result<R, String> {
        let what = format!("the {} component", self.name);
        bounded(&what, &self.limits, out_of_cpu, call).await
    }
}

/// Meter the guest code a store runs against `limits.cpu_ticks`, and return the flag set when it
/// runs out.
///
/// The deadline fires at the first check after each tick while guest code runs. A callback long
/// after the previous one means the guest was waiting in between (on HTTP or a timer), so it is
/// not charged: the count is the ticks during which guest code ran. Yielding at each tick lets the
/// call timeout fire even if the guest never waits.
pub(crate) fn meter<T: 'static>(store: &mut Store<T>, limits: &Limits) -> Arc<AtomicBool> {
    let budget = limits.cpu_ticks;
    let mut used = 0;
    let mut last = Instant::now();
    let out_of_cpu = Arc::new(AtomicBool::new(false));
    let flag = out_of_cpu.clone();
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(move |_| {
        let now = Instant::now();
        if now.duration_since(last) < 2 * TICK {
            used += 1;
        }
        last = now;
        if used > budget {
            flag.store(true, Ordering::Relaxed);
            return Ok(UpdateDeadline::Interrupt);
        }
        Ok(UpdateDeadline::Yield(1))
    });
    out_of_cpu
}

/// Run `call` within `limits.call_timeout`; any failure becomes a message naming `what` (such as
/// "the echo component") for the caller.
pub(crate) async fn bounded<R>(
    what: &str,
    limits: &Limits,
    out_of_cpu: &AtomicBool,
    call: impl Future<Output = wasmtime::Result<R>>,
) -> Result<R, String> {
    match tokio::time::timeout(limits.call_timeout, call).await {
        Ok(Ok(r)) => Ok(r),
        Err(_) => Err(format!(
            "{what} timed out after {} s; try again later or with a smaller request",
            limits.call_timeout.as_secs_f32()
        )),
        Ok(Err(_)) if out_of_cpu.load(Ordering::Relaxed) => Err(format!(
            "{what} used up its compute budget (about {} s of running time) and was stopped; try \
             a smaller request",
            limits.cpu_ticks as f32 * TICK.as_secs_f32()
        )),
        Ok(Err(e)) => Err(format!("{what} failed: {}", e.root_cause())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::{Caller, Linker as CoreLinker, Module};

    /// `spin` never returns; `wait` calls the host's `sleep` five times in a loop, so it spends
    /// about a second waiting and almost none running.
    const GUEST: &str = r#"
        (module
          (import "host" "sleep" (func $sleep))
          (func (export "spin") (loop br 0))
          (func (export "wait") (local $n i32)
            (loop
              call $sleep
              local.get $n
              i32.const 1
              i32.add
              local.tee $n
              i32.const 5
              i32.lt_u
              br_if 0)))
    "#;

    async fn call(host: &ComponentHost, limits: &Limits, export: &str) -> Result<(), String> {
        let module = Module::new(host.engine(), GUEST).unwrap();
        let mut linker = CoreLinker::<()>::new(host.engine());
        linker
            .func_wrap_async("host", "sleep", |_: Caller<'_, ()>, (): ()| {
                Box::new(tokio::time::sleep(2 * TICK))
            })
            .unwrap();
        let mut store = Store::new(host.engine(), ());
        let out_of_cpu = meter(&mut store, limits);
        let run = async {
            let instance = linker.instantiate_async(&mut store, &module).await?;
            let f = instance.get_typed_func::<(), ()>(&mut store, export)?;
            f.call_async(&mut store, ()).await
        };
        bounded("the test guest", limits, &out_of_cpu, run).await
    }

    #[tokio::test]
    async fn metering_charges_only_running_ticks() {
        let host = ComponentHost::new().unwrap();
        let limits = Limits {
            cpu_ticks: 3,
            ..Limits::default()
        };
        let started = Instant::now();
        let e = call(&host, &limits, "spin").await.unwrap_err();
        assert!(e.contains("compute budget"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(5), "{e}");
        // Ten ticks of waiting, more than the budget of three, none of it running.
        call(&host, &limits, "wait").await.unwrap();
    }
}
