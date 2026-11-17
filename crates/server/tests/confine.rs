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
    config: fronde_hub::config::Config,
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
        config,
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
            oauth_token: None,
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
/// fail with the sandbox's error, which [`refused_without_landlock`] checks; with
/// `FRONDE_REQUIRE_LANDLOCK=1` (CI on a kernel that has it), it is a failure instead.
fn landlock() -> bool {
    let support = Support::probe();
    if !support.ready() {
        assert!(
            std::env::var_os("FRONDE_REQUIRE_LANDLOCK").is_none(),
            "FRONDE_REQUIRE_LANDLOCK is set, but this kernel cannot confine the agent \
             ({support:?})"
        );
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
    report.insert(
        "device open /dev/tty".to_owned(),
        outcome(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty"),
        ),
    );
    report.insert(
        "device list /dev/shm".to_owned(),
        outcome(std::fs::read_dir("/dev/shm")),
    );
    report.insert(
        "device create in /dev/shm".to_owned(),
        outcome(std::fs::write("/dev/shm/fronde-probe", b"probe")),
    );
    report.insert(
        "device read /dev/urandom".to_owned(),
        outcome(std::fs::File::open("/dev/urandom").and_then(|mut f| {
            use std::io::Read;
            f.read_exact(&mut [0; 8])
        })),
    );
    report.insert(
        "device write /dev/null".to_owned(),
        outcome(std::fs::write("/dev/null", b"probe")),
    );
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
/// `initialize` and `session/new`. Signed out, as its home is new: a turn is `live.rs`'s. The
/// session's MCP header names the hub's token by its variable, as Fronde sends it, and Claude
/// Code expands it from the agent's environment: the stand-in hub receives the token itself.
#[test]
#[ignore = "needs the adapter from `mise run agent` in FRONDE_LIVE_AGENT"]
fn real_adapter_starts_confined() {
    use std::io::BufRead;
    let adapter = std::env::var("FRONDE_LIVE_AGENT").expect("FRONDE_LIVE_AGENT");
    let setup = setup(&format!("command = [{adapter:?}]"));
    assert!(landlock(), "this kernel cannot confine the agent");
    let hub = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let hub_port = hub.local_addr().unwrap().port();
    let (heard, authorization) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::{BufRead, Write};
        for stream in hub.incoming().map_while(Result::ok) {
            let mut reader = std::io::BufReader::new(&stream);
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("authorization:") {
                    let _ = heard.send(line[line.len() - value.len()..].trim().to_owned());
                }
                line.clear();
            }
            let _ = (&stream).write_all(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n");
        }
    });
    let spec = setup.spec_with_hub(&[&adapter], &[], hub_port);
    let token = "hub-token-from-the-environment";
    let secrets = vec![(
        fronde_agent::acp::HUB_TOKEN_VAR.to_owned(),
        token.to_owned(),
    )];
    let helper = setup.confine.command_with(&spec, secrets).unwrap();
    assert!(!helper.args.join(" ").contains(token));
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
        serde_json::json!({
            "cwd": setup.data.join("agent"),
            "mcpServers": [{
                "type": "http",
                "name": "fronde",
                "url": format!("http://127.0.0.1:{hub_port}/mcp"),
                "headers": [{
                    "name": "Authorization",
                    "value": format!("Bearer ${{{}}}", fronde_agent::acp::HUB_TOKEN_VAR),
                }],
            }],
        }),
    );
    assert!(session["result"]["sessionId"].is_string(), "{session}");
    let header = authorization
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("Claude Code never reached the hub");
    assert_eq!(header, format!("Bearer {token}"));
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    // It kept to its own home.
    assert!(
        setup.data.join("home/.claude").exists() || setup.data.join("home/.claude.json").exists()
    );
}

#[test]
fn agent_cannot_reach_terminals_or_shared_memory() {
    let probe = probe_setup();
    let Some(report) = probe.run() else { return };
    for attempt in [
        "device open /dev/tty",
        "device list /dev/shm",
        "device create in /dev/shm",
    ] {
        assert_eq!(report[attempt], "denied", "{attempt}: {report:#?}");
    }
    assert!(!Path::new("/dev/shm/fronde-probe").exists());
    for attempt in ["device read /dev/urandom", "device write /dev/null"] {
        assert_eq!(report[attempt], "ok", "{attempt}: {report:#?}");
    }
}

