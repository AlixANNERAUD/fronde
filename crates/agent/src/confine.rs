//! `fronde __confine`: the helper Fronde starts instead of its agent. It keeps only the
//! environment variables its policy names, lowers its own priority, raises its OOM score, confines
//! itself with Landlock and a seccomp filter, and then `exec`s the agent, which inherits all of it
//! and cannot undo any.
//! It runs in a fresh, single-threaded process, so nothing runs between `fork` and `exec` in
//! Fronde's own.

use crate::acp::AgentSpec;
use crate::provider::Install;
use crate::scope::{MANAGER_ENV, Scope, Unavailable};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// The subcommand of the `fronde` binary that runs [`run_helper`].
pub const SUBCOMMAND: &str = "__confine";

/// The variables the agent keeps from Fronde's environment, by name or by prefix (ending in `*`):
/// the `PATH`, the locale and time zone, Claude's own settings and token, proxies and CA
/// certificates. Fronde sets `HOME` and `TMPDIR`; `[agent] env` adds its own names.
pub const KEEP_ENV: [&str; 16] = [
    "PATH",
    "LANG",
    "LANGUAGE",
    "LC_*",
    "TZ",
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "HOME",
    "TMPDIR",
];

/// What the system lets every confined agent read and run. Of `/dev`, only the devices every
/// program may use: no terminal, no shared memory, no other device.
pub const SYSTEM_READ: [&str; 12] = [
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/etc",
    "/proc",
    "/sys",
    "/dev/zero",
    "/dev/random",
    "/dev/urandom",
];

/// The devices the confined agent may write to, besides reading them.
pub const DEVICES_WRITE: [&str; 2] = ["/dev/null", "/dev/full"];

/// TCP ports the agent may connect to besides the hub's and a proxy's: https and DNS over TCP.
pub const CONNECT_PORTS: [u16; 2] = [443, 53];

/// What the helper does before it starts the agent. It holds names and paths only, never a value:
/// it travels on the command line, which every user can read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// The agent's working directory.
    pub cwd: PathBuf,
    /// Variables kept, by exact name or by a prefix ending in `*`; every other one is dropped.
    pub keep_env: Vec<String>,
    /// Names dropped even when `keep_env` matches them, exact or a prefix ending in `*`:
    /// Fronde's `{ env }` secrets and the providers' sign-in variables.
    pub drop_env: Vec<String>,
    /// The Landlock sandbox; `None` (`[agent] sandbox = false`) runs the agent unconfined.
    pub sandbox: Option<Sandbox>,
}

/// What the confined agent may reach. Everything else on the filesystem is out of its reach, and
/// so are other TCP ports, signals to processes outside it and every Unix socket but a connected
/// pair it makes itself (see [`SOCKETS`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sandbox {
    /// Read and run, canonical paths.
    pub read: Vec<PathBuf>,
    /// Read, run, write, create and remove, canonical paths.
    pub write: Vec<PathBuf>,
    /// TCP ports it may connect to; it may bind none.
    pub connect_ports: Vec<u16>,
}

impl Policy {
    /// The policy as one compact command-line argument.
    pub fn to_arg(&self) -> String {
        serde_json::to_string(self).expect("a policy always serialises")
    }

    pub fn from_arg(arg: &str) -> anyhow::Result<Self> {
        serde_json::from_str(arg).context("invalid --policy")
    }

    /// Whether the agent gets the variable `name`: never one `drop_env` names exactly; one
    /// `keep_env` names exactly (a variable Fronde sets for it) even when a dropped prefix
    /// matches it; else one a kept prefix matches and no dropped prefix does.
    pub fn keeps(&self, name: &str) -> bool {
        if self.drop_env.iter().any(|dropped| dropped == name) {
            return false;
        }
        if self.keep_env.iter().any(|kept| kept == name) {
            return true;
        }
        !self.drop_env.iter().any(|dropped| matches(dropped, name))
            && self.keep_env.iter().any(|pattern| matches(pattern, name))
    }
}

/// Whether `name` is `pattern`, or starts with it when it ends in `*` (a lone `*` matches
/// nothing).
fn matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => !prefix.is_empty() && name.starts_with(prefix),
        None => pattern == name,
    }
}

/// How Fronde starts its agents through the helper, whichever thread each works for.
#[derive(Clone, Debug)]
pub struct Confine {
    /// The `fronde` binary.
    pub helper: PathBuf,
    /// Everything but what the spec gives: its directories, its variables and the hub's port.
    /// The sandbox's write grants hold no agent's directory: each agent gets its own two, its
    /// thread's, from its spec, so that it reaches no other thread's.
    pub policy: Policy,
    /// What no grant may equal or contain, as [`GrantRequest::protected`]: the spec's
    /// directories are checked against it as every other grant is.
    pub protected: Vec<PathBuf>,
    /// The systemd scope that caps the agent's resources, when `systemd-run` is there; whether
    /// the user manager answers is checked at each start ([`Confine::ready`]).
    pub scope: Option<Scope>,
    /// Read grants for the owner's agents only (`[agent] read`), canonical: an agent whose spec
    /// has `owners_grants` gets them besides the policy's, any other none of them.
    pub owner_read: Vec<PathBuf>,
}

/// The command that starts an agent through the helper.
#[derive(Clone, Debug)]
pub struct HelperCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Set in the helper's environment, which also has Fronde's; the helper keeps only those the
    /// policy names.
    pub env: Vec<(String, String)>,
    pub policy: Policy,
    /// The agent's scope unit, without `.scope`, when `systemd-run` starts the helper.
    pub unit: Option<String>,
}

impl Confine {
    /// This confinement as the agent starts now. With a scope, it checks that the user manager makes a scope; when it does not, the agent
    /// starts without one, with a warning: the limits are a safeguard for the machine, not
    /// part of the agent's confinement, which Landlock and seccomp make.
    pub async fn ready(&self) -> Self {
        let mut confine = self.clone();
        if let Some(scope) = &self.scope {
            match scope.probe().await {
                Ok(()) => {}
                Err(Unavailable::Missing) => {
                    tracing::info!(
                        "agent limits: none, {} is gone; the agent is OOM-killed first",
                        scope.run.display()
                    );
                    confine.scope = None;
                }
                Err(Unavailable::Failed(why)) => {
                    tracing::warn!(
                        "agent limits: the systemd user manager made no scope ({why}); the \
                         agent runs without memory, CPU or task caps, and is OOM-killed first"
                    );
                    confine.scope = None;
                }
            }
        }
        confine
    }

    /// The helper's command line and environment for `spec`.
    pub fn command(&self, spec: &AgentSpec) -> anyhow::Result<HelperCommand> {
        self.command_with(spec, Vec::new())
    }

