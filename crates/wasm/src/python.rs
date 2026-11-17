//! The script world: CPython for WASI, which runs short calculations for agents.
//!
//! A host-side profile, not a WIT world: `python.wasm` is a plain `wasm32-wasip1` command run with
//! an argv, a JSON envelope on stdin and a JSON reply on stdout. It has its own core linker, which
//! holds WASI preview 1 and nothing else, so a module importing anything more (sockets, HTTP)
//! cannot instantiate. Its WASI grants arguments, stdin, a capped stdout, stderr to the log, the
//! clocks, random, and one read-only directory, the standard library at `/lib`: no environment, no
//! other file, no `/tmp` (prints are captured in memory), no network.
//!
//! The module is compiled once, in memory, from the bytes whose hash matched the pinned build.
//! Each run gets a fresh store and instance, with its own memory, CPU and time limits, so nothing
//! carries from one run to the next and a failure only ends that run.

use crate::guest_log::GuestLog;
use crate::host::{ComponentHost, Limits, TICK, bounded, meter};
use anyhow::{Context, bail, ensure};
use async_trait::async_trait;
use fronde_hub::config::PYTHON_SOURCE;
use fronde_hub::source::{Session, Tool, ToolSource};
use fronde_hub::tools::{ToolDef, ToolHandler, ToolOutput};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use wasmtime::{InstancePre, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::p1::WasiP1Ctx;
use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::{FsPerms, I32Exit, WasiCtxBuilder};

/// SHA-256 of the only `python.wasm` Fronde runs: CPython 3.15.2 for WASI SDK 33, from
/// brettcannon/cpython-wasi-build. The `python` task of `mise.toml` pins the same hash.
pub const PYTHON_WASM_SHA256: &str =
    "dd076791fb6d1286a5dfe105852dfb3367e10562de68ca0a618e615625cdf636";
/// The version of the pinned build, for messages.
pub const PYTHON_VERSION: &str = "3.15.2";
/// The standard library, under the runtime's directory; it is `/lib/python3.15` in the guest.
const STDLIB: &str = "lib/python3.15";

/// Reads the envelope, runs the code and writes the reply (`python/runner.py`).
const RUNNER: &str = include_str!("python/runner.py");

/// Everything the guest may write to stdout. The runner keeps its reply under 60 KiB.
const OUTPUT_BYTES: usize = 64 << 10;
/// Characters of printed output repeated in an error message.
const ERROR_TAIL: usize = 2_000;

/// CPython declares a 40 MiB minimum memory; 256 MiB leaves room to load and aggregate about
/// 100 000 rows of JSON. Only pages actually touched are committed.
const MEMORY_BYTES: usize = 256 << 20;
/// CPython is one core module with one memory and one table of 6005 functions, which cannot grow
/// (nothing is loaded dynamically).
const MAX_TABLES: usize = 1;
const MAX_TABLE_ELEMENTS: usize = 10_000;

fn script_limits() -> Limits {
    Limits {
        call_timeout: Duration::from_secs(30),
        cpu_ticks: 100,
        memory_bytes: MEMORY_BYTES,
        // The script world makes no requests.
        request_timeout: Duration::ZERO,
    }
}

struct ScriptState {
    wasi: WasiP1Ctx,
    limits: StoreLimits,
}

/// The loaded runtime: cheap to clone, each run in its own instance.
#[derive(Clone)]
pub struct Python {
    inner: Arc<Inner>,
    limits: Limits,
}

struct Inner {
    host: ComponentHost,
    pre: InstancePre<ScriptState>,
    lib: PathBuf,
}

/// What the runner writes as the last line of its stdout.
#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    value: Value,
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stdout_truncated: bool,
    error: Option<String>,
    next: Option<String>,
}

impl Python {
    /// Load `dir/python.wasm` if it is the pinned build, and compile it (about 0.6 s on 8 cores,
    /// 3 s on one). `dir/lib/python3.15` holds the standard library.
    pub async fn load(host: &ComponentHost, dir: &Path) -> anyhow::Result<Self> {
        Self::load_checked(host, dir, Some(PYTHON_WASM_SHA256)).await
    }

    /// Load whatever `python.wasm` is there: for the approval test only.
    #[cfg(feature = "testing")]
    pub async fn load_unapproved(host: &ComponentHost, dir: &Path) -> anyhow::Result<Self> {
        Self::load_checked(host, dir, None).await
    }