/// A fake `systemd-run` and `systemctl` in a directory of their own, to put on the `PATH`. Each
/// records its arguments, one per line, then `---`. `systemd-run` also records the
/// `XDG_RUNTIME_DIR` it got, then runs what follows `--` as the real one does with `--scope`, or,
/// when it does not `work`, fails as it does without a user manager.
struct FakeSystemd {
    dir: tempfile::TempDir,
}

impl FakeSystemd {
    fn new(works: bool) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let record = r#"#!/bin/sh
here=$(dirname "$0")
log="$here/$(basename "$0").log"
for arg in "$@"; do printf '%s\n' "$arg"; done >> "$log"
printf -- '---\n' >> "$log"
"#;
        let run = if works {
            r#"printf '%s\n' "${XDG_RUNTIME_DIR-unset}" >> "$here/xdg"
while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do shift; done
shift
exec "$@"
"#
        } else {
            "echo 'Failed to connect to user scope bus via local transport: No such file or \
             directory' >&2\nexit 1\n"
        };
        for (name, script) in [
            ("systemd-run", format!("{record}{run}")),
            ("systemctl", record.to_owned()),
        ] {
            let file = dir.path().join(name);
            std::fs::write(&file, script).unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self { dir }
    }

    fn path(&self) -> std::ffi::OsString {
        self.dir.path().as_os_str().to_owned()
    }

    /// Each call of `program`, as its arguments.
    fn calls(&self, program: &str) -> Vec<Vec<String>> {
        let log = std::fs::read_to_string(self.dir.path().join(format!("{program}.log")))
            .unwrap_or_default();
        log.split_terminator("---\n")
            .map(|call| call.lines().map(str::to_owned).collect())
            .collect()
    }
}

/// Run as an ACP agent by the tests below (`FRONDE_ACP_CHILD` set): the fake agent, on standard
/// input and output, until its input ends. A no-op otherwise.
#[test]
fn acp_child() {
    if std::env::var_os("FRONDE_ACP_CHILD").is_none() {
        return;
    }
    let script = fronde_agent::fake::FakeScript {
        updates: vec![fronde_agent::fake::text_chunk("depuis le scope")],
        ..Default::default()
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let served = runtime.block_on(fronde_agent::fake::serve_on_stdio(script));
    // Before the test harness writes its own lines on the standard output.
    std::process::exit(i32::from(served.is_err()));
}

/// A setup whose agent is [`acp_child`], run by a shell that keeps only its JSON lines: the test
/// harness writes its own on the same output.
fn acp_setup(limits: &str) -> (Setup, AgentSpec) {
    let exe = std::env::current_exe().unwrap();
    let exe_dir = exe.parent().unwrap().display().to_string();
    let setup = setup(&format!("read = [{exe_dir:?}]\n{limits}"));
    let script = r#""$0" --exact acp_child --nocapture | grep --line-buffered '^{'"#;
    let spec = setup.spec(
        &["/bin/sh", "-c", script, exe.to_str().unwrap()],
        &[("FRONDE_ACP_CHILD", "1")],
    );
    (setup, spec)
}

/// Starts the agent as Fronde does, and runs one turn: `false` when this kernel refused to
/// confine it, as it must.
async fn chat(spec: &AgentSpec, confine: &Confine) -> Option<fronde_agent::acp::AgentClient> {
    use fronde_agent::acp::{AgentClient, AgentEvent, TurnEnd};
    let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
    let started = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        AgentClient::spawn_confined(spec, confine, None, events),
    )
    .await
    .expect("the agent took too long to start");
    if !landlock() {
        let error = format!("{:#}", started.err().expect("an unconfined agent started"));
        assert!(error.contains("[agent] sandbox = false"), "{error}");
        return None;
    }
    let client = started.unwrap();
    assert_eq!(client.prompt("Bonjour").await.unwrap(), TurnEnd::Done);
    let mut text = String::new();
    while let Ok(event) = received.try_recv() {
        if let AgentEvent::Text(chunk) = event {
            text.push_str(&chunk);
        }
    }
    assert_eq!(text, "depuis le scope");
    Some(client)
}

