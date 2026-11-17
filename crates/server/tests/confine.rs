//! The agent's confinement, through the real `fronde __confine` helper. The confined agent is a
//! shell, or this test binary run again as `probe_child`, which tries what an agent must not do
//! and reports each outcome.
//!
//! `real_adapter_starts_confined` runs the Claude ACP adapter itself under the sandbox. It is
//! ignored by default; run it after `mise run agent`:
//!
//! ```sh
//! FRONDE_LIVE_AGENT=$(which claude-agent-acp) \
//!   mise exec -- cargo nextest run -p fronde --test confine --run-ignored only --no-capture
//! ```

#![cfg(target_os = "linux")]

use fronde_agent::acp::AgentSpec;
use fronde_agent::confine::{Confine, Support};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A configuration and data directory in a temporary directory, and the agent's confinement.
struct Setup {
    root: tempfile::TempDir,
    data: PathBuf,
    config_dir: PathBuf,
    confine: Confine,
}

fn setup(agent: &str) -> Setup {
    setup_with(agent, |_| {})
}

/// `agent`: lines of the `[agent]` table. `adjust` changes the loaded configuration.
fn setup_with(agent: &str, adjust: impl FnOnce(&mut fronde_hub::config::Config)) -> Setup {
    try_setup(agent, adjust).unwrap()
}

fn try_setup(
    agent: &str,
    adjust: impl FnOnce(&mut fronde_hub::config::Config),
) -> anyhow::Result<Setup> {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let config_dir = root.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let file = config_dir.join("fronde.toml");
    std::fs::write(
        &file,
        format!(
            r#"
[server]
token = {{ env = "FRONDE_TEST_SERVER_TOKEN" }}
[data]
dir = "{}"
timezone = "Europe/Paris"
[web]
password = {{ env = "CLAUDE_TEST_PASSWORD" }}
[agent]
{agent}
"#,
            data.display()
        ),
    )
    .unwrap();
    let mut config = fronde_hub::config::load(&file).unwrap();
    adjust(&mut config);
    let confine = fronde::agent_confine(&config, Path::new(env!("CARGO_BIN_EXE_fronde")))?;
    Ok(Setup {
        root,
        data,
        config_dir,
        confine,
    })
}

impl Setup {
    fn spec(&self, command: &[&str], env: &[(&str, &str)]) -> AgentSpec {
        self.spec_with_hub(command, env, 8484)
    }

    fn spec_with_hub(&self, command: &[&str], env: &[(&str, &str)], hub_port: u16) -> AgentSpec {
        AgentSpec {
            command: command.iter().map(|s| s.to_string()).collect(),
            env: env
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            cwd: self.data.join("agent"),
            hub_url: format!("http://127.0.0.1:{hub_port}/mcp"),
            hub_token: SecretString::from("unused"),
            system_prompt: String::new(),
            builtin_tools: Vec::new(),
        }
    }

    /// Runs `spec` through the helper, as Fronde would, with `parent` added to the environment
    /// Fronde has.
    fn run(&self, spec: &AgentSpec, parent: &[(&str, &str)]) -> Output {
        let helper = self.confine.command(spec).unwrap();
        Command::new(&helper.program)
            .args(&helper.args)
            .envs(parent.iter().copied())
            .envs(helper.env.iter().map(|(n, v)| (n.as_str(), v.as_str())))
            .output()
            .unwrap()
    }
}

/// Whether this kernel can confine the agent. When it cannot, every confined run must instead
/// fail with the sandbox's error, which [`refused_without_landlock`] checks.
fn landlock() -> bool {
    let support = Support::probe();
    if !support.ready() {
        eprintln!("this kernel cannot confine the agent ({support:?}): checking the refusal");
    }
    support.ready()
}

fn refused_without_landlock(output: &Output) {
    assert_eq!(output.status.code(), Some(127), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("[agent] sandbox = false"), "{stderr}");
}

fn shell(script: &str) -> Vec<&str> {
    vec!["/bin/sh", "-c", script]
}

fn variables(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(n, v)| (n.to_owned(), v.to_owned()))
        .collect()
}

