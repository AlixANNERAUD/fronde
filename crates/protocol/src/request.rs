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

/// What a request's card shows. Never a secret.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Card {
    /// Questions to answer together, then sent at once.
    Form {
        /// Shown above the fields; empty when the fields say it all.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        message: String,
        fields: Vec<Field>,
        /// The send button's label, "Envoyer" when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        submit: Option<String>,
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
    /// The card's kind, as stored with its request.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Form { .. } => "form",
            Self::Secret { .. } => "secret",
        }
    }
}

/// Where a secret card offers to keep the secret: under `name`, for the canonical `origin`
/// only.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Remember {
    pub name: String,
    pub origin: String,
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
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
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
    },
}

impl Field {
    pub fn key(&self) -> &str {
        match self {
            Self::Choice { key, .. } => key,
        }
    }
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
        values: BTreeMap<String, ChoiceAnswer>,
    },
    /// One of a permission request's options. Until permissions are requests, it answers a
    /// [`crate::Event::Permission`] by its id, with `allow_once` or `reject_once`.
    Permission { choice: String },
    /// The secret a [`Card::Secret`] asks for, and how long to keep it.
    Secret { value: SecretValue, keep: Keep },
    /// The user refuses to answer.
    Decline,
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

/// Checks that `response` answers `card`: for a form, known fields only, offered options only,
/// one pick for a single choice, required fields answered, free text non-empty and short
/// enough; for a secret, a value of at most [`MAX_SECRET`] bytes, kept only where the card
/// offers it. A decline answers any card.
pub fn check(card: &Card, response: &Response) -> Result<(), Invalid> {
    match (card, response) {
        (_, Response::Decline) => Ok(()),
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

fn check_form(fields: &[Field], values: &BTreeMap<String, ChoiceAnswer>) -> Result<(), Invalid> {
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
        let Field::Choice {
            key,
            options,
            multi,
            other,
            required,
            ..
        } = field;
        let answer = values.get(key);
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
        let picked = answer.map_or(0, |a| a.selected.len() + usize::from(a.other.is_some()));
        if picked == 0 {
            if *required {
                return Err(invalid(Some(key), "Réponds à cette question."));
            }
            continue;
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
    }
    Ok(())
}

/// A short account of an answer, for the closed card and the audit trail: the picked labels
/// and "Autre" texts, field after field. Notes are left out.
pub fn summary(card: &Card, response: &Response) -> String {
    match (card, response) {
        // A secret is never summed up, not even by its length.
        (Card::Secret { .. }, _) | (_, Response::Secret { .. }) => String::new(),
        (Card::Form { fields, .. }, Response::Form { values }) => fields
            .iter()
            .filter_map(|field| values.get(field.key()))
            .map(|answer| {
                answer
                    .selected
                    .iter()
                    .map(String::as_str)
                    .chain(answer.other.as_deref().map(str::trim))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" ; "),
        (_, Response::Permission { choice }) => choice.clone(),
        (_, Response::Decline) => String::new(),
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
        }
    }

    fn form(fields: Vec<Field>) -> Card {
        Card::Form {
            message: String::new(),
            fields,
            submit: None,
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
                .map(|(key, answer)| ((*key).to_owned(), answer.clone()))
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
            }],
            submit: None,
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
}