#[tokio::test]
async fn scope_wraps_the_helper() {
    let (mut setup, spec) = acp_setup("");
    let fake = FakeSystemd::new(true);
    let scope = fronde::agent_scope(&setup.config, Some(&fake.path())).expect("systemd-run found");
    assert_eq!(scope.run, fake.dir.path().join("systemd-run"));
    let prefix = scope.prefix.clone();
    assert!(
        prefix.starts_with("fronde-agent-") && prefix.len() == 21,
        "{prefix}"
    );
    setup.confine.scope = Some(scope);
    let Some(client) = chat(&spec, &setup.confine).await else {
        return;
    };

    // First the leftovers of this data directory are stopped, then the user manager is asked
    // for a scope, then the agent starts in one.
    let ctl = fake.calls("systemctl");
    assert_eq!(ctl, [["--user", "stop", &format!("{prefix}-*.scope")]]);
    let runs = fake.calls("systemd-run");
    assert_eq!(runs.len(), 2, "{runs:#?}");
    let limits = [
        "--property=MemoryMax=2147483648",
        "--property=MemoryHigh=1610612736",
        "--property=CPUQuota=200%",
        "--property=TasksMax=512",
    ];
    let options = [
        "--user",
        "--scope",
        "--quiet",
        "--collect",
        "--slice=fronde-agents.slice",
    ];
    let probe: Vec<&str> = options
        .iter()
        .chain(&limits)
        .chain(&["--", "true"])
        .copied()
        .collect();
    assert_eq!(runs[0], probe);
    let start = &runs[1];
    assert_eq!(start[..5], options);
    let unit = start[5].strip_prefix("--unit=").expect("a unit name");
    assert!(
        unit.starts_with(&format!("{prefix}-")) && unit.len() == prefix.len() + 9,
        "{unit}"
    );
    assert_eq!(start[6..10], limits);
    assert_eq!(start[10], "--");
    assert_eq!(start[11], env!("CARGO_BIN_EXE_fronde"));
    assert_eq!(start[12], "__confine");
    assert_eq!(start[15..17], ["--", "/bin/sh"]);
    // systemd-run reached the user manager with Fronde's variable, in the check as at the start,
    // although the agent never gets it.
    let xdg = std::fs::read_to_string(fake.dir.path().join("xdg")).unwrap();
    let expected = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "unset".to_owned());
    assert_eq!(xdg.lines().collect::<Vec<_>>(), [&expected, &expected]);

    // Once the client goes, so does its scope.
    drop(client);
    let stop = vec![
        "--user".to_owned(),
        "stop".to_owned(),
        "--no-block".to_owned(),
        format!("{unit}.scope"),
    ];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !fake.calls("systemctl").contains(&stop) {
        assert!(
            std::time::Instant::now() < deadline,
            "the scope was never stopped"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn no_systemd_user_manager_means_no_scope() {
    let (mut setup, spec) = acp_setup("");
    let fake = FakeSystemd::new(false);
    setup.confine.scope = fronde::agent_scope(&setup.config, Some(&fake.path()));
    let line = fronde::limits_check_line(&setup.config, setup.confine.scope.as_ref()).await;
    assert!(
        line.starts_with("none (the systemd user manager made no scope: Failed to connect"),
        "{line}"
    );
    assert!(line.ends_with("the agent is OOM-killed first"), "{line}");
    // The agent starts all the same, without systemd-run.
    let Some(client) = chat(&spec, &setup.confine).await else {
        return;
    };
    drop(client);
    let runs = fake.calls("systemd-run");
    assert_eq!(
        runs.len(),
        2,
        "the check and the agent's start ask once each: {runs:#?}"
    );
    assert!(
        runs.iter().all(|run| run.last().unwrap() == "true"),
        "{runs:#?}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        fake.calls("systemctl")
            .iter()
            .all(|call| !call.contains(&"--no-block".to_owned())),
        "no scope to stop"
    );

    // Without systemd-run, as in a container, nothing is asked.
    let empty = tempfile::tempdir().unwrap();
    let scope = fronde::agent_scope(&setup.config, Some(empty.path().as_os_str()));
    assert!(scope.is_none());
    let line = fronde::limits_line(&setup.config, None);
    assert!(line.starts_with("none (no systemd-run here)"), "{line}");
    assert_eq!(fronde::limits_check_line(&setup.config, None).await, line);
}

#[tokio::test]
async fn limits_turned_off_mean_no_scope() {
    let (setup, _) =
        acp_setup("[agent.limits]\nmemory = \"none\"\ncpus = \"none\"\ntasks = \"none\"");
    let fake = FakeSystemd::new(true);
    assert!(fronde::agent_scope(&setup.config, Some(&fake.path())).is_none());
    let line = fronde::limits_line(&setup.config, None);
    assert!(
        line.starts_with("none ([agent.limits] turns them off)"),
        "{line}"
    );
    // One limit off is left out of the scope, the others kept.
    let (setup, _) = acp_setup("[agent.limits]\nmemory = \"none\"\ncpus = 0.5");
    let scope = fronde::agent_scope(&setup.config, Some(&fake.path())).unwrap();
    let args = scope.args(None);
    assert!(
        args.contains(&"--property=CPUQuota=50%".to_owned()),
        "{args:?}"
    );
    assert!(
        args.contains(&"--property=TasksMax=512".to_owned()),
        "{args:?}"
    );
    assert!(!args.iter().any(|a| a.contains("Memory")), "{args:?}");
    assert_eq!(
        fronde::limits_check_line(&setup.config, Some(&scope)).await,
        "memory uncapped, 0.5 CPUs, 512 tasks, in a systemd scope under fronde-agents.slice"
    );
}

#[test]
fn scope_keeps_the_agent_confined_and_its_environment_explicit() {
    let fake = FakeSystemd::new(true);
    let mut probe = probe_setup();
    probe.setup.confine.scope = fronde::agent_scope(&probe.setup.config, Some(&fake.path()));
    let Some(report) = probe.run() else { return };
    assert_eq!(
        fake.calls("systemd-run").len(),
        1,
        "the probe ran in the scope"
    );
    for file in &probe.plan.protected_files {
        let attempt = format!("read {}", file.display());
        assert_eq!(report[&attempt], "denied", "{attempt}: {report:#?}");
    }
    let spec = probe.setup.spec(&shell(r#"env > "$PWD/env.txt""#), &[]);
    let output = probe.setup.run(
        &spec,
        &[
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
            ("INVOCATION_ID", "0123"),
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let text = std::fs::read_to_string(probe.setup.data.join("agent/env.txt")).unwrap();
    for absent in ["XDG_", "DBUS_", "INVOCATION_ID"] {
        assert!(
            !text.contains(absent),
            "{absent} reached the agent:\n{text}"
        );
    }
}

#[test]
fn agent_core_dumps_are_off() {
    let setup = setup("");
    let spec = setup.spec(&shell("ulimit -c; ulimit -H -c"), &[]);
    let helper = setup.confine.command(&spec).unwrap();
    // Started with core dumps allowed, as far as this process may allow them.
    let output = Command::new("/bin/sh")
        .args([
            "-c",
            r#"ulimit -c "$(ulimit -H -c)" 2>/dev/null; exec "$@""#,
            "sh",
        ])
        .arg(&helper.program)
        .args(&helper.args)
        .envs(helper.env.iter().map(|(n, v)| (n.as_str(), v.as_str())))
        .output()
        .unwrap();
    if !landlock() {
        return refused_without_landlock(&output);
    }
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "0\n0\n");
}

/// Runs `systemctl --user` with `args`, for the real-scope tests.
fn systemctl(args: &[&str]) -> String {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// With the real user manager: the agent runs in `fronde-agents.slice`, capped, and its scope
/// goes when it ends. Run by hand on a machine with a systemd user session:
/// `mise exec -- cargo nextest run -p fronde --test confine real_scope --run-ignored only`.
#[tokio::test]
#[ignore = "needs a systemd user manager"]
async fn real_scope_caps_the_agent() {
    use std::io::{BufRead, Write};
    let mut setup = setup("");
    assert!(landlock(), "this kernel cannot confine the agent");
    setup.confine.scope = fronde::agent_scope(&setup.config, std::env::var_os("PATH").as_deref());
    let confine = setup.confine.ready().await;
    assert!(confine.scope.is_some(), "no systemd user manager answered");
    let script = r#"cg=$(cut -d: -f3 /proc/self/cgroup)
echo "cgroup=$cg"
echo "memory=$(cat /sys/fs/cgroup$cg/memory.max)"
echo "high=$(cat /sys/fs/cgroup$cg/memory.high)"
echo "pids=$(cat /sys/fs/cgroup$cg/pids.max)"
echo "cpu=$(cat /sys/fs/cgroup$cg/cpu.max)"
echo "invocation=${INVOCATION_ID-unset}"
echo ready
read line"#;
    let helper = confine.command(&setup.spec(&shell(script), &[])).unwrap();
    let unit = helper.unit.clone().unwrap();
    let mut child = Command::new(&helper.program)
        .args(&helper.args)
        .envs(helper.env.iter().map(|(n, v)| (n.as_str(), v.as_str())))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut seen = BTreeMap::new();
    for line in std::io::BufReader::new(child.stdout.take().unwrap()).lines() {
        let line = line.unwrap();
        if line == "ready" {
            break;
        }
        let (name, value) = line.split_once('=').unwrap();
        seen.insert(name.to_owned(), value.to_owned());
    }
    assert!(
        seen["cgroup"].ends_with(&format!("/fronde.slice/fronde-agents.slice/{unit}.scope")),
        "{seen:?}"
    );
    assert_eq!(seen["memory"], "2147483648");
    assert_eq!(seen["high"], "1610612736");
    assert_eq!(seen["pids"], "512");
    assert_eq!(seen["cpu"], "200000 100000");
    assert_eq!(
        seen["invocation"], "unset",
        "systemd-run's variable reached the agent"
    );
    // The same, as the user manager tells it.
    let shown = systemctl(&[
        "show",
        &format!("{unit}.scope"),
        "-p",
        "Slice",
        "-p",
        "MemoryMax",
        "-p",
        "TasksMax",
        "-p",
        "CPUQuotaPerSecUSec",
        "-p",
        "ActiveState",
    ]);
    for expected in [
        "Slice=fronde-agents.slice",
        "MemoryMax=2147483648",
        "TasksMax=512",
        "CPUQuotaPerSecUSec=2s",
        "ActiveState=active",
    ] {
        assert!(shown.lines().any(|l| l == expected), "{expected}:\n{shown}");
    }
    // The agent ends at the end of its input, and its scope with it.
    writeln!(stdin).unwrap();
    drop(stdin);
    assert!(child.wait().unwrap().success());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while systemctl(&["is-active", &format!("{unit}.scope")]) == "active" {
        assert!(
            std::time::Instant::now() < deadline,
            "{unit} outlived the agent"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// With the real user manager: a process that outlives its agent, as one could when Fronde is
/// killed with SIGKILL, keeps the scope; the agent's next start stops it.
#[tokio::test]
#[ignore = "needs a systemd user manager"]
async fn real_scope_leftovers_are_stopped_at_the_next_start() {
    let mut setup = setup("");
    assert!(landlock(), "this kernel cannot confine the agent");
    setup.confine.scope = fronde::agent_scope(&setup.config, std::env::var_os("PATH").as_deref());
    let confine = setup.confine.ready().await;
    // Its input is not a pipe from Fronde: it never sees an end, and ignores it anyway.
    let helper = confine
        .command(&setup.spec(&shell("sleep 300 & wait"), &[]))
        .unwrap();
    let unit = format!("{}.scope", helper.unit.clone().unwrap());
    let mut child = Command::new(&helper.program)
        .args(&helper.args)
        .envs(helper.env.iter().map(|(n, v)| (n.as_str(), v.as_str())))
        .stdin(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while systemctl(&["is-active", &unit]) != "active" {
        assert!(std::time::Instant::now() < deadline, "{unit} never started");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // Only the agent's own process dies; `sleep` stays, and so does the scope.
    child.kill().unwrap();
    child.wait().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(systemctl(&["is-active", &unit]), "active");
    let _ = setup.confine.ready().await;
    assert_ne!(systemctl(&["is-active", &unit]), "active");
}
