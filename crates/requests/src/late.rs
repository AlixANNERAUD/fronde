//! Late answers: a question or a secret answered once nobody waits for it any more. Fronde
//! restarted since the card was shown, or the tool call that asked was cut by its MCP client.
//! Only a thread's agent gets one, and only for what it asked with `user_ask` or `user_secret`,
//! as a message in its thread (see [`Journal::answered_late`](crate::Journal::answered_late)).
//! An answer meant for an upstream server, for the agent's own form or for a permission never
//! goes there: those requests die with the call that waited on them.

use fronde_protocol::request::{Card, Field, FieldValue, RequestId, Response};

use crate::{Resolution, Saved};

/// What the asking thread's agent gets, as a message of the owner's, once its request was
/// answered late.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LateAnswer {
    /// The request answered.
    pub request: RequestId,
    /// The message, as the thread's stream shows it: « Réponse à ta question « … » : … ».
    pub text: String,
    /// What the agent may do with it, for the agent only: how long a `secret_ref` lasts.
    pub note: Option<String>,
}

/// The late answer to `card` that `resolution` makes for request `request`: `None` when there is
/// nothing to give (the request expired or was cancelled, or the answer does not fit the card).
pub(crate) fn answer(
    request: RequestId,
    card: &Card,
    resolution: &Resolution,
) -> Option<LateAnswer> {
    let (text, note) = match (card, resolution) {
        (Card::Form { fields, .. }, Resolution::Answered(Response::Form { values })) => {
            let lines: Vec<String> = fields
                .iter()
                .filter_map(|field| {
                    let value = values.get(field.key())?;
                    Some(format!(
                        "Réponse à ta question « {} » : {}",
                        one_line(field.label()),
                        shown(value)
                    ))
                })
                .collect();
            if lines.is_empty() {
                (no_answer(fields), None)
            } else {
                (lines.join("\n"), None)
            }
        }
        (Card::Form { fields, .. }, Resolution::Declined) => (no_answer(fields), None),
        (
            Card::Secret { .. },
            Resolution::Saved(Saved::Conversation {
                secret_ref,
                expires_at,
            }),
        ) => (
            format!("Secret enregistré : secret_ref {secret_ref} (pour cette conversation)"),
            Some(format!(
                "The secret_ref is usable once, in this conversation only, until {}.",
                expires_at.to_rfc3339()
            )),
        ),
        (Card::Secret { .. }, Resolution::Saved(Saved::Stored { name, origin })) => (
            format!("Secret enregistré sous le nom « {name} », pour {origin} seulement"),
            Some(format!(
                "A component sends it to {origin} when its [[sources.credentials]] entry for \
                 {origin} names it as {{ stored = \"{name}\" }}; the owner edits the configuration."
            )),
        ),
        (Card::Secret { label, .. }, Resolution::Declined) => (
            format!(
                "Secret « {} » : je préfère ne pas le donner.",
                one_line(label)
            ),
            None,
        ),
        _ => return None,
    };
    Some(LateAnswer {
        request,
        text,
        note,
    })
}

/// A decline of the questions `fields` ask.
fn no_answer(fields: &[Field]) -> String {
    let questions: Vec<String> = fields
        .iter()
        .map(|field| format!("« {} »", one_line(field.label())))
        .collect();
    let to = if questions.len() > 1 {
        "tes questions"
    } else {
        "ta question"
    };
    format!(
        "Réponse à {to} {} : je préfère ne pas répondre.",
        questions.join(", ")
    )
}

/// An answer to a field, in a line.
fn shown(value: &FieldValue) -> String {
    match value {
        FieldValue::Choice(answer) => {
            let mut picked: Vec<String> = answer
                .selected
                .iter()
                .map(|label| one_line(label))
                .collect();
            if let Some(other) = answer
                .other
                .as_deref()
                .map(one_line)
                .filter(|o| !o.is_empty())
            {
                picked.push(other);
            }
            let mut text = if picked.is_empty() {
                "(aucun choix)".to_owned()
            } else {
                picked.join(", ")
            };
            if let Some(note) = answer
                .note
                .as_deref()
                .map(one_line)
                .filter(|n| !n.is_empty())
            {
                text.push_str(&format!(" (précision : {note})"));
            }
            text
        }
        FieldValue::Text(text) => one_line(text),
        FieldValue::Number(number) => number.to_string(),
        FieldValue::Toggle(true) => "oui".to_owned(),
        FieldValue::Toggle(false) => "non".to_owned(),
    }
}

/// `text` on one line, its runs of white space as one space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone as _, Utc};
    use fronde_protocol::request::{ChoiceAnswer, ChoiceOption};

    fn question(text: &str) -> Field {
        Field::Choice {
            key: "q1".into(),
            header: None,
            question: text.into(),
            options: vec![ChoiceOption {
                label: "Perso".into(),
                description: None,
                preview: None,
                recommended: false,
            }],
            multi: false,
            other: true,
            required: true,
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
            during: Vec::new(),
        }
    }

    #[test]
    fn answers_read_as_the_owner_would_write_them() {
        let card = form(vec![question("Quel agenda ?")]);
        let answered = Resolution::Answered(Response::Form {
            values: [(
                "q1".to_owned(),
                ChoiceAnswer {
                    selected: vec!["Perso".into()],
                    other: Some("  et  le\ntravail ".into()),
                    note: Some("vite".into()),
                }
                .into(),
            )]
            .into(),
        });
        assert_eq!(
            answer(4, &card, &answered).unwrap().text,
            "Réponse à ta question « Quel agenda ? » : Perso, et le travail (précision : vite)"
        );
        assert_eq!(
            answer(4, &card, &Resolution::Declined).unwrap().text,
            "Réponse à ta question « Quel agenda ? » : je préfère ne pas répondre."
        );
        for ended in [Resolution::Expired, Resolution::Cancelled] {
            assert_eq!(answer(4, &card, &ended), None);
        }
        let secret = Card::Secret {
            label: "Mot de passe".into(),
            reason: "Pour lire l'agenda.".into(),
            placeholder: None,
            url: None,
            remember: None,
        };
        let saved = Resolution::Saved(Saved::Conversation {
            secret_ref: "sr_abc".into(),
            expires_at: Utc.with_ymd_and_hms(2026, 11, 17, 12, 0, 0).unwrap(),
        });
        let late = answer(5, &secret, &saved).unwrap();
        assert_eq!(
            late.text,
            "Secret enregistré : secret_ref sr_abc (pour cette conversation)"
        );
        assert!(late.note.unwrap().contains("2026-11-17T12:00:00"));
        let stored = Resolution::Saved(Saved::Stored {
            name: "nextcloud".into(),
            origin: "https://cloud.example.com".into(),
        });
        assert_eq!(
            answer(5, &secret, &stored).unwrap().text,
            "Secret enregistré sous le nom « nextcloud », pour https://cloud.example.com seulement"
        );
        // A form's answer to a secret card, or the other way round, gives nothing.
        assert_eq!(answer(5, &secret, &answered), None);
        assert_eq!(answer(4, &card, &saved), None);
    }
}
