//! The agent's confinement, through the real `fronde __confine` helper.

use fronde_agent::acp::AgentSpec;
use fronde_agent::confine::Confine;
use secrecy::SecretString;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A configuration and data directory in a temporary directory, and the agent's confinement.
struct Setup {
    _root: tempfile::TempDir,
    data: PathBuf,
    confine: Confine,
}

fn setup(agent_env: &str) -> Setup {
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
env = {{ {agent_env} }}
"#,
            data.display()
        ),
    )
    .unwrap();
    let config = fronde_hub::config::load(&file).unwrap();
    let confine = fronde::agent_confine(&config, Path::new(env!("CARGO_BIN_EXE_fronde"))).unwrap();
    Setup {
        _root: root,
        data,
        confine,
    }
}

impl Setup {
    fn spec(&self, command: &[&str], env: &[(&str, &str)]) -> AgentSpec {
        AgentSpec {
            command: command.iter().map(|s| s.to_string()).collect(),
            env: env
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            cwd: self.data.join("agent"),
            hub_url: "http://127.0.0.1:8484/mcp".into(),
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

fn shell(script: &str) -> Vec<&str> {
    vec!["/bin/sh", "-c", script]
}

fn variables(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(n, v)| (n.to_owned(), v.to_owned()))
        .collect()
}

#[test]
fn agent_environment_is_explicit() {
    let setup = setup(r#"FOO = "bar""#);
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
fn agent_oom_score_is_raised() {
    let setup = setup("");
    let spec = setup.spec(&shell("cat /proc/self/oom_score_adj"), &[]);
    let output = setup.run(&spec, &[]);
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
    assert!(stderr.contains("fronde-test-no-such-agent"), "{stderr}");
}
