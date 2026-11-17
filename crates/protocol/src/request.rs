//! Requests to the user: what a card shows, what the page answers, how the request ended, and
//! the rules an answer must follow. The server and the page check answers with the same
//! [`check`], so the page can say what is wrong before sending, and the server stays the judge.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use zeroize::Zeroize;

/// A request's number: unique for the life of the database, whatever stream shows it.
pub type RequestId = u64;

/// The longest free text an answer may carry ("Autre" or a note), in characters.
pub const MAX_TEXT: usize = 2000;

/// The longest secret a card takes, in bytes: far more than any password or token, and well
/// under the chat's largest message.
pub const MAX_SECRET: usize = 16 * 1024;

/// The longest title of a permission card, in characters.
pub const MAX_PERMISSION_TITLE: usize = 200;

/// The longest body of a permission card (what the tool would do), in characters.
pub const MAX_PERMISSION_BODY: usize = 4000;

/// The longest line of a permission card's body, in characters.
pub const MAX_PERMISSION_LINE: usize = 300;

/// The longest detail of a permission option (the agent's own words for it), in characters.
pub const MAX_OPTION_DETAIL: usize = 160;

/// The longest id of a permission option: agents use short words such as `allow-once`.
pub const MAX_OPTION_ID: usize = 128;

/// A question card's texts, short enough for a phone.
pub const MAX_QUESTION: usize = 300;
pub const MAX_HEADER: usize = 12;
pub const MAX_LABEL: usize = 60;
pub const MAX_DESCRIPTION: usize = 200;
pub const MAX_PREVIEW: usize = 4000;
pub const MAX_MESSAGE: usize = 2000;
pub const MAX_KEY: usize = 64;
/// The most fields a form shows, and the most options a question offers: a form from an
/// elicitation may be longer than `user_ask`'s, never endless.
pub const MAX_FIELDS: usize = 12;
pub const MAX_OPTIONS: usize = 50;

/// A secret card's texts.
pub const MAX_SECRET_LABEL: usize = 60;
pub const MAX_REASON: usize = 300;
pub const MAX_PLACEHOLDER: usize = 60;
pub const MAX_URL: usize = 2000;

/// What a request's card shows. Never a secret.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Card {
    /// The agent asks to use a tool. Its texts come from the agent: one line of at most
    /// [`MAX_PERMISSION_TITLE`] characters, a body of at most [`MAX_PERMISSION_BODY`], neither
    /// with a control or bidirectional character.
    Permission {
        /// What the tool would do, in one line.
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<ToolKind>,
        /// What the tool would do, in detail: the command, the files, a summary of the changes,
        /// a plan.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
        /// The agent's own options, in its order.
        options: Vec<PermissionChoice>,
        /// Part of the title or the body was left out to fit: the card says so.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
        /// Hidden characters were replaced by `�`: the card says so.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        replaced: bool,
    },
    /// Questions to answer together, then sent at once.
    Form {
        /// Shown above the fields; empty when the fields say it all.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        message: String,
        fields: Vec<Field>,
        /// The send button's label, "Envoyer" when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        submit: Option<String>,
        /// Part of a text was left out to fit (an elicitation's): the card says so.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
        /// Hidden characters were replaced by `�`: the card says so.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        replaced: bool,
    },
    /// One secret, typed in a password box. Its value never comes back: not in the card, the
    /// stream, the request's row or the asker's answer.
    Secret {
        /// What to type, in a few words.
        label: String,
        /// Why it is needed.
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        /// A sign-in or setup page, over https, shown as a button naming its host.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        /// Offered to an admin besides "this conversation": keep the secret, sealed, under this
        /// name for this origin.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remember: Option<Remember>,
    },
}

impl Card {
    /// Checks that every text of the card fits its cap and holds no hidden character (see
    /// [`crate::text::hidden`]; line breaks and tabs only where a text may span lines), and that
    /// a permission's options are offered once each: the server's last word on what a card may
    /// show, whoever built it. The error says what to fix, in English, for the asker.
    pub fn check(&self) -> Result<(), String> {
        let text = fits;
        match self {
            Self::Permission {
                title,
                body,
                options,
                ..
            } => {
                text("title", title, MAX_PERMISSION_TITLE, false)?;
                if let Some(body) = body {
                    text("body", body, MAX_PERMISSION_BODY, true)?;
                    for line in body.lines() {
                        text("a line of the body", line, MAX_PERMISSION_LINE, true)?;
                    }
                }
                if options.is_empty() {
                    return Err("a permission card needs at least one option".into());
                }
                let mut ids = BTreeSet::new();
                for option in options {
                    if option.id.is_empty() || !ids.insert(&option.id) {
                        return Err(format!("option id {:?} is empty or repeated", option.id));
                    }
                    text("an option id", &option.id, MAX_OPTION_ID, false)?;
                    if let Some(detail) = &option.detail {
                        text("an option detail", detail, MAX_OPTION_DETAIL, false)?;
                    }
                }
            }
            Self::Form {
                message,
                fields,
                submit,
                ..
            } => {
                text("message", message, MAX_MESSAGE, true)?;
                if let Some(submit) = submit {
                    text("submit", submit, MAX_LABEL, false)?;
                }
                if fields.len() > MAX_FIELDS {
                    return Err(format!(
                        "the form has {} fields; use at most {MAX_FIELDS}",
                        fields.len()
                    ));
                }
                let mut keys = BTreeSet::new();
                for field in fields {
                    let key = field.key();
                    if key.is_empty() || !keys.insert(key) {
                        return Err(format!("field key {key:?} is empty or repeated"));
                    }
                    text("a field key", key, MAX_KEY, false)?;
                    check_field(field)?;
                }
            }
            Self::Secret {
                label,
                reason,
                placeholder,
                url,
                remember,
            } => {
                text("label", label, MAX_SECRET_LABEL, false)?;
                text("reason", reason, MAX_REASON, false)?;
                if let Some(placeholder) = placeholder {
                    text("placeholder", placeholder, MAX_PLACEHOLDER, false)?;
                }
                if let Some(url) = url {
                    text("url", url, MAX_URL, false)?;
                }
                if let Some(remember) = remember {
                    text("remember.name", &remember.name, MAX_KEY, false)?;
                    text("remember.origin", &remember.origin, MAX_URL, false)?;
                }
            }
        }
        Ok(())
    }

    /// The card's kind, as stored with its request.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Permission { .. } => "permission",
            Self::Form { .. } => "form",
            Self::Secret { .. } => "secret",
        }
    }
}

/// Whether `text`, called `what` in the error, has at most `max` characters and no hidden
/// character (line breaks and tabs allowed when `multiline`).
fn fits(what: &str, text: &str, max: usize, multiline: bool) -> Result<(), String> {
    let length = text.chars().count();
    if length > max {
        return Err(format!("{what} has {length} characters; use at most {max}"));
    }
    match crate::text::first_hidden(text, multiline) {
        Some(c) => Err(format!(
            "{what} contains the hidden character U+{:04X}; remove it",
            u32::from(c)
        )),
        None => Ok(()),
    }
}