    /// [`Confine::command`], with `secrets` added to the agent's environment: the hub's token
    /// and Claude's, which win over Fronde's own variables of the same names.
    pub fn command_with(
        &self,
        spec: &AgentSpec,
        secrets: Vec<(String, String)>,
    ) -> anyhow::Result<HelperCommand> {
        let (program, args) = spec
            .command
            .split_first()
            .context("the agent command is empty; set it to claude-agent-acp")?;
        let env: Vec<(String, String)> = spec
            .env
            .iter()
            .cloned()
            .chain([
                ("HOME".to_owned(), path_string(&spec.home)?),
                ("TMPDIR".to_owned(), path_string(&spec.home.join("tmp"))?),
            ])
            .chain(secrets)
            .collect();
        let mut policy = self.policy.clone();
        policy.cwd = spec.cwd.clone();
        // The agent writes its thread's directories, and no other thread's; it reads what only
        // the owner granted when its thread is the owner's.
        if let Some(sandbox) = &mut policy.sandbox {
            for dir in [&spec.cwd, &spec.home] {
                push(&mut sandbox.write, write_grant(dir, &self.protected)?);
            }
            if spec.owners_grants {
                for dir in &self.owner_read {
                    push(&mut sandbox.read, dir.clone());
                }
            }
        }
        for (name, _) in &env {
            if !policy.keep_env.contains(name) {
                policy.keep_env.push(name.clone());
            }
        }
        // A secret's variable that Fronde sets holds the value Fronde gives instead.
        policy
            .drop_env
            .retain(|name| !env.iter().any(|(set, _)| set == name));
        // The helper is started with Fronde's environment, which the ACP crate cannot clear:
        // every variable the agent does not get is set empty, so that its value, a `{ env }`
        // secret's among them, never reaches the helper.
        let mut env = env;
        // `systemd-run` needs the user manager's variables; the helper drops them.
        let manager = |name: &str| {
            self.scope.is_some()
                && MANAGER_ENV.contains(&name)
                && !policy.drop_env.iter().any(|dropped| matches(dropped, name))
        };
        for (name, _) in std::env::vars_os() {
            if let Some(name) = name.to_str()
                && !policy.keeps(name)
                && !manager(name)
                && !env.iter().any(|(set, _)| set == name)
            {
                env.push((name.to_owned(), String::new()));
            }
        }
        if let Some(sandbox) = &mut policy.sandbox {
            let port = url_port(&spec.hub_url)
                .with_context(|| format!("no port in the hub's URL {}", spec.hub_url))?;
            if !sandbox.connect_ports.contains(&port) {
                sandbox.connect_ports.push(port);
            }
        }
        let mut helper_args = vec![
            SUBCOMMAND.to_owned(),
            "--policy".to_owned(),
            policy.to_arg(),
            "--".to_owned(),
            program.clone(),
        ];
        helper_args.extend(args.iter().cloned());
        let Some(scope) = &self.scope else {
            return Ok(HelperCommand {
                program: self.helper.clone(),
                args: helper_args,
                env,
                policy,
                unit: None,
            });
        };
        // `systemd-run --scope` puts itself in the scope and `exec`s the helper.
        let unit = scope.new_unit(spec.thread.0);
        let mut run_args = scope.args(Some(&unit));
        run_args.push(path_string(&self.helper)?);
        run_args.extend(helper_args);
        Ok(HelperCommand {
            program: scope.run.clone(),
            args: run_args,
            env,
            policy,
            unit: Some(unit),
        })
    }
}

fn path_string(path: &Path) -> anyhow::Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("{} is not valid UTF-8", path.display()))
}

/// The port of an `http`, `https` or `socks` URL, given or implied by its scheme.
pub fn url_port(url: &str) -> Option<u16> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    match host.rsplit_once(':') {
        Some((before, port)) if !before.ends_with(':') && !port.ends_with(']') => port.parse().ok(),
        _ => match scheme.to_ascii_lowercase().as_str() {
            "http" => Some(80),
            "https" => Some(443),
            "socks" | "socks5" | "socks5h" => Some(1080),
            _ => None,
        },
    }
}

/// What the kernel can do for the agent's sandbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Support {
    /// The kernel's Landlock ABI, as far as Fronde tells them apart (7 and 8 read as 6): 0 when
    /// Landlock is missing or turned off.
    pub abi: u8,
    /// Whether the helper can install its seccomp filter: the kernel has seccomp filters that
    /// return an error, on an architecture the filter is written for (x86_64 or aarch64).
    pub seccomp: bool,
}

/// What the seccomp filter leaves the agent, for `fronde check` and the log.
pub const SOCKETS: &str = "seccomp: no Unix sockets but connected pairs, only IPv4, IPv6 and \
                           netlink route sockets, no io_uring";

/// The lowest Landlock ABI the agent runs under: ABI 1 forbids every rename across directories,
/// which Claude Code's atomic writes need, and ABI 2 cannot stop `truncate`, which would empty
/// `fronde.db` or the configuration.
pub const MIN_ABI: u8 = 3;

impl Support {
    /// Asks the running kernel, without restricting anything.
    pub fn probe() -> Self {
        Self {
            abi: probe_abi(),
            seccomp: seccomp_rules::available(),
        }
    }

    /// Whether the agent can start confined.
    pub fn ready(self) -> bool {
        self.files() && self.seccomp
    }

    pub fn files(self) -> bool {
        self.abi >= MIN_ABI
    }

    pub fn tcp_ports(self) -> bool {
        self.abi >= 4
    }

    /// `ioctl` on the devices the agent may open.
    pub fn device_ioctls(self) -> bool {
        self.abi >= 5
    }

    /// Signals to processes outside the sandbox and abstract Unix sockets.
    pub fn scopes(self) -> bool {
        self.abi >= 6
    }

    /// Unix sockets reached by path.
    pub fn unix_paths(self) -> bool {
        self.abi >= 9
    }

    /// An error saying why the agent cannot be confined here, if it cannot.
    pub fn require(self) -> anyhow::Result<()> {
        if self.files() && !self.seccomp {
            bail!(
                "the agent's sandbox needs seccomp filters (Linux 3.17) on x86_64 or aarch64, and \
                 this kernel or machine ({}) has none; set [agent] sandbox = false to run the \
                 agent unconfined, with access to everything your user can read and write",
                std::env::consts::ARCH
            );
        }
        if self.files() {
            return Ok(());
        }
        let found = match self.abi {
            0 => {
                "no Landlock (check that `cat /sys/kernel/security/lsm` lists landlock)".to_owned()
            }
            abi => format!("Landlock ABI {abi}"),
        };
        bail!(
            "the agent's sandbox needs Landlock ABI {MIN_ABI} (Linux 6.2) or later, and this \
             kernel has {found}; upgrade the kernel, or set [agent] sandbox = false to run the \
             agent unconfined, with access to everything your user can read and write"
        )
    }

