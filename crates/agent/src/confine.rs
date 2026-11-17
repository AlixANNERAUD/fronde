//! `fronde __confine`: the helper Fronde starts instead of its agent. It keeps only the
//! environment variables its policy names, lowers its own priority, raises its OOM score and then
//! `exec`s the agent, which inherits all of it.
//! It runs in a fresh, single-threaded process, so nothing runs between `fork` and `exec` in
//! Fronde's own.

use crate::acp::AgentSpec;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// The subcommand of the `fronde` binary that runs [`run_helper`].
pub const SUBCOMMAND: &str = "__confine";

/// The variables the agent keeps from Fronde's environment, by name or by prefix (ending in `*`):
/// the `PATH`, the locale and time zone, Claude's own settings and token, proxies and CA
/// certificates. Fronde sets `HOME` and `TMPDIR`; `[agent] env` adds its own names.
pub const KEEP_ENV: [&str; 18] = [
    "PATH",
    "LANG",
    "LANGUAGE",
    "LC_*",
    "TZ",
    "CLAUDE_*",
    "ANTHROPIC_*",
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

/// What the helper does before it starts the agent. It holds names and paths only, never a value:
/// it travels on the command line, which every user can read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// The agent's working directory.
    pub cwd: PathBuf,
    /// Variables kept, by exact name or by a prefix ending in `*`; every other one is dropped.
    pub keep_env: Vec<String>,
    /// Exact names dropped even when `keep_env` matches them: Fronde's `{ env }` secrets.
    pub drop_env: Vec<String>,
}

impl Policy {
    /// The policy as one compact command-line argument.
    pub fn to_arg(&self) -> String {
        serde_json::to_string(self).expect("a policy always serialises")
    }

    pub fn from_arg(arg: &str) -> anyhow::Result<Self> {
        serde_json::from_str(arg).context("invalid --policy")
    }

    /// Whether the agent gets the variable `name`.
    pub fn keeps(&self, name: &str) -> bool {
        !self.drop_env.iter().any(|dropped| dropped == name)
            && self
                .keep_env
                .iter()
                .any(|pattern| match pattern.strip_suffix('*') {
                    Some(prefix) => !prefix.is_empty() && name.starts_with(prefix),
                    None => pattern == name,
                })
    }
}

/// How Fronde starts its agent through the helper.
#[derive(Clone, Debug)]
pub struct Confine {
    /// The `fronde` binary.
    pub helper: PathBuf,
    /// The agent's own home, `<data>/home`; its `tmp` is the agent's `TMPDIR`.
    pub home: PathBuf,
    /// Everything but what the spec gives: its directory and its variables.
    pub policy: Policy,
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
}

impl Confine {
    /// The helper's command line and environment for `spec`.
    pub fn command(&self, spec: &AgentSpec) -> anyhow::Result<HelperCommand> {
        let (program, args) = spec
            .command
            .split_first()
            .context("the agent command is empty; set it to claude-agent-acp")?;
        let env: Vec<(String, String)> = spec
            .env
            .iter()
            .cloned()
            .chain([
                ("HOME".to_owned(), path_string(&self.home)?),
                ("TMPDIR".to_owned(), path_string(&self.home.join("tmp"))?),
            ])
            .collect();
        let mut policy = self.policy.clone();
        policy.cwd = spec.cwd.clone();
        for (name, _) in &env {
            if !policy.keep_env.contains(name) {
                policy.keep_env.push(name.clone());
            }
        }
        // A secret's variable that `[agent] env` sets holds the configuration's value instead.
        policy
            .drop_env
            .retain(|name| !spec.env.iter().any(|(set, _)| set == name));
        let mut helper_args = vec![
            SUBCOMMAND.to_owned(),
            "--policy".to_owned(),
            policy.to_arg(),
            "--".to_owned(),
            program.clone(),
        ];
        helper_args.extend(args.iter().cloned());
        Ok(HelperCommand {
            program: self.helper.clone(),
            args: helper_args,
            env,
            policy,
        })
    }
}

fn path_string(path: &Path) -> anyhow::Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("{} is not valid UTF-8", path.display()))
}