fn check_environment(sandbox: bool) {
    let agent = format!("env = {{ FOO = \"bar\" }}\nsandbox = {sandbox}");
    let setup = setup(&agent);
    let spec = setup.spec(&shell(r#"env > "$PWD/env.txt""#), &[("FOO", "bar")]);
    let parent = [
        ("FRONDE_TEST_CANARY", "canary-value"),
        ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ("XDG_CONFIG_HOME", "/home/owner/.config"),
        ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
        ("CREDENTIALS_DIRECTORY", "/run/credentials/fronde.service"),
        ("SSH_AUTH_SOCK", "/run/user/1000/openssh_agent"),
        ("FRONDE_TEST_SERVER_TOKEN", "server-token-value"),
        // A secret's variable is dropped even when a kept prefix matches its name.
        ("CLAUDE_TEST_PASSWORD", "password-value"),
        ("CLAUDE_CODE_OAUTH_TOKEN", "oauth-token"),
        ("LANG", "fr_FR.UTF-8"),
    ];
    let output = setup.run(&spec, &parent);
    if sandbox && !landlock() {
        return refused_without_landlock(&output);
    }
    assert!(output.status.success(), "{output:?}");
    let text = std::fs::read_to_string(setup.data.join("agent/env.txt")).unwrap();
    let env = variables(&text);
    for absent in [
        "FRONDE_TEST_CANARY",
        "XDG_RUNTIME_DIR",
        "XDG_CONFIG_HOME",
        "DBUS_SESSION_BUS_ADDRESS",
        "CREDENTIALS_DIRECTORY",
        "SSH_AUTH_SOCK",
        "FRONDE_TEST_SERVER_TOKEN",
        "CLAUDE_TEST_PASSWORD",
    ] {
        assert!(
            !env.contains_key(absent),
            "{absent} reached the agent:\n{text}"
        );
    }
    assert!(
        !text.contains("value"),
        "a secret's value reached the agent:\n{text}"
    );
    assert!(!text.contains("XDG_") && !text.contains("DBUS_"), "{text}");
    assert_eq!(env["FOO"], "bar");
    assert_eq!(env["LANG"], "fr_FR.UTF-8");
    assert_eq!(env["CLAUDE_CODE_OAUTH_TOKEN"], "oauth-token");
    assert!(env.contains_key("PATH"));
    let home = setup.data.join("home");
    assert_eq!(env["HOME"], home.display().to_string());
    assert_eq!(env["TMPDIR"], home.join("tmp").display().to_string());
    assert!(home.join("tmp").is_dir());
    // Only what the shell sets itself besides what the policy keeps.
    let policy = setup.confine.command(&spec).unwrap().policy;
    for name in env.keys() {
        assert!(
            policy.keeps(name) || ["PWD", "SHLVL", "_", "OLDPWD"].contains(&name.as_str()),
            "{name} is not on the allow list:\n{text}"
        );
    }
}

#[test]
fn agent_environment_is_explicit() {
    check_environment(true);
    // Without the sandbox, the environment is the same.
    check_environment(false);
}

#[test]
fn agent_oom_score_is_raised() {
    let setup = setup("");
    let spec = setup.spec(&shell("cat /proc/self/oom_score_adj"), &[]);
    let output = setup.run(&spec, &[]);
    if !landlock() {
        return refused_without_landlock(&output);
    }
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "1000");
}

#[test]
fn helper_reports_a_missing_program() {
    let setup = setup("");
    let spec = setup.spec(&["fronde-test-no-such-agent"], &[]);
    let output = setup.run(&spec, &[]);
    assert_eq!(output.status.code(), Some(127), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    if landlock() {
        assert!(stderr.contains("fronde-test-no-such-agent"), "{stderr}");
    }
}

/// What the probe tries, and where.
#[derive(Serialize, Deserialize)]
struct Plan {
    protected_files: Vec<PathBuf>,
    protected_dirs: Vec<PathBuf>,
    allowed_dirs: Vec<PathBuf>,
    open_port: u16,
    closed_port: u16,
    /// Unix sockets listening outside the sandbox, by path.
    unix_paths: Vec<PathBuf>,
    /// The name of an abstract Unix socket listening outside the sandbox.
    unix_abstract: String,
}

/// What the probe tried, by name: `ok`, `denied`, or another error.
type Report = BTreeMap<String, String>;

fn outcome<T>(result: std::io::Result<T>) -> String {
    match result {
        Ok(_) => "ok".to_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => "denied".to_owned(),
        Err(error) => format!("error: {error}"),
    }
}

/// `ok`, or the error number: to tell the seccomp filter's `EACCES` from any other refusal.
fn errno<T>(result: std::io::Result<T>) -> String {
    match result {
        Ok(_) => "ok".to_owned(),
        Err(error) => format!("errno {}", error.raw_os_error().unwrap_or(-1)),
    }
}

fn eacces() -> String {
    format!("errno {}", rustix::io::Errno::ACCESS.raw_os_error())
}

/// The descriptors this process inherited that are sockets: none, since the agent's standard
/// streams are pipes and Rust opens everything else close-on-exec.
fn inherited_sockets() -> String {
    let mut sockets = Vec::new();
    for entry in std::fs::read_dir("/proc/self/fd").unwrap().flatten() {
        if let Ok(target) = std::fs::read_link(entry.path())
            && target.to_string_lossy().starts_with("socket:")
        {
            sockets.push(format!("{:?} {}", entry.file_name(), target.display()));
        }
    }
    sockets.join(", ")
}

/// Every kind of socket the agent might try, by name.
fn probe_sockets(report: &mut Report, plan: &Plan) {
    use rustix::net::{AddressFamily, SocketType, netlink, socket};
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};
    let raw = |family, kind, protocol| errno(socket(family, kind, protocol).map_err(Into::into));
    for path in &plan.unix_paths {
        report.insert(
            format!("unix connect {}", path.display()),
            errno(UnixStream::connect(path)),
        );
    }
    report.insert(
        "unix abstract".to_owned(),
        errno(
            SocketAddr::from_abstract_name(&plan.unix_abstract)
                .and_then(|address| UnixStream::connect_addr(&address)),
        ),
    );
    report.insert(
        "unix listen".to_owned(),
        errno(UnixListener::bind("probe.sock")),
    );
    report.insert("unix datagram".to_owned(), errno(UnixDatagram::unbound()));
    report.insert("unix datagram pair".to_owned(), errno(UnixDatagram::pair()));
    report.insert(
        "unix raw pair".to_owned(),
        errno(
            rustix::net::socketpair(
                AddressFamily::UNIX,
                SocketType::RAW,
                rustix::net::SocketFlags::CLOEXEC,
                None,
            )
            .map_err(Into::into),
        ),
    );
    // What Node gives its children as standard streams: it must still work, both ways.
    let pair = UnixStream::pair().and_then(|(mut a, mut b)| {
        use std::io::{Read, Write};
        a.write_all(b"ping")?;
        let mut buffer = [0; 4];
        b.read_exact(&mut buffer)?;
        b.write_all(&buffer)?;
        a.read_exact(&mut buffer)?;
        Ok(buffer)
    });
    report.insert(
        "unix stream pair".to_owned(),
        match pair {
            Ok(buffer) if &buffer == b"ping" => "ok".to_owned(),
            other => format!("{other:?}"),
        },
    );
    report.insert(
        "seqpacket pair".to_owned(),
        errno(
            rustix::net::socketpair(
                AddressFamily::UNIX,
                SocketType::SEQPACKET,
                rustix::net::SocketFlags::CLOEXEC,
                None,
            )
            .map_err(Into::into),
        ),
    );
    report.insert(
        "ipv4".to_owned(),
        raw(AddressFamily::INET, SocketType::STREAM, None),
    );
    report.insert(
        "udp".to_owned(),
        errno(std::net::UdpSocket::bind("127.0.0.1:0")),
    );
    report.insert(
        "netlink route".to_owned(),
        raw(AddressFamily::NETLINK, SocketType::RAW, None),
    );
    report.insert(
        "netlink sock_diag".to_owned(),
        raw(
            AddressFamily::NETLINK,
            SocketType::RAW,
            Some(netlink::SOCK_DIAG),
        ),
    );
    report.insert(
        "packet".to_owned(),
        raw(AddressFamily::PACKET, SocketType::DGRAM, None),
    );
    report.insert(
        "vsock".to_owned(),
        raw(AddressFamily::VSOCK, SocketType::STREAM, None),
    );
}

fn probe_file(report: &mut Report, file: &Path) {
    let name = file.display();
    report.insert(format!("read {name}"), outcome(std::fs::read(file)));
    report.insert(
        format!("append {name}"),
        outcome(std::fs::OpenOptions::new().append(true).open(file)),
    );
    let moved = file.with_extension("moved");
    let renamed = std::fs::rename(file, &moved);
    if renamed.is_ok() {
        std::fs::rename(&moved, file).unwrap();
    }
    report.insert(format!("rename {name}"), outcome(renamed));
    report.insert(
        format!("unlink {name}"),
        outcome(std::fs::remove_file(file)),
    );
}

fn probe_dir(report: &mut Report, dir: &Path) {
    let name = dir.display();
    report.insert(format!("list {name}"), outcome(std::fs::read_dir(dir)));
    let created = dir.join("probe-new.txt");
    report.insert(
        format!("create in {name}"),
        outcome(std::fs::write(&created, b"new")),
    );
}

/// Run as the confined agent by the tests below (`FRONDE_PROBE` set); a no-op otherwise.
#[test]
fn probe_child() {
    let Ok(plan) = std::env::var("FRONDE_PROBE") else {
        return;
    };
    let plan: Plan = serde_json::from_str(&plan).unwrap();
    let mut report = Report::new();
    report.insert("inherited sockets".to_owned(), inherited_sockets());
    probe_sockets(&mut report, &plan);
    for file in &plan.protected_files {
        probe_file(&mut report, file);
    }
    for dir in plan.protected_dirs.iter().chain(&plan.allowed_dirs) {
        probe_dir(&mut report, dir);
    }
    for dir in &plan.allowed_dirs {
        let file = dir.join("probe.txt");
        std::fs::write(&file, b"probe").unwrap();
        probe_file(&mut report, &file);
    }
    let parent = rustix::process::getppid().expect("a parent");
    report.insert(
        "signal parent".to_owned(),
        outcome(rustix::process::test_kill_process(parent).map_err(std::io::Error::from)),
    );
    report.insert(
        "read parent environ".to_owned(),
        outcome(std::fs::read(format!(
            "/proc/{}/environ",
            parent.as_raw_pid()
        ))),
    );
    for (name, port) in [
        ("open port", plan.open_port),
        ("closed port", plan.closed_port),
    ] {
        report.insert(
            name.to_owned(),
            outcome(std::net::TcpStream::connect(("127.0.0.1", port))),
        );
    }
    std::fs::write("report.json", serde_json::to_vec(&report).unwrap()).unwrap();
}

/// A confinement with a full data and configuration directory, a fake systemd credentials
/// directory and a file standing for the owner's home, and this test binary's directory granted
/// so that it can run as the probe.
struct Probe {
    setup: Setup,
    plan: Plan,
    _listeners: [std::net::TcpListener; 2],
    _unix: [UnixListener; 2],
    _credentials: tempfile::TempDir,
    originals: Vec<(PathBuf, Vec<u8>)>,
}

fn probe_setup() -> Probe {
    let exe = std::env::current_exe().unwrap();
    let exe_dir = exe.parent().unwrap().to_path_buf();
    let credentials_dir = tempfile::tempdir().unwrap();
    let credentials = credentials_dir.path().to_path_buf();
    let setup = setup_with(
        &format!("read = [{:?}]", exe_dir.display().to_string()),
        |c| c.secrets.set_credentials_dir(Some(credentials.clone())),
    );
    let data = &setup.data;
    let config = &setup.config_dir;
    let owner = setup.root.path().join("owner-home");
    std::fs::create_dir_all(data.join("memory")).unwrap();
    std::fs::create_dir_all(&owner).unwrap();
    let protected_files = vec![
        data.join("fronde.db"),
        data.join("fronde.db-wal"),
        data.join("fronde.db-shm"),
        data.join("memory/MEMORY.md"),
        data.join("other.txt"),
        config.join("fronde.toml"),
        config.join("master.key"),
        config.join("fronde-master-key.cred"),
        credentials.join("fronde-master-key"),
        owner.join("secret.txt"),
    ];
    let mut originals = Vec::new();
    for file in &protected_files {
        if !file.exists() {
            std::fs::write(file, format!("secret {}", file.display())).unwrap();
        }
        originals.push((file.clone(), std::fs::read(file).unwrap()));
    }
    let open = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    // Stand-ins for the user manager, D-Bus, ssh-agent or Docker: a path outside the grants, and
    // an abstract name.
    let unix_path = setup.root.path().join("outside.sock");
    let unix = UnixListener::bind(&unix_path).unwrap();
    let unix_abstract = format!("fronde-test-{}", setup.root.path().display());
    let abstract_listener = {
        use std::os::linux::net::SocketAddrExt;
        let address = std::os::unix::net::SocketAddr::from_abstract_name(&unix_abstract).unwrap();
        UnixListener::bind_addr(&address).unwrap()
    };
    UnixStream::connect(&unix_path).expect("the test reaches its own socket");
    let plan = Plan {
        protected_files,
        protected_dirs: vec![
            data.clone(),
            data.join("memory"),
            config.clone(),
            credentials.clone(),
            owner,
        ],
        allowed_dirs: vec![data.join("agent"), data.join("home"), data.join("home/tmp")],
        open_port: open.local_addr().unwrap().port(),
        closed_port: closed.local_addr().unwrap().port(),
        unix_paths: vec![unix_path],
        unix_abstract,
    };
    Probe {
        setup,
        plan,
        _listeners: [open, closed],
        _unix: [unix, abstract_listener],
        _credentials: credentials_dir,
        originals,
    }
}

impl Probe {
    /// Runs the probe confined; `None` when this kernel refused to confine it, as it must.
    fn run(&self) -> Option<Report> {
        let exe = std::env::current_exe().unwrap();
        let plan = serde_json::to_string(&self.plan).unwrap();
        let spec = self.setup.spec_with_hub(
            &[
                exe.to_str().unwrap(),
                "--exact",
                "probe_child",
                "--nocapture",
            ],
            &[("FRONDE_PROBE", &plan)],
            self.plan.open_port,
        );
        let output = self.setup.run(&spec, &[]);
        if !landlock() {
            refused_without_landlock(&output);
            return None;
        }
        assert!(output.status.success(), "{output:?}");
        let report = std::fs::read(self.setup.data.join("agent/report.json")).unwrap();
        Some(serde_json::from_slice(&report).unwrap())
    }
}

#[test]
fn agent_cannot_touch_protected_paths() {
    let probe = probe_setup();
    let Some(report) = probe.run() else { return };
    let protected: Vec<String> = probe
        .plan
        .protected_files
        .iter()
        .chain(&probe.plan.protected_dirs)
        .map(|p| p.display().to_string())
        .collect();
    let allowed: Vec<String> = probe
        .plan
        .allowed_dirs
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    let mut checked = 0;
    for (attempt, result) in &report {
        let Some((_, target)) = attempt.split_once(' ') else {
            continue;
        };
        let target = target.trim_start_matches("in ");
        if protected.iter().any(|p| p == target) {
            assert_eq!(result, "denied", "{attempt}");
            checked += 1;
        } else if allowed.iter().any(|a| Path::new(target).starts_with(a)) {
            assert_eq!(result, "ok", "{attempt}");
            checked += 1;
        }
    }
    // Four attempts on each protected file, two on each directory, six in each allowed one.
    let expected = 4 * probe.plan.protected_files.len()
        + 2 * probe.plan.protected_dirs.len()
        + 6 * probe.plan.allowed_dirs.len();
    assert_eq!(checked, expected, "{report:#?}");
    for (file, content) in &probe.originals {
        assert_eq!(&std::fs::read(file).unwrap(), content, "{}", file.display());
    }
    for dir in &probe.plan.protected_dirs {
        assert!(!dir.join("probe-new.txt").exists(), "{}", dir.display());
    }
}

#[test]
fn a_child_of_the_agent_is_confined_too() {
    let setup = setup("");
    let db = setup.data.join("fronde.db");
    std::fs::write(&db, "secret").unwrap();
    let script = format!("cat {:?}", db.display().to_string());
    // Messages in English, whatever the machine's locale.
    let output = setup.run(&setup.spec(&shell(&script), &[]), &[("LC_ALL", "C")]);
    if !landlock() {
        return refused_without_landlock(&output);
    }
    assert!(!output.status.success(), "{output:?}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("secret"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Permission denied"), "{stderr}");
}