/// A field's own texts and bounds, by [`fits`].
fn check_field(field: &Field) -> Result<(), String> {
    let text = fits;
    let description = |description: &Option<String>| match description {
        Some(description) => text("description", description, MAX_DESCRIPTION, false),
        None => Ok(()),
    };
    match field {
        Field::Choice {
            header,
            question,
            options,
            ..
        } => {
            if let Some(header) = header {
                text("header", header, MAX_HEADER, false)?;
            }
            text("question", question, MAX_QUESTION, false)?;
            if options.is_empty() || options.len() > MAX_OPTIONS {
                return Err(format!(
                    "a question has {} options; give 1 to {MAX_OPTIONS}",
                    options.len()
                ));
            }
            let mut labels = BTreeSet::new();
            for option in options {
                text("label", &option.label, MAX_LABEL, false)?;
                // The answer names an option by its label.
                if option.label.trim().is_empty() || !labels.insert(&option.label) {
                    return Err(format!("label {:?} is empty or repeated", option.label));
                }
                if let Some(description) = &option.description {
                    text("description", description, MAX_DESCRIPTION, false)?;
                }
                if let Some(preview) = &option.preview {
                    text("preview", preview, MAX_PREVIEW, true)?;
                }
            }
        }
        Field::Text {
            title,
            description: about,
            placeholder,
            min_length,
            max_length,
            default,
            ..
        } => {
            text("title", title, MAX_QUESTION, false)?;
            description(about)?;
            if let Some(placeholder) = placeholder {
                text("placeholder", placeholder, MAX_PLACEHOLDER, false)?;
            }
            let min = min_length.map_or(0, |min| min as usize);
            if min > MAX_TEXT || max_length.is_some_and(|max| (max as usize) < min) {
                return Err(format!(
                    "a text field asks for {min_length:?} to {max_length:?} characters; at most {MAX_TEXT} fit"
                ));
            }
            if let Some(default) = default {
                text("default", default, MAX_TEXT, false)?;
            }
        }
        Field::Number {
            title,
            description: about,
            min,
            max,
            default,
            ..
        } => {
            text("title", title, MAX_QUESTION, false)?;
            description(about)?;
            let bounds = [min, max, default].into_iter().flatten();
            if bounds.clone().any(|n| !n.is_finite()) {
                return Err("a number field's bounds and default must be finite".into());
            }
            if min.zip(*max).is_some_and(|(min, max)| min > max) {
                return Err(format!(
                    "a number field's minimum {min:?} is above its maximum {max:?}"
                ));
            }
        }
        Field::Toggle {
            title,
            description: about,
            ..
        } => {
            text("title", title, MAX_QUESTION, false)?;
            description(about)?;
        }
    }
    Ok(())
}

/// One option of a permission card.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct PermissionChoice {
    /// The agent's id for the option: what the answer names.
    pub id: String,
    pub kind: PermissionKind,
    /// The agent's own words for the option, when they say more than its kind, such as what
    /// "always" covers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// What a permission option grants, as ACP names it.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

impl PermissionKind {
    /// The button's label: the card names options by kind, whatever the agent calls them.
    pub fn label(self) -> &'static str {
        match self {
            Self::AllowOnce => "Autoriser une fois",
            Self::AllowAlways => "Toujours autoriser",
            Self::RejectOnce => "Refuser",
            Self::RejectAlways => "Toujours refuser",
        }
    }

    pub fn allows(self) -> bool {
        matches!(self, Self::AllowOnce | Self::AllowAlways)
    }

    /// A lasting grant or refusal, which only an admin is offered.
    pub fn lasts(self) -> bool {
        matches!(self, Self::AllowAlways | Self::RejectAlways)
    }
}

/// What kind of tool a permission card is about, as ACP names it.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    #[serde(other)]
    Other,
}

impl ToolKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Read => "Lecture",
            Self::Edit => "Modification",
            Self::Delete => "Suppression",
            Self::Move => "Déplacement",
            Self::Search => "Recherche",
            Self::Execute => "Commande",
            Self::Think => "Réflexion",
            Self::Fetch => "Accès web",
            Self::SwitchMode => "Changement de mode",
            Self::Other => "Outil",
        }
    }
}

/// Where a secret card offers to keep the secret: under `name`, for the canonical `origin`
/// only.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Remember {
    pub name: String,
    pub origin: String,
    /// A secret was already kept under `name` for `origin` when the card was asked: keeping this
    /// one replaces it, and the card says so.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replaces: bool,
}

/// How long a secret given in a card is kept.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Keep {
    /// In memory, for this conversation, until used once or expired.
    Conversation,
    /// Sealed in the database, as the card's [`Remember`] says.
    Stored,
}

/// Where a master key is kept: the backends of the stored secrets.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum KeyPlace {
    Keychain,
    File,
    Systemd,
}

/// A secret on its way from the page to the server's store. It prints as `[REDACTED]` and is
/// wiped from memory when dropped; the server turns it into a `SecretString` at once.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// Its length in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The value itself, moved out so that no copy is left behind.
    pub fn into_inner(mut self) -> String {
        std::mem::take(&mut self.0)
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl Serialize for SecretValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SecretValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self)
    }
}

/// One field of a form; `key` is unique in its form.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Field {
    /// A question with options to pick from.
    Choice {
        key: String,
        /// A short chip above the question.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        header: Option<String>,
        question: String,
        options: Vec<ChoiceOption>,
        /// Several options may be picked.
        #[serde(default)]
        multi: bool,
        /// "Autre…" with free text is offered besides the options.
        #[serde(default)]
        other: bool,
        #[serde(default)]
        required: bool,
        /// A precision may be added to the answer: false where the asker has nowhere to put
        /// it, such as a plain choice of an elicitation.
        #[serde(default = "yes", skip_serializing_if = "is_yes")]
        note: bool,
    },
    /// One line of free text, answered with [`FieldValue::Text`].
    Text {
        key: String,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        /// What the text must be, checked by [`check`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<TextFormat>,
        /// In characters; [`MAX_TEXT`] at most whatever `max_length` says.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_length: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_length: Option<u32>,
        /// What the box holds at first.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        #[serde(default)]
        required: bool,
    },
    /// A number, answered with [`FieldValue::Number`]; whole when `integer`.
    Number {
        key: String,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default)]
        integer: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<f64>,
        #[serde(default)]
        required: bool,
    },
    /// Yes or no, answered with [`FieldValue::Toggle`].
    Toggle {
        key: String,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// Whether it starts on.
        #[serde(default)]
        default: bool,
    },
}

/// Numbers in a card are finite ([`Card::check`]), so they equal themselves.
impl Eq for Field {}

