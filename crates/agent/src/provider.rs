//! What sets one agent provider apart from another over ACP, as data: how the session's MCP header
//! carries the hub's token, what the session's `_meta` asks of the adapter, the mode a session
//! is put in once open, the variable its sign-in token goes in, which permission options a card
//! may offer, where its program is installed for the sandbox to grant, and where its agent says
//! how much of the subscription is used.
//!
//! Claude ([`Provider::CLAUDE`], through `@agentclientprotocol/claude-agent-acp`) is the only
//! provider so far: `[agent]` holds its settings. Another one gets a profile of its own, started
//! from [`Provider::bare`], which grants nothing lasting and asks nothing the adapter may not
//! understand; what Codex and Antigravity would need is in the user guide
//! (`explanation/subscriptions.md`).

use crate::usage::UsageSource;
use serde_json::json;

/// Claude Code's own tool that asks the user questions: the adapter turns it into a form
/// elicitation, which becomes a card. Enabled, never allowed up front, so that every use goes
/// through the adapter's permission check, which is where it asks.
pub const ASK_USER_QUESTION: &str = "AskUserQuestion";

/// One provider's profile. Data, not a trait: a provider differs from another in what it is told
/// and what Fronde accepts from it, not in how the ACP client works.
#[derive(Debug, PartialEq, Eq)]
pub struct Provider {
    /// As `threads.provider` stores it, and as `threads_start` will name it.
    pub id: &'static str,
    /// How the session's MCP header towards the hub carries the token.
    pub hub_header: HubHeader,
    /// The session's `_meta` on `session/new` and `session/resume`, from the built-in tools the
    /// agent gets besides the hub's.
    pub session_meta: SessionMeta,
    /// The mode a session is put in once opened or resumed, through `session/set_mode`.
    pub mode_after_open: Option<&'static str>,
    /// The variable the provider reads its sign-in token from: where `[agent] oauth_token` goes.
    pub sign_in_env: Option<&'static str>,
    /// Which permission options a card may offer.
    pub permissions: PermissionRules,
    /// What the sandbox grants for the provider's program to run.
    pub install: Install,
    /// Where its agent says how much of the subscription's windows is used.
    pub usage: UsageSource,
}

/// How a spawned agent is told the hub's token in its session's MCP header. The token itself
/// reaches the agent only through its environment (`FRONDE_HUB_TOKEN`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HubHeader {
    /// `Bearer ${VAR}`, which the agent expands from its environment: Claude Code does, and the
    /// Claude adapter puts the header on Claude Code's command line, which every user can read.
    Expanded,
}

impl HubHeader {
    /// The `Authorization` header's value for an agent whose environment holds the token in
    /// `var`.
    pub fn value(self, var: &str) -> String {
        match self {
            Self::Expanded => format!("Bearer ${{{var}}}"),
        }
    }
}

/// What the session's `_meta` holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionMeta {
    /// Nothing: the adapter gets no `_meta`.
    None,
    /// The Claude adapter's options: see [`claude_session_meta`].
    Claude,
}

impl SessionMeta {
    /// The `_meta` for a session whose agent gets `builtin_tools` besides the hub's.
    pub fn build(
        self,
        builtin_tools: &[String],
    ) -> Option<serde_json::Map<String, serde_json::Value>> {
        match self {
            Self::None => None,
            Self::Claude => Some(claude_session_meta(builtin_tools)),
        }
    }
}

/// Which permission options a card may offer: one-time options always (but on a mode switch),
/// lasting ones only those `lasting` knows, and on a tool call that switches the session's mode
/// only `mode_switch_options`. A provider whose options Fronde has not read gets
/// [`PermissionRules::NONE`]: no lasting option, and no option at all on a mode switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PermissionRules {
    /// The prefix of option ids that switch the session's mode, caught even when the request
    /// does not say its tool's kind.
    pub mode_switch_prefix: Option<&'static str>,
    /// The only options offered on a tool call that switches the session's mode.
    pub mode_switch_options: &'static [&'static str],
    /// The lasting options Fronde knows the reach of.
    pub lasting: Lasting,
}

