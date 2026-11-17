//! Elicitations as form cards: an agent's (ACP `elicitation/create`) and an upstream MCP
//! server's (MCP `elicitation/create`) ask for a form described by the same subset of JSON
//! Schema, a flat object of strings, numbers, booleans and arrays of options. [`form`] turns
//! that schema into a [`Card::Form`] and remembers how to give the answers back
//! ([`Elicited::content`]); a schema the card cannot show is [`Refused`], with a reason for the
//! asker.
//!
//! - A string with `oneOf` (titled options) or `enum` is a question with one answer; an array
//!   whose `items` have `anyOf` or `enum` is a question with several. The answer gives back each
//!   option's value, never its title.
//! - A plain string beside a question `<k>`, named `<k>_custom` and not required, is folded into
//!   that question as its *Autre…* and its note, both given back as `<k>_custom`: Claude's
//!   `AskUserQuestion` form then reads as one question per card field, as `user_ask`'s do. Any
//!   other question takes no free text and no note, since nothing would carry them back.
//! - Other strings are text fields (`format` `email`, `uri`, `date`, `date-time`; `minLength`,
//!   `maxLength`; `pattern` is not checked here, the asker checks its own), `number` and
//!   `integer` number fields (`minimum`, `maximum`), `boolean` toggles.
//! - **No secret in a form, as far as a plain rule can tell.** MCP forbids sensitive data in
//!   form mode, and a value typed there reaches the asker, its logs and the request's row. A form
//!   is declined unseen when the text around anything typed reads like it asks for a secret
//!   ([`looks_secret`]): the message, and the name, title and description of every text and
//!   number field and of every question that takes free text (its `<k>_custom` field's too). A
//!   `format` of `password` is declined as well. This catches an honest asker's mistake. It cannot
//!   stop a hostile one, which can always word a question so that no rule sees it: the card says
//!   where the answer goes, and the user never types a password or a code into a card.
//!
//! Every text comes from the asker: it is cleaned as a permission card's are (hidden characters
//! replaced, head and tail kept), and the card says so.

use fronde_protocol::request::{
    MAX_DESCRIPTION, MAX_FIELDS, MAX_HEADER, MAX_KEY, MAX_LABEL, MAX_MESSAGE, MAX_OPTIONS,
    MAX_PREVIEW, MAX_QUESTION, MAX_TEXT,
};
use fronde_protocol::text::{Notes, block, hidden, line};
use fronde_protocol::{Card, ChoiceOption, Field, FieldValue, TextFormat};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::time::Duration;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;
use unicode_security::confusable_detection::skeleton;

/// How long an elicitation's card waits for an answer: no answer means no.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The longest an elicitation's card may wait, whatever is set.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Where Claude's adapter puts an `AskUserQuestion` option's preview.
const PREVIEW_META: &str = "_claude/askUserQuestionOption";

/// The suffix of the free-text field the Claude adapter puts beside each question.
const CUSTOM: &str = "_custom";

/// Words that, alone, make a text read like it asks for a secret, compared as [`normalize`]d.
const SECRET_WORDS: &[&str] = &[
    "password",
    "passwords",
    "passwd",
    "passphrase",
    "passcode",
    "pass",
    "pwd",
    "mdp",
    "secret",
    "secrets",
    "token",
    "tokens",
    "jeton",
    "jetons",
    "key",
    "keys",
    "apikey",
    "clé",
    "clés",
    "credential",
    "credentials",
    "identifiant",
    "identifiants",
    "otp",
    "totp",
    "2fa",
    "mfa",
    "pin",
    "code",
    "codes",
    "cvv",
    "cvc",
    "cvv2",
    "iban",
];

/// Words written together or apart (`mot de passe`, `api_key`), found in a text's words joined.
const SECRET_PHRASES: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "passcode",
    "motdepasse",
    "codesecret",
    "apikey",
    "secretkey",
    "privatekey",
    "accesstoken",
    "authtoken",
];

/// What the asker learns when its form looks like it asks for a secret: nothing that would help
/// it reword the form past the rule.
pub const SECRET_REFUSAL: &str = "Fronde does not show this form: never ask for a secret in a \
     form; have the owner configure it for you instead";

/// Why an elicitation cannot be shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    /// For the asker, in English.
    pub reason: String,
    /// For Fronde's log: the reason, and for a secret which field and text tripped the rule.
    pub detail: String,
    /// It looks like it asks for a secret.
    pub secret: bool,
}

impl Refused {
    /// A refusal whose reason the asker and the log share.
    pub fn new(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            detail: reason.clone(),
            reason,
            secret: false,
        }
    }
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for Refused {}

fn refuse(reason: impl Into<String>) -> Refused {
    Refused::new(reason)
}

fn refuse_secret(detail: String) -> Refused {
    Refused {
        reason: SECRET_REFUSAL.into(),
        detail,
        secret: true,
    }
}

/// An elicitation as a card, and how its answers go back.
#[derive(Clone, Debug, PartialEq)]
pub struct Elicited {
    pub card: Card,
    slots: Vec<Slot>,
}

/// How one field's answer goes back, under the schema's own names and values.
#[derive(Clone, Debug, PartialEq)]
enum Slot {
    Choice {
        key: String,
        /// Each option's label on the card and value in the schema.
        values: Vec<(String, String)>,
        /// The schema asks for an array.
        array: bool,
        /// The folded `<key>_custom` field.
        custom: Option<String>,
    },
    Text {
        key: String,
    },
    Number {
        key: String,
        integer: bool,
    },
    Toggle {
        key: String,
    },
}