fn yes() -> bool {
    true
}

fn is_yes(value: &bool) -> bool {
    *value
}

impl Field {
    pub fn key(&self) -> &str {
        match self {
            Self::Choice { key, .. }
            | Self::Text { key, .. }
            | Self::Number { key, .. }
            | Self::Toggle { key, .. } => key,
        }
    }

    /// What the field asks, in a line: a question's question, another field's title.
    pub fn label(&self) -> &str {
        match self {
            Self::Choice { question, .. } => question,
            Self::Text { title, .. } | Self::Number { title, .. } | Self::Toggle { title, .. } => {
                title
            }
        }
    }
}

/// What a [`Field::Text`] must hold, as JSON Schema's `format` names it.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum TextFormat {
    /// `name@domain.tld`.
    Email,
    /// An absolute address, such as `https://example.com/page`.
    Uri,
    /// `YYYY-MM-DD`.
    Date,
    /// RFC 3339, such as `2026-11-17T14:30:00+01:00`.
    DateTime,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct ChoiceOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Text shown in monospace when the option is picked: a sample, a diff, a layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recommended: bool,
}

/// What the page answers to a request.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// The form's answers, by field key.
    Form {
        values: BTreeMap<String, FieldValue>,
    },
    /// One of a [`Card::Permission`]'s options, by its id.
    Permission { choice: String },
    /// The secret a [`Card::Secret`] asks for, and how long to keep it.
    Secret { value: SecretValue, keep: Keep },
    /// The user refuses to answer.
    Decline,
}

/// The answer to one field, as its JSON says: an object for a [`Field::Choice`], a string for a
/// [`Field::Text`], a number for a [`Field::Number`], `true` or `false` for a [`Field::Toggle`].
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(untagged)]
pub enum FieldValue {
    Toggle(bool),
    Number(f64),
    Text(String),
    Choice(ChoiceAnswer),
}

/// JSON carries no NaN, and [`check`] refuses an infinite number.
impl Eq for FieldValue {}

impl From<ChoiceAnswer> for FieldValue {
    fn from(answer: ChoiceAnswer) -> Self {
        Self::Choice(answer)
    }
}

/// The answer to a [`Field::Choice`].
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct ChoiceAnswer {
    /// Labels of the picked options, in the card's order.
    #[serde(default)]
    pub selected: Vec<String>,
    /// The "Autre…" text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
    /// A precision added to the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// How a request ended, as its card shows it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outcome {
    /// `summary`: what was answered, short (see [`summary`]).
    Answered {
        summary: String,
    },
    /// A secret was kept as `kept` says; never its value. `new_key`: where keeping it made the
    /// master key, when it did, to be backed up.
    Saved {
        kept: Keep,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_key: Option<KeyPlace>,
    },
    Declined,
    Expired,
    Cancelled,
}

/// Why an answer does not fit its card: the field it is about, if any, and what to fix, in
/// French for the page.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Invalid {
    pub key: Option<String>,
    pub message: String,
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn invalid(key: Option<&str>, message: impl Into<String>) -> Invalid {
    Invalid {
        key: key.map(str::to_owned),
        message: message.into(),
    }
}

/// Checks that `response` answers `card`: for a permission, one of its options; for a form,
/// known fields only, each answered with its own kind of value, offered options only, one pick
/// for a single choice, required fields answered, free text non-empty and short enough, a text
/// field's length and format, a number's bounds (whole when it asks for an integer); for a secret, a value of at most
/// [`MAX_SECRET`] bytes, kept only where the card offers it. A decline answers any card.
pub fn check(card: &Card, response: &Response) -> Result<(), Invalid> {
    match (card, response) {
        (_, Response::Decline) => Ok(()),
        (Card::Permission { options, .. }, Response::Permission { choice }) => {
            if options.iter().any(|option| option.id == *choice) {
                Ok(())
            } else {
                Err(invalid(None, "Ce choix n'est pas proposé."))
            }
        }
        (Card::Form { fields, .. }, Response::Form { values }) => check_form(fields, values),
        (Card::Secret { remember, .. }, Response::Secret { value, keep }) => {
            check_secret(remember.as_ref(), value, *keep)
        }
        _ => Err(invalid(
            None,
            "Cette réponse ne correspond pas à la demande.",
        )),
    }
}

fn check_secret(
    remember: Option<&Remember>,
    value: &SecretValue,
    keep: Keep,
) -> Result<(), Invalid> {
    if value.is_empty() {
        return Err(invalid(None, "Saisis le secret, ou refuse la demande."));
    }
    if value.len() > MAX_SECRET {
        return Err(invalid(
            None,
            format!("Secret trop long : {MAX_SECRET} octets au plus."),
        ));
    }
    if keep == Keep::Stored && remember.is_none() {
        return Err(invalid(
            None,
            "Cette demande ne propose pas de garder le secret.",
        ));
    }
    Ok(())
}

fn check_form(fields: &[Field], values: &BTreeMap<String, FieldValue>) -> Result<(), Invalid> {
    if let Some(unknown) = values
        .keys()
        .find(|key| !fields.iter().any(|field| field.key() == key.as_str()))
    {
        return Err(invalid(
            Some(unknown),
            "La réponse nomme une question inconnue.",
        ));
    }
    for field in fields {
        let key = field.key();
        let value = values.get(key);
        match (field, value) {
            (Field::Choice { .. }, None) => check_choice(field, None)?,
            (Field::Choice { .. }, Some(FieldValue::Choice(answer))) => {
                check_choice(field, Some(answer))?;
            }
            (
                Field::Text {
                    format,
                    min_length,
                    max_length,
                    required,
                    ..
                },
                value,
            ) => {
                let text = match value {
                    None => "",
                    Some(FieldValue::Text(text)) => text.as_str(),
                    Some(_) => return Err(mismatch(key)),
                };
                check_text(key, text, *format, *min_length, *max_length, *required)?;
            }
            (
                Field::Number {
                    integer,
                    min,
                    max,
                    required,
                    ..
                },
                value,
            ) => match value {
                None if *required => return Err(invalid(Some(key), "Remplis ce champ.")),
                None => {}
                Some(FieldValue::Number(number)) => {
                    check_number(key, *number, *integer, *min, *max)?;
                }
                Some(_) => return Err(mismatch(key)),
            },
            (Field::Toggle { .. }, None | Some(FieldValue::Toggle(_))) => {}
            _ => return Err(mismatch(key)),
        }
    }
    Ok(())
}

fn mismatch(key: &str) -> Invalid {
    invalid(Some(key), "Cette réponse ne correspond pas à la question.")
}