    /// The same runtime with other limits: for tests that need a small budget.
    #[cfg(feature = "testing")]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    async fn load_checked(
        host: &ComponentHost,
        dir: &Path,
        approved: Option<&'static str>,
    ) -> anyhow::Result<Self> {
        let lib = dir.join("lib");
        ensure!(
            lib.join("python3.15").is_dir(),
            "{} has no standard library ({STDLIB}): run `mise run python`, or point [python] dir \
             at the runtime's directory",
            dir.display()
        );
        let path = dir.join("python.wasm");
        let engine = host.engine().clone();
        // Hashing and compiling take CPU for a while: keep them off the runtime's worker threads.
        let module = tokio::task::spawn_blocking(move || {
            let bytes =
                std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
            // The bytes compiled are the bytes checked, whatever happens to the file meanwhile.
            if let Some(approved) = approved {
                let sha256 = crate::source::sha256(&bytes);
                if sha256 != approved {
                    bail!(
                        "{} is not the approved CPython {PYTHON_VERSION} build (its sha256 is \
                         {sha256}, expected {approved}): run `mise run python` to fetch it",
                        path.display()
                    );
                }
            }
            Module::new(&engine, &bytes)
                .map_err(|e| anyhow::anyhow!("cannot compile {}: {e:#}", path.display()))
        })
        .await
        .context("compiling Python failed")??;
        let pre = link(host, &module)?;
        Ok(Self {
            inner: Arc::new(Inner {
                host: host.clone(),
                pre,
                lib,
            }),
            limits: script_limits(),
        })
    }

    /// Run `code` with `data` as its global `data` and `timezone` as its `TZ`.
    pub async fn run(&self, code: &str, data: Option<&Value>, timezone: &str) -> ToolOutput {
        let envelope = json!({ "code": code, "data": data, "timezone": timezone });
        let stdout = MemoryOutputPipe::new(OUTPUT_BYTES);
        let mut wasi = WasiCtxBuilder::new();
        wasi.args(&["python", "-I", "-B", "-c", RUNNER])
            .stdin(MemoryInputPipe::new(envelope.to_string()))
            .stdout(stdout.clone())
            .stderr(GuestLog::new("python"))
            .allow_tcp(false)
            .allow_udp(false)
            .allow_ip_name_lookup(false)
            // Imports read hundreds of small files from the standard library, which sits in the
            // page cache: sending each read to the blocking pool would triple a run's time.
            // Must precede the preopen, which takes it.
            .allow_blocking_current_thread(true);
        if let Err(e) = wasi.preopened_dir(&self.inner.lib, "/lib", FsPerms::ReadOnly) {
            return ToolOutput::error(format!(
                "Python's standard library cannot be opened ({e:#}); the hub's [python] dir needs \
                 fixing, try again later"
            ));
        }
        let mut store = Store::new(
            self.inner.host.engine(),
            ScriptState {
                wasi: wasi.build_p1(),
                limits: StoreLimitsBuilder::new()
                    .memory_size(self.limits.memory_bytes)
                    .memories(1)
                    .tables(MAX_TABLES)
                    .table_elements(MAX_TABLE_ELEMENTS)
                    .instances(1)
                    .build(),
            },
        );
        store.limiter(|s| &mut s.limits);
        let out_of_cpu = meter(&mut store, &self.limits);
        let pre = &self.inner.pre;
        let call = async {
            let instance = pre.instantiate_async(&mut store).await?;
            let start = instance.get_typed_func::<(), ()>(&mut store, "_start")?;
            match start.call_async(&mut store, ()).await {
                Ok(()) => Ok(0),
                Err(e) => match e.downcast_ref::<I32Exit>() {
                    Some(I32Exit(code)) => Ok(*code),
                    None => Err(e),
                },
            }
        };
        let exit = match bounded("Python", &self.limits, &out_of_cpu, call).await {
            Ok(code) => code,
            Err(message) => return ToolOutput::error(message),
        };
        drop(store);
        reply(&stdout.contents(), exit, &self.limits)
    }
}

/// The tool's name; `code_` is reserved for it.
pub const TOOL: &str = "code_run_python";

const DESCRIPTION: &str = "Run short Python 3.15 to calculate or aggregate; not for writing programs. \
`data` (any JSON) is the global `data`; `TZ` is the user's time zone. The last expression is the \
result (else `result`); prints are returned too. Standard library only: json, math, statistics, \
decimal, fractions, datetime, zoneinfo, collections, itertools, re, csv. No numpy/pandas, files, \
network or processes. Limits: 10 s CPU, 256 MiB, 64 KiB output: return aggregates, not rows.";

impl Python {
    /// `code_run_python`, its code run with `timezone` as `TZ`.
    pub fn tool(&self, timezone: &str) -> Tool {
        let def = ToolDef {
            name: TOOL.into(),
            description: DESCRIPTION.into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "Python source; its last expression is the result"
                    },
                    "data": {
                        "description": "Any JSON, read by the code as the global `data`"
                    }
                },
                "required": ["code"],
                "additionalProperties": false
            }),
            deferred: true,
            source: PYTHON_SOURCE.into(),
        };
        let handler = Arc::new(RunPython {
            python: self.clone(),
            timezone: timezone.into(),
        });
        (def, handler)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunArgs {
    code: String,
    #[serde(default)]
    data: Option<Value>,
}

struct RunPython {
    python: Python,
    timezone: String,
}