impl PermissionRules {
    /// Nothing lasting, nothing on a mode switch.
    pub const NONE: Self = Self {
        mode_switch_prefix: None,
        mode_switch_options: &[],
        lasting: Lasting::None,
    };

    /// Whether an option id names a mode switch.
    pub fn switches_mode(&self, id: &str) -> bool {
        self.mode_switch_prefix
            .is_some_and(|prefix| id.starts_with(prefix))
    }
}

/// Which lasting options only add an allow rule whose reach Fronde knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lasting {
    /// None: every lasting option is left out.
    None,
    /// The Claude adapter's: see [`claude_lasting`].
    Claude,
}

impl Lasting {
    /// Whether the lasting option `id`, on the tool named `tool`, may be offered.
    pub fn allows(self, tool: Option<&str>, id: &str) -> bool {
        match self {
            Self::None => false,
            Self::Claude => claude_lasting(tool, id),
        }
    }
}

/// What the sandbox grants for the provider's program to run, besides the `PATH`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Install {
    /// The topmost `node_modules` above the program's canonical path, so that an npm adapter
    /// can load the CLI it drives; else the program's directory.
    NodeModules,
    /// The program's directory: a native binary.
    Directory,
}

impl Provider {
    /// Claude Code through `@agentclientprotocol/claude-agent-acp` (0.107.0, locked in
    /// `agent/package-lock.json`).
    pub const CLAUDE: Self = Self {
        id: "claude",
        hub_header: HubHeader::Expanded,
        session_meta: SessionMeta::Claude,
        // The Claude Code adapter starts sessions in `auto`; `default` asks before acting.
        mode_after_open: Some("default"),
        sign_in_env: Some("CLAUDE_CODE_OAUTH_TOKEN"),
        permissions: PermissionRules {
            mode_switch_prefix: Some(CLAUDE_EXIT_PLAN),
            mode_switch_options: CLAUDE_MODE_SWITCH_OPTIONS,
            lasting: Lasting::Claude,
        },
        install: Install::NodeModules,
        usage: UsageSource::ClaudeRateLimitMeta,
    };

    /// A provider Fronde knows nothing of yet, the start of a new profile: no `_meta`, no mode
    /// set, no sign-in variable, nothing lasting, nothing on a mode switch, a native binary, no
    /// usage reading.
    pub const fn bare(id: &'static str) -> Self {
        Self {
            id,
            hub_header: HubHeader::Expanded,
            session_meta: SessionMeta::None,
            mode_after_open: None,
            sign_in_env: None,
            permissions: PermissionRules::NONE,
            install: Install::Directory,
            usage: UsageSource::None,
        }
    }

    /// The providers configured: `[agent]` is Claude's.
    pub const ALL: [&'static Provider; 1] = [&Self::CLAUDE];

    /// The configured provider named `id`.
    pub fn named(id: &str) -> Option<&'static Provider> {
        Self::ALL.into_iter().find(|provider| provider.id == id)
    }
}

/// The prefix of the Claude adapter's options for leaving plan mode, which name the mode the
/// session switches to.
const CLAUDE_EXIT_PLAN: &str = "exit-plan-";

/// The only options a card offers on a tool call that switches the session's mode: the Claude
/// adapter's "leave plan mode and keep asking" and "no". Any other may switch to a mode that asks
/// less (auto mode, accepting every edit, bypassing permissions), which must never be one tap
/// away.
const CLAUDE_MODE_SWITCH_OPTIONS: &[&str] = &["exit-plan-default", "reject"];