#[test]
fn agent_cannot_signal_or_read_fronde() {
    let probe = probe_setup();
    let Some(report) = probe.run() else { return };
    // Landlock's ptrace rule, from its first ABI.
    assert_eq!(report["read parent environ"], "denied", "{report:#?}");
    if Support::probe().scopes() {
        assert_eq!(report["signal parent"], "denied", "{report:#?}");
    } else {
        eprintln!("this kernel has no Landlock signal scope (Linux 6.12): signals not checked");
    }
}

#[test]
fn agent_tcp_is_limited_to_its_ports() {
    let probe = probe_setup();
    let Some(report) = probe.run() else { return };
    assert_eq!(report["open port"], "ok", "{report:#?}");
    if Support::probe().tcp_ports() {
        assert_eq!(report["closed port"], "denied", "{report:#?}");
    } else {
        eprintln!("this kernel has no Landlock TCP rules (Linux 6.7): ports not checked");
    }
}

#[test]
fn grants_never_cover_the_credentials_or_the_configuration() {
    // The service's credentials directory.
    let credentials = tempfile::tempdir().unwrap();
    let dir = credentials.path().to_path_buf();
    let read = format!("read = [{:?}]", dir.display().to_string());
    let error = try_setup(&read, |c| c.secrets.set_credentials_dir(Some(dir.clone())))
        .err()
        .expect("a grant over the credentials directory was accepted")
        .to_string();
    assert!(error.contains("[agent] read"), "{error}");
    // Where systemd keeps credentials; a directory that does not exist is no grant at all.
    for granted in ["/run", "/run/credentials"] {
        let result = try_setup(&format!("read = [{granted:?}]"), |_| {});
        if Path::new(granted).exists() {
            let error = result.err().expect(granted).to_string();
            assert!(error.contains("[agent] read"), "{error}");
        }
    }
    // The configuration's directory, which holds fronde.toml, master.key and a .cred file:
    // `[agent] read` is read from it, so "." names it.
    let error = try_setup(r#"read = ["."]"#, |_| {})
        .err()
        .expect("a grant over the configuration's directory was accepted")
        .to_string();
    assert!(error.contains("[agent] read"), "{error}");
}

#[test]
fn agent_cannot_reach_unix_sockets() {
    let probe = probe_setup();
    let Some(report) = probe.run() else { return };
    let denied = eacces();
    for path in &probe.plan.unix_paths {
        let attempt = format!("unix connect {}", path.display());
        assert_eq!(report[&attempt], denied, "{attempt}: {report:#?}");
    }
    for attempt in [
        "unix abstract",
        "unix listen",
        "unix datagram",
        // A datagram pair could still send to any datagram socket by path.
        "unix datagram pair",
        "unix raw pair",
        // Other families than IPv4, IPv6 and netlink route sockets.
        "netlink sock_diag",
        "packet",
        "vsock",
    ] {
        assert_eq!(report[attempt], denied, "{attempt}: {report:#?}");
    }
    assert!(
        !probe.setup.data.join("agent/probe.sock").exists(),
        "{report:#?}"
    );
    // What the agent still needs: pairs for its children's streams, TCP to its ports, UDP for
    // DNS, and the netlink route socket glibc reads the interfaces through.
    for attempt in [
        "unix stream pair",
        "seqpacket pair",
        "ipv4",
        "udp",
        "netlink route",
        "open port",
    ] {
        assert_eq!(report[attempt], "ok", "{attempt}: {report:#?}");
    }
    assert_eq!(report["inherited sockets"], "", "{report:#?}");
}

/// The sockets of this machine's D-Bus buses the test itself can reach.
fn buses() -> Vec<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(
        || PathBuf::from(format!("/run/user/{}", rustix::process::getuid().as_raw())),
        PathBuf::from,
    );
    [
        runtime.join("bus"),
        PathBuf::from("/run/dbus/system_bus_socket"),
    ]
    .into_iter()
    .filter(|path| UnixStream::connect(path).is_ok())
    .collect()
}