impl Elicited {
    /// The content of an accepted elicitation: each answered field under its schema name, an
    /// option by its value, a whole number as an integer.
    pub fn content(&self, values: &BTreeMap<String, FieldValue>) -> Map<String, Value> {
        let mut content = Map::new();
        for slot in &self.slots {
            match slot {
                Slot::Choice {
                    key,
                    values: options,
                    array,
                    custom,
                } => {
                    let Some(FieldValue::Choice(answer)) = values.get(key) else {
                        continue;
                    };
                    let picked: Vec<Value> = answer
                        .selected
                        .iter()
                        .filter_map(|label| {
                            options
                                .iter()
                                .find(|(shown, _)| shown == label)
                                .map(|(_, value)| Value::String(value.clone()))
                        })
                        .collect();
                    if *array {
                        if !picked.is_empty() {
                            content.insert(key.clone(), Value::Array(picked));
                        }
                    } else if let Some(value) = picked.into_iter().next() {
                        content.insert(key.clone(), value);
                    }
                    let free: Vec<&str> = [&answer.other, &answer.note]
                        .into_iter()
                        .flatten()
                        .map(|text| text.trim())
                        .filter(|text| !text.is_empty())
                        .collect();
                    if let Some(custom) = custom
                        && !free.is_empty()
                    {
                        content.insert(custom.clone(), Value::String(free.join(" ; ")));
                    }
                }
                Slot::Text { key } => {
                    if let Some(FieldValue::Text(text)) = values.get(key)
                        && !text.trim().is_empty()
                    {
                        content.insert(key.clone(), Value::String(text.trim().to_owned()));
                    }
                }
                Slot::Number { key, integer } => {
                    if let Some(FieldValue::Number(number)) = values.get(key) {
                        // `check` let only a whole number within 2^53 through for an integer.
                        let value = if *integer {
                            Value::from(*number as i64)
                        } else {
                            serde_json::Number::from_f64(*number).map_or(Value::Null, Value::Number)
                        };
                        content.insert(key.clone(), value);
                    }
                }
                Slot::Toggle { key } => {
                    if let Some(FieldValue::Toggle(on)) = values.get(key) {
                        content.insert(key.clone(), Value::Bool(*on));
                    }
                }
            }
        }
        content
    }
}

/// A word as it is compared: NFKC, lower case, common digit-for-letter spellings read as
/// letters (`p4ssw0rd`), then its UTS #39 confusable skeleton (a Cyrillic `а` reads as `a`),
/// without accents, in lower case again.
fn normalize(word: &str) -> String {
    let unleet: String = word
        .nfkc()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            '4' | '@' => 'a',
            '3' => 'e',
            '1' => 'i',
            '0' => 'o',
            '5' | '$' => 's',
            '7' => 't',
            c => c,
        })
        .collect();
    skeleton(&unleet)
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .collect()
}

/// The words of `text`: split on anything but letters, digits, `$` and `@`, and between a
/// lower-case letter and an upper-case one. Invisible characters join what they sit in
/// (`pass\u{200b}word` is one word), so that they cannot split a word away from the rule.
fn words(text: &str) -> Vec<String> {
    let visible: String = text
        .chars()
        .filter(|c| !hidden(*c) || c.is_whitespace())
        .nfkc()
        .collect();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut previous_lower = false;
    for c in visible.chars() {
        if !(c.is_alphanumeric() || c == '$' || c == '@') {
            words.push(std::mem::take(&mut word));
            previous_lower = false;
            continue;
        }
        if c.is_uppercase() && previous_lower {
            words.push(std::mem::take(&mut word));
        }
        previous_lower = c.is_lowercase();
        word.push(c);
    }
    words.push(word);
    words.retain(|word| !word.is_empty());
    words
}

/// Whether `text` reads like it asks for a secret: one of its [`words`] is one of
/// [`SECRET_WORDS`], or its words joined hold one of [`SECRET_PHRASES`], all compared
/// [`normalize`]d. Plain on purpose: it also refuses a harmless "code postal".
pub fn looks_secret(text: &str) -> bool {
    let words: Vec<String> = words(text).iter().map(|word| normalize(word)).collect();
    let joined: String = words.concat();
    SECRET_WORDS
        .iter()
        .any(|secret| words.contains(&normalize(secret)))
        || SECRET_PHRASES
            .iter()
            .any(|phrase| joined.contains(&normalize(phrase)))
}

/// The first text of a form that reads like it asks for a secret, as `what: text`: the message,
/// and the name, title and description of each field something is typed into (a text, a number,
/// a question that takes free text, that free text's own field).
fn secret_text(
    message: &str,
    properties: &Map<String, Value>,
    folded: &HashSet<&str>,
) -> Option<String> {
    if looks_secret(message) {
        return Some(format!("the message {message:?}"));
    }
    for (name, property) in properties {
        let typed = match kind(property) {
            Some("number" | "integer") => true,
            Some("string") if !is_choice(property) => true,
            _ => folded.contains(format!("{name}{CUSTOM}").as_str()),
        };
        if !typed {
            continue;
        }
        if property.get("format").and_then(Value::as_str) == Some("password") {
            return Some(format!("the field {name:?}, of format password"));
        }
        let texts = [
            Some(name.as_str()),
            text_of(property, "title"),
            text_of(property, "description"),
        ];
        if let Some(text) = texts.into_iter().flatten().find(|text| looks_secret(text)) {
            return Some(format!("the field {name:?}: {text:?}"));
        }
    }
    None
}

