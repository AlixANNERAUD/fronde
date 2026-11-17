//! The systemd scope Fronde starts its agent in when a systemd user manager answers: a cgroup
//! under `fronde-agents.slice` that caps the memory, the CPU time and the tasks of the agent and
//! of everything it starts. `systemd-run --scope` moves itself into the scope and then `exec`s the
//! helper, so the helper keeps the PID, the process group and the pipes Fronde gave it.
//!
//! A scope holds a process systemd did not start, so none of systemd's sandboxing settings apply
//! to it: it only caps resources. The helper's Landlock and seccomp filter confine the agent.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// The slice every agent's scope goes in, itself under `fronde.slice` (a dash nests slices).
pub const SLICE: &str = "fronde-agents.slice";

/// The variables `systemd-run` needs to reach the user manager. The helper drops them, so the
/// agent never gets them; `systemd-run`, which runs before it, keeps them.
pub const MANAGER_ENV: [&str; 2] = ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"];

/// How long the user manager has to answer before Fronde starts the agent without a scope.
const TIMEOUT: Duration = Duration::from_secs(5);

/// What the scope caps; `None` for no cap from Fronde (systemd's own defaults apply).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    /// Bytes: `MemoryMax`, with `MemoryHigh` at three quarters, where the kernel starts
    /// reclaiming the agent's memory before it reaches the hard cap.
    pub memory: Option<u64>,
    /// CPUs' worth of time: `CPUQuota`, 100 % per CPU.
    pub cpus: Option<f64>,
    /// Processes and threads: `TasksMax`.
    pub tasks: Option<u64>,
}

impl Limits {
    /// Whether no limit is set, so that a scope would cap nothing.
    pub fn is_none(&self) -> bool {
        self.memory.is_none() && self.cpus.is_none() && self.tasks.is_none()
    }

    /// The scope's properties, as `systemd-run --property` takes them.
    pub fn properties(&self) -> Vec<String> {
        let mut properties = Vec::new();
        if let Some(bytes) = self.memory {
            properties.push(format!("MemoryMax={bytes}"));
            properties.push(format!("MemoryHigh={}", bytes / 4 * 3));
        }
        if let Some(cpus) = self.cpus {
            // A whole percent, at least 1 %: what systemd takes.
            let percent = (cpus * 100.0).round().max(1.0);
            properties.push(format!("CPUQuota={percent}%"));
        }
        if let Some(tasks) = self.tasks {
            properties.push(format!("TasksMax={tasks}"));
        }
        properties
    }

    /// For the log and `fronde check`: `2 GiB memory, 2 CPUs, 512 tasks`.
    pub fn describe(&self) -> String {
        let memory = match self.memory {
            Some(bytes) if bytes % (1 << 20) == 0 => format!("{} memory", bytes_text(bytes)),
            Some(bytes) => format!("{bytes} bytes of memory"),
            None => "memory uncapped".to_owned(),
        };
        let cpus = match self.cpus {
            Some(1.0) => "1 CPU".to_owned(),
            Some(cpus) => format!("{cpus} CPUs"),
            None => "CPU uncapped".to_owned(),
        };
        let tasks = match self.tasks {
            Some(tasks) => format!("{tasks} tasks"),
            None => "tasks uncapped".to_owned(),
        };
        format!("{memory}, {cpus}, {tasks}")
    }
}

/// How the agent's scope is made: `systemd-run`, with `systemctl` beside it.
#[derive(Clone, Debug)]
pub struct Scope {
    /// The `systemd-run` program, found on Fronde's `PATH`.
    pub run: PathBuf,
    /// Every scope of this Fronde is named `<prefix>-<random>.scope`; the prefix names the data
    /// directory, so that scopes another Fronde started are never touched.
    pub prefix: String,
    pub limits: Limits,
}

/// Why the user manager made no scope.
#[derive(Debug)]
pub enum Unavailable {
    /// `systemd-run` is not there.
    Missing,
    /// It is, but it failed: no user manager to talk to, or it refused.
    Failed(String),
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("no systemd-run"),
            Self::Failed(why) => f.write_str(why),
        }
    }
}

impl Scope {
    /// The scope made with the `systemd-run` found on `path`; `None` when there is none, as in
    /// a container.
    pub fn find(path: Option<&OsStr>, prefix: String, limits: Limits) -> Option<Self> {
        let run = std::env::split_paths(path?)
            .map(|dir| dir.join("systemd-run"))
            .find(|run| crate::acp::executable(run))?;
        Some(Self {
            run,
            prefix,
            limits,
        })
    }