/// The helper itself: never returns but with an error, which the binary prints before exiting
/// with 127, and which reaches Fronde through the agent's standard error.
pub fn run_helper(
    policy: &Policy,
    program: &OsStr,
    args: &[OsString],
) -> anyhow::Result<Infallible> {
    use std::os::unix::process::CommandExt;
    let mut command = command_for(policy, program, args);
    yield_to_fronde();
    let error = command.exec();
    Err(anyhow::Error::new(error).context(format!(
        "cannot start the agent {}",
        Path::new(program).display()
    )))
}

/// The agent's command, with only the variables the policy keeps.
fn command_for(policy: &Policy, program: &OsStr, args: &[OsString]) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    command.args(args).env_clear().current_dir(&policy.cwd);
    for (name, value) in std::env::vars_os() {
        if name.to_str().is_some_and(|name| policy.keeps(name)) {
            command.env(name, value);
        }
    }
    command
}

/// The kernel kills the agent first when memory runs out, and Fronde comes first for the CPU.
/// Neither needs a privilege; neither is worth stopping the agent for.
fn yield_to_fronde() {
    #[cfg(target_os = "linux")]
    {
        if let Err(error) = std::fs::write("/proc/self/oom_score_adj", "1000") {
            eprintln!("fronde __confine: cannot raise the agent's OOM score: {error}");
        }
        if let Err(error) = rustix::process::setpriority_process(None, 10) {
            eprintln!("fronde __confine: cannot lower the agent's priority: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::SecretString;

    fn spec(env: Vec<(String, String)>) -> AgentSpec {
        AgentSpec {
            command: vec!["claude-agent-acp".into(), "--flag".into()],
            env,
            cwd: PathBuf::from("/data/agent"),
            hub_url: "http://127.0.0.1:8484/mcp".into(),
            hub_token: SecretString::from("token-value-never-on-the-command-line"),
            system_prompt: String::new(),
            builtin_tools: Vec::new(),
        }
    }

    fn confine() -> Confine {
        Confine {
            helper: PathBuf::from("/usr/bin/fronde"),
            home: PathBuf::from("/data/home"),
            policy: Policy {
                cwd: PathBuf::new(),
                keep_env: KEEP_ENV.map(str::to_owned).to_vec(),
                drop_env: vec!["SERVER_TOKEN".into(), "SHARED".into()],
            },
        }
    }

    #[test]
    fn policy_holds_no_values() {
        let value = "a-value-only-the-environment-holds";
        let spec = spec(vec![
            ("FOO".into(), value.into()),
            ("CLAUDE_CODE_OAUTH_TOKEN".into(), value.into()),
        ]);
        let command = confine().command(&spec).unwrap();
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
    fn helper_command_runs_the_agent_through_the_helper() {
        let command = confine()
            .command(&spec(vec![("SHARED".into(), "set".into())]))
            .unwrap();
        assert_eq!(command.program, PathBuf::from("/usr/bin/fronde"));
        let args: Vec<&str> = command.args.iter().map(String::as_str).collect();
        assert_eq!(args[0], SUBCOMMAND);
        assert_eq!(args[1], "--policy");
        assert_eq!(&args[3..], ["--", "claude-agent-acp", "--flag"]);
        let policy = Policy::from_arg(args[2]).unwrap();
        assert_eq!(policy.cwd, PathBuf::from("/data/agent"));
        // `[agent] env` sets SHARED itself, so its value is the configuration's, and kept.
        assert_eq!(policy.drop_env, ["SERVER_TOKEN"]);
        assert!(policy.keeps("SHARED"));
        let env: Vec<(&str, &str)> = command
            .env
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        assert!(env.contains(&("HOME", "/data/home")));
        assert!(env.contains(&("TMPDIR", "/data/home/tmp")));
    }

    #[test]
    fn keep_env_is_an_allow_list() {
        let policy = confine().policy;
        for kept in [
            "PATH",
            "LANG",
            "LC_ALL",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
        ] {
            assert!(policy.keeps(kept), "{kept}");
        }
        for dropped in [
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
        policy.drop_env.push("CLAUDE_FRONDE_TOKEN".into());
        assert!(!policy.keeps("CLAUDE_FRONDE_TOKEN"));
        // A lone `*` keeps nothing.
        policy.keep_env = vec!["*".into()];
        assert!(!policy.keeps("PATH"));
    }

    #[test]
    fn policy_refuses_unknown_fields() {
        assert!(Policy::from_arg(r#"{"cwd":"/","keep_env":[],"drop_env":[],"x":1}"#).is_err());
    }
}