/// The card for an elicitation asking `message` with the form `schema`, or why it cannot be
/// shown.
pub fn form(message: &str, schema: &Value) -> Result<Elicited, Refused> {
    let schema = schema
        .as_object()
        .ok_or_else(|| refuse("the requested schema is not an object"))?;
    if let Some(kind) = schema.get("type")
        && kind != "object"
    {
        return Err(refuse(format!(
            "the requested schema has type {kind}; a form is an object"
        )));
    }
    let empty = Map::new();
    let properties = match schema.get("properties") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(properties)) => properties,
        Some(_) => {
            return Err(refuse(
                "the requested schema's properties are not an object",
            ));
        }
    };
    let required: HashSet<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    // The free-text fields to fold into the question beside them.
    let folded: HashSet<&str> = properties
        .iter()
        .filter_map(|(name, property)| {
            let base = name.strip_suffix(CUSTOM)?;
            (properties.get(base).is_some_and(is_choice)
                && is_plain_string(property)
                && !required.contains(name.as_str()))
            .then_some(name.as_str())
        })
        .collect();
    if let Some(found) = secret_text(message, properties, &folded) {
        return Err(refuse_secret(format!(
            "it looks like it asks for a secret, by {found}"
        )));
    }
    let mut notes = Notes::default();
    let mut fields = Vec::new();
    let mut slots = Vec::new();
    for (name, property) in properties {
        if folded.contains(name.as_str()) {
            continue;
        }
        if name.is_empty() || name.chars().count() > MAX_KEY || name.chars().any(hidden) {
            return Err(refuse(format!(
                "the field name {name:?} is empty, longer than {MAX_KEY} characters or holds a \
                 hidden character"
            )));
        }
        let custom = format!("{name}{CUSTOM}");
        let custom = folded.contains(custom.as_str()).then_some(custom);
        let (field, slot) = field(
            name,
            property,
            required.contains(name.as_str()),
            custom,
            &mut notes,
        )?;
        fields.push(field);
        slots.push(slot);
    }
    if fields.len() > MAX_FIELDS {
        return Err(refuse(format!(
            "the form has {} fields; a card shows at most {MAX_FIELDS}",
            fields.len()
        )));
    }
    let mut message = block(message, MAX_MESSAGE, MAX_MESSAGE, &mut notes);
    // One question, as Claude's `AskUserQuestion` sends one: the message's first line is its
    // question, the rest its body. Its own text moves to the chip, unless that would lose it.
    if let [
        Field::Choice {
            header,
            question,
            other,
            required,
            ..
        },
    ] = fields.as_mut_slice()
    {
        let (first, body) = message
            .split_once('\n')
            .map_or((message.as_str(), ""), |(first, body)| (first, body.trim()));
        if !first.is_empty()
            && first.chars().count() <= MAX_QUESTION
            && (header.is_some()
                || question.is_empty()
                || question.chars().count() <= MAX_HEADER
                || *question == first)
        {
            if header.is_none() && !question.is_empty() && *question != first {
                *header = Some(question.clone());
            }
            *question = first.to_owned();
            message = body.to_owned();
        }
        // A form that is one plain choice, such as the adapter's offer to retry a refused
        // request with another model: nothing is sent until the user picks, and *Refuser*
        // stays the way out.
        if !*other {
            *required = true;
        }
    }
    for (field, (name, _)) in fields.iter_mut().zip(
        properties
            .iter()
            .filter(|(name, _)| !folded.contains(name.as_str())),
    ) {
        if let Field::Choice { question, .. } = field
            && question.is_empty()
        {
            *question = line(name, MAX_QUESTION, &mut notes);
        }
    }
    let card = Card::Form {
        message,
        fields,
        submit: None,
        truncated: notes.truncated,
        replaced: notes.replaced,
        during: Vec::new(),
    };
    // Cleaned texts fit by construction; a check that fails here is a bug to refuse, not show.
    card.check().map_err(refuse)?;
    Ok(Elicited { card, slots })
}

fn kind(property: &Value) -> Option<&str> {
    property.get("type").and_then(Value::as_str)
}

fn is_choice(property: &Value) -> bool {
    match kind(property) {
        Some("string") => property.get("oneOf").is_some() || property.get("enum").is_some(),
        Some("array") => true,
        _ => false,
    }
}

fn is_plain_string(property: &Value) -> bool {
    kind(property) == Some("string")
        && ["oneOf", "enum", "format"]
            .iter()
            .all(|key| property.get(key).is_none())
}

fn text_of<'a>(property: &'a Value, key: &str) -> Option<&'a str> {
    property
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
}