#[test]
fn dbus_is_unreachable() {
    let mut probe = probe_setup();
    let buses = buses();
    if buses.is_empty() {
        eprintln!("no D-Bus bus on this machine: only the test's own socket is checked");
    }
    probe.plan.unix_paths.extend(buses.iter().cloned());
    let Some(report) = probe.run() else { return };
    // The Secret Service keychain is spoken over the session bus.
    for bus in &buses {
        let attempt = format!("unix connect {}", bus.display());
        assert_eq!(report[&attempt], eacces(), "{attempt}: {report:#?}");
    }
}

#[test]
fn escape_through_the_user_manager_fails() {
    let Some(systemd_run) = ["/usr/bin/systemd-run", "/bin/systemd-run"]
        .into_iter()
        .find(|path| Path::new(path).exists())
    else {
        eprintln!("no systemd-run on this machine: nothing to escape through");
        return;
    };
    let setup = setup("");
    let db = setup.data.join("fronde.db");
    std::fs::write(&db, "secret").unwrap();
    let runtime = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/run/user/{}", rustix::process::getuid().as_raw()));
    let bus = format!("unix:path={runtime}/bus");
    // The agent is told where the user manager is, so that only the sandbox stops it.
    let spec = setup.spec(
        &shell(&format!(
            "{systemd_run} --user --wait --pipe --quiet --collect cat {:?}",
            db.display().to_string()
        )),
        &[
            ("XDG_RUNTIME_DIR", &runtime),
            ("DBUS_SESSION_BUS_ADDRESS", &bus),
        ],
    );
    let output = setup.run(&spec, &[("LC_ALL", "C")]);
    if !landlock() {
        return refused_without_landlock(&output);
    }
    assert!(!output.status.success(), "{output:?}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("secret"));
    // sd-bus reports the filter's refusal of its Unix socket as a missing privilege.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Failed to connect to user scope bus via local transport")
            && (stderr.contains("Permission denied") || stderr.contains("Operation not permitted")),
        "{stderr}"
    );
}

