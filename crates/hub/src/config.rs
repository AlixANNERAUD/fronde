//! Configuration: one strict TOML file, secrets referenced by file or environment variable.

use crate::origin::{Locality, Origin};
use anyhow::{Context, bail};
use secrecy::SecretString;
use secrecy::zeroize::Zeroize;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use url::Url;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub data: DataConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    /// Where tools come from besides the hub's own: upstream MCP servers and components.
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
    /// The chat page; without it the server is only the hub.
    pub web: Option<WebConfig>,
    #[serde(default)]
    pub agent: AgentConfig,
    /// How many agents run at once, and how long an idle one is kept.
    #[serde(default)]
    pub threads: ThreadsConfig,
    /// The `code_run_python` tool; without it there is none.
    pub python: Option<PythonConfig>,
    /// Where the master key of the stored secrets is kept.
    #[serde(default)]
    pub secrets: SecretsConfig,
    /// The people who sign in, by name. Left out, the legacy form stands for one: see
    /// [`Config::users`].
    #[serde(default)]
    pub users: BTreeMap<String, UserConfig>,
    /// The provider accounts agents sign in with, by name. Left out, the legacy form stands for
    /// one account: see [`Config::accounts`].
    #[serde(default)]
    pub accounts: BTreeMap<String, AccountConfig>,
    /// The file this configuration was read from, absolute; [`load`] sets it.
    #[serde(skip)]
    path: Option<PathBuf>,
    /// The sign-in variables ([`sign_in_vars`]) Fronde's environment held when [`load`] read
    /// the file: the legacy form's owner account reads `CLAUDE_CODE_OAUTH_TOKEN`, and with
    /// `[accounts]` each must be an account's (see [`Config::check_inherited`]).
    #[serde(skip)]
    inherited: Vec<String>,
}

impl Config {
    /// The file this configuration was read from, when [`load`] read it.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Every `{ env = "…" }` secret's variable: Fronde reads them from its own environment, and
    /// the agent's never holds them.
    pub fn secret_env_names(&self) -> Vec<String> {
        self.secrets()
            .filter_map(|secret| match secret {
                Secret::Env { env } => Some(env.clone()),
                _ => None,
            })
            .collect()
    }

    /// Every `{ file = "…" }` secret's file, the master key file included.
    pub fn secret_files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = self
            .secrets()
            .filter_map(|secret| match secret {
                Secret::File { file } => Some(file.clone()),
                _ => None,
            })
            .collect();
        files.extend(self.secrets.master_key_file.clone());
        files
    }

    fn secrets(&self) -> impl Iterator<Item = &Secret> {
        let sources = self.sources.iter().flat_map(|source| -> Vec<&Secret> {
            match source {
                SourceConfig::Mcp(m) => m.token.iter().collect(),
                SourceConfig::Component(c) => c
                    .credentials
                    .iter()
                    .flat_map(|credential| {
                        credential
                            .basic
                            .as_ref()
                            .map(|b| &b.password)
                            .into_iter()
                            .chain(credential.bearer.as_ref())
                    })
                    .collect(),
            }
        });
        // The legacy form's inherited variable is not listed: the file does not name it, and the
        // agent's environment keeps it from Fronde's.
        let accounts = self
            .accounts
            .values()
            .filter_map(|account| account.oauth_token.as_ref());
        let users = self
            .users
            .values()
            .filter_map(|user| user.password.as_ref());
        std::iter::once(&self.server.token)
            .chain(self.web.as_ref().and_then(|web| web.password.as_ref()))
            .chain(users)
            .chain(self.secrets.master_key.as_ref())
            .chain(self.agent.oauth_token.as_ref())
            .chain(accounts)
            .chain(sources)
    }

    /// The people who sign in: the `[users]` declared, or else the one the legacy form stands
    /// for, `[data] user`, an admin signing in with `[web] password`. The file is never
    /// rewritten.
    pub fn users(&self) -> fronde_requests::Users {
        if self.users.is_empty() {
            return [fronde_requests::User {
                name: self.data.user.clone(),
                admin: true,
            }]
            .into_iter()
            .collect();
        }
        self.users
            .iter()
            .map(|(name, user)| fronde_requests::User {
                name: name.clone(),
                admin: user.admin,
            })
            .collect()
    }

    /// The password the user `name` signs in with, and the key that names it (`web.password` in
    /// the legacy form, `[users.<name>] password` otherwise).
    pub fn password_of(&self, name: &str) -> Option<(String, &Secret)> {
        if self.users.is_empty() {
            return (name == self.data.user)
                .then(|| self.web.as_ref()?.password.as_ref())
                .flatten()
                .map(|password| ("web.password".to_owned(), password));
        }
        let password = self.users.get(name)?.password.as_ref()?;
        Some((format!("[users.{}] password", table_key(name)), password))
    }

    /// The names of the people who sign in.
    pub fn user_names(&self) -> Vec<String> {
        self.users().iter().map(|user| user.name.clone()).collect()
    }

    /// The provider accounts agents sign in with: the `[accounts]` declared, or else the one
    /// account the legacy form stands for, named [`LEGACY_ACCOUNT`], on Claude, owned by
    /// `[data] user` and reading `[agent] oauth_token`, else `CLAUDE_CODE_OAUTH_TOKEN` when
    /// Fronde's environment held it at load, else no token (the agent starts unsigned). Only
    /// that implicit account may lack a token. The file is never rewritten.
    pub fn accounts(&self) -> Vec<Account> {
        if !self.accounts.is_empty() {
            return self
                .accounts
                .iter()
                .map(|(name, account)| Account {
                    name: name.clone(),
                    provider: account.provider.clone(),
                    owner: account.owner.clone(),
                    users: account.users.clone(),
                    default_for: account.default_for.clone(),
                    token: account.oauth_token.clone(),
                    form: AccountForm::Declared,
                })
                .collect();
        }
        let (token, form) = match &self.agent.oauth_token {
            Some(token) => (Some(token.clone()), AccountForm::AgentToken),
            None if self.inherited.iter().any(|name| name == INHERITED_SIGN_IN) => (
                Some(Secret::Env {
                    env: INHERITED_SIGN_IN.to_owned(),
                }),
                AccountForm::Inherited,
            ),
            None => (None, AccountForm::Unsigned),
        };
        vec![Account {
            name: LEGACY_ACCOUNT.to_owned(),
            provider: "claude".to_owned(),
            owner: self.data.user.clone(),
            users: Vec::new(),
            default_for: Vec::new(),
            token,
            form,
        }]
    }

    /// Whether `user` may run threads on the account named `account`: they own it or its
    /// `users` lists them.
    pub fn may_use(&self, user: &str, account: &str) -> bool {
        self.accounts()
            .iter()
            .any(|found| found.name == account && found.usable_by(user))
    }

    /// `user`'s default account on `provider`: the one whose `default_for` names them, else the
    /// only one of that provider they may use, else none (the load rules leave no other case).
    pub fn default_account(&self, user: &str, provider: &str) -> Option<Account> {
        let mut usable: Vec<Account> = self
            .accounts()
            .into_iter()
            .filter(|account| account.provider == provider && account.usable_by(user))
            .collect();
        if let Some(named) = usable
            .iter()
            .position(|account| account.default_for.iter().any(|name| name == user))
        {
            return Some(usable.swap_remove(named));
        }
        (usable.len() == 1).then(|| usable.remove(0))
    }

    /// With `[accounts]`, refuses a sign-in variable in Fronde's environment that no account
    /// names: each agent signs in with its own account's token and nothing else, so a token
    /// Fronde inherited would belong to nobody. Says which variable, never its value. `serve`
    /// and `fronde check` call it.
    pub fn check_inherited(&self) -> anyhow::Result<()> {
        if self.accounts.is_empty() {
            return Ok(());
        }
        for name in &self.inherited {
            let named = self.accounts.values().any(
                |account| matches!(&account.oauth_token, Some(Secret::Env { env }) if env == name),
            );
            if !named {
                bail!(
                    "Fronde's environment holds {name}, which signs an agent in, and no \
                     [accounts.<name>] names it: unset it before starting Fronde, or name it as \
                     its account's oauth_token = {{ env = \"{name}\" }}"
                );
            }
        }
        Ok(())
    }

    /// The master key file, which [`load`] always sets: `[secrets] master_key`, or `master.key`
    /// beside the configuration file, out of the data directory, so that a backup of the data
    /// directory does not hold the key with the secrets it opens.
    /// A configuration parsed without [`load`] has only the file it names, as written.
    pub fn master_key_file(&self) -> anyhow::Result<&Path> {
        match (&self.secrets.master_key_file, &self.secrets.master_key) {
            (Some(file), _) | (None, Some(Secret::File { file })) => Ok(file),
            _ => bail!(
                "no master key file: this configuration was not read by config::load; set \
                 [secrets] master_key = {{ file = \"…\" }}"
            ),
        }
    }

    /// Where the master key may be kept, as `[secrets]` says: the backend, the key file, the
    /// service's systemd credentials and the platform's keychain for this data directory.
    pub fn master_key_backends(&self) -> anyhow::Result<fronde_db::master_key::Backends> {
        use fronde_db::master_key::{Backend, Backends, Choice, Keychain};
        let choice = match self.secrets.backend {
            KeyBackend::Auto => Choice::Auto,
            KeyBackend::Systemd => Choice::Only(Backend::Systemd),
            KeyBackend::Keychain => Choice::Only(Backend::Keychain),
            KeyBackend::File => Choice::Only(Backend::File),
        };
        let keychain = match &self.secrets.keychain {
            Some(keychain) => keychain.clone(),
            None => Keychain::platform(&self.data.dir),
        };
        Ok(Backends::new(
            choice,
            self.master_key_file()?.to_path_buf(),
            self.secrets.credentials_dir.clone(),
            keychain,
        ))
    }
}