/// One property as a card field, and how its answer goes back.
fn field(
    name: &str,
    property: &Value,
    required: bool,
    custom: Option<String>,
    notes: &mut Notes,
) -> Result<(Field, Slot), Refused> {
    let title = text_of(property, "title");
    let description = text_of(property, "description");
    let clean = |text: &str, max: usize, notes: &mut Notes| line(text, max, notes);
    let kind = kind(property).ok_or_else(|| refuse(format!("the field {name:?} has no type")))?;
    if is_choice(property) {
        let array = kind == "array";
        let Options {
            options,
            multi,
            required,
        } = options(name, property, array, required, notes)?;
        // A short title is a chip above the question; a long one leads the question.
        let (header, question) = match (title, description) {
            (Some(title), Some(description)) if title.chars().count() <= MAX_HEADER => (
                Some(clean(title, MAX_HEADER, notes)),
                clean(description, MAX_QUESTION, notes),
            ),
            (Some(title), Some(description)) => (
                None,
                clean(&format!("{title} : {description}"), MAX_QUESTION, notes),
            ),
            (Some(text), None) | (None, Some(text)) => (None, clean(text, MAX_QUESTION, notes)),
            // Untitled: `form` gives it the message's first line, else its name.
            (None, None) => (None, String::new()),
        };
        let values = options
            .iter()
            .map(|(option, value)| (option.label.clone(), value.clone()))
            .collect();
        let folded = custom.is_some();
        return Ok((
            Field::Choice {
                key: name.to_owned(),
                header,
                question,
                options: options.into_iter().map(|(option, _)| option).collect(),
                multi,
                other: folded,
                required,
                note: folded,
            },
            Slot::Choice {
                key: name.to_owned(),
                values,
                array,
                custom,
            },
        ));
    }
    let title_text = clean(title.unwrap_or(name), MAX_QUESTION, notes);
    let description = description.map(|text| clean(text, MAX_DESCRIPTION, notes));
    match kind {
        "string" => {
            let format = match property.get("format").and_then(Value::as_str) {
                None => None,
                Some("email") => Some(TextFormat::Email),
                Some("uri") => Some(TextFormat::Uri),
                Some("date") => Some(TextFormat::Date),
                Some("date-time") => Some(TextFormat::DateTime),
                // `secret_text` declined it before.
                Some("password") => return Err(refuse_secret(format!("{name:?} is a password"))),
                Some(other) => {
                    return Err(refuse(format!(
                        "the field {name:?} has the format {other:?}, which a card cannot check; \
                         use email, uri, date or date-time, or none"
                    )));
                }
            };
            let length = |key: &str| {
                property
                    .get(key)
                    .and_then(Value::as_u64)
                    .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
            };
            let (min_length, max_length) = (length("minLength"), length("maxLength"));
            if min_length.is_some_and(|min| min as usize > MAX_TEXT) {
                return Err(refuse(format!(
                    "the field {name:?} asks for at least {min_length:?} characters; a card takes \
                     at most {MAX_TEXT}"
                )));
            }
            // An address the asker fills in is one a tap could open: web addresses only.
            let default = text_of(property, "default")
                .filter(|text| {
                    format != Some(TextFormat::Uri)
                        || ["https://", "http://"]
                            .iter()
                            .any(|scheme| text.to_ascii_lowercase().starts_with(scheme))
                })
                .map(|text| clean(text, MAX_TEXT, notes));
            Ok((
                Field::Text {
                    key: name.to_owned(),
                    title: title_text,
                    description,
                    placeholder: None,
                    format,
                    min_length,
                    max_length,
                    default,
                    required,
                },
                Slot::Text {
                    key: name.to_owned(),
                },
            ))
        }
        "number" | "integer" => {
            let integer = kind == "integer";
            let bound = |key: &str| property.get(key).and_then(Value::as_f64);
            let (min, max) = (bound("minimum"), bound("maximum"));
            let default = bound("default").filter(|default| {
                min.is_none_or(|min| *default >= min) && max.is_none_or(|max| *default <= max)
            });
            Ok((
                Field::Number {
                    key: name.to_owned(),
                    title: title_text,
                    description,
                    integer,
                    min,
                    max,
                    default,
                    required,
                },
                Slot::Number {
                    key: name.to_owned(),
                    integer,
                },
            ))
        }
        // A toggle starts off whatever the asker suggests: turning it on is the user's own tap.
        "boolean" => Ok((
            Field::Toggle {
                key: name.to_owned(),
                title: title_text,
                description,
                default: false,
            },
            Slot::Toggle {
                key: name.to_owned(),
            },
        )),
        other => Err(refuse(format!(
            "the field {name:?} has the type {other:?}, which a card cannot show; use string, \
             number, integer, boolean, or an array of options"
        ))),
    }
}

/// An option as the schema lists it.
struct Listed<'a> {
    value: String,
    title: Option<&'a str>,
    description: Option<&'a str>,
    preview: Option<&'a str>,
}

/// A question's options with their values, whether several may be picked, and whether it must
/// be answered.
struct Options {
    options: Vec<(ChoiceOption, String)>,
    multi: bool,
    required: bool,
}