fn check_choice(field: &Field, answer: Option<&ChoiceAnswer>) -> Result<(), Invalid> {
    let Field::Choice {
        key,
        options,
        multi,
        other,
        required,
        note,
        ..
    } = field
    else {
        unreachable!("only choices are checked here")
    };
    // Before anything else: an optional question left unanswered may still carry a note.
    let texts = answer
        .into_iter()
        .flat_map(|a| [&a.other, &a.note])
        .flatten();
    for text in texts {
        if text.chars().count() > MAX_TEXT {
            return Err(invalid(
                Some(key),
                format!("Texte trop long : {MAX_TEXT} caractères au plus."),
            ));
        }
    }
    if !*note && answer.is_some_and(|answer| answer.note.is_some()) {
        return Err(invalid(
            Some(key),
            "Cette question n'accepte pas de précision.",
        ));
    }
    let picked = answer.map_or(0, |a| a.selected.len() + usize::from(a.other.is_some()));
    if picked == 0 {
        if *required {
            return Err(invalid(Some(key), "Réponds à cette question."));
        }
        return Ok(());
    }
    let answer = answer.expect("something was picked");
    let mut seen = BTreeSet::new();
    for label in &answer.selected {
        if !options.iter().any(|option| option.label == *label) {
            return Err(invalid(
                Some(key),
                format!("« {label} » n'est pas une des réponses proposées."),
            ));
        }
        if !seen.insert(label) {
            return Err(invalid(
                Some(key),
                format!("« {label} » est choisie deux fois."),
            ));
        }
    }
    if let Some(text) = &answer.other {
        if !*other {
            return Err(invalid(
                Some(key),
                "Cette question n'accepte pas d'autre réponse.",
            ));
        }
        if text.trim().is_empty() {
            return Err(invalid(
                Some(key),
                "Écris ta réponse, ou choisis une option.",
            ));
        }
    }
    if !*multi && picked > 1 {
        return Err(invalid(Some(key), "Une seule réponse possible."));
    }
    Ok(())
}

/// A text field's answer: blank counts as unanswered.
fn check_text(
    key: &str,
    text: &str,
    format: Option<TextFormat>,
    min_length: Option<u32>,
    max_length: Option<u32>,
    required: bool,
) -> Result<(), Invalid> {
    if text.trim().is_empty() {
        if required {
            return Err(invalid(Some(key), "Remplis ce champ."));
        }
        return Ok(());
    }
    let length = text.chars().count();
    let max = max_length.map_or(MAX_TEXT, |max| (max as usize).min(MAX_TEXT));
    if length > max {
        return Err(invalid(
            Some(key),
            format!("Texte trop long : {max} caractères au plus."),
        ));
    }
    if let Some(min) = min_length.filter(|min| length < *min as usize) {
        return Err(invalid(
            Some(key),
            format!("Texte trop court : {min} caractères au moins."),
        ));
    }
    let Some(format) = format else {
        return Ok(());
    };
    let (valid, message) = match format {
        TextFormat::Email => (is_email(text), "Adresse e-mail invalide."),
        TextFormat::Uri => (
            is_uri(text),
            "Adresse invalide : elle commence par https:// par exemple.",
        ),
        TextFormat::Date => (
            chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").is_ok(),
            "Date invalide : AAAA-MM-JJ.",
        ),
        TextFormat::DateTime => (
            chrono::DateTime::parse_from_rfc3339(text).is_ok(),
            "Date et heure invalides.",
        ),
    };
    if valid {
        Ok(())
    } else {
        Err(invalid(Some(key), message))
    }
}

/// `local@domain.tld`, no space: what a phone's e-mail box lets through, no more.
fn is_email(text: &str) -> bool {
    let Some((local, domain)) = text.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !text.chars().any(char::is_whitespace)
        && !domain.contains('@')
        && domain
            .split_once('.')
            .is_some_and(|(name, tld)| !name.is_empty() && !tld.is_empty())
}

/// An absolute URI: a scheme (RFC 3986: a letter, then letters, digits, `+`, `-` or `.`), a
/// colon, something after it, no space.
fn is_uri(text: &str) -> bool {
    let Some((scheme, rest)) = text.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        && !rest.is_empty()
        && !text.chars().any(char::is_whitespace)
}

fn check_number(
    key: &str,
    number: f64,
    integer: bool,
    min: Option<f64>,
    max: Option<f64>,
) -> Result<(), Invalid> {
    // Integers beyond 2^53 lose their last digits as JSON numbers.
    const SAFE: f64 = 9_007_199_254_740_991.0;
    if !number.is_finite() {
        return Err(invalid(Some(key), "Nombre invalide."));
    }
    if integer && (number.fract() != 0.0 || number.abs() > SAFE) {
        return Err(invalid(Some(key), "Un nombre entier est attendu."));
    }
    if let Some(min) = min.filter(|min| number < *min) {
        return Err(invalid(
            Some(key),
            format!("Le nombre doit être au moins {}.", show_number(min)),
        ));
    }
    if let Some(max) = max.filter(|max| number > *max) {
        return Err(invalid(
            Some(key),
            format!("Le nombre doit être au plus {}.", show_number(max)),
        ));
    }
    Ok(())
}

/// A number as a person writes it: no `.0` on a whole one.
pub fn show_number(number: f64) -> String {
    if number.fract() == 0.0 && number.abs() < 1e15 {
        format!("{number:.0}")
    } else {
        number.to_string()
    }
}