    /// What the sandbox covers on this kernel, and what it misses, for `fronde check` and the log.
    pub fn describe(self, ports: &[u16]) -> String {
        if !self.ready() {
            return format!(
                "unavailable ({})",
                self.require()
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_default()
            );
        }
        let ports: Vec<String> = ports.iter().map(u16::to_string).collect();
        let mut covered = vec!["files".to_owned()];
        let mut missing = Vec::new();
        if self.tcp_ports() {
            covered.push(format!("TCP ports {}", ports.join(" ")));
        } else {
            missing.push("TCP ports (Linux 6.7)");
        }
        if self.device_ioctls() {
            covered.push("device ioctls".to_owned());
        } else {
            missing.push("device ioctls (Linux 6.10)");
        }
        if self.scopes() {
            covered.push("signals and abstract sockets".to_owned());
        } else {
            missing.push("signals to Fronde and abstract sockets (Linux 6.12)");
        }
        // Below ABI 9, the seccomp filter alone keeps the agent from Unix sockets.
        if self.unix_paths() {
            covered.push("Unix socket paths".to_owned());
        }
        let mut text = format!(
            "Landlock ABI {} ({}); {SOCKETS}",
            self.abi,
            covered.join(", ")
        );
        if !missing.is_empty() {
            text.push_str(&format!("; not confined: {}", missing.join(", ")));
        }
        text
    }
}

/// What the confined agent may read and write, built from what it needs and checked against
/// what it must never reach.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Grants {
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
    /// `PATH` entries left out, each with why: to log as warnings.
    pub skipped: Vec<String>,
    /// Grants under the owner's home, which the agent can read: to log as warnings.
    pub under_home: Vec<PathBuf>,
}

/// What [`grants`] starts from.
#[derive(Clone, Debug)]
pub struct GrantRequest<'a> {
    /// [`SYSTEM_READ`], or another list in tests.
    pub system: &'a [PathBuf],
    /// The agent's `PATH`.
    pub path: Option<&'a OsStr>,
    /// The agent's program, as `[agent] command` names it.
    pub program: &'a str,
    /// How its provider's program is installed: what is granted for it to run.
    pub install: Install,
    /// `[agent] read`.
    pub extra: &'a [PathBuf],
    /// Directories the agent writes besides its thread's, which [`Confine::command_with`] adds
    /// from its spec: none in Fronde, which grants every agent only its own thread's.
    pub write: &'a [PathBuf],
    /// What no grant may equal or contain: the data directory, the configuration's directory,
    /// the master key, secret files, the memory.
    pub protected: &'a [PathBuf],
    /// The owner's home, which no `PATH` entry or install may equal or contain either.
    pub home: Option<&'a Path>,
}

/// The read and write grants of the agent's sandbox. Every path is canonical, so a symbolic link
/// cannot hide what it points to, and a missing one is left out. A grant that equals or contains
/// a protected path is an error, except for a `PATH` entry, which is skipped with a warning.
pub fn grants(request: &GrantRequest<'_>) -> anyhow::Result<Grants> {
    let protected: Vec<PathBuf> = request.protected.iter().map(|p| canonical(p)).collect();
    let covering = |grant: &Path| protected.iter().find(|p| p.starts_with(grant)).cloned();
    let home = request.home.map(canonical);
    // A `PATH` entry or an install must not hold the whole home either: `~/.ssh` is in it.
    let covering_home = |grant: &Path| {
        covering(grant).or_else(|| home.clone().filter(|home| home.starts_with(grant)))
    };
    let note_home = |grants: &mut Grants, grant: &Path| {
        if home.as_ref().is_some_and(|home| grant.starts_with(home))
            && !grants.under_home.iter().any(|g| g == grant)
        {
            grants.under_home.push(grant.to_path_buf());
        }
    };
    let mut grants = Grants::default();

    for dir in request.system {
        let Ok(dir) = dir.canonicalize() else {
            continue;
        };
        if let Some(p) = covering(&dir) {
            bail!(
                "the agent's sandbox lets it read {}, which holds {}: keep Fronde's data and \
                 configuration out of the system directories",
                dir.display(),
                p.display()
            );
        }
        push(&mut grants.read, dir);
    }
    // A resolver configuration managed elsewhere (systemd-resolved) is a link out of /etc.
    if let Ok(resolv) = Path::new("/etc/resolv.conf").canonicalize()
        && covering(&resolv).is_none()
    {
        push(&mut grants.read, resolv);
    }
    if let Some(path) = request.path {
        for dir in std::env::split_paths(path) {
            if dir.is_relative() {
                grants.skipped.push(format!(
                    "{}: a relative PATH entry is not granted to the confined agent",
                    dir.display()
                ));
                continue;
            }
            let Ok(dir) = dir.canonicalize() else {
                continue;
            };
            match covering_home(&dir) {
                Some(p) => grants.skipped.push(format!(
                    "{}: this PATH entry holds {}, so the confined agent does not get it",
                    dir.display(),
                    p.display()
                )),
                None => {
                    note_home(&mut grants, &dir);
                    push(&mut grants.read, dir);
                }
            }
        }
    }
    if let Some(install) = install_grant(request.install, request.program, request.path) {
        if let Some(p) = covering_home(&install) {
            bail!(
                "the agent's program {} is installed in {}, which holds {}: install the adapter \
                 elsewhere, or move Fronde's data and configuration out of it",
                request.program,
                install.display(),
                p.display()
            );
        }
        note_home(&mut grants, &install);
        push(&mut grants.read, install);
    }
    for dir in request.extra {
        let Ok(dir) = dir.canonicalize() else {
            continue;
        };
        if let Some(p) = covering(&dir) {
            bail!(
                "[agent] read: {} holds {}, which the agent must not read; grant a directory \
                 inside it instead",
                dir.display(),
                p.display()
            );
        }
        note_home(&mut grants, &dir);
        push(&mut grants.read, dir);
    }
    for dir in request
        .write
        .iter()
        .map(PathBuf::as_path)
        .chain(DEVICES_WRITE.map(Path::new))
    {
        push(&mut grants.write, write_grant(dir, &protected)?);
    }
    Ok(grants)
}

/// `dir` as a write grant: canonical, so that a symbolic link cannot hide what it points to, and
/// holding none of the `protected` paths, which must be canonical.
pub fn write_grant(dir: &Path, protected: &[PathBuf]) -> anyhow::Result<PathBuf> {
    let canonical = dir
        .canonicalize()
        .with_context(|| format!("cannot find the agent's directory {}", dir.display()))?;
    if let Some(p) = protected.iter().find(|p| p.starts_with(&canonical)) {
        bail!(
            "the agent may write {}, which holds {}: keep Fronde's configuration and secrets \
             out of the data directory's agent/, home/ and threads/",
            canonical.display(),
            p.display()
        );
    }
    Ok(canonical)
}

/// `paths` resolved as [`grants`] resolves protected paths, for [`Confine::protected`].
pub fn canonical_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths.iter().map(|p| canonical(p)).collect()
}

