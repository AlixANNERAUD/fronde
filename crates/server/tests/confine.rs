//! The agent's confinement, through the real `fronde __confine` helper. The confined agent is a
//! shell, or this test binary run again as `probe_child`, which tries what an agent must not do
//! and reports each outcome.

#![cfg(target_os = "linux")]

use fronde_agent::acp::AgentSpec;
use fronde_agent::confine::{Confine, Support};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    if !support.files() {
        eprintln!("this kernel cannot confine the agent ({support:?}): checking the refusal");
    }
    support.files()
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
    };
    Probe {
        setup,
        plan,
        _listeners: [open, closed],
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
