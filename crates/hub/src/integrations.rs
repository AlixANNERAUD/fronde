//! `integrations_request_setup`: set up, from a card the admin answers, a component shipped with
//! this build of Fronde that has no `[[sources]]` entry yet.
//!
//! The agent names the component and may suggest its non-secret settings; the admin sees every
//! field, types any password on the card, and sees where each one goes. Only Fronde's own
//! process acts, and only on that answer: the entry is checked by a dry run of `prepare`, the
//! secrets are sealed in the store for the origins their URL settings give, the full entry is
//! checked again, then written to `fronde.toml` (or shown to paste when the file cannot be
//! written) and started. A secret never reaches the agent, the stream, the request's row or the
//! log: the entry names it as `{ stored = "…" }`.

use crate::config::{
    Allow, BasicCredential, ComponentConfig, CredentialConfig, Secret, is_valid_name,
};
use crate::origin::Origin;
use crate::source::{Supervisor, Tool};
use crate::threads::ThreadControl;
use crate::tools::{CallContext, Registry, ToolOutput, typed_tool_with_context};
use crate::user::{Asking, NotShown, check_timeout};
use async_trait::async_trait;
use fronde_protocol::request::{MAX_DESCRIPTION, MAX_LABEL, MAX_MESSAGE, MAX_QUESTION, MAX_TEXT};
use fronde_protocol::text::{Notes, first_hidden, line};
use fronde_protocol::{Card, ChoiceOption, Field, FieldValue, KeyPlace, Response, TextFormat};
use fronde_requests::{Ask, Caller, Requests, Resolution, Waiter};
use schemars::JsonSchema;
use secrecy::SecretString;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// The tool's name.
pub const TOOL: &str = "integrations_request_setup";

/// What `search_tools` says of an integration it lists.
pub const AVAILABLE: &str = "available, not set up: call integrations_request_setup";

/// The key of the card's toggle that lets the origins of URL settings be local addresses.
const LOCAL: &str = "allow.local";

/// How long a card that shows the entry to paste stays open: an hour.
const SNIPPET_TIMEOUT: Duration = Duration::from_secs(3600);

const DESCRIPTION: &str = "Ask the owner to set up an integration that search_tools lists as available, not set up, and wait. The owner sees a card with its settings, types any password there and sees where it goes; Fronde then adds it and returns its tools. prefill suggests settings and user names only, never a password, token or key. Blocks up to timeout_ms (default 10 min).";

/// A component shipped with this build of Fronde, with no `[[sources]]` entry yet, as its
/// manifest describes it.
#[derive(Clone, Debug)]
pub struct Integration {
    /// The component's name, and the name of the source it becomes.
    pub name: String,
    pub description: String,
    /// The `.wasm` file the entry names, its manifest beside it.
    pub path: PathBuf,
    /// Every tool it brings starts with one of these.
    pub tool_prefixes: Vec<String>,
    pub settings: Vec<Setting>,
    pub credentials: Vec<CredentialAsk>,
    /// The origins it asks to reach.
    pub network: Vec<Reach>,
    /// Why a card cannot set it up, when it cannot (an origin asked for by wildcard, a
    /// credential sent in a header): its entry is written by hand.
    pub unsupported: Option<String>,
}

/// One of its settings, from the manifest's schema.
#[derive(Clone, Debug)]
pub struct Setting {
    pub key: String,
    pub description: Option<String>,
    pub required: bool,
    pub kind: SettingKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingKind {
    Text,
    /// A URL (`format = "uri"`), whose origin the component may be allowed to reach.
    Url,
    /// One of these values (`enum`).
    Choice(Vec<String>),
}

/// An origin a component asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reach {
    Origin(Origin),
    /// The origin of the URL in this setting.
    Setting(String),
}

/// A secret the host adds to the component's requests to some of its origins.
#[derive(Clone, Debug)]
pub struct CredentialAsk {
    pub name: String,
    pub kind: CredentialKind,
    pub origins: Vec<Reach>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialKind {
    /// A user name and a password.
    Basic,
    /// A token.
    Bearer,
}

/// Where a new entry went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Written {
    /// Into the configuration file.
    File,
    /// Nowhere: the file cannot be written, and this is the entry to paste into it.
    Snippet(String),
}

/// The components a card can set up, and what adds one: the server implements it over the
/// components shipped with the build, `prepare` and the configuration file.
#[async_trait]
pub trait Integrations: Send + Sync {
    /// The components shipped with this build that have no `[[sources]]` entry.
    fn available(&self) -> Vec<Integration>;
    /// Whether `settings` fit the manifest of the component `name` (every required one there
    /// when `complete`): what to fix, for the agent, else.
    fn check_settings(
        &self,
        name: &str,
        settings: &BTreeMap<String, String>,
        complete: bool,
    ) -> Result<(), String>;
    /// `prepare` on `entry` as its start would, reading the stored secrets: nothing is written
    /// or started.
    async fn dry_run(&self, entry: &ComponentConfig) -> anyhow::Result<()>;
    /// Whether `entry` could be added to the configuration file now, read again: no entry of
    /// its name there, and the file passing every check of `config::load` with it. Nothing is
    /// written; a file that cannot be written passes (its entry is pasted by hand). The error
    /// never quotes the file.
    async fn check_file(&self, entry: &ComponentConfig) -> anyhow::Result<()>;
    /// Adds `entry` to the configuration file, keeping what the file holds, or gives the entry
    /// to paste when the file cannot be written; then starts it under `sources`.
    async fn add(&self, entry: &ComponentConfig, sources: &Supervisor) -> anyhow::Result<Written>;
}

/// The integrations among `available` that `need` may be about: a word of the need is a word
/// of the name, the description or a tool prefix (or begins one, or one begins it, from four
/// letters).
pub fn matching<'a>(available: &'a [Integration], need: &str) -> Vec<&'a Integration> {
    let words = |text: &str| -> Vec<String> {
        text.split(|c: char| !c.is_alphanumeric())
            .filter(|word| word.chars().count() >= 3)
            .map(str::to_lowercase)
            .collect()
    };
    let asked = words(need);
    available
        .iter()
        .filter(|integration| {
            let text = format!(
                "{} {} {}",
                integration.name,
                integration.description,
                integration.tool_prefixes.join(" ")
            );
            let known = words(&text);
            asked.iter().any(|a| {
                known.iter().any(|k| {
                    a == k
                        || (a.len() >= 4 && k.len() >= 4 && (a.starts_with(k) || k.starts_with(a)))
                })
            })
        })
        .collect()
}