fn options(
    name: &str,
    property: &Value,
    array: bool,
    required: bool,
    notes: &mut Notes,
) -> Result<Options, Refused> {
    let source = if array {
        property
            .get("items")
            .ok_or_else(|| refuse(format!("the array field {name:?} has no items")))?
    } else {
        property
    };
    let listed: Vec<Listed> =
        if let Some(titled) = source.get(if array { "anyOf" } else { "oneOf" }) {
            titled
                .as_array()
                .ok_or_else(|| refuse(format!("the options of {name:?} are not a list")))?
                .iter()
                .map(|option| {
                    let value = option.get("const").and_then(Value::as_str).ok_or_else(|| {
                        refuse(format!("an option of {name:?} has no string const"))
                    })?;
                    let preview = option
                        .get("_meta")
                        .and_then(|meta| meta.get(PREVIEW_META))
                        .and_then(|meta| meta.get("preview"))
                        .and_then(Value::as_str);
                    Ok(Listed {
                        value: value.to_owned(),
                        title: text_of(option, "title"),
                        description: text_of(option, "description"),
                        preview,
                    })
                })
                .collect::<Result<_, Refused>>()?
        } else if let Some(values) = source.get("enum").and_then(Value::as_array) {
            // MCP's legacy `enumNames` titles plain values.
            let names = property.get("enumNames").and_then(Value::as_array);
            values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    let value = value
                        .as_str()
                        .ok_or_else(|| refuse(format!("an option of {name:?} is not a string")))?;
                    let title = names
                        .and_then(|names| names.get(index))
                        .and_then(Value::as_str);
                    Ok(Listed {
                        value: value.to_owned(),
                        title,
                        description: None,
                        preview: None,
                    })
                })
                .collect::<Result<_, Refused>>()?
        } else {
            return Err(refuse(format!(
                "the field {name:?} lists no options (oneOf, anyOf or enum)"
            )));
        };
    if listed.is_empty() || listed.len() > MAX_OPTIONS {
        return Err(refuse(format!(
            "the field {name:?} has {} options; a card shows 1 to {MAX_OPTIONS}",
            listed.len()
        )));
    }
    let mut labels = HashSet::new();
    let mut options = Vec::new();
    for Listed {
        value,
        title,
        description,
        preview,
    } in listed
    {
        let label = line(title.unwrap_or(&value), MAX_LABEL, notes);
        if label.is_empty() || !labels.insert(label.clone()) {
            return Err(refuse(format!(
                "two options of {name:?} read the same, {label:?}, or one reads as nothing; give \
                 each a title of its own of at most {MAX_LABEL} characters"
            )));
        }
        options.push((
            ChoiceOption {
                label,
                description: description.map(|text| line(text, MAX_DESCRIPTION, notes)),
                preview: preview
                    .filter(|text| !text.trim().is_empty())
                    .map(|text| block(text, MAX_PREVIEW, MAX_PREVIEW, notes)),
                recommended: false,
            },
            value,
        ));
    }
    let count = |key: &str| property.get(key).and_then(Value::as_u64);
    let (min_items, max_items) = (count("minItems"), count("maxItems"));
    if !array {
        return Ok(Options {
            options,
            multi: false,
            required,
        });
    }
    // A card picks one, or any number: other counts it cannot hold to.
    if min_items.is_some_and(|min| min > 1)
        || max_items.is_some_and(|max| max > 1 && (max as usize) < options.len())
    {
        return Err(refuse(format!(
            "the field {name:?} asks for {min_items:?} to {max_items:?} picks; a card takes one \
             pick or any number of them"
        )));
    }
    Ok(Options {
        options,
        multi: max_items != Some(1),
        required: required || min_items == Some(1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fronde_protocol::ChoiceAnswer;
    use fronde_protocol::request::check;
    use fronde_protocol::{Response, request};
    use serde_json::json;

    fn choice(selected: &[&str], other: Option<&str>, note: Option<&str>) -> FieldValue {
        FieldValue::Choice(ChoiceAnswer {
            selected: selected.iter().map(|s| (*s).to_owned()).collect(),
            other: other.map(str::to_owned),
            note: note.map(str::to_owned),
        })
    }

    fn values(pairs: &[(&str, FieldValue)]) -> BTreeMap<String, FieldValue> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect()
    }

    #[test]
    fn secret_looking_names_are_caught() {
        for (name, title) in [
            ("password", None),
            ("api_key", None),
            ("apiKey", None),
            ("APIKey", None),
            ("github-token", None),
            ("x", Some("Your secret")),
            ("x", Some("Mot de passe Nextcloud")),
            ("client_credentials", None),
            ("otp", None),
        ] {
            assert!(
                looks_secret(name) || title.is_some_and(looks_secret),
                "{name} {title:?}"
            );
        }
        for (name, title) in [
            ("monkey", None),
            ("keyboard_layout", None),
            ("name", Some("Ton prénom")),
            ("passage", None),
            ("tokenizer", None),
        ] {
            assert!(
                !looks_secret(name) && !title.is_some_and(looks_secret),
                "{name} {title:?}"
            );
        }
    }

    #[test]
    fn secret_words_are_caught_however_written() {
        for text in [
            // Leetspeak, case, width and look-alike letters.
            "p4ss",
            "passw0rd",
            "P4SSW0RD",
            "\u{ff30}\u{ff41}\u{ff53}\u{ff53}\u{ff57}\u{ff4f}\u{ff52}\u{ff44}",
            "p\u{430}ssword",
            "\u{3c1}assword",
            // Invisible characters join the word they split.
            "pass\u{200b}word",
            "pa\u{ad}ss\u{2060}wd",
            // Words apart or together.
            "Mot de passe",
            "mot-de-passe",
            "motDePasse",
            "Ton code secret",
            "Ta clé API",
            "cle d'acces",
            "Ton jeton",
            "Tes identifiants",
            "Code PIN",
            "pin",
            "passcode",
            "CVV",
            "cvc",
            "IBAN",
            "2FA",
            "TOTP",
            "mfa_code",
            "api_key",
            "accessToken",
            "private-key",
        ] {
            assert!(looks_secret(text), "{text:?}");
        }
        for text in [
            "monkey",
            "keyboard layout",
            "tokenizer",
            "Ton prénom",
            "passage",
            "Nombre de nuits",
            "Quelle appli installer ?",
            "Type your own answer, or add a note to the option you chose above (optional).",
        ] {
            assert!(!looks_secret(text), "{text:?}");
        }
    }

    #[test]
    fn every_typed_text_is_scanned_for_a_secret() {
        let declined = |message: &str, schema: Value| {
            let error = form(message, &schema).unwrap_err();
            assert!(error.secret, "{schema}: {error}");
            // The asker learns only that it was declined, the log which text tripped the rule.
            assert_eq!(error.reason, SECRET_REFUSAL);
            error.detail
        };
        let one = |property: Value| json!({"type": "object", "properties": {"f": property}});
        // The message, even above a harmless field.
        let detail = declined("Saisis ton mot de passe", one(json!({"type": "string"})));
        assert!(detail.contains("the message"), "{detail}");
        // A text field's description.
        let detail = declined(
            "",
            one(json!({"type": "string", "description": "Le code reçu par SMS"})),
        );
        assert!(detail.contains("\"f\""), "{detail}");
        // A question that takes free text, as AskUserQuestion's do: its question...
        let mut ask = ask_user_question();
        ask["properties"]["question_0"]["description"] = json!("Quel est ton code PIN ?");
        declined("Please answer the following questions.", ask);
        // ...and its free-text field.
        let mut ask = ask_user_question();
        ask["properties"]["question_1_custom"]["title"] = json!("Password");
        declined("Please answer the following questions.", ask);
        // A plain choice takes nothing typed: its words are the asker's own.
        assert!(
            form(
                "",
                &one(json!({"type": "string", "title": "Key size", "enum": ["2048", "4096"]}))
            )
            .is_ok()
        );
    }

    #[test]
    fn an_oversized_card_is_refused() {
        let options: Vec<Value> = (0..MAX_OPTIONS)
            .map(|n| json!({"const": format!("o{n}"), "title": format!("Option {n}"), "description": "d".repeat(MAX_DESCRIPTION)}))
            .collect();
        let properties: Map<String, Value> = (0..MAX_FIELDS)
            .map(|n| {
                (
                    format!("q{n}"),
                    json!({"type": "string", "title": "Q", "oneOf": options}),
                )
            })
            .collect();
        let error = form("m", &json!({"type": "object", "properties": properties})).unwrap_err();
        assert!(!error.secret);
        assert!(error.reason.contains("bytes as JSON"), "{error}");
    }

    #[test]
    fn defaults_never_choose_for_the_user() {
        let schema = json!({
            "type": "object",
            "properties": {
                "agree": {"type": "boolean", "title": "D'accord", "default": true},
                "site": {"type": "string", "format": "uri", "default": "javascript:alert(1)"},
                "home": {"type": "string", "format": "uri", "default": "https://example.com"},
            },
        });
        let elicited = form("m", &schema).unwrap();
        let Card::Form { fields, .. } = &elicited.card else {
            unreachable!()
        };
        // A toggle starts off, whatever the asker suggests.
        assert!(matches!(&fields[0], Field::Toggle { default: false, .. }));
        // Only a web address is filled in for the user.
        assert!(matches!(&fields[1], Field::Text { default: None, .. }));
        assert!(
            matches!(&fields[2], Field::Text { default: Some(home), .. } if home == "https://example.com")
        );
    }

    #[test]
    fn a_refusal_fallback_reads_as_a_question_to_pick() {
        // The Claude adapter's `refusalFallbackToCreateRequest` (0.107.0).
        let schema = json!({
            "type": "object",
            "properties": {"choice": {"type": "string", "oneOf": [
                {"const": "retry_fallback", "title": "Retry with Opus", "description": "The session continues on Opus."},
                {"const": "cancelled", "title": "Keep the refusal", "description": "You can send a new message."},
            ]}},
        });
        let elicited = form(
            "Fable declined this request (cyber). Retry with Opus?\n\nSome guidance.",
            &schema,
        )
        .unwrap();
        let Card::Form {
            message, fields, ..
        } = &elicited.card
        else {
            unreachable!()
        };
        assert_eq!(message, "Some guidance.");
        let Field::Choice {
            header,
            question,
            required,
            other,
            ..
        } = &fields[0]
        else {
            unreachable!()
        };
        assert_eq!(header, &None);
        assert_eq!(
            question,
            "Fable declined this request (cyber). Retry with Opus?"
        );
        // Nothing is chosen until the user picks; sending without a pick is refused.
        assert!(*required && !*other);
        let empty = Response::Form {
            values: BTreeMap::new(),
        };
        assert!(check(&elicited.card, &empty).is_err());
    }

    #[test]
    fn plain_fields_map_to_their_kinds() {
        let schema = json!({
            "type": "object",
            "properties": {
                "email": {"type": "string", "title": "E-mail", "format": "email"},
                "nights": {"type": "integer", "title": "Nuits", "minimum": 1, "maximum": 14, "default": 2},
                "budget": {"type": "number", "description": "En euros"},
                "pets": {"type": "boolean", "title": "Animaux", "default": true},
                "day": {"type": "string", "format": "date"},
                "name": {"type": "string", "minLength": 2, "maxLength": 40, "default": "Alix"},
            },
            "required": ["email", "nights"],
        });
        let elicited = form("Réserver une chambre", &schema).unwrap();
        let Card::Form {
            message, fields, ..
        } = &elicited.card
        else {
            unreachable!()
        };
        assert_eq!(message, "Réserver une chambre");
        assert_eq!(
            fields[0],
            Field::Text {
                key: "email".into(),
                title: "E-mail".into(),
                description: None,
                placeholder: None,
                format: Some(TextFormat::Email),
                min_length: None,
                max_length: None,
                default: None,
                required: true,
            }
        );
        assert_eq!(
            fields[1],
            Field::Number {
                key: "nights".into(),
                title: "Nuits".into(),
                description: None,
                integer: true,
                min: Some(1.0),
                max: Some(14.0),
                default: Some(2.0),
                required: true,
            }
        );
        assert!(matches!(
            &fields[2],
            Field::Number { key, title, description: Some(d), integer: false, required: false, .. }
                if key == "budget" && title == "budget" && d == "En euros"
        ));
        assert!(matches!(
            &fields[3],
            // Starts off whatever the schema suggests (`defaults_never_choose_for_the_user`).
            Field::Toggle { key, default: false, .. } if key == "pets"
        ));
        assert!(matches!(
            &fields[4],
            Field::Text {
                format: Some(TextFormat::Date),
                ..
            }
        ));
        assert!(matches!(
            &fields[5],
            Field::Text { min_length: Some(2), max_length: Some(40), default: Some(d), .. } if d == "Alix"
        ));

        let answer = values(&[
            ("email", FieldValue::Text(" alix@example.com ".into())),
            ("nights", FieldValue::Number(3.0)),
            ("budget", FieldValue::Number(120.5)),
            ("pets", FieldValue::Toggle(false)),
            ("name", FieldValue::Text("  ".into())),
        ]);
        let response = Response::Form {
            values: answer.clone(),
        };
        assert_eq!(check(&elicited.card, &response), Ok(()));
        assert_eq!(
            Value::Object(elicited.content(&answer)),
            json!({"email": "alix@example.com", "nights": 3, "budget": 120.5, "pets": false})
        );
    }

    /// The form the Claude adapter 0.107.0 builds for two questions
    /// (`askUserQuestionsToCreateRequest`, `dist/elicitation.js`).
    fn ask_user_question() -> Value {
        json!({
            "type": "object",
            "properties": {
                "question_0": {
                    "type": "string",
                    "title": "Appli",
                    "description": "Quelle appli installer ?",
                    "oneOf": [
                        {"const": "Mealie", "title": "Mealie", "description": "Recettes",
                         "_meta": {"_claude/askUserQuestionOption": {"preview": "[ Mealie ]\n  recettes"}}},
                        {"const": "Seerr", "title": "Seerr", "description": "Films"},
                    ],
                },
                "question_0_custom": {
                    "type": "string",
                    "title": "Other",
                    "description": "Type your own answer, or add a note to the option you chose above (optional).",
                },
                "question_1": {
                    "type": "array",
                    "title": "Pièces",
                    "description": "Quelles pièces ?",
                    "items": {"anyOf": [
                        {"const": "Salon", "title": "Salon"},
                        {"const": "Cuisine", "title": "Cuisine"},
                        {"const": "Chambre", "title": "Chambre"},
                    ]},
                },
                "question_1_custom": {
                    "type": "string",
                    "title": "Other",
                    "description": "Type your own answer to add to your selection above (optional).",
                },
            },
        })
    }

    #[test]
    fn ask_user_question_folds_its_free_text() {
        let elicited = form(
            "Please answer the following questions.",
            &ask_user_question(),
        )
        .unwrap();
        let Card::Form {
            message, fields, ..
        } = &elicited.card
        else {
            unreachable!()
        };
        assert_eq!(message, "Please answer the following questions.");
        assert_eq!(fields.len(), 2, "{fields:#?}");
        let Field::Choice {
            key,
            header,
            question,
            options,
            multi,
            other,
            required,
            note,
        } = &fields[0]
        else {
            panic!("{:?}", fields[0]);
        };
        assert_eq!(key, "question_0");
        assert_eq!(header.as_deref(), Some("Appli"));
        assert_eq!(question, "Quelle appli installer ?");
        assert_eq!(options[0].label, "Mealie");
        assert_eq!(options[0].description.as_deref(), Some("Recettes"));
        assert_eq!(
            options[0].preview.as_deref(),
            Some("[ Mealie ]\n  recettes")
        );
        assert!(!multi && *other && *note && !required);
        assert!(matches!(
            &fields[1],
            Field::Choice {
                multi: true,
                other: true,
                ..
            }
        ));

        let answer = values(&[
            ("question_0", choice(&["Seerr"], None, Some("le soir"))),
            (
                "question_1",
                choice(&["Salon", "Chambre"], Some("Garage"), None),
            ),
        ]);
        assert_eq!(
            check(
                &elicited.card,
                &Response::Form {
                    values: answer.clone()
                }
            ),
            Ok(())
        );
        assert_eq!(
            Value::Object(elicited.content(&answer)),
            json!({
                "question_0": "Seerr",
                "question_0_custom": "le soir",
                "question_1": ["Salon", "Chambre"],
                "question_1_custom": "Garage",
            })
        );
        // "Autre…" alone answers a question with one answer through its free text.
        let answer = values(&[("question_0", choice(&[], Some(" Tandoor "), None))]);
        assert_eq!(
            Value::Object(elicited.content(&answer)),
            json!({"question_0_custom": "Tandoor"})
        );
        // Nothing answered, nothing given back: the adapter reads it as skipped.
        assert_eq!(elicited.content(&BTreeMap::new()), Map::new());
    }

    #[test]
    fn one_question_takes_the_message_as_its_question() {
        let mut schema = ask_user_question();
        let properties = schema["properties"].as_object_mut().unwrap();
        properties.remove("question_1");
        properties.remove("question_1_custom");
        properties["question_0"]
            .as_object_mut()
            .unwrap()
            .remove("description");
        let elicited = form("Quelle appli installer ?", &schema).unwrap();
        let Card::Form {
            message, fields, ..
        } = &elicited.card
        else {
            unreachable!()
        };
        assert_eq!(message, "");
        let Field::Choice {
            header, question, ..
        } = &fields[0]
        else {
            unreachable!()
        };
        assert_eq!(header.as_deref(), Some("Appli"));
        assert_eq!(question, "Quelle appli installer ?");
    }

    #[test]
    fn plain_choices_take_no_free_text_and_give_values_back() {
        let schema = json!({
            "type": "object",
            "properties": {
                "color": {"type": "string", "title": "Couleur", "enum": ["r", "g"], "enumNames": ["Rouge", "Vert"]},
                "one": {"type": "array", "maxItems": 1, "items": {"type": "string", "enum": ["a", "b"]}},
                "some": {"type": "array", "minItems": 1, "items": {"type": "string", "enum": ["x", "y", "z"]}},
            },
            "required": ["color"],
        });
        let elicited = form("", &schema).unwrap();
        let Card::Form { fields, .. } = &elicited.card else {
            unreachable!()
        };
        assert!(matches!(
            &fields[0],
            Field::Choice { other: false, note: false, required: true, multi: false, options, .. }
                if options[0].label == "Rouge"
        ));
        assert!(matches!(
            &fields[1],
            Field::Choice {
                multi: false,
                required: false,
                ..
            }
        ));
        assert!(matches!(
            &fields[2],
            Field::Choice {
                multi: true,
                required: true,
                ..
            }
        ));
        let answer = values(&[
            ("color", choice(&["Vert"], None, None)),
            ("one", choice(&["b"], None, None)),
            ("some", choice(&["x", "z"], None, None)),
        ]);
        assert_eq!(
            check(
                &elicited.card,
                &Response::Form {
                    values: answer.clone()
                }
            ),
            Ok(())
        );
        assert_eq!(
            Value::Object(elicited.content(&answer)),
            json!({"color": "g", "one": ["b"], "some": ["x", "z"]})
        );
        // A note has nowhere to go: the card refuses it.
        let noted = values(&[("color", choice(&["Vert"], None, Some("foncé")))]);
        assert!(check(&elicited.card, &Response::Form { values: noted }).is_err());
    }

    #[test]
    fn unsupported_and_secret_schemas_are_refused() {
        let refused = |schema: Value| form("m", &schema).unwrap_err();
        let one = |property: Value| json!({"type": "object", "properties": {"f": property}});
        for (schema, says) in [
            (json!([]), "not an object"),
            (json!({"type": "array"}), "a form is an object"),
            (one(json!({"type": "object"})), "type \"object\""),
            (one(json!({"description": "x"})), "has no type"),
            (
                one(json!({"type": "string", "format": "ipv4"})),
                "format \"ipv4\"",
            ),
            (
                one(json!({"type": "array", "items": {"type": "number"}})),
                "lists no options",
            ),
            (one(json!({"type": "string", "enum": []})), "has 0 options"),
            (
                one(
                    json!({"type": "string", "oneOf": [{"const": "a", "title": "Même"}, {"const": "b", "title": "Même"}]}),
                ),
                "read the same",
            ),
            (
                one(json!({"type": "array", "minItems": 2, "items": {"enum": ["a", "b", "c"]}})),
                "picks",
            ),
            (
                one(json!({"type": "string", "minLength": 5000})),
                "at least",
            ),
            (
                json!({"type": "object", "properties": {"f\u{200b}": {"type": "string"}}}),
                "hidden character",
            ),
            (
                json!({"type": "object", "properties": (0..13).map(|n| (format!("b{n}"), json!({"type": "boolean"}))).collect::<Map<_, _>>()}),
                "at most 12",
            ),
        ] {
            let error = refused(schema.clone());
            assert!(!error.secret, "{schema}");
            assert!(error.reason.contains(says), "{schema}: {error}");
        }
        for schema in [
            one(json!({"type": "string", "title": "Mot de passe"})),
            json!({"type": "object", "properties": {"api_token": {"type": "string"}}}),
            json!({"type": "object", "properties": {"pin": {"type": "string", "format": "password"}}}),
            json!({"type": "object", "properties": {"secret_number": {"type": "integer"}}}),
        ] {
            let error = refused(schema.clone());
            assert!(error.secret, "{schema}");
            assert_eq!(error.reason, SECRET_REFUSAL);
            assert!(
                error.detail.contains("looks like it asks for a secret"),
                "{error}"
            );
        }
        // Options may be named anything: they carry no secret the asker does not know.
        assert!(
            form(
                "m",
                &one(json!({"type": "string", "title": "Key size", "enum": ["2048", "4096"]}))
            )
            .is_ok()
        );
    }

    #[test]
    fn asker_texts_are_cleaned_and_the_card_says_so() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "title": format!("Nom{}", "\u{202e}"), "description": "d".repeat(500)},
            },
        });
        let elicited = form(&format!("début {} fin", "x".repeat(3000)), &schema).unwrap();
        let Card::Form {
            message,
            fields,
            truncated,
            replaced,
            ..
        } = &elicited.card
        else {
            unreachable!()
        };
        assert!(*truncated && *replaced);
        assert!(
            message.starts_with("début") && message.ends_with("fin"),
            "{message}"
        );
        assert!(message.contains("caractères omis"));
        let Field::Text {
            title, description, ..
        } = &fields[0]
        else {
            unreachable!()
        };
        assert_eq!(title, "Nom\u{fffd}");
        assert!(description.as_ref().unwrap().chars().count() <= MAX_DESCRIPTION);
        assert_eq!(elicited.card.check(), Ok(()));
        assert_eq!(
            request::summary(
                &elicited.card,
                &Response::Form {
                    values: values(&[("name", FieldValue::Text("Alix".into()))])
                }
            ),
            "Alix"
        );
    }
}