/// Sends one JSON-RPC request to the agent and waits for its answer, skipping notifications.
fn rpc(
    stdin: &mut impl std::io::Write,
    lines: &std::sync::mpsc::Receiver<String>,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let request =
        serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    writeln!(stdin, "{request}").unwrap();
    stdin.flush().unwrap();
    loop {
        let line = lines
            .recv_timeout(std::time::Duration::from_secs(60))
            .unwrap_or_else(|_| panic!("no answer to {method}"));
        let message: serde_json::Value = serde_json::from_str(&line).unwrap();
        if message["id"] == id {
            return message;
        }
    }
}

/// The real adapter, with its Claude Code, under the whole sandbox: it starts, and answers
/// `initialize` and `session/new`. Signed out, as its home is new: a turn is `live.rs`'s.
#[test]
#[ignore = "needs the adapter from `mise run agent` in FRONDE_LIVE_AGENT"]
fn real_adapter_starts_confined() {
    use std::io::BufRead;
    let adapter = std::env::var("FRONDE_LIVE_AGENT").expect("FRONDE_LIVE_AGENT");
    let setup = setup(&format!("command = [{adapter:?}]"));
    assert!(landlock(), "this kernel cannot confine the agent");
    let spec = setup.spec(&[&adapter], &[]);
    let helper = setup.confine.command(&spec).unwrap();
    let mut child = Command::new(&helper.program)
        .args(&helper.args)
        .envs(helper.env.iter().map(|(n, v)| (n.as_str(), v.as_str())))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (send, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if send.send(line).is_err() {
                break;
            }
        }
    });
    let init = rpc(
        &mut stdin,
        &lines,
        0,
        "initialize",
        serde_json::json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    assert!(init["result"]["protocolVersion"].is_number(), "{init}");
    let session = rpc(
        &mut stdin,
        &lines,
        1,
        "session/new",
        serde_json::json!({"cwd": setup.data.join("agent"), "mcpServers": []}),
    );
    assert!(session["result"]["sessionId"].is_string(), "{session}");
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    // It kept to its own home.
    assert!(
        setup.data.join("home/.claude").exists() || setup.data.join("home/.claude.json").exists()
    );
}