#[derive(Deserialize, JsonSchema)]
struct SetupArgs {
    /// The integration to set up, as search_tools names it, such as "calendar".
    integration: String,
    /// Suggested values, by setting name, such as {"events_url": "https://cloud.example/remote.php/dav/calendars/me/personal/"}, and a user name as "<credential>.username"; the owner can change them on the card. Never a password, token or key.
    #[serde(default)]
    prefill: BTreeMap<String, String>,
    /// How long to wait for the owner, in milliseconds: 1000 to 3600000 (default 600000).
    timeout_ms: Option<u64>,
}

/// What `integrations_request_setup` works with.
pub struct Setup {
    pub requests: Arc<Requests>,
    /// The threads, which show a card during the turn that asks and refuse it in a turn a
    /// sub-agent's result started, or from a sub-agent: without them, every setup is refused.
    pub threads: Option<Arc<dyn ThreadControl>>,
    pub integrations: Arc<dyn Integrations>,
    pub sources: Supervisor,
    pub registry: Arc<Registry>,
    /// The admin, who answers every setup card, whoever the caller acts for.
    pub admin: String,
    /// Held per integration from the first secret stored to the entry added, so that two
    /// setups of one integration answered at once never store over each other.
    pub(crate) setting_up: SettingUp,
}

/// A lock per integration name.
#[derive(Default)]
pub(crate) struct SettingUp(std::sync::Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>);

impl SettingUp {
    async fn lock(&self, name: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(name.to_owned())
            .or_default()
            .clone();
        lock.lock_owned().await
    }
}

/// `integrations_request_setup`, over `setup`.
pub fn integration_tools(setup: Setup) -> Vec<Tool> {
    let setup = Arc::new(setup);
    vec![typed_tool_with_context(
        TOOL,
        DESCRIPTION,
        false,
        move |context: CallContext, args: SetupArgs| {
            let setup = setup.clone();
            async move { request_setup(&setup, context, args).await }
        },
    )]
}

fn username_key(credential: &str) -> String {
    format!("{credential}.username")
}

fn secret_key(credential: &str) -> String {
    format!("{credential}.secret")
}

/// The origin of a URL, if it is one.
fn origin_of(url: &str) -> Option<Origin> {
    Origin::of_url(&url::Url::parse(url).ok()?).ok()
}

fn push_new(origins: &mut Vec<Origin>, origin: Origin) {
    if !origins.contains(&origin) {
        origins.push(origin);
    }
}

/// The origins `reaches` name with `settings`, and the URL settings among them not given.
fn origins_of(
    reaches: &[Reach],
    settings: &BTreeMap<String, String>,
) -> (Vec<Origin>, Vec<String>) {
    let mut origins = Vec::new();
    let mut missing = Vec::new();
    for reach in reaches {
        match reach {
            Reach::Origin(origin) => push_new(&mut origins, origin.clone()),
            Reach::Setting(key) => match settings.get(key).and_then(|url| origin_of(url)) {
                Some(origin) => push_new(&mut origins, origin),
                None if !missing.contains(key) => missing.push(key.clone()),
                None => {}
            },
        }
    }
    (origins, missing)
}

/// Where `reaches` go, for the card: the origins known, then the URL settings not given yet.
fn shown_places(origins: &[Origin], missing: &[String]) -> String {
    let mut places: Vec<String> = origins.iter().map(ToString::to_string).collect();
    if !missing.is_empty() {
        places.push(format!("l'origine de {}", missing.join(" et ")));
    }
    places.join(", ")
}

/// Where the entry sends what: the origins it may reach, and each credential's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Destinations {
    allow: Vec<Origin>,
    credentials: Vec<(String, Vec<Origin>)>,
}

impl Destinations {
    fn of(integration: &Integration, settings: &BTreeMap<String, String>) -> Self {
        Self {
            allow: origins_of(&integration.network, settings).0,
            credentials: integration
                .credentials
                .iter()
                .map(|c| (c.name.clone(), origins_of(&c.origins, settings).0))
                .collect(),
        }
    }

    /// Whether every place of `self` is one `shown` names for the same purpose.
    fn within(&self, shown: &Self) -> bool {
        self.allow.iter().all(|o| shown.allow.contains(o))
            && self.credentials.iter().all(|(name, origins)| {
                shown
                    .credentials
                    .iter()
                    .find(|(shown_name, _)| shown_name == name)
                    .is_some_and(|(_, shown)| origins.iter().all(|o| shown.contains(o)))
            })
    }
}