/// Test hooks, behind the `testing` feature that only dev-dependencies enable.
#[cfg(feature = "testing")]
impl Config {
    /// Read the legacy form as if Fronde's environment held `CLAUDE_CODE_OAUTH_TOKEN`, or not.
    pub fn set_inherited_sign_in(&mut self, inherited: bool) {
        self.inherited.retain(|name| name != INHERITED_SIGN_IN);
        if inherited {
            self.inherited.push(INHERITED_SIGN_IN.to_owned());
        }
    }
}

/// `[users.<name>]`: someone who signs in to Fronde.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
    /// Sees and manages everything; `[data] user`, the owner, is one.
    #[serde(default)]
    pub admin: bool,
    /// Signs in with this password: `{ file }` or `{ env }`. One sign-in method is required,
    /// and this is the only one so far; OIDC is planned as another, beside it.
    pub password: Option<Secret>,
}

/// The longest user name, in characters.
pub const MAX_USER_NAME: usize = 64;

/// `name` as a key of a TOML table: bare when it can be, quoted as TOML quotes it otherwise.
fn table_key(name: &str) -> String {
    let bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        name.to_owned()
    } else {
        toml::Value::String(name.to_owned()).to_string()
    }
}

/// Whether two names as [`look`] reads them are the same under full Unicode case folding.
fn looks_alike(a: &str, b: &str) -> bool {
    unicase::UniCase::unicode(a) == unicase::UniCase::unicode(b)
}

/// The characters a user's name may not hold: it names a session, and may name a directory.
const FORBIDDEN_IN_NAMES: [char; 3] = ['/', '\\', ':'];

/// Checks a user's name, `key` naming it: 1 to [`MAX_USER_NAME`] characters, no hidden
/// character, no space at either end, no `/`, `\`, `:` or `..`.
fn check_user_name(name: &str, key: &str) -> anyhow::Result<()> {
    let length = name.chars().count();
    if !(1..=MAX_USER_NAME).contains(&length) {
        bail!("{key}: a user's name is 1 to {MAX_USER_NAME} characters");
    }
    if let Some(hidden) = fronde_protocol::text::first_hidden(name, false) {
        bail!(
            "{key}: a user's name may not hold a hidden character ({hidden:?}); write it with \
             visible characters only"
        );
    }
    if name.trim() != name {
        bail!("{key}: a user's name may not start or end with a space");
    }
    if let Some(c) = name.chars().find(|c| FORBIDDEN_IN_NAMES.contains(c)) {
        bail!("{key}: a user's name may not hold {c:?}");
    }
    if name.contains("..") {
        bail!("{key}: a user's name may not hold \"..\"");
    }
    Ok(())
}

/// A name as two names are compared for looking alike: NFKC, lower case, then its UTS #39
/// confusable skeleton (a Cyrillic `а` reads as a Latin `a`), NFKC and lower case again; two
/// of these are then compared with full Unicode case folding ([`looks_alike`]).
fn look(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let folded: String = name.nfkc().flat_map(char::to_lowercase).collect();
    unicode_security::confusable_detection::skeleton(&folded)
        .collect::<String>()
        .nfkc()
        .flat_map(char::to_lowercase)
        .collect()
}

/// `[accounts.<name>]`: a subscription of one provider, its owner's, which the people it lists
/// may use too.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    /// One of [`PROVIDERS`].
    pub provider: String,
    /// The subscriber: whose subscription it is, whose `[threads] reserve` applies on it.
    pub owner: String,
    /// The other people who may run threads on it.
    #[serde(default)]
    pub users: Vec<String>,
    /// The people whose default account it is on its provider.
    #[serde(default)]
    pub default_for: Vec<String>,
    /// The token `claude setup-token` printed; a stored one is stored for [`CLAUDE_ORIGIN`].
    /// Required: an `Option` only so that its absence is refused with how to fix it.
    pub oauth_token: Option<Secret>,
}

/// The name of the one account the legacy form stands for.
pub const LEGACY_ACCOUNT: &str = "claude";

/// The variable the legacy form's account reads when Fronde inherits it (the Docker how-to).
pub const INHERITED_SIGN_IN: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// A provider account, as [`Config::accounts`] reads it.
#[derive(Debug, Clone)]
pub struct Account {
    pub name: String,
    pub provider: String,
    /// The subscriber, the user it belongs to.
    pub owner: String,
    /// The other people who may use it; none for the legacy form.
    pub users: Vec<String>,
    /// The people whose default account it is on its provider; none for the legacy form.
    pub default_for: Vec<String>,
    /// Where its token comes from; `None` only for the legacy form without a token.
    pub token: Option<Secret>,
    /// Which part of the file it was read from.
    pub form: AccountForm,
}

impl Account {
    /// Whether `user` may run threads on it: they own it or `users` lists them.
    pub fn usable_by(&self, user: &str) -> bool {
        self.owner == user || self.users.iter().any(|name| name == user)
    }
}

/// Where an [`Account`] was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountForm {
    /// An `[accounts.<name>]` section.
    Declared,
    /// The legacy form's `[agent] oauth_token`.
    AgentToken,
    /// The legacy form, with `CLAUDE_CODE_OAUTH_TOKEN` in Fronde's environment.
    Inherited,
    /// The legacy form without a token: the agent starts unsigned.
    Unsigned,
}

/// The variables a provider's agent signs in with, which `[agent] env` never sets: only an
/// account's token signs an agent in. `fronde_agent::provider::Provider::sign_in_vars` lists the
/// same, which a test of the server pins.
pub fn sign_in_vars(provider: &str) -> &'static [&'static str] {
    match provider {
        "claude" => &[
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "AWS_BEARER_TOKEN_BEDROCK",
        ],
        _ => &[],
    }
}

/// The prefixes of the variables that choose where and how a provider's agent signs in (its
/// API's address, another cloud): `[agent] env` sets none, and an agent inherits none.
/// `fronde_agent::provider::Provider::sign_in_prefixes` lists the same.
pub fn sign_in_prefixes(provider: &str) -> &'static [&'static str] {
    match provider {
        "claude" => &[
            "ANTHROPIC_",
            "CLAUDE_CODE_USE_",
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_CODE_OAUTH_",
            "CLAUDE_CODE_API_KEY_",
            "CLAUDE_CODE_CLIENT_",
        ],
        _ => &[],
    }
}

/// The provider whose agent's sign-in the variable `name` is, or decides: see
/// [`sign_in_vars`] and [`sign_in_prefixes`].
pub fn signs_in(name: &str) -> Option<&'static str> {
    PROVIDERS.into_iter().find(|provider| {
        sign_in_vars(provider).contains(&name)
            || sign_in_prefixes(provider)
                .iter()
                .any(|prefix| name.starts_with(prefix))
    })
}

