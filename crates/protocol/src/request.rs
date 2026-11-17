//! Requests to the user: what a card shows, what the page answers, how the request ended, and
//! the rules an answer must follow. The server and the page check answers with the same
//! [`check`], so the page can say what is wrong before sending, and the server stays the judge.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A request's number: unique for the life of the database, whatever stream shows it.
pub type RequestId = u64;

/// The longest free text an answer may carry ("Autre" or a note), in characters.
pub const MAX_TEXT: usize = 2000;

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
}

impl Card {
    /// The card's kind, as stored with its request.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Form { .. } => "form",
        }
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

/// Checks that `response` answers `card`: known fields only, offered options only, one pick
/// for a single choice, required fields answered, free text non-empty and short enough.
pub fn check(card: &Card, response: &Response) -> Result<(), Invalid> {
    let (Card::Form { fields, .. }, Response::Form { values }) = (card, response) else {
        return match response {
            Response::Decline => Ok(()),
            _ => Err(invalid(
                None,
                "Cette réponse ne correspond pas à la demande.",
            )),
        };
    };
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
        for text in [&answer.other, &answer.note].into_iter().flatten() {
            if text.chars().count() > MAX_TEXT {
                return Err(invalid(
                    Some(key),
                    format!("Texte trop long : {MAX_TEXT} caractères au plus."),
                ));
            }
        }
    }
    Ok(())
}

/// A short account of an answer, for the closed card and the audit trail: the picked labels
/// and "Autre" texts, field after field. Notes are left out.
pub fn summary(card: &Card, response: &Response) -> String {
    match (card, response) {
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

        let error = refused(
            &card,
            &Response::Permission {
                choice: "allow_once".into(),
            },
        );
        assert_eq!(error.key, None);
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