fn push(list: &mut Vec<PathBuf>, path: PathBuf) {
    if !list.contains(&path) {
        list.push(path);
    }
}

/// `path` with its links resolved; for a path that does not exist yet, its nearest existing
/// ancestor's, followed by the rest as written.
fn canonical(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut current = path;
    loop {
        if let Ok(found) = current.canonicalize() {
            return rest.iter().rev().fold(found, |acc, part| acc.join(part));
        }
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                current = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// What the sandbox grants for the agent's program to run, as its provider installs it.
fn install_grant(install: Install, program: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    match install {
        Install::NodeModules => install_dir(program, path),
        Install::Directory => program_path(program, path)?.parent().map(Path::to_path_buf),
    }
}

/// The canonical path of the agent's program, looked up on `path` when it has no directory.
fn program_path(program: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let program = Path::new(program);
    let found = if program.components().count() > 1 {
        program.to_path_buf()
    } else {
        std::env::split_paths(path?)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())?
    };
    found.canonicalize().ok()
}

/// Where an npm adapter is installed: the topmost `node_modules` above its program's canonical
/// path, so that the adapter can load its own CLI (Claude Code), or else its directory.
fn install_dir(program: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let canonical = program_path(program, path)?;
    canonical
        .ancestors()
        .filter(|dir| dir.file_name() == Some(OsStr::new("node_modules")))
        .last()
        .or_else(|| canonical.parent())
        .map(Path::to_path_buf)
}

/// The helper itself: never returns but with an error, which the binary prints before exiting
/// with 127, and which reaches Fronde through the agent's standard error.
pub fn run_helper(
    policy: &Policy,
    program: &OsStr,
    args: &[OsString],
) -> anyhow::Result<Infallible> {
    run_helper_with(policy, Support::probe(), program, args)
}

fn run_helper_with(
    policy: &Policy,
    support: Support,
    program: &OsStr,
    args: &[OsString],
) -> anyhow::Result<Infallible> {
    use std::os::unix::process::CommandExt;
    // First, while `/proc/self` is still the user's to write.
    yield_to_fronde();
    // Then other processes of the user cannot read the helper's environment or memory; `exec`
    // makes the agent dumpable again.
    #[cfg(target_os = "linux")]
    if let Err(error) =
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
    {
        eprintln!("fronde __confine: cannot make the helper non-dumpable: {error}");
    }
    let mut command = command_for(policy, support, program, args)?;
    if let Some(sandbox) = &policy.sandbox {
        // From here on, this process and every one it starts are confined.
        landlock_rules::apply(sandbox)?;
        seccomp_rules::apply()?;
    }
    let error = command.exec();
    Err(anyhow::Error::new(error).context(format!(
        "cannot start the agent {}",
        Path::new(program).display()
    )))
}

/// The agent's command, with only the variables the policy keeps; an error first when the policy
/// asks for a sandbox this kernel cannot give.
fn command_for(
    policy: &Policy,
    support: Support,
    program: &OsStr,
    args: &[OsString],
) -> anyhow::Result<std::process::Command> {
    if policy.sandbox.is_some() {
        support.require()?;
    }
    let mut command = std::process::Command::new(program);
    command.args(args).env_clear().current_dir(&policy.cwd);
    for (name, value) in std::env::vars_os() {
        if name.to_str().is_some_and(|name| policy.keeps(name)) {
            command.env(name, value);
        }
    }
    Ok(command)
}

/// The kernel kills the agent first when memory runs out, and Fronde comes first for the CPU.
/// A crash of the agent writes no core, which would hold its tokens (and, with the host's
/// `core_pattern`, could land outside a container). None needs a privilege; none is worth
/// stopping the agent for.
fn yield_to_fronde() {
    #[cfg(target_os = "linux")]
    {
        let no_core = rustix::process::Rlimit {
            current: Some(0),
            maximum: Some(0),
        };
        if let Err(error) = rustix::process::setrlimit(rustix::process::Resource::Core, no_core) {
            eprintln!("fronde __confine: cannot turn the agent's core dumps off: {error}");
        }
        if let Err(error) = std::fs::write("/proc/self/oom_score_adj", "1000") {
            eprintln!("fronde __confine: cannot raise the agent's OOM score: {error}");
        }
        if let Err(error) = rustix::process::setpriority_process(None, 10) {
            eprintln!("fronde __confine: cannot lower the agent's priority: {error}");
        }
    }
}

#[cfg(target_os = "linux")]
fn probe_abi() -> u8 {
    landlock_rules::probe_abi()
}

#[cfg(not(target_os = "linux"))]
fn probe_abi() -> u8 {
    0
}

#[cfg(target_os = "linux")]
mod landlock_rules {
    use super::Sandbox;
    use ::landlock::{
        ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, NetPort, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus, Scope, path_beneath_rules,
    };
    use anyhow::{Context, bail};

    /// The newest ABI this build knows; the kernel's own may be older, and what it lacks is left
    /// out (best effort), but [`super::Support::require`] has checked the minimum first.
    const ABI: ABI = ABI::V9;

    /// The highest ABI whose distinctive right the kernel accepts, asked without creating or
    /// enforcing anything.
    pub(super) fn probe_abi() -> u8 {
        let fs = |access: AccessFs| {
            Ruleset::default()
                .set_compatibility(CompatLevel::HardRequirement)
                .handle_access(access)
                .is_ok()
        };
        let checks: [(u8, bool); 7] = [
            (9, fs(AccessFs::ResolveUnix)),
            (
                6,
                Ruleset::default()
                    .set_compatibility(CompatLevel::HardRequirement)
                    .scope(Scope::Signal)
                    .is_ok(),
            ),
            (5, fs(AccessFs::IoctlDev)),
            (
                4,
                Ruleset::default()
                    .set_compatibility(CompatLevel::HardRequirement)
                    .handle_access(AccessNet::ConnectTcp)
                    .is_ok(),
            ),
            (3, fs(AccessFs::Truncate)),
            (2, fs(AccessFs::Refer)),
            (1, fs(AccessFs::Execute)),
        ];
        checks
            .into_iter()
            .find(|(_, supported)| *supported)
            .map_or(0, |(abi, _)| abi)
    }

    pub(super) fn apply(sandbox: &Sandbox) -> anyhow::Result<()> {
        let mut ruleset = Ruleset::default()
            .handle_access(AccessFs::from_all(ABI))?
            .handle_access(AccessNet::from_all(ABI))?
            .scope(Scope::from_all(ABI))?
            .create()?
            .add_rules(path_beneath_rules(&sandbox.read, AccessFs::from_read(ABI)))?
            .add_rules(path_beneath_rules(&sandbox.write, AccessFs::from_all(ABI)))?;
        for port in &sandbox.connect_ports {
            ruleset = ruleset.add_rule(NetPort::new(*port, AccessNet::ConnectTcp))?;
        }
        let status = ruleset
            .restrict_self()
            .context("cannot confine the agent")?;
        if status.ruleset == RulesetStatus::NotEnforced {
            bail!(
                "the kernel did not enforce the agent's sandbox; set [agent] sandbox = false to \
                 run the agent unconfined"
            );
        }
        // Without it, a set-user-ID program would run outside the sandbox.
        if !status.no_new_privs {
            bail!(
                "the agent's sandbox could not forbid new privileges; set [agent] sandbox = \
                 false to run the agent unconfined"
            );
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
mod landlock_rules {
    pub(super) fn apply(_: &super::Sandbox) -> anyhow::Result<()> {
        anyhow::bail!(
            "the agent's sandbox needs Linux; set [agent] sandbox = false to run it unconfined"
        )
    }
}

/// The seccomp filter, which closes what Landlock leaves open below ABI 9: a Unix socket reached
/// by path is how the systemd user manager, D-Bus and the keychain, ssh-agent, gnupg and Docker
/// would act for the agent outside its sandbox. The agent gets only the sockets it needs: IPv4 and
/// IPv6 (which Landlock limits by port), netlink route sockets (read-only for an unprivileged
/// process; glibc's `getaddrinfo` and `getifaddrs` read the interfaces through them), and a
/// connected pair of stream or sequenced Unix sockets, which reaches nothing but itself and is how
/// Node gives its child processes their standard streams. A datagram pair is refused: it could
/// still send to any datagram socket by path. io_uring is refused too, since it creates sockets
/// without the `socket` system call, and so are the `ioctl`s that type into a terminal or drive
/// the Linux console. Any other process architecture (32-bit x86, whose
/// `socketcall` this filter does not inspect) is killed at its first system call.
#[cfg(target_os = "linux")]
mod seccomp_rules {
    use anyhow::Context;
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule, TargetArch,
    };
    use std::collections::BTreeMap;

    /// The architectures whose system calls the filter covers, and was tested on.
    const ARCHES: [&str; 2] = ["x86_64", "aarch64"];

    /// The bits of a socket type that name it, without `SOCK_NONBLOCK` and `SOCK_CLOEXEC`.
    const SOCK_TYPE_MASK: u64 = 0xf;

    pub(super) fn available() -> bool {
        ARCHES.contains(&std::env::consts::ARCH)
            && std::fs::read_to_string("/proc/sys/kernel/seccomp/actions_avail")
                .is_ok_and(|actions| actions.split_whitespace().any(|a| a == "errno"))
            && program().is_ok()
    }

    /// An `int` argument: the kernel reads only its low 32 bits, and so does the filter.
    fn arg(index: u8, op: SeccompCmpOp, value: i32) -> anyhow::Result<SeccompCondition> {
        Ok(SeccompCondition::new(
            index,
            SeccompCmpArgLen::Dword,
            op,
            u64::from(value as u32),
        )?)
    }

    /// An `ioctl` request, an `unsigned int` for the kernel.
    fn request(value: libc::Ioctl) -> anyhow::Result<SeccompRule> {
        Ok(SeccompRule::new(vec![SeccompCondition::new(
            1,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            u64::from(value as u32),
        )?])?)
    }

    pub(super) fn program() -> anyhow::Result<BpfProgram> {
        use SeccompCmpOp::{Eq, MaskedEq, Ne};
        let arch = std::env::consts::ARCH;
        anyhow::ensure!(
            ARCHES.contains(&arch),
            "the agent's seccomp filter does not cover {arch}"
        );
        let socket = vec![
            SeccompRule::new(vec![
                arg(0, Ne, libc::AF_INET)?,
                arg(0, Ne, libc::AF_INET6)?,
                arg(0, Ne, libc::AF_NETLINK)?,
            ])?,
            SeccompRule::new(vec![
                arg(0, Eq, libc::AF_NETLINK)?,
                arg(2, Ne, libc::NETLINK_ROUTE)?,
            ])?,
        ];
        // `AF_UNIX` with `SOCK_RAW` is a datagram socket too.
        let socketpair = vec![
            SeccompRule::new(vec![arg(1, MaskedEq(SOCK_TYPE_MASK), libc::SOCK_DGRAM)?])?,
            SeccompRule::new(vec![arg(1, MaskedEq(SOCK_TYPE_MASK), libc::SOCK_RAW)?])?,
        ];
        // Typing into a terminal, should the agent ever hold one: Landlock grants none.
        let ioctl = vec![request(libc::TIOCSTI)?, request(libc::TIOCLINUX)?];
        let rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::from([
            (libc::SYS_socket, socket),
            (libc::SYS_socketpair, socketpair),
            (libc::SYS_ioctl, ioctl),
            (libc::SYS_io_uring_setup, Vec::new()),
            (libc::SYS_io_uring_enter, Vec::new()),
            (libc::SYS_io_uring_register, Vec::new()),
        ]);
        // The x32 ABI shares the x86_64 audit architecture, with its own system call numbers.
        #[cfg(target_arch = "x86_64")]
        let rules = {
            const X32_SYSCALL_BIT: i64 = 0x4000_0000;
            let x32: Vec<(i64, Vec<SeccompRule>)> = rules
                .iter()
                .map(|(number, chain)| (number | X32_SYSCALL_BIT, chain.clone()))
                .collect();
            rules.into_iter().chain(x32).collect()
        };
        let filter = SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::Errno(libc::EACCES as u32),
            TargetArch::try_from(arch)?,
        )?;
        Ok(filter.try_into()?)
    }

    pub(super) fn apply() -> anyhow::Result<()> {
        let program = program()?;
        seccompiler::apply_filter(&program).context(
            "cannot install the agent's seccomp filter; set [agent] sandbox = false to run the \
             agent unconfined",
        )
    }
}

#[cfg(not(target_os = "linux"))]
mod seccomp_rules {
    pub(super) fn available() -> bool {
        false
    }

    pub(super) fn apply() -> anyhow::Result<()> {
        anyhow::bail!(
            "the agent's sandbox needs Linux; set [agent] sandbox = false to run it unconfined"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fronde_protocol::ThreadId;
    use secrecy::SecretString;

    /// A data directory with the main thread's directories, `agent` and `home`.
    fn data() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["agent", "home"] {
            std::fs::create_dir(dir.path().join(sub)).unwrap();
        }
        dir
    }

    fn spec(data: &tempfile::TempDir, env: Vec<(String, String)>) -> AgentSpec {
        AgentSpec {
            thread: ThreadId::MAIN,
            provider: &crate::provider::Provider::CLAUDE,
            command: vec!["claude-agent-acp".into(), "--flag".into()],
            env,
            cwd: data.path().join("agent"),
            home: data.path().join("home"),
            hub_url: "http://127.0.0.1:8484/mcp".into(),
            hub_token: SecretString::from("token-value-never-on-the-command-line"),
            oauth_token: None,
            system_prompt: String::new(),
            builtin_tools: Vec::new(),
            usage: None,
            owners_grants: true,
            account: "claude".into(),
        }
    }

    fn confine(sandbox: bool) -> Confine {
        Confine {
            helper: PathBuf::from("/usr/bin/fronde"),
            policy: Policy {
                cwd: PathBuf::new(),
                keep_env: KEEP_ENV.map(str::to_owned).to_vec(),
                drop_env: vec!["SERVER_TOKEN".into(), "SHARED".into()],
                sandbox: sandbox.then(|| Sandbox {
                    read: vec!["/usr".into()],
                    write: vec!["/dev/null".into()],
                    connect_ports: CONNECT_PORTS.to_vec(),
                }),
            },
            protected: Vec::new(),
            scope: None,
            owner_read: Vec::new(),
        }
    }

    #[test]
    fn policy_holds_no_values() {
        let value = "a-value-only-the-environment-holds";
        let data = data();
        let spec = spec(
            &data,
            vec![
                ("FOO".into(), value.into()),
                ("CLAUDE_CODE_OAUTH_TOKEN".into(), value.into()),
            ],
        );
        let command = confine(true).command(&spec).unwrap();
        let line = command.args.join(" ");
        assert!(!line.contains(value), "{line}");
        assert!(!line.contains("token-value-never"), "{line}");
        assert!(command.policy.keep_env.contains(&"FOO".to_owned()));
        assert!(
            command
                .env
                .iter()
                .any(|(name, set)| name == "FOO" && set == value)
        );
        // The values reach the helper through its environment.
        assert_eq!(
            Policy::from_arg(&command.policy.to_arg()).unwrap(),
            command.policy
        );
    }

    #[test]
    fn secrets_reach_the_agent_through_its_environment_only() {
        let mut confine = confine(true);
        // Fronde's `{ env }` secret of that name is dropped, but the value Fronde gives is kept.
        confine
            .policy
            .drop_env
            .push("CLAUDE_CODE_OAUTH_TOKEN".into());
        let secrets = vec![
            ("FRONDE_HUB_TOKEN".to_owned(), "hub-secret-value".to_owned()),
            (
                "CLAUDE_CODE_OAUTH_TOKEN".to_owned(),
                "oauth-secret-value".to_owned(),
            ),
        ];
        let data = data();
        let command = confine
            .command_with(&spec(&data, Vec::new()), secrets)
            .unwrap();
        let line = command.args.join(" ");
        assert!(!line.contains("secret-value"), "{line}");
        for (name, value) in [
            ("FRONDE_HUB_TOKEN", "hub-secret-value"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "oauth-secret-value"),
        ] {
            assert!(command.policy.keeps(name), "{name}");
            assert!(command.env.contains(&(name.to_owned(), value.to_owned())));
        }
    }

    #[test]
    fn helper_command_runs_the_agent_through_the_helper() {
        let data = data();
        let command = confine(true)
            .command(&spec(&data, vec![("SHARED".into(), "set".into())]))
            .unwrap();
        assert_eq!(command.program, PathBuf::from("/usr/bin/fronde"));
        let args: Vec<&str> = command.args.iter().map(String::as_str).collect();
        assert_eq!(args[0], SUBCOMMAND);
        assert_eq!(args[1], "--policy");
        assert_eq!(&args[3..], ["--", "claude-agent-acp", "--flag"]);
        let policy = Policy::from_arg(args[2]).unwrap();
        assert_eq!(policy.cwd, data.path().join("agent"));
        assert_eq!(
            policy.sandbox.as_ref().unwrap().connect_ports,
            [443, 53, 8484],
            "the hub's port is added"
        );
        // `[agent] env` sets SHARED itself, so its value is the configuration's, and kept.
        assert_eq!(policy.drop_env, ["SERVER_TOKEN"]);
        assert!(policy.keeps("SHARED"));
        let env: Vec<(&str, &str)> = command
            .env
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        let home = data.path().join("home");
        let tmp = home.join("tmp");
        assert!(env.contains(&("HOME", home.to_str().unwrap())));
        assert!(env.contains(&("TMPDIR", tmp.to_str().unwrap())));
    }

    #[test]
    fn each_thread_writes_only_its_own_directories() {
        let data = data();
        let mut confine = confine(true);
        let second = data.path().join("threads/2");
        for sub in ["work", "home"] {
            std::fs::create_dir_all(second.join(sub)).unwrap();
        }
        let main = spec(&data, Vec::new());
        let thread = AgentSpec {
            thread: ThreadId(2),
            cwd: second.join("work"),
            home: second.join("home"),
            ..spec(&data, Vec::new())
        };
        let grants = |spec: &AgentSpec, confine: &Confine| {
            confine.command(spec).map(|command| {
                let env = command.env.clone();
                (command.policy.sandbox.unwrap().write, env)
            })
        };
        let canonical = |path: PathBuf| path.canonicalize().unwrap();
        let (write, env) = grants(&thread, &confine).unwrap();
        assert_eq!(
            write,
            [
                PathBuf::from("/dev/null"),
                canonical(second.join("work")),
                canonical(second.join("home")),
            ]
        );
        let home = second.join("home").display().to_string();
        assert!(env.contains(&("HOME".to_owned(), home.clone())), "{env:?}");
        assert!(env.contains(&("TMPDIR".to_owned(), format!("{home}/tmp"))));
        // The main thread keeps `agent` and `home`, and gets nothing of thread 2's.
        let (write, _) = grants(&main, &confine).unwrap();
        assert_eq!(
            write,
            [
                PathBuf::from("/dev/null"),
                canonical(data.path().join("agent")),
                canonical(data.path().join("home")),
            ]
        );
        // A thread's directory that holds a protected path, or that is missing, is refused.
        confine.protected = vec![canonical(second.join("home")).join("fronde.toml")];
        let error = grants(&thread, &confine).unwrap_err().to_string();
        assert!(error.contains("fronde.toml"), "{error}");
        confine.protected = Vec::new();
        let missing = AgentSpec {
            home: data.path().join("threads/3/home"),
            ..thread.clone()
        };
        assert!(grants(&missing, &confine).is_err());
        // Unconfined, no grant is checked: there is no sandbox to grant anything in.
        confine.policy.sandbox = None;
        assert!(confine.command(&missing).is_ok());
    }

    #[test]
    fn helper_command_runs_through_the_scope() {
        let mut confine = confine(true);
        let data = data();
        let spec = spec(&data, Vec::new());
        let plain = confine.command(&spec).unwrap();
        assert!(plain.unit.is_none());
        confine.scope = Some(Scope {
            run: PathBuf::from("/usr/bin/systemd-run"),
            prefix: "fronde-agent-0123abcd".to_owned(),
            limits: crate::scope::Limits {
                memory: Some(1 << 30),
                cpus: Some(1.5),
                tasks: Some(100),
            },
        });
        let command = confine.command(&spec).unwrap();
        assert_eq!(command.program, PathBuf::from("/usr/bin/systemd-run"));
        let unit = command.unit.clone().unwrap();
        assert!(unit.starts_with("fronde-agent-0123abcd-t1-"), "{unit}");
        let other = AgentSpec {
            thread: ThreadId(7),
            ..spec.clone()
        };
        let other = confine.command(&other).unwrap().unit.unwrap();
        assert!(other.starts_with("fronde-agent-0123abcd-t7-"), "{other}");
        let args: Vec<&str> = command.args.iter().map(String::as_str).collect();
        let dash = args.iter().position(|a| *a == "--").unwrap();
        assert_eq!(
            args[..dash],
            [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--slice=fronde-agents.slice",
                &format!("--unit={unit}"),
                "--property=MemoryMax=1073741824",
                "--property=MemoryHigh=805306368",
                "--property=CPUQuota=150%",
                "--property=TasksMax=100",
                "--property=OOMPolicy=stop",
            ]
        );
        // Then the helper's own command, unchanged.
        assert_eq!(args[dash + 1], "/usr/bin/fronde");
        assert_eq!(
            args[dash + 2..],
            plain.args.iter().map(String::as_str).collect::<Vec<_>>()[..]
        );
        assert_eq!(command.policy, plain.policy);
        // systemd-run keeps the user manager's variables; the policy still drops them.
        for name in MANAGER_ENV {
            assert!(!command.policy.keeps(name));
            assert!(!command.env.iter().any(|(n, _)| n == name), "{name}");
            if std::env::var_os(name).is_some() {
                assert!(
                    plain.env.contains(&(name.to_owned(), String::new())),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn keep_env_is_an_allow_list() {
        let policy = confine(false).policy;
        for kept in ["PATH", "LANG", "LC_ALL"] {
            assert!(policy.keeps(kept), "{kept}");
        }
        for dropped in [
            // How the agent signs in, where its API is, or its settings: never from Fronde's
            // environment, only from its spec and `[agent] env`.
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_CODE_EXECUTABLE",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_API_KEY",
            "XDG_RUNTIME_DIR",
            "DBUS_SESSION_BUS_ADDRESS",
            "CREDENTIALS_DIRECTORY",
            "SSH_AUTH_SOCK",
            "LD_PRELOAD",
            "NODE_OPTIONS",
            "SERVER_TOKEN",
            "PATHS",
            "LC",
        ] {
            assert!(!policy.keeps(dropped), "{dropped}");
        }
        // A secret's variable is dropped even when a kept prefix matches it.
        let mut policy = policy;
        policy.drop_env.push("LC_FRONDE_TOKEN".into());
        assert!(!policy.keeps("LC_FRONDE_TOKEN"));
        // So is every variable a dropped prefix matches, and only those.
        policy.drop_env.push("LC_SECRET_*".into());
        assert!(!policy.keeps("LC_SECRET_KEY"));
        assert!(policy.keeps("LC_TIME"));
        // A lone `*` keeps nothing.
        policy.keep_env = vec!["*".into()];
        assert!(!policy.keeps("PATH"));
    }

    #[test]
    fn policy_refuses_unknown_fields() {
        assert!(
            Policy::from_arg(r#"{"cwd":"/","keep_env":[],"drop_env":[],"sandbox":null,"x":1}"#)
                .is_err()
        );
    }

    #[test]
    fn url_ports() {
        assert_eq!(url_port("http://127.0.0.1:8484/mcp"), Some(8484));
        assert_eq!(url_port("http://[::1]:9000/mcp"), Some(9000));
        assert_eq!(url_port("http://[::1]/mcp"), Some(80));
        assert_eq!(url_port("https://proxy.example"), Some(443));
        assert_eq!(url_port("http://user:pw@proxy:3128"), Some(3128));
        assert_eq!(url_port("socks5://proxy"), Some(1080));
        assert_eq!(url_port("proxy:3128"), None);
    }

    #[test]
    fn sandbox_unavailable_stops_the_agent() {
        let missing = OsStr::new("/nonexistent/fronde-test-agent");
        for abi in [0, 1, 2] {
            let support = Support { abi, seccomp: true };
            let error = command_for(&confine(true).policy, support, missing, &[])
                .unwrap_err()
                .to_string();
            assert!(error.contains("[agent] sandbox = false"), "{error}");
            assert!(error.contains("Landlock ABI 3"), "{error}");
            let found = if abi == 0 {
                "no Landlock".to_owned()
            } else {
                format!("Landlock ABI {abi}")
            };
            assert!(error.contains(&found), "{error}");
        }
        // Landlock without seccomp is refused too.
        let support = Support {
            abi: 6,
            seccomp: false,
        };
        let error = command_for(&confine(true).policy, support, missing, &[])
            .unwrap_err()
            .to_string();
        assert!(error.contains("seccomp"), "{error}");
        assert!(error.contains("[agent] sandbox = false"), "{error}");
        assert!(support.describe(&[]).starts_with("unavailable"));
        // Without the sandbox, the same kernel runs the agent with the explicit environment.
        let mut policy = confine(false).policy;
        policy.cwd = PathBuf::from("/");
        let support = Support {
            abi: 0,
            seccomp: false,
        };
        let command = command_for(&policy, support, missing, &[]).unwrap();
        assert!(
            command
                .get_envs()
                .all(|(name, _)| policy.keeps(name.to_str().unwrap()))
        );
    }

    #[test]
    fn support_says_what_is_missing() {
        let support = |abi| Support { abi, seccomp: true };
        assert_eq!(
            support(9).describe(&[443, 53]),
            format!(
                "Landlock ABI 9 (files, TCP ports 443 53, device ioctls, signals and abstract \
                 sockets, Unix socket paths); {SOCKETS}"
            )
        );
        let text = support(3).describe(&[443]);
        assert!(
            text.contains(
                "not confined: TCP ports (Linux 6.7), device ioctls (Linux 6.10), signals to \
                 Fronde and abstract sockets (Linux 6.12)"
            ),
            "{text}"
        );
        // Below ABI 9, seccomp keeps the agent from Unix sockets on its own.
        assert!(support(2).describe(&[]).starts_with("unavailable"));
        let text = support(6).describe(&[443]);
        assert!(text.ends_with(SOCKETS), "{text}");
        assert!(!text.contains("not confined"), "{text}");
        assert!(support(1).describe(&[]).starts_with("unavailable"));
    }

    fn tree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let config = root.path().join("config");
        for dir in [data.join("agent"), data.join("home"), config.clone()] {
            std::fs::create_dir_all(dir).unwrap();
        }
        (root, data, config)
    }

    fn request<'a>(
        system: &'a [PathBuf],
        path: Option<&'a OsStr>,
        extra: &'a [PathBuf],
        write: &'a [PathBuf],
        protected: &'a [PathBuf],
    ) -> GrantRequest<'a> {
        GrantRequest {
            system,
            path,
            program: "/nonexistent/agent",
            install: Install::NodeModules,
            extra,
            write,
            protected,
            home: None,
        }
    }

    #[test]
    fn policy_refuses_grants_over_protected_paths() {
        let (root, data, config) = tree();
        let protected = vec![data.clone(), config.join("master.key"), config.clone()];
        let write = vec![data.join("agent"), data.join("home")];

        // `[agent] read` naming the data directory, or a link to it.
        let error = grants(&request(
            &[],
            None,
            std::slice::from_ref(&data),
            &write,
            &protected,
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("[agent] read"), "{error}");
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&data, &link).unwrap();
        assert!(grants(&request(&[], None, &[link], &write, &protected)).is_err());

        // A PATH entry holding the data directory is skipped with a warning, not granted.
        let path = std::env::join_paths([root.path(), Path::new("/usr/bin")]).unwrap();
        let granted = grants(&request(&[], Some(&path), &[], &write, &protected)).unwrap();
        let root_canonical = root.path().canonicalize().unwrap();
        assert!(!granted.read.contains(&root_canonical));
        assert_eq!(granted.skipped.len(), 1, "{:?}", granted.skipped);
        assert!(granted.read.contains(&PathBuf::from("/usr/bin")));

        // A configuration under a granted system directory.
        let system = vec![root.path().to_path_buf()];
        let error = grants(&request(&system, None, &[], &write, &protected))
            .unwrap_err()
            .to_string();
        assert!(error.contains("system directories"), "{error}");

        // A configuration kept in the agent's own directory.
        let protected = vec![data.join("home/fronde.toml")];
        assert!(grants(&request(&[], None, &[], &write, &protected)).is_err());
    }

    #[test]
    fn grants_are_canonical_and_complete() {
        let (root, data, config) = tree();
        let protected = vec![data.clone(), config.clone()];
        let write = vec![data.join("agent"), data.join("home")];
        let tools = root.path().join("tools");
        std::fs::create_dir(&tools).unwrap();
        let system = vec![PathBuf::from("/usr"), PathBuf::from("/nonexistent")];
        let granted = grants(&request(
            &system,
            None,
            std::slice::from_ref(&tools),
            &write,
            &protected,
        ))
        .unwrap();
        assert!(granted.read.contains(&PathBuf::from("/usr")));
        assert!(!granted.read.contains(&PathBuf::from("/nonexistent")));
        assert!(granted.read.contains(&tools.canonicalize().unwrap()));
        assert_eq!(
            granted.write,
            [
                data.join("agent").canonicalize().unwrap(),
                data.join("home").canonicalize().unwrap(),
                PathBuf::from("/dev/null"),
                PathBuf::from("/dev/full"),
            ]
        );
    }

    #[test]
    fn the_owners_home_is_never_granted_whole() {
        let (root, data, _) = tree();
        let home = root.path().join("owner");
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let write = vec![data.join("agent"), data.join("home")];
        let path = std::env::join_paths([root.path(), &home, &bin]).unwrap();
        let mut request = request(&[], Some(&path), &[], &write, &[]);
        request.home = Some(&home);
        let granted = grants(&request).unwrap();
        // Neither the home nor what holds it, through PATH.
        assert!(!granted.read.contains(&home.canonicalize().unwrap()));
        assert!(!granted.read.contains(&root.path().canonicalize().unwrap()));
        assert_eq!(granted.skipped.len(), 2, "{:?}", granted.skipped);
        // A directory inside it is granted, and reported.
        let bin = bin.canonicalize().unwrap();
        assert!(granted.read.contains(&bin));
        assert_eq!(granted.under_home, [bin]);
        // An install that holds the home is refused.
        let program = home.join("agent");
        std::fs::write(&program, "").unwrap();
        let program = program.to_str().unwrap().to_owned();
        let mut refused = request.clone();
        refused.path = None;
        refused.program = &program;
        let error = grants(&refused).unwrap_err().to_string();
        assert!(error.contains("install the adapter elsewhere"), "{error}");
    }

    #[test]
    fn helper_command_blanks_what_the_agent_does_not_keep() {
        let data = data();
        let command = confine(true).command(&spec(&data, Vec::new())).unwrap();
        let mut blanked = 0;
        for (name, _) in std::env::vars_os() {
            let Some(name) = name.to_str() else { continue };
            let set = command.env.iter().find(|(n, _)| n == name);
            if command.policy.keeps(name) {
                assert!(set.is_none_or(|(_, value)| !value.is_empty()), "{name}");
            } else {
                assert_eq!(set.map(|(_, value)| value.as_str()), Some(""), "{name}");
                blanked += 1;
            }
        }
        // The test runner sets variables the agent does not keep, such as `CARGO_*`.
        assert!(blanked > 0);
    }

    #[test]
    fn the_adapter_is_granted_from_its_topmost_node_modules() {
        let (root, ..) = tree();
        let modules = root.path().join("agent/node_modules");
        let package = modules.join("@scope/adapter/node_modules/inner/dist");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::create_dir_all(modules.join(".bin")).unwrap();
        let script = package.join("index.js");
        std::fs::write(&script, "").unwrap();
        let link = modules.join(".bin/claude-agent-acp");
        std::os::unix::fs::symlink(&script, &link).unwrap();
        let path = std::env::join_paths([modules.join(".bin")]).unwrap();
        assert_eq!(
            install_dir("claude-agent-acp", Some(&path)),
            Some(modules.canonicalize().unwrap())
        );
        let plain = root.path().join("bin");
        std::fs::create_dir(&plain).unwrap();
        std::fs::write(plain.join("agent"), "").unwrap();
        assert_eq!(
            install_dir(plain.join("agent").to_str().unwrap(), None),
            Some(plain.canonicalize().unwrap())
        );
    }

    /// Only an npm adapter's profile grants its whole `node_modules`; a native binary's grants
    /// its directory alone, wherever it is.
    #[test]
    fn the_install_grant_follows_the_provider() {
        let (root, ..) = tree();
        let modules = root.path().join("agent/node_modules");
        let package = modules.join("@scope/adapter/dist");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::create_dir_all(modules.join(".bin")).unwrap();
        let script = package.join("index.js");
        std::fs::write(&script, "").unwrap();
        let link = modules.join(".bin/claude-agent-acp");
        std::os::unix::fs::symlink(&script, &link).unwrap();
        let path = std::env::join_paths([modules.join(".bin")]).unwrap();
        let claude = crate::provider::Provider::CLAUDE.install;
        assert_eq!(
            install_grant(claude, "claude-agent-acp", Some(&path)),
            Some(modules.canonicalize().unwrap())
        );
        let native = crate::provider::Provider::bare("antigravity").install;
        assert_eq!(
            install_grant(native, "claude-agent-acp", Some(&path)),
            Some(package.canonicalize().unwrap())
        );
        assert_eq!(install_grant(native, "absent-agent", Some(&path)), None);
    }
}