/// `[secrets]`: where the master key that seals stored secrets is kept.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretsConfig {
    /// Which backend holds the key; `auto` by default.
    #[serde(default)]
    pub backend: KeyBackend,
    /// The file backend's key file. `{ file = "…" }` only: never `{ env }`, which every process
    /// Fronde starts outside the agent's helper inherits, nor `{ stored }`.
    master_key: Option<Secret>,
    /// The key file, read from the configuration's directory when relative; [`load`] sets it.
    #[serde(skip)]
    master_key_file: Option<PathBuf>,
    /// `$CREDENTIALS_DIRECTORY`, where systemd puts the service's credentials; [`load`] reads it.
    #[serde(skip)]
    credentials_dir: Option<PathBuf>,
    /// The keychain to use instead of the platform's: tests set a fake one.
    #[serde(skip)]
    keychain: Option<fronde_db::master_key::Keychain>,
}

impl SecretsConfig {
    /// `$CREDENTIALS_DIRECTORY`, when systemd set it.
    pub fn credentials_dir(&self) -> Option<&Path> {
        self.credentials_dir.as_deref()
    }
}

/// Test hooks, behind the `testing` feature that only dev-dependencies enable.
#[cfg(feature = "testing")]
impl SecretsConfig {
    /// Read systemd credentials from `dir` instead of `$CREDENTIALS_DIRECTORY`.
    pub fn set_credentials_dir(&mut self, dir: Option<PathBuf>) {
        self.credentials_dir = dir;
    }

    /// Use `keychain` instead of the platform's.
    pub fn set_keychain(&mut self, keychain: fronde_db::master_key::Keychain) {
        self.keychain = Some(keychain);
    }
}

/// `[secrets] backend`: where the master key is kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyBackend {
    /// For the first key, a systemd credential if the service has one, else the keychain if it
    /// answers, else a file; then always the one that got it.
    #[default]
    Auto,
    Systemd,
    Keychain,
    File,
}

/// `[python]`: where the Python runtime is, as `mise run python` lays it out.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PythonConfig {
    /// Holds `python.wasm` and `lib/`; the image has it at `/app/python`.
    pub dir: PathBuf,
}

/// The name of the source that serves `code_run_python`.
pub const PYTHON_SOURCE: &str = "python";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    /// The legacy form's password, the owner's (`[data] user`): required without `[users]`,
    /// refused with it.
    pub password: Option<Secret>,
    /// Mark the session cookie `Secure`; turn on when the page is served over https.
    #[serde(default)]
    pub secure_cookie: bool,
}

/// The coding agent Fronde talks to over ACP.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgentConfig {
    pub command: Vec<String>,
    /// Variables the agent gets besides the few it keeps from Fronde's environment.
    pub env: BTreeMap<String, String>,
    /// Claude Code's own tools the agent gets besides the hub's; empty for none.
    pub builtin_tools: Vec<String>,
    /// Confine the agent with Landlock; it does not start where the kernel cannot. `false` runs
    /// it unconfined, with a warning at each start.
    pub sandbox: bool,
    /// More directories the confined agent may read and run programs from.
    pub read: Vec<PathBuf>,
    /// The token `claude setup-token` printed, given to the agent as `CLAUDE_CODE_OAUTH_TOKEN`
    /// and read again at each start. A stored one must be stored for [`CLAUDE_ORIGIN`].
    pub oauth_token: Option<Secret>,
    /// The caps of the agent's systemd scope, when a systemd user manager runs it.
    pub limits: AgentLimits,
}

/// `[agent.limits]`: what each agent's systemd scope caps, and the slice of all of them; `None`
/// (`"none"`) for no cap of Fronde's. Without a systemd user manager there is no scope, and these
/// do not apply.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgentLimits {
    /// Bytes of memory of each agent (`MemoryMax`; `MemoryHigh` is three quarters of it).
    #[serde(deserialize_with = "memory_limit")]
    pub memory: Option<u64>,
    /// Bytes of memory of every agent together: `MemoryMax` of `fronde-agents.slice`, with
    /// `MemoryHigh` at three quarters of it.
    #[serde(deserialize_with = "total_memory_limit")]
    pub total_memory: Option<u64>,
    /// CPUs' worth of time (`CPUQuota`, 100 % per CPU).
    #[serde(deserialize_with = "cpus_limit")]
    pub cpus: Option<f64>,
    /// Processes and threads (`TasksMax`).
    #[serde(deserialize_with = "tasks_limit")]
    pub tasks: Option<u64>,
}

impl Default for AgentLimits {
    fn default() -> Self {
        Self {
            memory: Some(2 << 30),
            total_memory: Some(4 << 30),
            cpus: Some(2.0),
            tasks: Some(512),
        }
    }
}

/// `[threads]`: how many agent processes run at once, how long an idle one is kept, and how far
/// sub-agents may go. The limits on sub-agents bound what an agent, a prompt-injected one
/// included, may spend of the subscription's quota in the background: `reserve` keeps a share of
/// each subscription window for the owner, read from the provider's usage readings, and the
/// daily counts apply only when no reading decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ThreadsConfig {
    /// Agent processes running at once, the main thread's included; at least 1. Threads the
    /// owner did not start may use all of them but one, kept for the owner's.
    #[serde(deserialize_with = "max_running")]
    pub max_running: usize,
    /// How long an agent waits for a message once its turn ended before it is stopped; its
    /// session resumes on the next message.
    #[serde(deserialize_with = "idle_stop")]
    pub idle_stop: std::time::Duration,
    /// How deep a thread may be and still start sub-agents: 0 lets no thread delegate, 1 lets
    /// the owner's threads delegate but not their sub-agents; at most [`MAX_DEPTH`].
    #[serde(deserialize_with = "max_depth")]
    pub max_depth: u32,
    /// Sub-agents of one thread in a turn at once, those waiting for an agent included.
    #[serde(deserialize_with = "children_running")]
    pub children_running: usize,
    /// Sub-agents started by all threads together in the last 24 hours, archived ones
    /// included.
    #[serde(deserialize_with = "children_per_day")]
    pub children_per_day: usize,
    /// Sub-agents one thread keeps not archived: a guard against clutter only.
    #[serde(deserialize_with = "children_kept")]
    pub children_kept: usize,
    /// Turns of sub-agents that threads start in 24 hours, all threads together: each
    /// `threads_start`'s first turn and each `threads_send`.
    #[serde(deserialize_with = "child_turns_per_day")]
    pub child_turns_per_day: usize,
    /// How long one turn of a sub-agent, or a turn a sub-agent's result woke, may run once its
    /// agent has it; it is then stopped. At most [`MAX_TURN_TIMEOUT`].
    #[serde(deserialize_with = "child_turn_timeout")]
    pub child_turn_timeout: std::time::Duration,
    /// Turns a thread may be woken by its sub-agents' results in a row, without a message of
    /// the owner's in between; later results wait for the owner's next message.
    #[serde(deserialize_with = "wakes_without_owner")]
    pub wakes_without_owner: usize,
    /// The share of each subscription window (5 hours, weekly) kept for the owner: once a
    /// window's use reaches `100% - reserve`, sub-agents are neither started nor sent messages,
    /// and their results wait for the owner's next message. `"0%"` turns it off.
    pub reserve: Percent,
}

/// A percentage from `"0%"` to `"100%"`, written with its sign: `"30%"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Percent(pub u8);

impl std::fmt::Display for Percent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}%", self.0)
    }
}

impl<'de> Deserialize<'de> for Percent {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let expected = || {
            serde::de::Error::custom(
                "reserve: expected a percentage from \"0%\" to \"100%\", such as \"30%\"",
            )
        };
        let text = String::deserialize(d).map_err(|_| expected())?;
        let digits = text
            .strip_suffix('%')
            .map(str::trim_end)
            .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .ok_or_else(expected)?;
        match digits.parse::<u8>() {
            Ok(value) if value <= 100 => Ok(Self(value)),
            _ => Err(expected()),
        }
    }
}

/// The longest `[threads] child_turn_timeout` allowed.
pub const MAX_TURN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// The greatest count a `[threads]` limit takes.
pub const MAX_COUNT: i64 = 1_000;

/// The deepest `[threads] max_depth` allowed.
pub const MAX_DEPTH: u32 = 3;

impl Default for ThreadsConfig {
    fn default() -> Self {
        Self {
            max_running: 3,
            idle_stop: std::time::Duration::from_secs(10 * 60),
            max_depth: 1,
            children_running: 3,
            children_per_day: 30,
            children_kept: 200,
            child_turns_per_day: 100,
            child_turn_timeout: std::time::Duration::from_secs(30 * 60),
            wakes_without_owner: 10,
            reserve: Percent(30),
        }
    }
}