    /// `systemctl`, from the same systemd as `systemd-run`.
    pub fn ctl(&self) -> PathBuf {
        self.run.with_file_name("systemctl")
    }

    /// A new unit name for one start of the agent of thread `thread`:
    /// `<prefix>-t<thread>-<8 hex>`.
    pub fn new_unit(&self, thread: i64) -> String {
        let random = uuid::Uuid::new_v4().simple().to_string();
        format!("{}-t{thread}-{}", self.prefix, &random[..8])
    }

    /// `systemd-run`'s arguments up to `--`: the command follows. `unit` names the scope;
    /// without it, systemd picks a name.
    pub fn args(&self, unit: Option<&str>) -> Vec<String> {
        let mut args: Vec<String> = ["--user", "--scope", "--quiet", "--collect"]
            .map(str::to_owned)
            .to_vec();
        args.push(format!("--slice={SLICE}"));
        if let Some(unit) = unit {
            args.push(format!("--unit={unit}"));
        }
        for property in self.limits.properties() {
            args.push(format!("--property={property}"));
        }
        // A scope whose main process was OOM-killed goes as a whole.
        args.push("--property=OOMPolicy=stop".to_owned());
        args.push("--".to_owned());
        args
    }

    /// Whether the user manager makes a scope with these limits: one that runs `true` and goes.
    /// This is the agent's own path, so it fails where starting the agent in a scope would.
    pub async fn probe(&self) -> Result<(), Unavailable> {
        let mut args = self.args(None);
        args.push("true".to_owned());
        match manager_command(&self.run, &args).await {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(Unavailable::Missing),
            Err(error) => Err(Unavailable::Failed(error.to_string())),
            Ok(output) if output.status.success() => Ok(()),
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let why = stderr.trim();
                Err(Unavailable::Failed(if why.is_empty() {
                    format!("systemd-run {}", output.status)
                } else {
                    why.to_owned()
                }))
            }
        }
    }

    /// Stops the scopes an earlier agent of this Fronde left: one whose process outlived a
    /// Fronde killed with SIGKILL, say. Only this data directory's, and only once, when the server starts: later
    /// starts of other agents must keep the running ones.
    pub async fn stop_leftovers(&self) {
        let pattern = format!("{}-*.scope", self.prefix);
        let args = ["--user", "stop", &pattern].map(str::to_owned);
        match manager_command(&self.ctl(), &args).await {
            Ok(output) if output.status.success() => {}
            Ok(output) => tracing::debug!(
                "systemctl stop {pattern}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Err(error) => tracing::debug!("systemctl stop {pattern}: {error}"),
        }
    }
}

/// The memory of every agent together, as `fronde-agents.slice` caps it: `MemoryMax`, with
/// `MemoryHigh` at three quarters, as a scope's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slice {
    /// Bytes.
    pub memory: u64,
}

impl Slice {
    /// `systemctl`'s arguments that set the cap until the user manager stops (`--runtime`: a
    /// drop-in under `/run`, gone at the next boot, written again at each start of Fronde).
    pub fn args(&self) -> Vec<String> {
        vec![
            "--user".to_owned(),
            "set-property".to_owned(),
            "--runtime".to_owned(),
            SLICE.to_owned(),
            format!("MemoryMax={}", self.memory),
            format!("MemoryHigh={}", self.memory / 4 * 3),
        ]
    }

    /// Caps the slice through the user manager `scope` reaches. A failure is a warning: each
    /// agent's own scope still caps it.
    pub async fn cap(&self, scope: &Scope) {
        let args = self.args();
        let failed = match manager_command(&scope.ctl(), &args).await {
            Ok(output) if output.status.success() => return,
            Ok(output) => String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            Err(error) => error.to_string(),
        };
        tracing::warn!(
            "agent limits: cannot cap {SLICE} at {} for all agents ({failed}); each agent's \
             scope still caps it",
            bytes_text(self.memory)
        );
    }
}

/// `4 GiB`, `512 MiB` or a number of bytes.
pub fn bytes_text(bytes: u64) -> String {
    if bytes.is_multiple_of(1 << 30) {
        format!("{} GiB", bytes >> 30)
    } else if bytes.is_multiple_of(1 << 20) {
        format!("{} MiB", bytes >> 20)
    } else {
        format!("{bytes} bytes")
    }
}

/// Runs `program` with only what reaching the user manager needs from Fronde's environment, and
/// gives up after [`TIMEOUT`].
async fn manager_command(program: &Path, args: &[String]) -> std::io::Result<std::process::Output> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for name in MANAGER_ENV.iter().chain(&["PATH", "LANG"]) {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    tokio::time::timeout(TIMEOUT, command.output())
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("{} gave no answer within {TIMEOUT:?}", program.display()),
            )
        })?
}