#[async_trait]
impl ToolHandler for RunPython {
    async fn call(&self, args: Value) -> ToolOutput {
        match serde_json::from_value::<RunArgs>(args) {
            Ok(args) => {
                self.python
                    .run(&args.code, args.data.as_ref(), &self.timezone)
                    .await
            }
            Err(e) => ToolOutput::error(format!(
                "invalid arguments: {e}; pass {{\"code\": \"…\"}} and optionally \"data\""
            )),
        }
    }
}

/// `[python]` as a source of tools: the runtime is checked and compiled when the hub starts.
pub struct PythonSource {
    dir: PathBuf,
    timezone: String,
    host: ComponentHost,
}

impl PythonSource {
    /// The runtime in `dir`, run with the user's `timezone` (an IANA name).
    pub fn new(dir: PathBuf, timezone: impl Into<String>, host: ComponentHost) -> Self {
        Self {
            dir,
            timezone: timezone.into(),
            host,
        }
    }
}

#[async_trait]
impl ToolSource for PythonSource {
    fn name(&self) -> &str {
        PYTHON_SOURCE
    }

    async fn start(&self) -> anyhow::Result<Session> {
        let started = Instant::now();
        let python = Python::load(&self.host, &self.dir).await?;
        tracing::info!(
            "python: approved (CPython {PYTHON_VERSION}), compiled in {:.1} s",
            started.elapsed().as_secs_f32()
        );
        Ok(Session::lasting(vec![python.tool(&self.timezone)]))
    }
}

/// Link a module against WASI preview 1 alone: the script world's linker holds nothing else.
fn link(host: &ComponentHost, module: &Module) -> anyhow::Result<InstancePre<ScriptState>> {
    let mut linker = Linker::<ScriptState>::new(host.engine());
    wasmtime_wasi::p1::add_to_linker_async(&mut linker, |s| &mut s.wasi)?;
    linker
        .instantiate_pre(module)
        .map_err(|e| anyhow::anyhow!("python.wasm needs more than WASI preview 1: {e:#}"))
}

/// The tool's output from what the guest wrote: the runner's reply is its last line.
fn reply(stdout: &[u8], exit: i32, limits: &Limits) -> ToolOutput {
    let last = stdout
        .split(|&b| b == b'\n')
        .rfind(|line| !line.is_empty())
        .unwrap_or_default();
    let Ok(reply) = serde_json::from_slice::<Reply>(last) else {
        if stdout.len() >= OUTPUT_BYTES {
            return ToolOutput::error(format!(
                "output over {} KiB: return an aggregate, not the rows",
                OUTPUT_BYTES >> 10
            ));
        }
        return ToolOutput::error(format!(
            "Python stopped without a result (exit code {exit}); the memory limit is {} MiB and \
             the CPU budget about {} s: make the computation smaller",
            limits.memory_bytes >> 20,
            limits.cpu_ticks as f32 * TICK.as_secs_f32()
        ));
    };
    if let Some(error) = reply.error {
        let next = reply
            .next
            .unwrap_or_else(|| "fix the code and run again".into());
        let mut message = format!("{error}; {next}");
        if !reply.stdout.is_empty() {
            let skip = reply.stdout.chars().count().saturating_sub(ERROR_TAIL);
            let tail: String = reply.stdout.chars().skip(skip).collect();
            message.push_str("\nprinted before the error:\n");
            message.push_str(&tail);
        }
        return ToolOutput::error(message).with_next(next);
    }
    let mut out = Map::new();
    if !reply.value.is_null() {
        out.insert("value".into(), reply.value);
    }
    if !reply.stdout.is_empty() {
        out.insert("stdout".into(), Value::String(reply.stdout));
    }
    if reply.stdout_truncated {
        out.insert("stdout_truncated".into(), Value::Bool(true));
    }
    ToolOutput::ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn importing(module: &str, name: &str) -> String {
        format!(
            r#"(module (import "{module}" "{name}" (func (param i32 i32 i32 i32) (result i32))))"#
        )
    }

    #[test]
    fn the_linker_holds_only_wasi_preview_1() {
        let host = ComponentHost::new().unwrap();
        let module = |wat: &str| Module::new(host.engine(), wat).unwrap();
        assert!(
            link(
                &host,
                &module(&importing("wasi_snapshot_preview1", "fd_write"))
            )
            .is_ok()
        );
        for (m, name) in [
            ("wasi:sockets/tcp@0.2.12", "start-connect"),
            ("wasi:http/outgoing-handler@0.2.12", "handle"),
            ("wasi:filesystem/preopens@0.2.12", "get-directories"),
            ("wasi", "sock_open"),
            ("env", "system"),
        ] {
            let e = link(&host, &module(&importing(m, name))).err().unwrap();
            assert!(
                e.to_string().contains("WASI preview 1"),
                "{m} {name}: {e:#}"
            );
        }
    }
}