fn max_running<'de, D: serde::Deserializer<'de>>(d: D) -> Result<usize, D::Error> {
    count(d, "max_running", "a number of agents")
}

fn children_running<'de, D: serde::Deserializer<'de>>(d: D) -> Result<usize, D::Error> {
    count(d, "children_running", "a number of sub-agents")
}

fn children_per_day<'de, D: serde::Deserializer<'de>>(d: D) -> Result<usize, D::Error> {
    count(d, "children_per_day", "a number of sub-agents")
}

fn children_kept<'de, D: serde::Deserializer<'de>>(d: D) -> Result<usize, D::Error> {
    count(d, "children_kept", "a number of sub-agents")
}

fn child_turns_per_day<'de, D: serde::Deserializer<'de>>(d: D) -> Result<usize, D::Error> {
    count(d, "child_turns_per_day", "a number of turns")
}

fn wakes_without_owner<'de, D: serde::Deserializer<'de>>(d: D) -> Result<usize, D::Error> {
    count(d, "wakes_without_owner", "a number of turns")
}

/// A whole number from 1 to [`MAX_COUNT`], for the key `key`.
fn count<'de, D: serde::Deserializer<'de>>(d: D, key: &str, what: &str) -> Result<usize, D::Error> {
    let expected = format!("{key}: expected {what} from 1 to {MAX_COUNT}");
    match RawLimit::deserialize(d)? {
        RawLimit::Integer(count) if (1..=MAX_COUNT).contains(&count) => {
            usize::try_from(count).map_err(|_| serde::de::Error::custom(expected))
        }
        _ => Err(serde::de::Error::custom(expected)),
    }
}

fn max_depth<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let expected = format!("max_depth: expected a depth from 0 to {MAX_DEPTH}");
    match RawLimit::deserialize(d)? {
        RawLimit::Integer(depth) if (0..=i64::from(MAX_DEPTH)).contains(&depth) => {
            u32::try_from(depth).map_err(|_| serde::de::Error::custom(expected))
        }
        _ => Err(serde::de::Error::custom(expected)),
    }
}

fn idle_stop<'de, D: serde::Deserializer<'de>>(d: D) -> Result<std::time::Duration, D::Error> {
    duration(d, "idle_stop")
}

fn child_turn_timeout<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<std::time::Duration, D::Error> {
    let timeout = duration(d, "child_turn_timeout")?;
    if timeout > MAX_TURN_TIMEOUT {
        return Err(serde::de::Error::custom(format!(
            "child_turn_timeout: at most {}, not {}",
            duration_text(MAX_TURN_TIMEOUT),
            duration_text(timeout)
        )));
    }
    Ok(timeout)
}

/// A duration as humans write it: `"10m"`, `"1h 30m"`, `"90s"`; above zero.
fn duration<'de, D: serde::Deserializer<'de>>(
    d: D,
    key: &str,
) -> Result<std::time::Duration, D::Error> {
    let expected = "expected a duration above zero such as \"10m\", \"1h\" or \"90s\"";
    let text = String::deserialize(d)
        .map_err(|_| serde::de::Error::custom(format!("{key}: {expected}")))?;
    let parsed = text
        .parse::<jiff::SignedDuration>()
        .ok()
        .filter(|duration| duration.is_positive())
        .and_then(|duration| std::time::Duration::try_from(duration).ok());
    match parsed {
        Some(duration) => Ok(duration),
        None => Err(serde::de::Error::custom(format!(
            "{key}: {expected}, not {text:?}"
        ))),
    }
}

/// A duration as `[threads] idle_stop` writes it: `10m`, `1h 30m`.
pub fn duration_text(duration: std::time::Duration) -> String {
    match jiff::SignedDuration::try_from(duration) {
        Ok(duration) => format!("{duration:#}"),
        Err(_) => format!("{duration:?}"),
    }
}

/// A limit as TOML writes it: a number, or a string (a size, or `"none"`).
#[derive(Deserialize)]
#[serde(untagged)]
enum RawLimit {
    Integer(i64),
    Float(f64),
    Text(String),
}

fn memory_limit<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    size_limit(d, "memory")
}

fn total_memory_limit<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    size_limit(d, "total_memory")
}

fn size_limit<'de, D: serde::Deserializer<'de>>(d: D, key: &str) -> Result<Option<u64>, D::Error> {
    let expected = format!("{key}: expected a size such as \"2G\" or \"512M\", or \"none\"");
    match RawLimit::deserialize(d)? {
        RawLimit::Text(text) if text == "none" => Ok(None),
        RawLimit::Text(text) => parse_size(&text)
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom(format!("{expected}, not {text:?}"))),
        RawLimit::Integer(bytes) if bytes > 0 => Ok(Some(bytes.unsigned_abs())),
        _ => Err(serde::de::Error::custom(expected)),
    }
}

/// A size in bytes, as systemd writes it: a number with an optional `K`, `M`, `G` or `T`
/// (powers of 1024); `None` unless it is above zero.
pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let (number, unit) = match text.char_indices().last()? {
        (at, c) if c.is_ascii_alphabetic() => (&text[..at], c.to_ascii_uppercase()),
        _ => (text, 'B'),
    };
    let shift = match unit {
        'B' => 0,
        'K' => 10,
        'M' => 20,
        'G' => 30,
        'T' => 40,
        _ => return None,
    };
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let bytes = number.parse::<f64>().ok()? * 2f64.powi(shift);
    // Below 2^64, so the cast is exact enough and never saturates.
    (1.0..1.8e19).contains(&bytes).then_some(bytes as u64)
}

fn cpus_limit<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    let expected = "cpus: expected a number of CPUs above 0, such as 2 or 0.5, or \"none\"";
    let cpus = match RawLimit::deserialize(d)? {
        RawLimit::Text(text) if text == "none" => return Ok(None),
        RawLimit::Integer(cpus) => cpus as f64,
        RawLimit::Float(cpus) => cpus,
        RawLimit::Text(_) => return Err(serde::de::Error::custom(expected)),
    };
    // systemd counts in percent: at least 1 % of one CPU.
    if (0.01..=100_000.0).contains(&cpus) {
        Ok(Some(cpus))
    } else {
        Err(serde::de::Error::custom(expected))
    }
}

fn tasks_limit<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    let expected = "tasks: expected a number of processes and threads above 0, or \"none\"";
    match RawLimit::deserialize(d)? {
        RawLimit::Text(text) if text == "none" => Ok(None),
        RawLimit::Integer(tasks) if tasks > 0 => Ok(Some(tasks.unsigned_abs())),
        _ => Err(serde::de::Error::custom(expected)),
    }
}

/// The origin a stored `[agent] oauth_token` is stored for: Claude's API, which it opens.
pub const CLAUDE_ORIGIN: &str = "https://api.anthropic.com";

/// The providers a thread may run on: `[agent]` holds Claude's settings, the only provider so
/// far. Another one comes with its own `[providers.<id>]` table.
pub const PROVIDERS: [&str; 1] = ["claude"];

/// Providers named in the plans but not built: refused with a word saying so.
const PLANNED_PROVIDERS: [&str; 4] = ["codex", "gemini", "antigravity", "agy"];

/// Checks that a thread may run on `provider`: one of [`PROVIDERS`].
pub fn check_provider(provider: &str) -> anyhow::Result<()> {
    if PROVIDERS.contains(&provider) {
        return Ok(());
    }
    if PLANNED_PROVIDERS.contains(&provider.to_ascii_lowercase().as_str()) {
        bail!("only claude is configured; Codex and Gemini/Antigravity are planned");
    }
    bail!("unknown provider {provider:?}; only claude is configured");
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            command: vec!["claude-agent-acp".to_string()],
            env: BTreeMap::new(),
            builtin_tools: SAFE_BUILTIN_TOOLS.map(str::to_string).to_vec(),
            sandbox: true,
            read: Vec::new(),
            oauth_token: None,
            limits: AgentLimits::default(),
        }
    }
}

/// Variables `[agent] env` may not set: Fronde sets the agent's home and temporary directory,
/// and the others point at the owner's session, its buses and the service's credentials, or at
/// a Claude directory, which would hold one sign-in for every thread.
fn reserved_agent_variable(name: &str) -> bool {
    matches!(
        name,
        "HOME" | "TMPDIR" | "CREDENTIALS_DIRECTORY" | "CLAUDE_CONFIG_DIR"
    ) || name.starts_with("XDG_")
        || name.starts_with("DBUS_")
}