/// A short account of an answer, for the closed card and the audit trail: the picked labels
/// and "Autre" texts, a text field's text (cut to [`MAX_LABEL`] characters), a number, a toggle's
/// title when on, field after field, notes left out; for a permission, the chosen option's
/// label.
pub fn summary(card: &Card, response: &Response) -> String {
    match (card, response) {
        // A secret is never summed up, not even by its length.
        (Card::Secret { .. }, _) | (_, Response::Secret { .. }) => String::new(),
        (Card::Form { fields, .. }, Response::Form { values }) => fields
            .iter()
            .filter_map(|field| values.get(field.key()).map(|value| (field, value)))
            .map(|(field, value)| match value {
                FieldValue::Choice(answer) => answer
                    .selected
                    .iter()
                    .map(String::as_str)
                    .chain(answer.other.as_deref().map(str::trim))
                    .collect::<Vec<_>>()
                    .join(", "),
                FieldValue::Text(text) => {
                    let text = text.trim();
                    if text.chars().count() > MAX_LABEL {
                        let head: String = text.chars().take(MAX_LABEL - 1).collect();
                        format!("{head}…")
                    } else {
                        text.to_owned()
                    }
                }
                FieldValue::Number(number) => show_number(*number),
                FieldValue::Toggle(true) => format!("{} : oui", field.label()),
                FieldValue::Toggle(false) => format!("{} : non", field.label()),
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" ; "),
        (Card::Permission { options, .. }, Response::Permission { choice }) => options
            .iter()
            .find(|option| option.id == *choice)
            .map_or_else(|| choice.clone(), |option| option.kind.label().to_owned()),
        (_, Response::Permission { choice }) => choice.clone(),
        // `check` refuses a form's answers to a permission.
        (Card::Permission { .. }, Response::Form { .. }) | (_, Response::Decline) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(label: &str) -> ChoiceOption {
        ChoiceOption {
            label: label.into(),
            description: None,
            preview: None,
            recommended: false,
        }
    }

    fn choice(key: &str, multi: bool, other: bool, required: bool) -> Field {
        Field::Choice {
            key: key.into(),
            header: Some("Appli".into()),
            question: "Laquelle ?".into(),
            options: vec![option("Mealie"), option("Seerr"), option("Immich")],
            multi,
            other,
            required,
            note: true,
        }
    }

    fn form(fields: Vec<Field>) -> Card {
        Card::Form {
            message: String::new(),
            fields,
            submit: None,
            truncated: false,
            replaced: false,
        }
    }

    fn answer(selected: &[&str], other: Option<&str>) -> ChoiceAnswer {
        ChoiceAnswer {
            selected: selected.iter().map(|s| (*s).to_owned()).collect(),
            other: other.map(str::to_owned),
            note: None,
        }
    }

    fn values(pairs: &[(&str, ChoiceAnswer)]) -> Response {
        Response::Form {
            values: pairs
                .iter()
                .map(|(key, answer)| ((*key).to_owned(), answer.clone().into()))
                .collect(),
        }
    }

    fn refused(card: &Card, response: &Response) -> Invalid {
        check(card, response).expect_err("the answer should be refused")
    }

    #[test]
    fn card_and_response_json_shape() {
        let card = Card::Form {
            message: String::new(),
            fields: vec![Field::Choice {
                key: "q1".into(),
                header: Some("Appli".into()),
                question: "Laquelle ?".into(),
                options: vec![
                    ChoiceOption {
                        recommended: true,
                        description: Some("Recettes".into()),
                        ..option("Mealie")
                    },
                    option("Seerr"),
                ],
                multi: false,
                other: true,
                required: true,
                note: true,
            }],
            submit: None,
            truncated: false,
            replaced: false,
        };
        let json = serde_json::to_string(&card).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"form","fields":[{"type":"choice","key":"q1","header":"Appli","question":"Laquelle ?","options":[{"label":"Mealie","description":"Recettes","recommended":true},{"label":"Seerr"}],"multi":false,"other":true,"required":true}]}"#
        );
        assert_eq!(serde_json::from_str::<Card>(&json).unwrap(), card);

        let response = values(&[("q1", answer(&["Mealie"], None))]);
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(
            json,
            r#"{"type":"form","values":{"q1":{"selected":["Mealie"]}}}"#
        );
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), response);
        assert_eq!(
            serde_json::to_string(&Response::Decline).unwrap(),
            r#"{"type":"decline"}"#
        );
        assert_eq!(
            serde_json::to_string(&Outcome::Answered {
                summary: "Mealie".into()
            })
            .unwrap(),
            r#"{"type":"answered","summary":"Mealie"}"#
        );
        assert_eq!(
            serde_json::to_string(&Outcome::Expired).unwrap(),
            r#"{"type":"expired"}"#
        );
    }

    #[test]
    fn valid_answers_pass() {
        let card = form(vec![
            choice("single", false, true, true),
            choice("multi", true, true, true),
            choice("optional", false, false, false),
        ]);
        let ok = values(&[
            ("single", answer(&[], Some("Paperless"))),
            ("multi", answer(&["Mealie", "Immich"], Some("Jellyfin"))),
        ]);
        assert_eq!(check(&card, &ok), Ok(()));
        assert_eq!(check(&card, &Response::Decline), Ok(()));
    }

    #[test]
    fn answer_rules() {
        let card = form(vec![
            choice("single", false, true, true),
            choice("closed", false, false, false),
        ]);
        let cases = [
            (values(&[]), "single", "Réponds à cette question."),
            (
                values(&[("single", answer(&[], None))]),
                "single",
                "Réponds à cette question.",
            ),
            (
                values(&[("single", answer(&["Nextcloud"], None))]),
                "single",
                "« Nextcloud » n'est pas une des réponses proposées.",
            ),
            (
                values(&[("single", answer(&["Seerr", "Mealie"], None))]),
                "single",
                "Une seule réponse possible.",
            ),
            (
                values(&[("single", answer(&["Seerr"], Some("Autre")))]),
                "single",
                "Une seule réponse possible.",
            ),
            (
                values(&[("single", answer(&[], Some("  ")))]),
                "single",
                "Écris ta réponse, ou choisis une option.",
            ),
            (
                values(&[
                    ("single", answer(&["Seerr"], None)),
                    ("closed", answer(&[], Some("Paperless"))),
                ]),
                "closed",
                "Cette question n'accepte pas d'autre réponse.",
            ),
            (
                values(&[
                    ("single", answer(&["Seerr"], None)),
                    ("ghost", answer(&["Seerr"], None)),
                ]),
                "ghost",
                "La réponse nomme une question inconnue.",
            ),
        ];
        for (response, key, message) in cases {
            let error = refused(&card, &response);
            assert_eq!(error.key.as_deref(), Some(key), "{response:?}");
            assert_eq!(error.message, message, "{response:?}");
        }

        let multi = form(vec![choice("multi", true, false, true)]);
        let error = refused(
            &multi,
            &values(&[("multi", answer(&["Seerr", "Seerr"], None))]),
        );
        assert_eq!(error.message, "« Seerr » est choisie deux fois.");

        let long = "x".repeat(MAX_TEXT + 1);
        let mut noted = answer(&["Seerr"], None);
        noted.note = Some(long.clone());
        let error = refused(&card, &values(&[("single", noted)]));
        assert_eq!(error.message, "Texte trop long : 2000 caractères au plus.");
        let error = refused(&card, &values(&[("single", answer(&[], Some(&long)))]));
        assert_eq!(error.message, "Texte trop long : 2000 caractères au plus.");
        // An optional question left unanswered: its note is checked all the same.
        let mut only_a_note = answer(&[], None);
        only_a_note.note = Some(long);
        let error = refused(
            &card,
            &values(&[
                ("single", answer(&["Seerr"], None)),
                ("closed", only_a_note),
            ]),
        );
        assert_eq!(error.key.as_deref(), Some("closed"));
        assert_eq!(error.message, "Texte trop long : 2000 caractères au plus.");

        let error = refused(
            &card,
            &Response::Permission {
                choice: "allow_once".into(),
            },
        );
        assert_eq!(error.key, None);
    }

    fn secret_card(remember: bool) -> Card {
        Card::Secret {
            label: "Mot de passe Nextcloud".into(),
            reason: "Pour lire ton agenda.".into(),
            placeholder: None,
            url: Some("https://cloud.example/settings".into()),
            remember: remember.then(|| Remember {
                name: "nextcloud".into(),
                origin: "https://cloud.example".into(),
                replaces: false,
            }),
        }
    }

    fn secret(value: &str, keep: Keep) -> Response {
        Response::Secret {
            value: SecretValue::new(value.into()),
            keep,
        }
    }

    #[test]
    fn secret_card_and_answer_json_shape() {
        let card = secret_card(true);
        let json = serde_json::to_string(&card).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"secret","label":"Mot de passe Nextcloud","reason":"Pour lire ton agenda.","url":"https://cloud.example/settings","remember":{"name":"nextcloud","origin":"https://cloud.example"}}"#
        );
        assert_eq!(serde_json::from_str::<Card>(&json).unwrap(), card);
        assert_eq!(card.kind(), "secret");
        // A replacement says so; a card without the field replaces nothing.
        let Card::Secret {
            remember: Some(remember),
            ..
        } = card
        else {
            unreachable!()
        };
        let replacing = Remember {
            replaces: true,
            ..remember
        };
        assert_eq!(
            serde_json::to_string(&replacing).unwrap(),
            r#"{"name":"nextcloud","origin":"https://cloud.example","replaces":true}"#
        );

        let answer = secret("s3cret", Keep::Conversation);
        let json = serde_json::to_string(&answer).unwrap();
        assert_eq!(
            json,
            r#"{"type":"secret","value":"s3cret","keep":"conversation"}"#
        );
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), answer);
        assert_eq!(
            serde_json::to_string(&Outcome::Saved {
                kept: Keep::Stored,
                new_key: None
            })
            .unwrap(),
            r#"{"type":"saved","kept":"stored"}"#
        );
        assert_eq!(
            serde_json::to_string(&Outcome::Saved {
                kept: Keep::Stored,
                new_key: Some(KeyPlace::Keychain)
            })
            .unwrap(),
            r#"{"type":"saved","kept":"stored","new_key":"keychain"}"#
        );
    }

    #[test]
    fn secret_answer_debug_is_redacted() {
        let canary = "canary-secret-debug-51f0";
        let answer = secret(canary, Keep::Stored);
        for text in [format!("{answer:?}"), format!("{answer:#?}")] {
            assert!(!text.contains(canary), "{text}");
            assert!(text.contains("[REDACTED]"), "{text}");
        }
        let message = crate::ClientMsg::Answer {
            id: 3,
            response: answer,
        };
        assert!(!format!("{message:?}").contains(canary));
        // Moved out whole.
        assert_eq!(SecretValue::new(canary.into()).into_inner(), canary);
    }

    #[test]
    fn secret_answer_rules() {
        let card = secret_card(false);
        assert_eq!(check(&card, &secret("s3cret", Keep::Conversation)), Ok(()));
        assert_eq!(check(&card, &Response::Decline), Ok(()));
        assert_eq!(
            check(&secret_card(true), &secret("s3cret", Keep::Stored)),
            Ok(())
        );
        let long = "x".repeat(MAX_SECRET + 1);
        let cases = [
            (
                secret("", Keep::Conversation),
                "Saisis le secret, ou refuse la demande.",
            ),
            (
                secret(&long, Keep::Conversation),
                "Secret trop long : 16384 octets au plus.",
            ),
            (
                secret("s3cret", Keep::Stored),
                "Cette demande ne propose pas de garder le secret.",
            ),
            (
                values(&[("q1", answer(&["Mealie"], None))]),
                "Cette réponse ne correspond pas à la demande.",
            ),
        ];
        for (response, message) in cases {
            assert_eq!(refused(&card, &response).message, message);
        }
        // A form never takes a secret.
        let form = form(vec![choice("q1", false, true, true)]);
        assert_eq!(
            refused(&form, &secret("s3cret", Keep::Conversation)).message,
            "Cette réponse ne correspond pas à la demande."
        );
        // Nor does any summary show one.
        assert_eq!(summary(&card, &secret("s3cret", Keep::Conversation)), "");
    }

    fn permission_card() -> Card {
        Card::Permission {
            title: "Lire ~/notes.md".into(),
            tool: Some(ToolKind::Read),
            body: Some("~/notes.md".into()),
            options: vec![
                PermissionChoice {
                    id: "allow-once".into(),
                    kind: PermissionKind::AllowOnce,
                    detail: None,
                },
                PermissionChoice {
                    id: "allow-with-updates".into(),
                    kind: PermissionKind::AllowAlways,
                    detail: Some("Yes, and don't ask again for notes.md".into()),
                },
                PermissionChoice {
                    id: "reject".into(),
                    kind: PermissionKind::RejectOnce,
                    detail: None,
                },
            ],
            truncated: false,
            replaced: false,
        }
    }

    fn pick(choice: &str) -> Response {
        Response::Permission {
            choice: choice.into(),
        }
    }

    #[test]
    fn permission_card_and_answer_json_shape() {
        let card = permission_card();
        let json = serde_json::to_string(&card).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"permission","title":"Lire ~/notes.md","tool":"read","body":"~/notes.md","options":[{"id":"allow-once","kind":"allow_once"},{"id":"allow-with-updates","kind":"allow_always","detail":"Yes, and don't ask again for notes.md"},{"id":"reject","kind":"reject_once"}]}"#
        );
        assert_eq!(serde_json::from_str::<Card>(&json).unwrap(), card);
        assert_eq!(card.kind(), "permission");
        // A kind of tool this version does not know reads as a tool.
        let unknown = json.replace(r#""read""#, r#""teleport""#);
        let Card::Permission { tool, .. } = serde_json::from_str(&unknown).unwrap() else {
            unreachable!()
        };
        assert_eq!(tool, Some(ToolKind::Other));
        assert_eq!(
            serde_json::to_string(&pick("reject")).unwrap(),
            r#"{"type":"permission","choice":"reject"}"#
        );
    }

    #[test]
    fn cards_are_checked_whoever_built_them() {
        assert_eq!(permission_card().check(), Ok(()));
        assert_eq!(form(vec![choice("q1", false, true, true)]).check(), Ok(()));
        assert_eq!(secret_card(true).check(), Ok(()));
        let with = |change: &dyn Fn(&mut Card)| {
            let mut card = permission_card();
            change(&mut card);
            card.check().expect_err("the card should be refused")
        };
        fn fields(
            card: &mut Card,
        ) -> (&mut String, &mut Option<String>, &mut Vec<PermissionChoice>) {
            let Card::Permission {
                title,
                body,
                options,
                ..
            } = card
            else {
                unreachable!()
            };
            (title, body, options)
        }
        let error = with(&|card| *fields(card).0 = "t".repeat(MAX_PERMISSION_TITLE + 1));
        assert!(error.contains("title has 201 characters"), "{error}");
        let error = with(&|card| *fields(card).0 = "mv a\u{202e}b".into());
        assert!(error.contains("U+202E"), "{error}");
        let error = with(&|card| *fields(card).1 = Some("a\u{2028}b".into()));
        assert!(error.contains("U+2028"), "{error}");
        let error = with(&|card| *fields(card).1 = Some("x".repeat(MAX_PERMISSION_LINE + 1)));
        assert!(error.contains("a line of the body"), "{error}");
        let error = with(&|card| fields(card).2.clear());
        assert!(error.contains("at least one option"), "{error}");
        let error = with(&|card| {
            let options = fields(card).2;
            options[1].id = options[0].id.clone();
        });
        assert!(error.contains("repeated"), "{error}");
        let error = with(&|card| fields(card).2[0].detail = Some("ok\u{200b}".into()));
        assert!(error.contains("U+200B"), "{error}");
        // Lines and tabs only where a text spans lines.
        let mut preview = choice("q1", false, true, true);
        let Field::Choice { options, .. } = &mut preview else {
            unreachable!()
        };
        options[0].preview = Some("a\n\tb".into());
        assert_eq!(form(vec![preview.clone()]).check(), Ok(()));
        let Field::Choice { options, .. } = &mut preview else {
            unreachable!()
        };
        options[0].label = "Mea\u{e0041}lie".into();
        let error = form(vec![preview]).check().unwrap_err();
        assert!(
            error.contains("label contains the hidden character U+E0041"),
            "{error}"
        );
        let mut secret = secret_card(false);
        if let Card::Secret { reason, .. } = &mut secret {
            *reason = "ligne\nsuivante".into();
        }
        assert!(secret.check().unwrap_err().contains("U+000A"));
        // What a cleaned card says about itself travels with it.
        let mut card = permission_card();
        if let Card::Permission {
            truncated,
            replaced,
            ..
        } = &mut card
        {
            *truncated = true;
            *replaced = true;
        }
        let json = serde_json::to_string(&card).unwrap();
        assert!(
            json.ends_with(r#""truncated":true,"replaced":true}"#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<Card>(&json).unwrap(), card);
    }

    #[test]
    fn permission_answer_rules() {
        let card = permission_card();
        for choice in ["allow-once", "allow-with-updates", "reject"] {
            assert_eq!(check(&card, &pick(choice)), Ok(()), "{choice}");
        }
        assert_eq!(check(&card, &Response::Decline), Ok(()));
        // Only the agent's own options, by id, never by label or kind.
        for choice in ["allow_once", "Autoriser une fois", "", "reject_always"] {
            let error = refused(&card, &pick(choice));
            assert_eq!(error.message, "Ce choix n'est pas proposé.", "{choice}");
        }
        let error = refused(&card, &values(&[("q1", answer(&["Mealie"], None))]));
        assert_eq!(
            error.message,
            "Cette réponse ne correspond pas à la demande."
        );
        assert_eq!(summary(&card, &pick("reject")), "Refuser");
        assert_eq!(
            summary(&card, &pick("allow-with-updates")),
            "Toujours autoriser"
        );
        assert!(PermissionKind::AllowAlways.lasts() && PermissionKind::RejectAlways.lasts());
        assert!(!PermissionKind::AllowOnce.lasts() && !PermissionKind::RejectOnce.allows());
    }

    #[test]
    fn summary_lists_picks_field_by_field() {
        let card = form(vec![
            choice("a", true, true, true),
            choice("b", false, true, false),
            choice("c", false, true, true),
        ]);
        let mut noted = answer(&[], Some(" Paperless "));
        noted.note = Some("le soir".into());
        let response = values(&[("a", answer(&["Mealie", "Seerr"], None)), ("c", noted)]);
        assert_eq!(summary(&card, &response), "Mealie, Seerr ; Paperless");
    }

    fn text_field(key: &str, format: Option<TextFormat>, required: bool) -> Field {
        Field::Text {
            key: key.into(),
            title: "Adresse".into(),
            description: None,
            placeholder: None,
            format,
            min_length: None,
            max_length: None,
            default: None,
            required,
        }
    }

    fn number_field(key: &str, integer: bool, min: Option<f64>, max: Option<f64>) -> Field {
        Field::Number {
            key: key.into(),
            title: "Combien ?".into(),
            description: None,
            integer,
            min,
            max,
            default: None,
            required: true,
        }
    }

    fn toggle_field(key: &str) -> Field {
        Field::Toggle {
            key: key.into(),
            title: "Rappel".into(),
            description: Some("Un rappel la veille".into()),
            default: true,
        }
    }

    fn fill(pairs: &[(&str, FieldValue)]) -> Response {
        Response::Form {
            values: pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect(),
        }
    }

    #[test]
    fn text_number_and_toggle_json_shape() {
        let card = form(vec![
            text_field("mail", Some(TextFormat::Email), true),
            number_field("n", true, Some(1.0), Some(10.0)),
            toggle_field("remind"),
        ]);
        let json = serde_json::to_string(&card).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"form","fields":[{"type":"text","key":"mail","title":"Adresse","format":"email","required":true},{"type":"number","key":"n","title":"Combien ?","integer":true,"min":1.0,"max":10.0,"required":true},{"type":"toggle","key":"remind","title":"Rappel","description":"Un rappel la veille","default":true}]}"#
        );
        assert_eq!(serde_json::from_str::<Card>(&json).unwrap(), card);
        assert_eq!(card.check(), Ok(()));

        let answer = fill(&[
            ("mail", FieldValue::Text("alix@example.com".into())),
            ("n", FieldValue::Number(3.0)),
            ("remind", FieldValue::Toggle(false)),
        ]);
        let json = serde_json::to_string(&answer).unwrap();
        assert_eq!(
            json,
            r#"{"type":"form","values":{"mail":"alix@example.com","n":3.0,"remind":false}}"#
        );
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), answer);
        // A whole number reads back as one, whichever way it was written.
        let written =
            r#"{"type":"form","values":{"mail":"alix@example.com","n":3,"remind":false}}"#;
        assert_eq!(serde_json::from_str::<Response>(written).unwrap(), answer);
        assert_eq!(check(&card, &answer), Ok(()));
        assert_eq!(
            summary(&card, &answer),
            "alix@example.com ; 3 ; Rappel : non"
        );
        // A question still reads as an object, and a cleaned form says so.
        let mut cleaned = form(Vec::new());
        if let Card::Form {
            truncated,
            replaced,
            ..
        } = &mut cleaned
        {
            *truncated = true;
            *replaced = true;
        }
        let json = serde_json::to_string(&cleaned).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"form","fields":[],"truncated":true,"replaced":true}"#
        );
        let mut closed = choice("q1", false, false, false);
        if let Field::Choice { note, .. } = &mut closed {
            *note = false;
        }
        assert!(
            serde_json::to_string(&closed)
                .unwrap()
                .ends_with(r#""required":false,"note":false}"#)
        );
    }

    #[test]
    fn text_number_and_toggle_answer_rules() {
        let card = form(vec![
            text_field("mail", Some(TextFormat::Email), true),
            text_field("site", Some(TextFormat::Uri), false),
            text_field("day", Some(TextFormat::Date), false),
            text_field("at", Some(TextFormat::DateTime), false),
            number_field("n", true, Some(1.0), Some(10.0)),
            toggle_field("remind"),
            choice("q", false, false, false),
        ]);
        let base = || {
            vec![
                ("mail", FieldValue::Text("alix@example.com".into())),
                ("n", FieldValue::Number(10.0)),
            ]
        };
        let ok = |extra: &[(&str, FieldValue)]| {
            let mut pairs = base();
            pairs.retain(|(key, _)| !extra.iter().any(|(other, _)| other == key));
            pairs.extend(extra.iter().cloned());
            fill(&pairs)
        };
        for good in [
            ok(&[]),
            ok(&[("site", FieldValue::Text("https://cloud.example/x".into()))]),
            ok(&[("site", FieldValue::Text("mailto:alix@example.com".into()))]),
            ok(&[("day", FieldValue::Text("2026-02-28".into()))]),
            ok(&[("at", FieldValue::Text("2026-11-17T14:30:00+01:00".into()))]),
            ok(&[("at", FieldValue::Text("2026-11-17T13:30:00Z".into()))]),
            ok(&[("remind", FieldValue::Toggle(true))]),
            // Blank counts as unanswered for an optional text.
            ok(&[("site", FieldValue::Text("  ".into()))]),
            ok(&[("n", FieldValue::Number(1.0))]),
        ] {
            assert_eq!(check(&card, &good), Ok(()), "{good:?}");
        }
        let cases = [
            (
                ok(&[("mail", FieldValue::Text(" ".into()))]),
                "mail",
                "Remplis ce champ.",
            ),
            (
                fill(&[("mail", FieldValue::Text("a@b.c".into()))]),
                "n",
                "Remplis ce champ.",
            ),
            (
                ok(&[("mail", FieldValue::Text("alix.example.com".into()))]),
                "mail",
                "Adresse e-mail invalide.",
            ),
            (
                ok(&[("mail", FieldValue::Text("alix @example.com".into()))]),
                "mail",
                "Adresse e-mail invalide.",
            ),
            (
                ok(&[("site", FieldValue::Text("cloud.example".into()))]),
                "site",
                "Adresse invalide : elle commence par https:// par exemple.",
            ),
            (
                ok(&[("day", FieldValue::Text("2026-02-30".into()))]),
                "day",
                "Date invalide : AAAA-MM-JJ.",
            ),
            (
                ok(&[("at", FieldValue::Text("2026-11-17T14:30".into()))]),
                "at",
                "Date et heure invalides.",
            ),
            (
                ok(&[("n", FieldValue::Number(2.5))]),
                "n",
                "Un nombre entier est attendu.",
            ),
            (
                ok(&[("n", FieldValue::Number(0.0))]),
                "n",
                "Le nombre doit être au moins 1.",
            ),
            (
                ok(&[("n", FieldValue::Number(11.0))]),
                "n",
                "Le nombre doit être au plus 10.",
            ),
            (
                ok(&[("n", FieldValue::Number(f64::INFINITY))]),
                "n",
                "Nombre invalide.",
            ),
            (
                ok(&[("n", FieldValue::Text("3".into()))]),
                "n",
                "Cette réponse ne correspond pas à la question.",
            ),
            (
                ok(&[("remind", FieldValue::Text("oui".into()))]),
                "remind",
                "Cette réponse ne correspond pas à la question.",
            ),
            (
                ok(&[("q", FieldValue::Toggle(true))]),
                "q",
                "Cette réponse ne correspond pas à la question.",
            ),
            (
                ok(&[("q", {
                    let mut noted = answer(&["Seerr"], None);
                    noted.note = Some("le soir".into());
                    noted.into()
                })]),
                "q",
                "Cette question n'accepte pas de précision.",
            ),
        ];
        // The choice above takes no note.
        let mut card = card;
        if let Card::Form { fields, .. } = &mut card
            && let Some(Field::Choice { note, .. }) = fields.last_mut()
        {
            *note = false;
        }
        for (response, key, message) in cases {
            let error = refused(&card, &response);
            assert_eq!(error.key.as_deref(), Some(key), "{response:?}");
            assert_eq!(error.message, message, "{response:?}");
        }

        let mut bounded = text_field("t", None, true);
        if let Field::Text {
            min_length,
            max_length,
            ..
        } = &mut bounded
        {
            *min_length = Some(3);
            *max_length = Some(5);
        }
        let card = form(vec![bounded]);
        let text = |t: &str| fill(&[("t", FieldValue::Text(t.into()))]);
        assert_eq!(check(&card, &text("abcde")), Ok(()));
        assert_eq!(
            refused(&card, &text("ab")).message,
            "Texte trop court : 3 caractères au moins."
        );
        assert_eq!(
            refused(&card, &text("abcdef")).message,
            "Texte trop long : 5 caractères au plus."
        );
        // Whatever the field allows, no text is longer than MAX_TEXT.
        let card = form(vec![text_field("t", None, true)]);
        assert_eq!(
            refused(&card, &text(&"x".repeat(MAX_TEXT + 1))).message,
            "Texte trop long : 2000 caractères au plus."
        );
    }

    #[test]
    fn new_fields_are_checked_whoever_built_them() {
        let refuse = |field: Field| form(vec![field]).check().unwrap_err();
        let mut field = text_field("t", None, false);
        if let Field::Text { title, .. } = &mut field {
            *title = "Adr\u{202e}esse".into();
        }
        assert!(refuse(field).contains("U+202E"));
        let mut field = text_field("t", None, false);
        if let Field::Text {
            min_length,
            max_length,
            ..
        } = &mut field
        {
            *min_length = Some(5);
            *max_length = Some(2);
        }
        assert!(refuse(field).contains("characters"));
        assert!(refuse(number_field("n", false, Some(5.0), Some(1.0))).contains("above"));
        assert!(refuse(number_field("n", false, Some(f64::NAN), None)).contains("finite"));
        let mut field = toggle_field("b");
        if let Field::Toggle { description, .. } = &mut field {
            *description = Some("d".repeat(MAX_DESCRIPTION + 1));
        }
        assert!(refuse(field).contains("description has 201 characters"));
        // Options are told apart by their labels.
        let mut field = choice("q", false, false, false);
        if let Field::Choice { options, .. } = &mut field {
            options[1].label = options[0].label.clone();
        }
        assert!(refuse(field).contains("repeated"));
        let mut field = choice("q", false, false, false);
        if let Field::Choice { options, .. } = &mut field {
            options.clear();
        }
        assert!(refuse(field).contains("give 1 to 50"));
        let many = (0..=MAX_FIELDS)
            .map(|n| toggle_field(&format!("b{n}")))
            .collect();
        assert!(form(many).check().unwrap_err().contains("at most 12"));
    }
}
