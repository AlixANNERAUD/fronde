//! What every component world shares: the engine and its epoch ticker, the linker (WASI without
//! filesystem or sockets, gated HTTP, settings), and per-call stores with their limits.

use crate::ComponentSpec;
use crate::egress::{Egress, Grants};
use anyhow::{Context, bail};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder, UpdateDeadline};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

/// Bindings for the interfaces every world imports; each world's bindgen reuses them through `with`.
pub(crate) mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit",
        inline: "package jarvis:host; world shared { import jarvis:component/settings@0.1.0; }",
        world: "jarvis:host/shared",
    });
}

/// How often the engine's epoch advances: the unit of `Limits::cpu_ticks`.
const TICK: Duration = Duration::from_millis(100);

/// Interfaces a component may not import, checked when it loads.
const DENIED_IMPORTS: [&str; 2] = ["wasi:filesystem/", "wasi:sockets/"];

/// What one call may use.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Wall-clock time, waiting on HTTP included.
    pub call_timeout: Duration,
    /// Time running guest code, in ticks of 100 ms.
    pub cpu_ticks: u64,
    pub memory_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            call_timeout: Duration::from_secs(60),
            cpu_ticks: 100,
            memory_bytes: 64 << 20,
        }
    }
}

/// The engine and linker shared by every component, and the thread that ticks the epoch.
#[derive(Clone)]
pub struct ComponentHost {
    inner: Arc<HostInner>,
}

struct HostInner {
    engine: Engine,
    linker: Linker<HostState>,
    limits: Limits,
    /// Dropping it stops the ticker.
    _stop_ticker: mpsc::Sender<()>,
}

impl ComponentHost {
    pub fn new() -> anyhow::Result<Self> {
        Self::with_limits(Limits::default())
    }

    pub fn with_limits(limits: Limits) -> anyhow::Result<Self> {
        let mut config = Config::new();
        config.epoch_interruption(true);
        let engine = Engine::new(&config).map_err(anyhow::Error::from)?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
        bindings::jarvis::component::settings::add_to_linker::<_, HasSelf<HostState>>(
            &mut linker,
            |s| s,
        )?;
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

    /// Compile a component and refuse it if it asks for the filesystem or sockets.
    pub(crate) fn compile(&self, spec: &ComponentSpec) -> anyhow::Result<Component> {
        let engine = self.engine();
        let component = Component::from_file(engine, &spec.path)
            .map_err(|e| anyhow::anyhow!("cannot load component {}: {e:#}", spec.path.display()))?;
        for (import, _) in component.component_type().imports(engine) {
            if DENIED_IMPORTS.iter().any(|d| import.starts_with(d)) {
                bail!(
                    "component {} imports {import}: components get no filesystem or sockets",
                    spec.name
                );
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

impl bindings::jarvis::component::settings::Host for HostState {
    fn get(&mut self, key: String) -> Option<String> {
        self.settings.get(&key).cloned()
    }
}

/// One component's runtime rights: what it may reach and the settings it reads.
pub(crate) struct Sandbox {
    host: ComponentHost,
    pub(crate) name: String,
    grants: Grants,
    settings: Arc<BTreeMap<String, String>>,
}

impl Sandbox {
    pub(crate) fn new(host: &ComponentHost, spec: ComponentSpec) -> anyhow::Result<Self> {
        Ok(Self {
            host: host.clone(),
            grants: Grants::new(&spec.allow, &spec.credentials)?,
            name: spec.name,
            settings: Arc::new(spec.settings),
        })
    }

    /// A fresh store for one call, with the call's limits, and whether it ran out of CPU.
    pub(crate) fn store(&self) -> (Store<HostState>, Arc<AtomicBool>) {
        let limits = &self.host.inner.limits;
        let mut store = Store::new(
            self.host.engine(),
            HostState {
                wasi: WasiCtx::builder().build(),
                http: WasiHttpCtx::new(),
                table: ResourceTable::new(),
                egress: Egress {
                    grants: self.grants.clone(),
                },
                settings: self.settings.clone(),
                limits: StoreLimitsBuilder::new()
                    .memory_size(limits.memory_bytes)
                    .build(),
            },
        );
        store.limiter(|s| &mut s.limits);
        // The deadline comes every tick while guest code runs, and once when it resumes after
        // waiting, so the count follows CPU time; waiting on HTTP is left to the timeout.
        // Yielding at each tick lets that timeout fire even if the guest never waits.
        let budget = limits.cpu_ticks;
        let mut used = 0;
        let out_of_cpu = Arc::new(AtomicBool::new(false));
        let flag = out_of_cpu.clone();
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(move |_| {
            used += 1;
            if used > budget {
                flag.store(true, Ordering::Relaxed);
                return Ok(UpdateDeadline::Interrupt);
            }
            Ok(UpdateDeadline::Yield(1))
        });
        (store, out_of_cpu)
    }

    /// Run one export within the call timeout; any failure becomes a message for the caller.
    pub(crate) async fn run<R>(
        &self,
        out_of_cpu: &AtomicBool,
        call: impl Future<Output = wasmtime::Result<R>>,
    ) -> Result<R, String> {
        let limits = &self.host.inner.limits;
        let name = &self.name;
        match tokio::time::timeout(limits.call_timeout, call).await {
            Ok(Ok(r)) => Ok(r),
            Err(_) => Err(format!(
                "the {name} component timed out after {} s; try again later or with a smaller request",
                limits.call_timeout.as_secs_f32()
            )),
            Ok(Err(_)) if out_of_cpu.load(Ordering::Relaxed) => Err(format!(
                "the {name} component used more than {} ms of CPU and was stopped; try a smaller request",
                limits.cpu_ticks * TICK.as_millis() as u64
            )),
            Ok(Err(e)) => Err(format!("the {name} component failed: {}", e.root_cause())),
        }
    }
}