/// Built-in tools an assistant with no shell and no files may use: they read the web and touch
/// nothing on the machine. Widening this list is a deliberate decision.
pub const SAFE_BUILTIN_TOOLS: [&str; 2] = ["WebSearch", "WebFetch"];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    pub token: Secret,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataConfig {
    /// Where Fronde keeps its files. Left out, [`default_data_dir`]; `load` fills it in.
    #[serde(default)]
    pub dir: PathBuf,
    pub timezone: chrono_tz::Tz,
    #[serde(default = "default_user")]
    pub user: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MemoryConfig {
    pub memory_limit: usize,
    pub user_limit: usize,
    pub git: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            memory_limit: 4000,
            user_limit: 2000,
            git: true,
        }
    }
}

/// One `[[sources]]` entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SourceConfig {
    /// An upstream MCP server, whose tools become `{name}_{tool}`.
    Mcp(UpstreamConfig),
    /// A tool component (`.wasm`), run in the sandbox with what this entry grants it.
    Component(ComponentConfig),
}

impl SourceConfig {
    pub fn name(&self) -> &str {
        match self {
            Self::Mcp(m) => &m.name,
            Self::Component(c) => &c.name,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    pub name: String,
    pub url: Option<Url>,
    pub command: Option<Vec<String>>,
    pub token: Option<Secret>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentConfig {
    pub name: String,
    /// The component's `.wasm` file.
    pub path: PathBuf,
    /// Its manifest; by default the `.toml` file beside `path` with the same name.
    pub manifest: Option<PathBuf>,
    /// SHA-256 of the `.wasm` file, as `fronde check` prints it: approves this build.
    pub sha256: Option<String>,
    /// SHA-256 of the manifest: approves what the component asks for.
    pub manifest_sha256: Option<String>,
    /// Origins the component may reach, each one its manifest asks for.
    #[serde(default)]
    pub allow: Vec<Allow>,
    /// What the component reads through `settings.get`, checked against its manifest; never
    /// secrets.
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
    /// Secrets the host adds to requests for an allowed origin.
    #[serde(default)]
    pub credentials: Vec<CredentialConfig>,
}

impl ComponentConfig {
    /// The manifest's path: `manifest`, or `path` with a `.toml` extension.
    pub fn manifest_path(&self) -> PathBuf {
        self.manifest
            .clone()
            .unwrap_or_else(|| self.path.with_extension("toml"))
    }
}

/// An origin a component may reach: `"https://cloud.example"`, or
/// `{ origin = "http://192.168.1.10:5232", local = true }` for a server on the host or its
/// network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allow {
    pub origin: Origin,
    /// Whether it may resolve to a local address (loopback, private, link-local).
    pub local: bool,
}

impl<'de> Deserialize<'de> for Allow {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::{self, MapAccess, Visitor, value::MapAccessDeserializer};

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Table {
            origin: Origin,
            #[serde(default)]
            local: bool,
        }

        struct AllowVisitor;

        impl<'de> Visitor<'de> for AllowVisitor {
            type Value = Allow;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(
                    "an origin such as \"https://cloud.example\", or { origin = \"…\", local = true }",
                )
            }

            fn visit_str<E: de::Error>(self, s: &str) -> Result<Allow, E> {
                Ok(Allow {
                    origin: Origin::parse(s).map_err(E::custom)?,
                    local: false,
                })
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Allow, A::Error> {
                let t = Table::deserialize(MapAccessDeserializer::new(map))?;
                Ok(Allow {
                    origin: t.origin,
                    local: t.local,
                })
            }
        }

        d.deserialize_any(AllowVisitor)
    }
}

/// A secret the host sends to one origin: exactly one of `basic` or `bearer`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialConfig {
    pub origin: Origin,
    pub basic: Option<BasicCredential>,
    pub bearer: Option<Secret>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BasicCredential {
    pub username: String,
    pub password: Secret,
}

/// A secret is never written in the TOML: it points to a file, an environment variable, or, in a
/// component's credentials, a URL upstream server's token and an account's `oauth_token` only, a
/// secret stored encrypted in `fronde.db`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Secret {
    File {
        file: PathBuf,
    },
    Env {
        env: String,
    },
    /// The secret `fronde secret set` stored under this name. Only what names it opens it, for
    /// its origin (the preparation of a component, the start of an upstream server, the
    /// agent's start), so [`Secret::resolve`] refuses it.
    Stored {
        stored: String,
    },
}

/// Per-user data directory used when `[data] dir` is not set: `$XDG_DATA_HOME/fronde`, else
/// `~/.local/share/fronde` on Linux and other Unixes, the platform's equivalent on macOS and
/// Windows.
pub fn default_data_dir() -> Option<PathBuf> {
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        xdg_data_dir(
            std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
            etcetera::home_dir().ok(),
        )
    }
    #[cfg(any(target_os = "macos", windows))]
    {
        use etcetera::BaseStrategy as _;
        let dir = etcetera::base_strategy::choose_native_strategy()
            .ok()
            .filter(|d| d.home_dir().is_absolute())?
            .data_dir()
            .join("fronde");
        // Same layout as before the switch from `directories`: %APPDATA%\fronde\data on Windows.
        #[cfg(windows)]
        let dir = dir.join("data");
        Some(dir)
    }
}

/// The XDG rule: an absolute `XDG_DATA_HOME` wins, then `HOME/.local/share` (an absolute home only).
#[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
pub fn xdg_data_dir(xdg_data_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    let base = match xdg_data_home {
        Some(p) if p.is_absolute() => p,
        _ => home.filter(|h| h.is_absolute())?.join(".local/share"),
    };
    Some(base.join("fronde"))
}

impl Secret {
    /// Make a relative secret file path relative to `base`, the config file's directory, so it
    /// does not depend on the directory Fronde was started from.
    fn anchor(&mut self, base: &Path) {
        if let Secret::File { file } = self
            && file.is_relative()
        {
            *file = base.join(&*file);
        }
    }

    /// Refuse a stored secret where only a file or an environment variable may be, saying so
    /// for `place`.
    fn not_stored(&self, place: &str) -> anyhow::Result<()> {
        if let Secret::Stored { stored } = self {
            bail!(
                "{place} = {{ stored = {stored:?} }}: a stored secret can only be a component's \
                 credential, in [[sources.credentials]]; use {{ file = \"…\" }} or \
                 {{ env = \"…\" }} here"
            );
        }
        Ok(())
    }

    /// The value of a file or environment secret. A stored secret is refused: only the
    /// preparation of the component whose credential names it opens it.
    pub fn resolve(&self) -> anyhow::Result<SecretString> {
        match self {
            Secret::Stored { stored } => bail!(
                "the stored secret {stored} is opened only when the component whose credential \
                 names it is prepared"
            ),
            Secret::File { file } => {
                crate::private::check_secret_file(file)?;
                let mut s = std::fs::read_to_string(file)
                    .with_context(|| format!("cannot read secret file {}", file.display()))?;
                let trimmed = s.trim_end_matches(['\n', '\r']).to_owned();
                s.zeroize();
                Ok(SecretString::from(trimmed))
            }
            Secret::Env { env } => {
                // `std::env::var` would fail with `NotUnicode`, which prints the value.
                let Some(value) = std::env::var_os(env) else {
                    bail!(
                        "environment variable {env} is not set; set it or use {{ file = \"…\" }}"
                    );
                };
                // The value is left out of the message: it stays in the environment anyway.
                let value = value.into_string().map_err(|_| {
                    anyhow::anyhow!(
                        "environment variable {env} must be valid UTF-8; fix its value or use \
                         {{ file = \"…\" }}"
                    )
                })?;
                Ok(SecretString::from(value))
            }
        }
    }
}

fn default_listen() -> SocketAddr {
    "127.0.0.1:8484".parse().expect("valid default address")
}

fn default_user() -> String {
    "me".to_string()
}

/// Prefixes of the hub's own tools (`search_tools`, `use_tool`, `memory_*`, `code_run_python`,
/// `user_*`, `threads_*`, `integrations_*`) and of its source name: no component may declare
/// them.
pub const RESERVED_TOOL_PREFIXES: [&str; 8] = [
    "code_",
    "hub_",
    "integrations_",
    "memory_",
    "search_",
    "threads_",
    "use_",
    "user_",
];