/// Whether a lasting option of the Claude ACP adapter (0.107.0, `permissions/options/`) only
/// adds an allow rule for the tool at hand: `allow-with-updates` on Read, Glob and Grep (reading
/// during the session), Bash and PowerShell (a command prefix and the directories it names),
/// WebFetch (a domain), SandboxNetworkAccess (a host) and an MCP tool (that tool), and the Skill
/// options. Never on Edit, Write or NotebookEdit, where the same id may also switch the session
/// to accepting every edit, nor anywhere Fronde does not know what it grants.
fn claude_lasting(tool: Option<&str>, id: &str) -> bool {
    match id {
        "allow-with-updates" => tool.is_some_and(|tool| {
            matches!(
                tool,
                "Read"
                    | "Glob"
                    | "Grep"
                    | "Bash"
                    | "PowerShell"
                    | "WebFetch"
                    | "SandboxNetworkAccess"
            ) || tool.starts_with("mcp__")
        }),
        "allow-skill-exact" | "allow-skill-prefix" => tool == Some("Skill"),
        _ => false,
    }
}

/// Fronde's agent gets the hub's tools and the listed built-in ones, all allowed up front, and
/// [`ASK_USER_QUESTION`]. The Claude Code adapter loads the user's, project's and local settings
/// unless told otherwise, and with them the owner's plugins, MCP servers, hooks and allow rules:
/// none of them apply. The sign-in is not a setting and still works.
fn claude_session_meta(builtin_tools: &[String]) -> serde_json::Map<String, serde_json::Value> {
    let allowed: Vec<&str> = std::iter::once("mcp__fronde")
        .chain(builtin_tools.iter().map(String::as_str))
        .filter(|tool| *tool != ASK_USER_QUESTION)
        .collect();
    let tools: Vec<&str> = builtin_tools
        .iter()
        .map(String::as_str)
        .filter(|tool| *tool != ASK_USER_QUESTION)
        .chain([ASK_USER_QUESTION])
        .collect();
    // The adapter prefers `tools` over `disableBuiltInTools`; the flag stays so that an adapter
    // ignoring `tools` falls back to no built-in tool rather than all of them.
    let serde_json::Value::Object(meta) = json!({
        "disableBuiltInTools": true,
        "claudeCode": {"options": {
            "tools": tools,
            "allowedTools": allowed,
            "settingSources": [],
            "strictMcpConfig": true,
            "allowDangerouslySkipPermissions": false,
        }},
    }) else {
        unreachable!("the literal is an object")
    };
    meta
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// The `_meta` Claude's sessions got before the profile, byte for byte as sent (serde_json
    /// keeps the keys' order here).
    #[test]
    fn claude_session_meta_is_unchanged() {
        let meta = Provider::CLAUDE
            .session_meta
            .build(&tools(&["WebSearch", "WebFetch"]))
            .unwrap();
        assert_eq!(
            serde_json::to_string(&meta).unwrap(),
            r#"{"disableBuiltInTools":true,"claudeCode":{"options":{"tools":["WebSearch","WebFetch","AskUserQuestion"],"allowedTools":["mcp__fronde","WebSearch","WebFetch"],"settingSources":[],"strictMcpConfig":true,"allowDangerouslySkipPermissions":false}}}"#
        );
        let meta = Provider::CLAUDE.session_meta.build(&[]).unwrap();
        assert_eq!(
            serde_json::to_string(&meta).unwrap(),
            r#"{"disableBuiltInTools":true,"claudeCode":{"options":{"tools":["AskUserQuestion"],"allowedTools":["mcp__fronde"],"settingSources":[],"strictMcpConfig":true,"allowDangerouslySkipPermissions":false}}}"#
        );
        // AskUserQuestion listed by the owner is still never allowed up front.
        let meta = Provider::CLAUDE
            .session_meta
            .build(&tools(&["AskUserQuestion", "WebSearch"]))
            .unwrap();
        assert_eq!(
            serde_json::Value::Object(meta),
            json!({
                "disableBuiltInTools": true,
                "claudeCode": {"options": {
                    "tools": ["WebSearch", "AskUserQuestion"],
                    "allowedTools": ["mcp__fronde", "WebSearch"],
                    "settingSources": [],
                    "strictMcpConfig": true,
                    "allowDangerouslySkipPermissions": false,
                }},
            })
        );
    }

    /// What `acp.rs`, `permission.rs` and `confine.rs` hard-coded for Claude before the profile.
    #[test]
    fn claude_profile_holds_what_was_hard_coded() {
        let claude = &Provider::CLAUDE;
        assert_eq!(claude.id, "claude");
        assert_eq!(
            claude.hub_header.value("FRONDE_HUB_TOKEN"),
            "Bearer ${FRONDE_HUB_TOKEN}"
        );
        assert_eq!(claude.mode_after_open, Some("default"));
        assert_eq!(claude.sign_in_env, Some("CLAUDE_CODE_OAUTH_TOKEN"));
        assert_eq!(claude.install, Install::NodeModules);
        assert_eq!(claude.usage, UsageSource::ClaudeRateLimitMeta);
        let rules = claude.permissions;
        assert_eq!(rules.mode_switch_prefix, Some("exit-plan-"));
        assert_eq!(rules.mode_switch_options, ["exit-plan-default", "reject"]);
        assert!(rules.switches_mode("exit-plan-auto"));
        assert!(!rules.switches_mode("allow-once"));
        // The allowlist of lasting options, as B4 left it.
        assert_eq!(rules.lasting, Lasting::Claude);
        let lasting = |tool, id| rules.lasting.allows(tool, id);
        for tool in [
            "Read",
            "Glob",
            "Grep",
            "Bash",
            "PowerShell",
            "WebFetch",
            "SandboxNetworkAccess",
            "mcp__calendar__list",
        ] {
            assert!(lasting(Some(tool), "allow-with-updates"), "{tool}");
        }
        for tool in [
            "Edit",
            "Write",
            "NotebookEdit",
            "WebSearch",
            "Agent",
            "Skill",
        ] {
            assert!(!lasting(Some(tool), "allow-with-updates"), "{tool}");
        }
        assert!(!lasting(None, "allow-with-updates"));
        assert!(lasting(Some("Skill"), "allow-skill-exact"));
        assert!(lasting(Some("Skill"), "allow-skill-prefix"));
        assert!(!lasting(Some("Bash"), "allow-skill-exact"));
        assert!(!lasting(Some("Bash"), "allow-always"));
        assert!(!lasting(Some("Bash"), "always"));
    }

    /// A provider whose options Fronde has not read gets nothing lasting and nothing on a mode
    /// switch, whatever ids it uses, Claude's included.
    #[test]
    fn an_unknown_provider_has_empty_allowlists() {
        let codex = Provider::bare("codex");
        assert_eq!(codex.permissions, PermissionRules::NONE);
        assert!(codex.permissions.mode_switch_options.is_empty());
        assert!(!codex.permissions.switches_mode("exit-plan-default"));
        for (tool, id) in [
            (Some("Read"), "allow-with-updates"),
            (Some("mcp__fronde__memory_search"), "allow-with-updates"),
            (Some("Skill"), "allow-skill-exact"),
            (None, "allow-always"),
        ] {
            assert!(!codex.permissions.lasting.allows(tool, id), "{tool:?} {id}");
        }
        assert_eq!(codex.session_meta.build(&tools(&["WebSearch"])), None);
        assert_eq!(codex.mode_after_open, None);
        assert_eq!(codex.sign_in_env, None);
        assert_eq!(codex.install, Install::Directory);
        assert_eq!(codex.usage, UsageSource::None);
    }

    #[test]
    fn only_claude_is_configured() {
        assert_eq!(Provider::named("claude"), Some(&Provider::CLAUDE));
        for id in ["codex", "gemini", "antigravity", "", "Claude"] {
            assert_eq!(Provider::named(id), None, "{id}");
        }
    }
}