/// The prefill's settings and user names, checked: known keys only, one line each, values that
/// fit the manifest. What to fix, else.
fn check_prefill(
    integration: &Integration,
    integrations: &dyn Integrations,
    prefill: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    let mut known: Vec<String> = integration.settings.iter().map(|s| s.key.clone()).collect();
    known.extend(
        integration
            .credentials
            .iter()
            .filter(|c| c.kind == CredentialKind::Basic)
            .map(|c| username_key(&c.name)),
    );
    let mut checked = BTreeMap::new();
    for (key, value) in prefill {
        if !known.contains(key) {
            return Err(format!(
                "prefill names {key:?}, which {} does not take: give only {}; never a password, token or key, which the owner types on the card",
                integration.name,
                if known.is_empty() {
                    "nothing".to_owned()
                } else {
                    known.join(", ")
                }
            ));
        }
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        if value.chars().count() > MAX_TEXT || first_hidden(value, false).is_some() {
            return Err(format!(
                "prefill.{key} must be one line of plain text of at most {MAX_TEXT} characters"
            ));
        }
        if key.ends_with(".username") && value.contains(':') {
            return Err(format!(
                "prefill.{key} contains ':', which a user name sent as Basic authorization cannot hold"
            ));
        }
        checked.insert(key.clone(), value.to_owned());
    }
    let settings: BTreeMap<String, String> = checked
        .iter()
        .filter(|(key, _)| !key.ends_with(".username"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    integrations
        .check_settings(&integration.name, &settings, false)
        .map_err(|error| format!("prefill: {error}"))?;
    Ok(checked)
}

/// What the card says of a value the agent suggested: the admin checks it, a URL above all,
/// since the secrets go there.
const SUGGESTED: &str = "Suggéré par l'agent : vérifie-le.";
const SUGGESTED_URL: &str =
    "Suggéré par l'agent : vérifie que c'est bien ton serveur, ses mots de passe y iront.";

/// `description` with `mark` before it, when the agent suggested the value.
fn marked(mark: Option<&str>, description: Option<String>, notes: &mut Notes) -> Option<String> {
    match (mark, description) {
        (Some(mark), Some(description)) => Some(line(
            &format!("{mark} {description}"),
            MAX_DESCRIPTION,
            notes,
        )),
        (Some(mark), None) => Some(mark.to_owned()),
        (None, description) => description,
    }
}

/// Whether some of `reaches` follow from a URL the agent suggested.
fn from_prefill(reaches: &[Reach], prefill: &BTreeMap<String, String>) -> bool {
    reaches
        .iter()
        .any(|reach| matches!(reach, Reach::Setting(key) if prefill.contains_key(key)))
}

/// The setup card: the settings, a user name and a secret per credential, the toggle for local
/// addresses, and a message that says what Fronde will do, where each secret goes and which
/// stored secret it replaces (`replaces`, planned from the prefill). Each value the agent
/// suggested is marked so.
fn setup_card(
    integration: &Integration,
    prefill: &BTreeMap<String, String>,
    family: Option<&str>,
    replaces: &[Planned],
) -> Result<Card, String> {
    let mut notes = Notes::default();
    let mut fields = Vec::new();
    for setting in &integration.settings {
        let description = setting
            .description
            .as_deref()
            .map(|d| line(d, MAX_DESCRIPTION, &mut notes));
        let title = line(&setting.key, MAX_QUESTION, &mut notes);
        let default = prefill.get(&setting.key).cloned();
        let mark = default.is_some().then_some(match setting.kind {
            SettingKind::Url => SUGGESTED_URL,
            _ => SUGGESTED,
        });
        fields.push(match &setting.kind {
            SettingKind::Choice(values) => Field::Choice {
                key: setting.key.clone(),
                header: None,
                question: description.unwrap_or(title),
                options: values
                    .iter()
                    .map(|value| {
                        let suggested = default.as_deref() == Some(value);
                        ChoiceOption {
                            label: line(value, MAX_LABEL, &mut notes),
                            description: suggested.then(|| SUGGESTED.to_owned()),
                            preview: None,
                            recommended: suggested,
                        }
                    })
                    .collect(),
                multi: false,
                other: false,
                required: setting.required,
                note: false,
            },
            kind => Field::Text {
                key: setting.key.clone(),
                title,
                description: marked(mark, description, &mut notes),
                placeholder: None,
                format: (*kind == SettingKind::Url).then_some(TextFormat::Uri),
                min_length: None,
                max_length: None,
                default,
                required: setting.required,
            },
        });
    }
    let mut message = format!(
        "Ajouter l'intégration « {} » : {}",
        integration.name,
        line(&integration.description, MAX_QUESTION, &mut notes)
    );
    if !integration.network.is_empty() {
        let (origins, missing) = origins_of(&integration.network, prefill);
        message.push_str(&format!(
            "\nElle pourra joindre : {}.",
            shown_places(&origins, &missing)
        ));
    }
    for credential in &integration.credentials {
        let (origins, missing) = origins_of(&credential.origins, prefill);
        let places = shown_places(&origins, &missing);
        let (what, title) = match credential.kind {
            CredentialKind::Basic => ("mot de passe", "Mot de passe"),
            CredentialKind::Bearer => ("jeton", "Jeton"),
        };
        if credential.kind == CredentialKind::Basic {
            let key = username_key(&credential.name);
            let default = prefill.get(&key).cloned();
            fields.push(Field::Text {
                key,
                title: format!("Identifiant « {} »", credential.name),
                description: default.is_some().then(|| SUGGESTED.to_owned()),
                placeholder: None,
                format: None,
                min_length: None,
                max_length: None,
                default,
                required: true,
            });
        }
        fields.push(Field::Secret {
            key: secret_key(&credential.name),
            title: format!("{title} « {} »", credential.name),
            description: Some(line(
                &format!("Gardé scellé, envoyé seulement à {places}, jamais montré à l'agent."),
                MAX_DESCRIPTION,
                &mut notes,
            )),
            required: true,
        });
        message.push_str(&format!(
            "\nSon {what} « {} » sera gardé scellé dans Fronde et envoyé seulement à : {places}.",
            credential.name
        ));
        for planned in replaces.iter().filter(|p| p.credential == credential.name) {
            message.push_str(&format!(
                "\nIl remplace le secret « {} » déjà gardé pour {}.",
                planned.name, planned.origin
            ));
        }
    }
    let reaches: Vec<Reach> = integration
        .network
        .iter()
        .chain(integration.credentials.iter().flat_map(|c| &c.origins))
        .cloned()
        .collect();
    if from_prefill(&reaches, prefill) {
        message.push_str(
            "\nCes adresses viennent d'URL suggérées par l'agent : vérifie-les avant d'ajouter.",
        );
    }
    let from_settings = reaches
        .iter()
        .any(|reach| matches!(reach, Reach::Setting(_)));
    if from_settings {
        fields.push(Field::Toggle {
            key: LOCAL.to_owned(),
            title: "Serveur sur cette machine ou son réseau local".to_owned(),
            description: Some(
                "Permet ses adresses locales (localhost, 192.168…, nas.lan), après confirmation. Laisse décoché pour un service sur Internet."
                    .to_owned(),
            ),
            default: false,
        });
    }
    message.push_str("\nFronde l'ajoutera à fronde.toml, puis la démarrera.");
    if let Some(user) = family {
        message.push_str(&format!(
            "\nDemandé par l'agent de {user}. Demande envoyée à l'administrateur : seul un administrateur ajoute une intégration."
        ));
    }
    if message.chars().count() > MAX_MESSAGE {
        return Err("its description is too long for a card".into());
    }
    let card = Card::Form {
        message,
        fields,
        submit: Some("Ajouter".to_owned()),
        truncated: notes.truncated,
        replaced: notes.replaced,
        during: Vec::new(),
    };
    card.check()?;
    Ok(card)
}

/// What the admin answered, the secrets apart.
struct Answer {
    settings: BTreeMap<String, String>,
    usernames: BTreeMap<String, String>,
    /// By credential name. Wiped when dropped.
    secrets: BTreeMap<String, SecretString>,
    local: bool,
}

fn read_answer(integration: &Integration, values: BTreeMap<String, FieldValue>) -> Answer {
    let mut answer = Answer {
        settings: BTreeMap::new(),
        usernames: BTreeMap::new(),
        secrets: BTreeMap::new(),
        local: false,
    };
    for (key, value) in values {
        if key == LOCAL {
            answer.local = matches!(value, FieldValue::Toggle(true));
            continue;
        }
        if let Some(credential) = integration
            .credentials
            .iter()
            .find(|c| key == secret_key(&c.name) || key == username_key(&c.name))
        {
            match value {
                FieldValue::Secret(given) => {
                    answer.secrets.insert(
                        credential.name.clone(),
                        SecretString::from(given.secret.into_inner()),
                    );
                }
                FieldValue::Text(text) => {
                    answer
                        .usernames
                        .insert(credential.name.clone(), text.trim().to_owned());
                }
                _ => {}
            }
            continue;
        }
        let text = match value {
            FieldValue::Text(text) => text.trim().to_owned(),
            FieldValue::Choice(choice) => choice.selected.into_iter().next().unwrap_or_default(),
            _ => continue,
        };
        if !text.is_empty() {
            answer.settings.insert(key, text);
        }
    }
    answer
}

/// Why a setup stopped once the admin answered, for the agent.
struct Failed(String);

async fn request_setup(setup: &Setup, context: CallContext, args: SetupArgs) -> ToolOutput {
    // Only the threads check whose turn asks: without them, nothing would refuse a turn a
    // sub-agent's result started.
    if setup.threads.is_none() {
        return ToolOutput::error(
            "integrations_request_setup needs Fronde's threads, which are not running; the owner adds a [[sources]] entry to fronde.toml instead",
        );
    }
    match &context.caller {
        Caller::Thread(_) if context.turn.is_some() => {}
        Caller::Thread(_) => {
            return ToolOutput::error(
                "integrations_request_setup works only during your turn; call it while answering the user",
            );
        }
        _ => {
            return ToolOutput::error(
                "only Fronde's own agents may ask for an integration's setup; the owner adds a [[sources]] entry to fronde.toml instead",
            );
        }
    }
    let available = setup.integrations.available();
    let Some(integration) = available.iter().find(|i| i.name == args.integration) else {
        let names: Vec<&str> = available.iter().map(|i| i.name.as_str()).collect();
        return ToolOutput::error(format!(
            "no integration {:?} is available to set up; {}. One already set up has its tools in search_tools; any other needs a hand-written [[sources]] entry",
            args.integration,
            if names.is_empty() {
                "none is".to_owned()
            } else {
                format!("available: {}", names.join(", "))
            }
        ));
    };
    if let Some(why) = &integration.unsupported {
        return ToolOutput::error(format!(
            "{} cannot be set up from a card: {why}; the owner writes its [[sources]] entry by hand",
            integration.name
        ));
    }
    if !integration.credentials.is_empty() && setup.requests.secrets().is_none() {
        return ToolOutput::error(format!(
            "{} needs a secret, and this Fronde keeps none; the owner writes its [[sources]] entry by hand",
            integration.name
        ));
    }
    let timeout = match check_timeout(args.timeout_ms) {
        Ok(timeout) => timeout,
        Err(message) => return ToolOutput::error(message),
    };
    let prefill = match check_prefill(integration, &*setup.integrations, &args.prefill) {
        Ok(prefill) => prefill,
        Err(message) => return ToolOutput::error(message),
    };
    let family = (context.user != setup.admin).then_some(context.user.as_str());
    let shown = Destinations::of(integration, &prefill);
    let replaces: Vec<Planned> = match plan(setup, integration, &shown).await {
        Ok(planned) => planned.into_iter().filter(|p| p.replaces).collect(),
        Err(Failed(message)) => return ToolOutput::error(message),
    };
    let card = match setup_card(integration, &prefill, family, &replaces) {
        Ok(card) => card,
        Err(error) => {
            tracing::error!(
                "{TOOL}: cannot build the card of {}: {error}",
                integration.name
            );
            return ToolOutput::error(format!(
                "{}'s card cannot be shown; the owner writes its [[sources]] entry by hand",
                integration.name
            ));
        }
    };
    let answer = match ask(setup, &context, card, timeout).await {
        Ok(Resolution::Answered(Response::Form { values })) => read_answer(integration, values),
        Ok(Resolution::Declined) => {
            return ToolOutput::ok(json!({"status": "declined"}))
                .with_next("The owner declined; do not ask again this turn.");
        }
        Ok(Resolution::Expired) => {
            return ToolOutput::ok(json!({"status": "expired"}))
                .with_next("No answer in time; say in your reply what you wanted to set up.");
        }
        Ok(Resolution::Cancelled) => {
            return ToolOutput::ok(json!({"status": "cancelled"})).with_next(
                "The request was cancelled; stop and wait for the user's next message.",
            );
        }
        Ok(_) => return ToolOutput::error("the answer does not fit the card; nothing was set up"),
        Err(output) => return output,
    };
    let shown = Shown {
        destinations: shown,
        replaces,
    };
    match apply(setup, &context, integration, answer, &shown, timeout).await {
        Ok(output) => output,
        Err(Failed(message)) => ToolOutput::error(message),
    }
}

/// Shows `card` to the admin, as a card of the calling thread's turn, and waits for its end. The
/// card is cancelled when the caller gives the call up: nobody would act on its answer.
async fn ask(
    setup: &Setup,
    context: &CallContext,
    card: Card,
    timeout: Duration,
) -> Result<Resolution, ToolOutput> {
    let secret_fields = card.asks_secret();
    let ask = Ask {
        topic: context.topic.clone(),
        turn: context.turn,
        caller: context.caller.clone(),
        client_request_id: None,
        card,
        for_user: setup.admin.clone(),
        admin_only: true,
        timeout,
        // Only this call acts on the answer: after a restart, or once the call is cut, nobody
        // would, so the card expires or is cancelled rather than answered late.
        tied: true,
        secret_fields,
    };
    let asking = Asking {
        requests: setup.requests.clone(),
        threads: setup.threads.clone(),
    };
    let waiter: Waiter = match asking.open(TOOL, context, ask).await {
        Ok(waiter) => waiter,
        Err(NotShown::Refused(message)) => return Err(ToolOutput::error(message)),
        Err(NotShown::TurnOver) => {
            return Err(ToolOutput::error(
                "the card was not shown: your turn ended before it could be; ask again in a later turn",
            ));
        }
        Err(NotShown::Reused(_) | NotShown::Failed) => {
            return Err(ToolOutput::error(
                "the card could not be shown to the owner; say in your reply what you wanted to set up",
            ));
        }
    };
    let id = waiter.id();
    // An answer that came first is taken; `apply` then sees a call cut since and acts on
    // nothing.
    tokio::select! {
        biased;
        resolution = waiter.wait() => Ok(resolution),
        () = context.cancelled.cancelled() => {
            if let Err(error) = setup.requests.cancel(id).await {
                tracing::error!("cannot cancel request {id}: {error:#}");
            }
            Ok(Resolution::Cancelled)
        }
    }
}

/// What the setup card showed: where the prefill sent what, and the stored secrets it said it
/// replaces.
struct Shown {
    destinations: Destinations,
    replaces: Vec<Planned>,
}

/// Why the call stops once it was cut, for nobody (the caller is gone) and the log: `undone`
/// says what became of the secrets it had stored, when it had.
fn cut(undone: &str) -> Failed {
    Failed(if undone.is_empty() {
        "the call was cut before the integration was set up; nothing was kept or written".into()
    } else {
        format!("the call was cut before the integration was added; nothing was written; {undone}")
    })
}

/// Removes the secrets this setup stored under new names (`kept`), and says what is left: a
/// secret it replaced keeps the value the admin typed, the one before is gone.
async fn undo(setup: &Setup, kept: &[Planned]) -> String {
    let mut replaced = Vec::new();
    for planned in kept {
        if planned.replaces {
            replaced.push(planned.name.clone());
            continue;
        }
        if let Err(error) = setup
            .requests
            .forget_secret(&planned.name, &planned.origin.to_string())
            .await
        {
            tracing::error!(
                "{TOOL}: cannot remove the secret {} it stored: {error:#}",
                planned.name
            );
            replaced.push(planned.name.clone());
        }
    }
    if replaced.is_empty() {
        "no secret was kept".into()
    } else {
        format!(
            "these stored secrets now hold what the owner typed: {}",
            replaced.join(", ")
        )
    }
}

/// Whether `origin`'s host looks like this machine or its network: a loopback or private
/// address, or a name only a local resolver knows (no dot, `.lan`, `.local`, `.home.arpa`).
/// Only such an origin may be allowed as local from a card; any other is written by hand.
fn looks_local(origin: &Origin) -> bool {
    let v4 = |ip: std::net::Ipv4Addr| ip.is_loopback() || ip.is_private();
    match origin.host() {
        url::Host::Domain(name) => {
            let name = name.strip_suffix('.').unwrap_or(name);
            !name.contains('.')
                || [".lan", ".local", ".home.arpa"]
                    .iter()
                    .any(|suffix| name.ends_with(suffix))
        }
        url::Host::Ipv4(ip) => v4(*ip),
        url::Host::Ipv6(ip) => match ip.to_ipv4_mapped() {
            Some(mapped) => v4(mapped),
            None => ip.is_loopback() || ip.is_unique_local(),
        },
    }
}

/// A user name typed on the card, checked as `prefill`'s are: one line of plain text, without
/// `:`, which Basic authorization cannot carry. What to fix, else.
fn check_username(credential: &str, username: &str) -> Result<(), String> {
    if username.chars().count() > MAX_TEXT || first_hidden(username, false).is_some() {
        return Err(format!(
            "the user name typed for {credential} must be one line of plain text of at most {MAX_TEXT} characters"
        ));
    }
    if username.contains(':') {
        return Err(format!(
            "the user name typed for {credential} contains ':', which a user name sent as Basic authorization cannot hold"
        ));
    }
    Ok(())
}

/// Sets the integration up as the admin answered: checks, a confirmation when the answer sends
/// something where the card did not say, allows local addresses or replaces a stored secret the
/// card did not name, then, under the integration's lock, the secrets sealed, a dry run, and the
/// entry written and started. A call cut meanwhile keeps nothing more.
async fn apply(
    setup: &Setup,
    context: &CallContext,
    integration: &Integration,
    answer: Answer,
    shown: &Shown,
    timeout: Duration,
) -> Result<ToolOutput, Failed> {
    let name = &integration.name;
    setup
        .integrations
        .check_settings(name, &answer.settings, true)
        .map_err(|error| {
            Failed(format!(
                "the owner's settings do not fit {name}: {error}; nothing was set up, ask again with corrected prefill"
            ))
        })?;
    for (credential, username) in &answer.usernames {
        check_username(credential, username)
            .map_err(|error| Failed(format!("{error}; nothing was set up, ask again")))?;
    }
    let destinations = Destinations::of(integration, &answer.settings);
    let from_settings: Vec<Origin> = integration
        .network
        .iter()
        .filter_map(|reach| match reach {
            Reach::Setting(key) => answer.settings.get(key).and_then(|url| origin_of(url)),
            Reach::Origin(_) => None,
        })
        .collect();
    // The toggle allows local addresses for the origins of the URL settings only, and only
    // those that look local: any other is refused, not allowed quietly.
    let local: Vec<Origin> = if answer.local {
        if let Some(far) = from_settings.iter().find(|origin| !looks_local(origin)) {
            return Err(Failed(format!(
                "the owner ticked the box for a server on this machine or its local network, but {far} does not look local: a card allows local addresses only for a loopback or private address, a name without a dot, or one ending in .lan, .local or .home.arpa; nothing was set up. Ask again with the box unticked, or the owner writes {{ origin = \"{far}\", local = true }} in fronde.toml by hand"
            )));
        }
        destinations
            .allow
            .iter()
            .filter(|origin| from_settings.contains(origin))
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let mut entry = ComponentConfig {
        name: name.clone(),
        path: integration.path.clone(),
        manifest: None,
        sha256: None,
        manifest_sha256: None,
        allow: destinations
            .allow
            .iter()
            .map(|origin| Allow {
                origin: origin.clone(),
                local: local.contains(origin),
            })
            .collect(),
        settings: answer.settings.clone(),
        credentials: Vec::new(),
    };
    for (credential, origins) in &destinations.credentials {
        if origins.is_empty() {
            return Err(Failed(format!(
                "the credential {credential} has no origin: fill the URL settings it is for; nothing was set up"
            )));
        }
        if let Some(plain) = origins.iter().find(|o| !o.carries_credentials()) {
            return Err(Failed(format!(
                "{plain} is plain http to another machine, where a password would cross the network readable; use its https:// address; nothing was set up"
            )));
        }
    }
    // Nothing kept yet: the settings, the origins allowed and the approval checked first.
    setup.integrations.dry_run(&entry).await.map_err(|error| {
        Failed(format!(
            "{name} cannot be set up as answered: {error:#}; nothing was set up"
        ))
    })?;
    let planned = plan(setup, integration, &destinations).await?;
    let unannounced: Vec<&Planned> = planned
        .iter()
        .filter(|p| p.replaces && !shown.replaces.contains(p))
        .collect();
    if answer.local || !destinations.within(&shown.destinations) || !unannounced.is_empty() {
        confirm(
            setup,
            context,
            integration,
            &destinations,
            answer.local.then_some(local.as_slice()),
            &planned,
            timeout,
        )
        .await?;
    }
    if context.cancelled.is_cancelled() {
        return Err(cut(""));
    }
    // From the first secret stored to the entry added, no other setup of this integration
    // runs: one set up meanwhile is refused here, before anything is stored over it. A call
    // cut while it waits stops there.
    let _setting_up = tokio::select! {
        biased;
        () = context.cancelled.cancelled() => return Err(cut("")),
        guard = setup.setting_up.lock(name) => guard,
    };
    if context.cancelled.is_cancelled() {
        return Err(cut(""));
    }
    if setup.sources.health().contains_key(name.as_str())
        || !setup
            .integrations
            .available()
            .iter()
            .any(|available| available.name == *name)
    {
        return Err(Failed(format!(
            "{name} was set up meanwhile, by another card; nothing was kept from this one"
        )));
    }
    // The file as it is now, before any secret is stored: an entry of that name added by hand
    // since the card was shown leaves every stored secret as it was.
    setup
        .integrations
        .check_file(&entry)
        .await
        .map_err(|error| {
            Failed(format!(
                "{name} cannot be added to fronde.toml: {error:#}; nothing was kept"
            ))
        })?;
    if plan(setup, integration, &destinations).await? != planned {
        return Err(Failed(
            "the stored secrets changed while the card was open; nothing was kept, ask again"
                .into(),
        ));
    }
    let mut kept = Vec::new();
    let keeping = keep_secrets(setup, integration, &planned, &answer, &mut kept).await;
    drop(answer);
    let (credentials, new_key) = match keeping {
        Ok(keeping) => keeping,
        Err(Failed(message)) => {
            let undone = undo(setup, &kept).await;
            return Err(Failed(format!("{message}; {undone}")));
        }
    };
    entry.credentials = credentials;
    let stored: Vec<String> = planned.iter().map(|p| p.name.clone()).collect();
    if context.cancelled.is_cancelled() {
        return Err(cut(&undo(setup, &kept).await));
    }
    if let Err(error) = setup.integrations.dry_run(&entry).await {
        let undone = undo(setup, &kept).await;
        return Err(Failed(format!(
            "{name} cannot be set up as answered: {error:#}; the configuration is unchanged; {undone}"
        )));
    }
    if context.cancelled.is_cancelled() {
        return Err(cut(&undo(setup, &kept).await));
    }
    let written = setup
        .integrations
        .add(&entry, &setup.sources)
        .await
        .map_err(|error| {
            tracing::error!("{TOOL}: cannot add {name}: {error:#}");
            Failed(format!(
                "{name} could not be added ({error:#}); its secrets stay stored ({}); say so in your reply",
                stored.join(", ")
            ))
        })?;
    let tools: Vec<String> = setup
        .registry
        .all()
        .into_iter()
        .filter(|tool| tool.source == *name)
        .map(|tool| tool.name)
        .collect();
    let mut result = json!({
        "status": "set_up",
        "integration": name,
        "tools": tools,
        "stored_secrets": stored,
    });
    if let Some(place) = new_key {
        result["master_key_made"] = Value::String(key_place(place).to_owned());
    }
    let mut next =
        String::from("Its tools are ready: call them with use_tool (search_tools lists them).");
    match written {
        Written::File => {
            result["config"] = json!("written");
        }
        Written::Snippet(snippet) => {
            result["config"] = json!("not_written");
            result["snippet"] = json!(snippet);
            next.push_str(" fronde.toml could not be written: the owner was shown the entry to paste into it; until then the integration lasts until Fronde restarts. Tell the user.");
            show_snippet(setup, context, name, &snippet).await;
        }
    }
    if new_key.is_some() {
        next.push_str(" Keeping its secret made Fronde's master key: tell the user to back it up apart from the data directory.");
    }
    Ok(ToolOutput::ok(result).with_next(next))
}

fn key_place(place: KeyPlace) -> &'static str {
    match place {
        KeyPlace::Keychain => "keychain",
        KeyPlace::File => "file",
        KeyPlace::Systemd => "systemd",
    }
}

/// Asks the admin to confirm where the entry sends what, before anything is kept: when the
/// answer sends a secret or allows an origin the card did not show, when it allows local
/// addresses (always: `local`, each origin they are allowed for), or when a secret replaces a
/// stored one the card did not name. The card lists every origin, every local one, and every
/// secret replaced.
async fn confirm(
    setup: &Setup,
    context: &CallContext,
    integration: &Integration,
    destinations: &Destinations,
    local: Option<&[Origin]>,
    planned: &[Planned],
    timeout: Duration,
) -> Result<(), Failed> {
    let mut message = format!("Confirme l'ajout de « {} ».", integration.name);
    if !destinations.allow.is_empty() {
        let places: Vec<String> = destinations.allow.iter().map(ToString::to_string).collect();
        message.push_str(&format!("\nElle pourra joindre : {}.", places.join(", ")));
    }
    if let Some(local) = local {
        let places: Vec<String> = local.iter().map(ToString::to_string).collect();
        message.push_str(&format!(
            "\nAdresses locales permises (cette machine ou son réseau) pour : {}.",
            if places.is_empty() {
                "aucune origine".to_owned()
            } else {
                places.join(", ")
            }
        ));
    }
    for (credential, origins) in &destinations.credentials {
        let places: Vec<String> = origins.iter().map(ToString::to_string).collect();
        message.push_str(&format!(
            "\nSon secret « {credential} » ira seulement à : {}.",
            places.join(", ")
        ));
    }
    for planned in planned.iter().filter(|p| p.replaces) {
        message.push_str(&format!(
            "\nIl remplace le secret « {} » déjà gardé pour {}.",
            planned.name, planned.origin
        ));
    }
    let card = Card::Form {
        message,
        fields: Vec::new(),
        submit: Some("Confirmer".to_owned()),
        truncated: false,
        replaced: false,
        during: Vec::new(),
    };
    match ask(setup, context, card, timeout).await {
        Ok(Resolution::Answered(_)) => Ok(()),
        Ok(_) => Err(Failed(
            "the owner did not confirm where the integration would send its secrets; nothing was set up".into(),
        )),
        Err(output) => Err(Failed(format!(
            "the confirmation could not be asked ({}); nothing was set up",
            output.value
        ))),
    }
}

/// Where one secret of the answer is kept: its credential, the origin it is sealed for, and the
/// name it is stored under, which `replaces` a secret stored there for that origin.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Planned {
    credential: String,
    origin: Origin,
    name: String,
    replaces: bool,
}

/// Where each credential's secret would be kept, once per origin of its credential.
async fn plan(
    setup: &Setup,
    integration: &Integration,
    destinations: &Destinations,
) -> Result<Vec<Planned>, Failed> {
    let mut planned: Vec<Planned> = Vec::new();
    for credential in &integration.credentials {
        let origins = destinations
            .credentials
            .iter()
            .find(|(name, _)| *name == credential.name)
            .map(|(_, origins)| origins.clone())
            .unwrap_or_default();
        for origin in origins {
            let taken: Vec<String> = planned.iter().map(|p| p.name.clone()).collect();
            let (name, replaces) =
                free_name(setup, integration, &credential.name, &origin, &taken).await?;
            planned.push(Planned {
                credential: credential.name.clone(),
                origin,
                name,
                replaces,
            });
        }
    }
    Ok(planned)
}

/// Seals each secret answered as `planned`, each one stored added to `kept`, and returns the
/// entry's credentials naming them, and where the master key was made, if keeping them made
/// it. A name stored meanwhile is not replaced unless the plan said so.
async fn keep_secrets(
    setup: &Setup,
    integration: &Integration,
    planned: &[Planned],
    answer: &Answer,
    kept: &mut Vec<Planned>,
) -> Result<(Vec<CredentialConfig>, Option<KeyPlace>), Failed> {
    for credential in &integration.credentials {
        if !answer.secrets.contains_key(&credential.name) {
            return Err(Failed(format!(
                "the owner gave no secret for {}; nothing was set up",
                credential.name
            )));
        }
    }
    let mut credentials = Vec::new();
    let mut new_key = None;
    for planned in planned {
        let Some(credential) = integration
            .credentials
            .iter()
            .find(|c| c.name == planned.credential)
        else {
            continue;
        };
        let Some(secret) = answer.secrets.get(&credential.name) else {
            continue;
        };
        let name = &planned.name;
        let made = setup
            .requests
            .keep_secret(name, &planned.origin.to_string(), secret, planned.replaces)
            .await
            .map_err(|error| {
                tracing::error!("{TOOL}: cannot keep the secret {name}: {error:#}");
                Failed(format!(
                    "the secret {name} could not be kept; nothing was written to the configuration"
                ))
            })?;
        kept.push(planned.clone());
        new_key = new_key.or(made);
        let stored = Secret::Stored {
            stored: name.clone(),
        };
        let origin = planned.origin.clone();
        credentials.push(match credential.kind {
            CredentialKind::Basic => CredentialConfig {
                origin,
                basic: Some(BasicCredential {
                    username: answer
                        .usernames
                        .get(&credential.name)
                        .cloned()
                        .unwrap_or_default(),
                    password: stored,
                }),
                bearer: None,
            },
            CredentialKind::Bearer => CredentialConfig {
                origin,
                basic: None,
                bearer: Some(stored),
            },
        });
    }
    Ok((credentials, new_key))
}

/// A name to store `credential`'s secret for `origin` under: `<integration>_<credential>`, with
/// a number after it when that name is taken by this setup or stored for another origin; and
/// whether a secret is stored under it for `origin` already, which keeping replaces.
async fn free_name(
    setup: &Setup,
    integration: &Integration,
    credential: &str,
    origin: &Origin,
    taken: &[String],
) -> Result<(String, bool), Failed> {
    let base = format!("{}_{}", integration.name, credential.replace('-', "_"));
    let base = if is_valid_name(&base) {
        base
    } else {
        format!("{}_secret", integration.name)
    };
    for n in 1..=20 {
        let name = if n == 1 {
            base.clone()
        } else {
            format!("{base}_{n}")
        };
        if taken.contains(&name) {
            continue;
        }
        let stored = setup.requests.stored_origin(&name).await.map_err(|error| {
            tracing::error!("{TOOL}: cannot read the stored secrets: {error:#}");
            Failed("the stored secrets cannot be read; nothing was set up".into())
        })?;
        match stored {
            None => return Ok((name, false)),
            Some(stored) if stored == origin.to_string() => return Ok((name, true)),
            Some(_) => {}
        }
    }
    Err(Failed(format!(
        "no free name to keep the secret {credential} under; remove old ones with `fronde secret remove`; nothing was set up"
    )))
}

/// Shows the admin the entry to paste into a configuration file Fronde cannot write, on a card
/// of its own, outside the turn: it stays until read, or for an hour.
async fn show_snippet(setup: &Setup, context: &CallContext, name: &str, snippet: &str) {
    let message = format!(
        "fronde.toml ne peut pas être écrit (lecture seule ?) : « {name} » marche jusqu'au prochain redémarrage. Pour la garder, colle cette entrée à la fin de fronde.toml :\n\n{snippet}"
    );
    let card = Card::Form {
        message,
        fields: Vec::new(),
        submit: Some("C'est noté".to_owned()),
        truncated: false,
        replaced: false,
        during: Vec::new(),
    };
    if let Err(error) = card.check() {
        tracing::warn!(
            "{TOOL}: the entry to paste does not fit a card ({error}); it went to the agent only"
        );
        return;
    }
    let ask = Ask {
        topic: context.topic.clone(),
        turn: None,
        caller: context.caller.clone(),
        client_request_id: None,
        card,
        for_user: setup.admin.clone(),
        admin_only: true,
        timeout: SNIPPET_TIMEOUT,
        tied: true,
        secret_fields: false,
    };
    // Nobody waits on it: reading it is all it asks.
    if let Err(error) = setup.requests.ask(ask).await {
        tracing::error!("{TOOL}: cannot show the entry to paste: {error:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn integration() -> Integration {
        Integration {
            name: "calendar".into(),
            description: "Calendar and reminder tools over CalDAV.".into(),
            path: "/app/components/calendar.wasm".into(),
            tool_prefixes: vec!["calendar_".into(), "reminders_".into()],
            settings: vec![Setting {
                key: "events_url".into(),
                description: Some("The events.".into()),
                required: true,
                kind: SettingKind::Url,
            }],
            credentials: vec![CredentialAsk {
                name: "caldav".into(),
                kind: CredentialKind::Basic,
                origins: vec![Reach::Setting("events_url".into())],
            }],
            network: vec![Reach::Setting("events_url".into())],
            unsupported: None,
        }
    }

    #[test]
    fn search_words_find_an_integration() {
        let available = [integration()];
        assert_eq!(matching(&available, "add an event to my calendar").len(), 1);
        assert_eq!(matching(&available, "Calendars, please").len(), 1);
        assert_eq!(matching(&available, "reminders").len(), 1);
        assert!(matching(&available, "shopping list").is_empty());
    }

    #[test]
    fn the_card_says_where_each_secret_goes() {
        let prefill = BTreeMap::from([(
            "events_url".to_owned(),
            "https://cloud.example/dav/".to_owned(),
        )]);
        let Card::Form {
            message, fields, ..
        } = setup_card(&integration(), &prefill, None, &[]).unwrap()
        else {
            panic!("a form");
        };
        assert!(
            message.contains("joindre : https://cloud.example."),
            "{message}"
        );
        assert!(
            message.contains("envoyé seulement à : https://cloud.example."),
            "{message}"
        );
        assert!(matches!(
            fields.last(),
            Some(Field::Toggle { key, default: false, .. }) if key == LOCAL
        ));
        assert!(fields.iter().any(|f| matches!(
            f,
            Field::Secret { description: Some(d), .. } if d.contains("https://cloud.example")
        )));
        let Card::Form { message, .. } =
            setup_card(&integration(), &BTreeMap::new(), Some("lea"), &[]).unwrap()
        else {
            panic!("a form");
        };
        assert!(message.contains("l'origine de events_url"), "{message}");
        assert!(
            message.contains("Demande envoyée à l'administrateur"),
            "{message}"
        );
    }

    #[test]
    fn only_a_local_looking_host_may_be_allowed_as_local() {
        let local = |origin: &str| looks_local(&Origin::parse(origin).unwrap());
        for origin in [
            "http://localhost:5232",
            "https://127.0.0.1",
            "https://[::1]",
            "https://192.168.1.10",
            "https://10.0.0.2",
            "https://172.16.4.1",
            "https://[fd00::1]",
            "https://nas",
            "https://nas.lan",
            "https://nas.local",
            "https://nas.home.arpa",
        ] {
            assert!(local(origin), "{origin}");
        }
        for origin in [
            "https://cloud.example",
            "https://nas.example.com",
            "https://8.8.8.8",
            "https://169.254.169.254",
            "https://100.64.0.1",
            "https://[2001:db8::1]",
            "https://lan.example",
        ] {
            assert!(!local(origin), "{origin}");
        }
    }

    #[test]
    fn a_typed_user_name_is_one_line_without_a_colon() {
        assert!(check_username("caldav", "alix").is_ok());
        assert!(check_username("caldav", "a:b").unwrap_err().contains("':'"));
        assert!(
            check_username("caldav", "a\nb")
                .unwrap_err()
                .contains("one line")
        );
        assert!(
            check_username("caldav", "a\u{202e}b")
                .unwrap_err()
                .contains("one line")
        );
    }

    #[test]
    fn destinations_within_what_was_shown() {
        let at = |url: &str| BTreeMap::from([("events_url".to_owned(), url.to_owned())]);
        let shown = Destinations::of(&integration(), &at("https://cloud.example/a"));
        assert!(Destinations::of(&integration(), &at("https://cloud.example/b")).within(&shown));
        assert!(!Destinations::of(&integration(), &at("https://evil.example/a")).within(&shown));
        assert!(
            !Destinations::of(&integration(), &at("https://cloud.example/a"))
                .within(&Destinations::of(&integration(), &BTreeMap::new()))
        );
    }
}