/// True for names usable as tool prefixes: `^[a-z][a-z0-9_]{0,63}$`.
pub fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z'))
        && name.len() <= 64
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_'))
}

pub fn load(path: &Path) -> anyhow::Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read config {}", path.display()))?;
    // Relative paths in the file are read from its directory, made absolute so that every path
    // derived from them (agent cwd, memory, database) is absolute too.
    let abs = std::path::absolute(path)
        .with_context(|| format!("cannot resolve config path {}", path.display()))?;
    let base: PathBuf = abs
        .parent()
        .unwrap_or(Path::new("/"))
        .components()
        .collect();
    let mut config = parse(&text, &base, &path.display().to_string())?;
    config.path = Some(base.join(abs.file_name().unwrap_or_default()));
    Ok(config)
}

/// The configuration `text` holds, for a file in the directory `base` (absolute), with every
/// check [`load`] makes: what Fronde would start with if the file held `text`. Its `path` is
/// unset.
pub fn load_str(text: &str, base: &Path) -> anyhow::Result<Config> {
    parse(text, base, "")
}

/// [`load_str`], its errors naming the file `name` when there is one.
fn parse(text: &str, base: &Path, name: &str) -> anyhow::Result<Config> {
    let invalid = || {
        if name.is_empty() {
            "invalid config".to_owned()
        } else {
            format!("invalid config {name}")
        }
    };
    replaced_sections(text).with_context(invalid)?;
    let mut config: Config = toml::from_str(text).with_context(invalid)?;
    if config.data.dir.as_os_str().is_empty() {
        config.data.dir = default_data_dir().context(
            "cannot work out the default data directory (no home directory); set [data] dir",
        )?;
    }
    if config.data.dir.is_relative() {
        config.data.dir = base.join(&config.data.dir).components().collect();
    }
    config.server.token.anchor(base);
    config.server.token.not_stored("server.token")?;
    if let Some(password) = config.web.as_mut().and_then(|web| web.password.as_mut()) {
        password.anchor(base);
        password.not_stored("web.password")?;
    }
    check_users(&mut config, base)?;
    for source in &mut config.sources {
        match source {
            SourceConfig::Mcp(m) => {
                if let Some(t) = &mut m.token {
                    t.anchor(base);
                    if let Secret::Stored { stored } = t {
                        let name = &m.name;
                        if m.url.is_none() {
                            bail!(
                                "source {name:?}: token = {{ stored = {stored:?} }}: a stored \
                                 secret is bound to the origin of a URL, and a command has none; \
                                 use {{ file = \"…\" }}"
                            );
                        }
                        fronde_db::secrets::check_name(stored)
                            .with_context(|| format!("source {name:?}: token"))?;
                    }
                }
            }
            SourceConfig::Component(c) => {
                c.path = base.join(&c.path);
                if let Some(m) = &mut c.manifest {
                    *m = base.join(&*m);
                }
                for credential in &mut c.credentials {
                    if let Some(b) = &mut credential.basic {
                        b.password.anchor(base);
                    }
                    if let Some(t) = &mut credential.bearer {
                        t.anchor(base);
                    }
                }
            }
        }
    }
    config.secrets.master_key_file = Some(match &config.secrets.master_key {
        None => base.join(fronde_db::master_key::FILE_NAME),
        Some(Secret::File { file }) => base.join(file).components().collect(),
        Some(_) => bail!(
            "[secrets] master_key must be {{ file = \"…\" }}: the master key never comes from \
             the environment, which the processes Fronde starts may inherit, nor from the store it \
             opens"
        ),
    });
    // Set by systemd, for this process only, when the unit loads credentials.
    config.secrets.credentials_dir = std::env::var_os("CREDENTIALS_DIRECTORY")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from);
    if let Some(python) = &mut config.python {
        python.dir = base.join(&python.dir).components().collect();
        if config.sources.iter().any(|s| s.name() == PYTHON_SOURCE) {
            bail!(
                "source name \"{PYTHON_SOURCE}\" is taken by [python]'s code_run_python; pick another name"
            );
        }
    }
    check_sources(&config.sources)?;
    for name in config.agent.env.keys() {
        if reserved_agent_variable(name) {
            bail!(
                "[agent] env: {name} cannot be set here: Fronde gives the agent its own HOME and \
                 TMPDIR in the data directory, with its own Claude directory, and never passes \
                 XDG_*, DBUS_*, CREDENTIALS_DIRECTORY or CLAUDE_CONFIG_DIR, which point at your \
                 session or one sign-in for every thread; remove it"
            );
        }
        if let Some(provider) = signs_in(name) {
            bail!(
                "[agent] env sets {name}, which decides how {provider}'s agent signs in: Fronde \
                 gives each agent its own account's token and nothing else; name the token in \
                 [accounts.<name>] oauth_token (or the legacy [agent] oauth_token), never in \
                 fronde.toml, and remove {name} from env"
            );
        }
    }
    for dir in &mut config.agent.read {
        *dir = base.join(&*dir).components().collect();
    }
    if let Some(token) = &mut config.agent.oauth_token {
        token.anchor(base);
        if let Secret::Stored { stored } = token {
            fronde_db::secrets::check_name(stored).context("[agent] oauth_token")?;
        }
    }
    for account in config.accounts.values_mut() {
        if let Some(token) = &mut account.oauth_token {
            token.anchor(base);
        }
    }
    check_shared_secrets(&config)?;
    check_accounts(&config)?;
    if !config.agent.sandbox
        && let Some(member) = config.users.iter().find(|(_, user)| !user.admin)
    {
        bail!(
            "[agent] sandbox = false with [users.{}], who is not an admin: an unconfined agent of \
             theirs could read everything Fronde can, your data and secrets included; keep the \
             sandbox on",
            table_key(member.0)
        );
    }
    // Read once, here: `Config::accounts` stays pure, and `serve` and `check` refuse what
    // `check_inherited` finds.
    config.inherited = PROVIDERS
        .into_iter()
        .flat_map(|provider| sign_in_vars(provider).iter())
        .filter(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
        .map(|name| (*name).to_owned())
        .collect();
    for tool in &config.agent.builtin_tools {
        if !SAFE_BUILTIN_TOOLS.contains(&tool.as_str()) {
            bail!(
                "agent.builtin_tools: {tool:?} is not allowed; use only {SAFE_BUILTIN_TOOLS:?}, or [] for none"
            );
        }
    }
    Ok(config)
}

/// The rules of `[users]` and the legacy `[web] password`, each refusal naming its key; anchors
/// the password files.
fn check_users(config: &mut Config, base: &Path) -> anyhow::Result<()> {
    let owner = config.data.user.clone();
    if config.users.is_empty() {
        check_user_name(&owner, "[data] user")?;
        if config
            .web
            .as_ref()
            .is_some_and(|web| web.password.is_none())
        {
            bail!(
                "[web] has no password: set [web] password = {{ file = \"…\" }}, or list who \
                 signs in as [users.<name>] sections, each with its password"
            );
        }
        return Ok(());
    }
    if config
        .web
        .as_ref()
        .is_some_and(|web| web.password.is_some())
    {
        bail!(
            "[web] password and [users] are both set: move the password to [users.{}] password \
             and remove it from [web]",
            table_key(&owner)
        );
    }
    // Each name as it looks: two that look alike are refused.
    let mut looks: Vec<(String, &str)> = Vec::new();
    for name in config.users.keys() {
        let key = table_key(name);
        check_user_name(name, &format!("[users.{key}]"))?;
        let looked = look(name);
        if let Some((_, other)) = looks.iter().find(|(seen, _)| looks_alike(seen, &looked)) {
            bail!(
                "[users.{}] and [users.{key}] look alike (in case, accents or letters of \
                 another script that look the same): two people never have names that could \
                 be taken one for the other; rename one",
                table_key(other)
            );
        }
        looks.push((looked, name));
    }
    for (name, user) in &mut config.users {
        let key = table_key(name);
        let Some(password) = &mut user.password else {
            bail!(
                "[users.{key}] has no way to sign in: set password = {{ file = \"…\" }} or \
                 {{ env = \"…\" }}"
            );
        };
        password.anchor(base);
        password.not_stored(&format!("[users.{key}] password"))?;
    }
    let key = table_key(&owner);
    match config.users.get(&owner) {
        Some(user) if user.admin => Ok(()),
        Some(_) => bail!(
            "[data] user = {owner:?} is the owner, an admin: set admin = true in [users.{key}]"
        ),
        None => bail!(
            "[data] user = {owner:?} is not in [users]: add [users.{key}] with admin = true, or \
             set [data] user to one of: {}",
            config.users.keys().cloned().collect::<Vec<_>>().join(", ")
        ),
    }
}