/// Stops an agent's scope when its client goes: the ACP crate kills the agent's process group,
/// and this also stops any process that left the group.
pub(crate) struct ScopeStop {
    pub(crate) ctl: PathBuf,
    pub(crate) unit: String,
}

impl Drop for ScopeStop {
    fn drop(&mut self) {
        // Not awaited: `Drop` cannot. Tokio reaps the child once it exits.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let _entered = runtime.enter();
        let unit = format!("{}.scope", self.unit);
        let mut command = tokio::process::Command::new(&self.ctl);
        command
            .args(["--user", "stop", "--no-block", &unit])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for name in MANAGER_ENV.iter().chain(&["PATH"]) {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        if let Err(error) = command.spawn() {
            tracing::debug!("systemctl stop {unit}: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULTS: Limits = Limits {
        memory: Some(2 << 30),
        cpus: Some(2.0),
        tasks: Some(512),
    };

    fn scope(limits: Limits) -> Scope {
        Scope {
            run: PathBuf::from("/usr/bin/systemd-run"),
            prefix: "fronde-agent-0123abcd".to_owned(),
            limits,
        }
    }

    #[test]
    fn limits_render_as_scope_properties() {
        assert_eq!(
            DEFAULTS.properties(),
            [
                "MemoryMax=2147483648",
                "MemoryHigh=1610612736",
                "CPUQuota=200%",
                "TasksMax=512"
            ]
        );
        assert_eq!(DEFAULTS.describe(), "2 GiB memory, 2 CPUs, 512 tasks");
        let half = Limits {
            memory: Some(512 << 20),
            cpus: Some(0.5),
            tasks: None,
        };
        assert_eq!(
            half.properties(),
            [
                "MemoryMax=536870912",
                "MemoryHigh=402653184",
                "CPUQuota=50%"
            ]
        );
        assert_eq!(half.describe(), "512 MiB memory, 0.5 CPUs, tasks uncapped");
        let tiny = Limits {
            memory: None,
            cpus: Some(0.001),
            tasks: None,
        };
        assert_eq!(tiny.properties(), ["CPUQuota=1%"]);
        assert!(!tiny.is_none());
        let none = Limits {
            memory: None,
            cpus: None,
            tasks: None,
        };
        assert!(none.is_none() && none.properties().is_empty());
    }

    #[test]
    fn scope_arguments() {
        let scope = scope(DEFAULTS);
        let unit = scope.new_unit(7);
        assert!(unit.starts_with("fronde-agent-0123abcd-t7-"), "{unit}");
        assert_eq!(unit.len(), "fronde-agent-0123abcd-t7-".len() + 8);
        assert_ne!(unit, scope.new_unit(7));
        assert_eq!(
            scope.args(Some(&unit)),
            [
                "--user".to_owned(),
                "--scope".to_owned(),
                "--quiet".to_owned(),
                "--collect".to_owned(),
                "--slice=fronde-agents.slice".to_owned(),
                format!("--unit={unit}"),
                "--property=MemoryMax=2147483648".to_owned(),
                "--property=MemoryHigh=1610612736".to_owned(),
                "--property=CPUQuota=200%".to_owned(),
                "--property=TasksMax=512".to_owned(),
                "--property=OOMPolicy=stop".to_owned(),
                "--".to_owned(),
            ]
        );
        assert_eq!(scope.ctl(), PathBuf::from("/usr/bin/systemctl"));
    }

    #[test]
    fn slice_cap_arguments() {
        let slice = Slice { memory: 4 << 30 };
        assert_eq!(
            slice.args(),
            [
                "--user",
                "set-property",
                "--runtime",
                "fronde-agents.slice",
                "MemoryMax=4294967296",
                "MemoryHigh=3221225472",
            ]
        );
        assert_eq!(bytes_text(4 << 30), "4 GiB");
        assert_eq!(bytes_text(1536 << 20), "1536 MiB");
        assert_eq!(bytes_text(1000), "1000 bytes");
    }

    #[test]
    fn scope_needs_systemd_run_on_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert!(Scope::find(Some(&path), "p".into(), DEFAULTS).is_none());
        assert!(Scope::find(None, "p".into(), DEFAULTS).is_none());
        let run = dir.path().join("systemd-run");
        std::fs::write(&run, "#!/bin/sh\n").unwrap();
        // Not executable: not there.
        assert!(Scope::find(Some(&path), "p".into(), DEFAULTS).is_none());
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
        let found = Scope::find(Some(&path), "p".into(), DEFAULTS).unwrap();
        assert_eq!(found.run, run);
    }
}