/// Where a secret is read from, as two references are compared: the same stored name, the same
/// variable, or the same file, however its path is written (`..`, a link: the file it leads to,
/// by device and inode).
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SecretPlace {
    Stored(String),
    Env(String),
    #[cfg(unix)]
    Inode(u64, u64),
    Path(PathBuf),
}

impl SecretPlace {
    fn of(secret: &Secret) -> Self {
        match secret {
            Secret::Stored { stored } => Self::Stored(stored.clone()),
            Secret::Env { env } => Self::Env(env.clone()),
            Secret::File { file } => Self::of_file(file),
        }
    }

    fn of_file(file: &Path) -> Self {
        #[cfg(unix)]
        if let Ok(metadata) = std::fs::metadata(file) {
            use std::os::unix::fs::MetadataExt;
            return Self::Inode(metadata.dev(), metadata.ino());
        }
        match std::fs::canonicalize(file) {
            Ok(file) => Self::Path(file),
            Err(_) => Self::Path(lexical(file)),
        }
    }
}

/// `path` with `.` and `..` resolved by name only, for a file that does not exist yet.
fn lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// What a secret reference reads from, for a message: never a value.
fn secret_place(secret: &Secret) -> String {
    match secret {
        Secret::Stored { stored } => format!("the stored secret {stored}"),
        Secret::Env { env } => format!("the environment variable {env}"),
        Secret::File { file } => format!("the file {}", file.display()),
    }
}

/// Refuses a secret that a person's credential (`server.token`, a password, an account's
/// token, the master key) shares with any other credential: the same stored name, variable or
/// file would let one open the other. Two credentials of sources (a component's, an upstream
/// server's) may share one; `[agent] env` never sets the variable of a person's credential.
fn check_shared_secrets(config: &Config) -> anyhow::Result<()> {
    // (key naming it, the secret, whether it is a person's)
    let mut named: Vec<(String, Secret, bool)> =
        vec![("server.token".to_owned(), config.server.token.clone(), true)];
    if let Some(password) = config.web.as_ref().and_then(|web| web.password.as_ref()) {
        named.push(("web.password".to_owned(), password.clone(), true));
    }
    for (name, user) in &config.users {
        if let Some(password) = &user.password {
            named.push((
                format!("[users.{}] password", table_key(name)),
                password.clone(),
                true,
            ));
        }
    }
    if let Some(token) = &config.agent.oauth_token {
        named.push(("[agent] oauth_token".to_owned(), token.clone(), true));
    }
    for (name, account) in &config.accounts {
        if let Some(token) = &account.oauth_token {
            named.push((
                format!("[accounts.{name}] oauth_token"),
                token.clone(),
                true,
            ));
        }
    }
    if let Some(file) = &config.secrets.master_key_file {
        named.push((
            "[secrets] master_key".to_owned(),
            Secret::File { file: file.clone() },
            true,
        ));
    }
    for source in &config.sources {
        let name = source.name();
        match source {
            SourceConfig::Mcp(m) => {
                if let Some(token) = &m.token {
                    named.push((format!("source {name:?}: token"), token.clone(), false));
                }
            }
            SourceConfig::Component(c) => {
                for credential in &c.credentials {
                    let secret = credential.basic.as_ref().map(|b| &b.password);
                    if let Some(secret) = secret.or(credential.bearer.as_ref()) {
                        named.push((
                            format!("source {name:?}: the credential for {}", credential.origin),
                            secret.clone(),
                            false,
                        ));
                    }
                }
            }
        }
    }
    let mut seen: BTreeMap<SecretPlace, (&str, &Secret, bool)> = BTreeMap::new();
    for (key, secret, person) in &named {
        let place = SecretPlace::of(secret);
        match seen.get(&place) {
            Some((other, first, other_person)) if *person || *other_person => bail!(
                "{other} and {key} both read their secret from {}: one would open the other, so \
                 a person's credential is never shared; give each its own",
                secret_place(first)
            ),
            Some(_) => {}
            None => {
                seen.insert(place, (key, secret, *person));
            }
        }
    }
    for name in config.agent.env.keys() {
        let place = SecretPlace::Env(name.clone());
        if let Some((key, _, true)) = seen.get(&place) {
            bail!(
                "[agent] env sets {name}, the variable {key} is read from: the agent would get a \
                 value under a secret's name; remove {name} from env"
            );
        }
    }
    Ok(())
}

/// The rules of `[accounts]`, each refusal naming its key. Their tokens are anchored, and checked
/// against every other secret by [`check_shared_secrets`], before.
fn check_accounts(config: &Config) -> anyhow::Result<()> {
    if config.accounts.is_empty() {
        return Ok(());
    }
    if config.agent.oauth_token.is_some() {
        bail!(
            "[agent] oauth_token and [accounts] are both set: move the token to the oauth_token \
             of its owner's account, [accounts.<name>] oauth_token, and remove it from [agent]"
        );
    }
    let users = config.user_names();
    for (name, account) in &config.accounts {
        if !is_valid_name(name) {
            bail!("[accounts.{name:?}]: an account's name must match ^[a-z][a-z0-9_]{{0,63}}$");
        }
        check_provider(&account.provider)
            .with_context(|| format!("[accounts.{name}] provider = {:?}", account.provider))?;
        let owner = &account.owner;
        if !users.iter().any(|user| user == owner) {
            bail!(
                "[accounts.{name}] owner = {owner:?} is not a user; name one of: {}",
                users.join(", ")
            );
        }
        let Some(token) = &account.oauth_token else {
            bail!(
                "[accounts.{name}] has no oauth_token: set oauth_token = {{ stored = \"…\" }}, \
                 {{ file = \"…\" }} or {{ env = \"…\" }}, the token `claude setup-token` prints"
            );
        };
        if let Secret::Stored { stored } = token {
            fronde_db::secrets::check_name(stored)
                .with_context(|| format!("[accounts.{name}] oauth_token"))?;
        }
        // The usage readings kept before accounts are the legacy account's, the owner's.
        if name == LEGACY_ACCOUNT && *owner != config.data.user {
            bail!(
                "[accounts.{LEGACY_ACCOUNT}] belongs to {owner}, but that name is the owner's \
                 account of the legacy form, [data] user = {:?}: give {owner}'s account another \
                 name",
                config.data.user
            );
        }
        for (i, user) in account.users.iter().enumerate() {
            if !users.contains(user) {
                bail!(
                    "[accounts.{name}] users names {user:?}, who is not a user; name one of: {}",
                    users.join(", ")
                );
            }
            if user == owner {
                bail!(
                    "[accounts.{name}] users names {user:?}, its owner, who may always use it: \
                     remove them from users"
                );
            }
            if account.users[..i].contains(user) {
                bail!("[accounts.{name}] users names {user:?} twice: remove one");
            }
        }
        for (i, user) in account.default_for.iter().enumerate() {
            if user != owner && !account.users.contains(user) {
                bail!(
                    "[accounts.{name}] default_for names {user:?}, who may not use it: add them \
                     to its users, or remove them from default_for"
                );
            }
            if account.default_for[..i].contains(user) {
                bail!("[accounts.{name}] default_for names {user:?} twice: remove one");
            }
        }
    }
    check_defaults(config, &users)
}

/// Each person has at most one default account per provider, and one exactly when they may use
/// several: Fronde never picks who pays, and adding someone to an account never changes their
/// default silently.
fn check_defaults(config: &Config, users: &[String]) -> anyhow::Result<()> {
    let providers: std::collections::BTreeSet<&str> = config
        .accounts
        .values()
        .map(|account| account.provider.as_str())
        .collect();
    for user in users {
        for &provider in &providers {
            let usable: Vec<&str> = config
                .accounts
                .iter()
                .filter(|(_, account)| {
                    account.provider == provider
                        && (account.owner == *user || account.users.contains(user))
                })
                .map(|(name, _)| name.as_str())
                .collect();
            let defaults: Vec<&str> = usable
                .iter()
                .copied()
                .filter(|name| config.accounts[*name].default_for.contains(user))
                .collect();
            if let [first, second, ..] = defaults[..] {
                bail!(
                    "[accounts.{first}] default_for and [accounts.{second}] default_for both name \
                     {user:?} on {provider}: a person has one default account per provider; \
                     remove them from one"
                );
            }
            if usable.len() > 1 && defaults.is_empty() {
                let listed: Vec<String> = usable
                    .iter()
                    .map(|name| format!("[accounts.{name}]"))
                    .collect();
                let listed = match listed.split_last() {
                    Some((last, rest)) if !rest.is_empty() => {
                        format!("{} and {last}", rest.join(", "))
                    }
                    _ => listed.join(""),
                };
                bail!(
                    "{user:?} may use {listed} on {provider}, and none is named their default: \
                     add default_for = [{user:?}] to the one their threads run on by default"
                );
            }
        }
    }
    Ok(())
}

/// `[calendar]` and `[[mcp]]` became `[[sources]]` entries: say how, rather than "unknown field".
fn replaced_sections(text: &str) -> anyhow::Result<()> {
    let Ok(table) = text.parse::<toml::Table>() else {
        return Ok(()); // the full parse reports it
    };
    if let Some(old) = table.get("calendar") {
        bail!(
            "[calendar] is gone: the calendar is now a component. Replace the section with:\n\n\
             {}\n\
             The calendar shipped with this Fronde needs no hashes: `fronde check` says \
             \"approved: shipped with this build\". A calendar.wasm from elsewhere needs the \
             sha256 and manifest_sha256 that `fronde check` prints, added to the entry once you \
             have read its manifest. See the \"Configure the calendar\" guide.",
            calendar_source(old)
        );
    }
    if table.contains_key("mcp") {
        bail!(
            "[[mcp]] is gone: write each server as a [[sources]] entry with kind = \"mcp\" and \
             the same name, url or command, and token"
        );
    }
    Ok(())
}

/// The `[[sources]]` entry that replaces an old `[calendar]` section, with its values.
fn calendar_source(old: &toml::Value) -> String {
    let text = |key: &str, default: &str| {
        old.get(key)
            .and_then(toml::Value::as_str)
            .unwrap_or(default)
            .to_string()
    };
    let quoted = |s: &str| toml::Value::String(s.to_string()).to_string();
    let events = text(
        "events_url",
        "https://cloud.example/remote.php/dav/calendars/you/personal/",
    );
    let reminders = text(
        "reminders_url",
        "https://cloud.example/remote.php/dav/calendars/you/tasks/",
    );
    let username = text("username", "you");
    let password = old.get("password").map_or_else(
        || "{ env = \"FRONDE_CALDAV_PASSWORD\" }".to_string(),
        toml::Value::to_string,
    );
    let mut origins: Vec<Origin> = Vec::new();
    for url in [&events, &reminders] {
        if let Ok(url) = Url::parse(url)
            && let Ok(origin) = Origin::of_url(&url)
            && !origins.contains(&origin)
        {
            origins.push(origin);
        }
    }
    let allow: Vec<String> = origins
        .iter()
        .map(|o| match o.locality() {
            Locality::Public => quoted(&o.to_string()),
            Locality::Loopback | Locality::Local => {
                format!("{{ origin = {}, local = true }}", quoted(&o.to_string()))
            }
        })
        .collect();
    let mut entry = format!(
        "[[sources]]\n\
         kind = \"component\"\n\
         name = \"calendar\"\n\
         path = {}\n\
         # The origins of both URLs; a server on this machine or its network: {{ origin = \"…\", local = true }}\n\
         allow = [{}]\n\
         [sources.settings]\n\
         events_url = {}\n\
         reminders_url = {}\n",
        quoted(&shipped_calendar().map_or_else(
            || "/app/components/calendar.wasm".to_string(),
            |p| p.display().to_string()
        )),
        allow.join(", "),
        quoted(&events),
        quoted(&reminders),
    );
    for origin in &origins {
        if !origin.carries_credentials() {
            entry.push_str(
                "# Plain http: Fronde sends the password only over https or to this machine (in \
                 Docker, the container). Serve CalDAV over https.\n",
            );
        }
        entry.push_str(&format!(
            "[[sources.credentials]]\n\
             origin = {}\n\
             basic = {{ username = {}, password = {password} }}\n",
            quoted(&origin.to_string()),
            quoted(&username),
        ));
    }
    entry
}

/// Where the shipped calendar component is: beside the binary as in the image
/// (`/app/components`), or in `components/dist` of the checkout this binary was built from.
pub fn shipped_calendar() -> Option<PathBuf> {
    shipped_component("calendar")
}

/// Where the component `name` shipped with this build is (`<name>.wasm`, its manifest beside
/// it): beside the binary as in the image (`/app/components`), or in `components/dist` of the
/// checkout this binary was built from. Canonical.
pub fn shipped_component(name: &str) -> Option<PathBuf> {
    if !is_valid_name(name) {
        return None;
    }
    let file = format!("{name}.wasm");
    let exe = std::env::current_exe().ok();
    let candidates = [
        exe.as_deref()
            .and_then(Path::parent)
            .map(|d| d.join("components").join(&file)),
        Some(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../components/dist")
                .join(&file),
        ),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|p| p.is_file())
        .and_then(|p| p.canonicalize().ok())
}

fn check_sources(sources: &[SourceConfig]) -> anyhow::Result<()> {
    let mut seen = std::collections::HashSet::new();
    for source in sources {
        let name = source.name();
        if name == "hub" {
            bail!("source name \"hub\" is reserved for the hub's own tools; pick another name");
        }
        // Whether [python] is on or not: its one tool is the only one anyone may use.
        if name == PYTHON_SOURCE {
            bail!(
                "source name \"{PYTHON_SOURCE}\" is reserved for [python]'s code_run_python; pick \
                 another name"
            );
        }
        // A server's form is headed with its name: never one that reads as Fronde's own.
        if matches!(name, "fronde" | "assistant") {
            bail!(
                "source name {name:?} is reserved: it would read as Fronde's own; pick another name"
            );
        }
        if !seen.insert(name) {
            bail!("source name {name:?} is used twice; names must be unique");
        }
        if !is_valid_name(name) {
            bail!("source name {name:?} must match ^[a-z][a-z0-9_]{{0,63}}$");
        }
        match source {
            SourceConfig::Mcp(m) => {
                let prefix = format!("{name}_");
                if let Some(reserved) = RESERVED_TOOL_PREFIXES
                    .iter()
                    .find(|r| prefix.starts_with(**r) || r.starts_with(&prefix))
                {
                    bail!(
                        "source name {name:?} is reserved for the hub's own tools ({reserved}…); pick another name"
                    );
                }
                if m.url.is_some() == m.command.is_some() {
                    bail!("source {name:?}: set exactly one of url or command");
                }
            }
            SourceConfig::Component(c) => check_component(c)?,
        }
    }
    Ok(())
}

fn check_component(c: &ComponentConfig) -> anyhow::Result<()> {
    let name = &c.name;
    for (key, hash) in [
        ("sha256", &c.sha256),
        ("manifest_sha256", &c.manifest_sha256),
    ] {
        if let Some(h) = hash
            && !(h.len() == 64
                && h.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        {
            bail!(
                "source {name:?}: {key} must be 64 lower-case hex digits, as `fronde check` prints it"
            );
        }
    }
    let mut origins = Vec::new();
    for a in &c.allow {
        let origin = &a.origin;
        if origins.contains(&origin) {
            bail!("source {name:?}: {origin} is in allow twice; list each origin once");
        }
        origins.push(origin);
    }
    let mut credited = Vec::new();
    for credential in &c.credentials {
        let origin = &credential.origin;
        if !origins.contains(&origin) {
            bail!(
                "source {name:?}: the credential for {origin} is for an origin not in allow; add it to allow or remove the credential"
            );
        }
        if credited.contains(&origin) {
            bail!("source {name:?}: two credentials for {origin}; keep one");
        }
        if credential.basic.is_some() == credential.bearer.is_some() {
            bail!(
                "source {name:?}: the credential for {origin} needs exactly one of basic or bearer"
            );
        }
        let secret = match (&credential.basic, &credential.bearer) {
            (Some(b), _) => &b.password,
            (_, Some(t)) => t,
            (None, None) => unreachable!("checked above"),
        };
        if let Secret::Stored { stored } = secret {
            fronde_db::secrets::check_name(stored)
                .with_context(|| format!("source {name:?}: the credential for {origin}"))?;
        }
        credited.push(origin);
    }
    Ok(())
}
