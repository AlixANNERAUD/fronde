//! The agent's elicitations as cards: a form the agent asks the user to fill (Claude's
//! `AskUserQuestion`, a refusal's fallback, an MCP server's form it forwards) becomes a
//! [`Card::Form`] of the running turn (see [`fronde_requests::elicitation`]), and the request's
//! end the agent's answer: the form's content when answered, `decline` when the user refused,
//! `cancel` when nobody answered (it expired, the turn was stopped or ended).

use agent_client_protocol::v1::schema::{
    CreateElicitationRequest, CreateElicitationResponse, ElicitationAcceptAction,
    ElicitationAction, ElicitationContentValue, ElicitationMode,
};
use fronde_protocol::Response;
use fronde_requests::Resolution;
use fronde_requests::elicitation::{Elicited, Refused, form};
use std::collections::BTreeMap;

/// Where a refusal's reason goes in the agent's answer, which has no field for it.
pub const REFUSED_META: &str = "fronde/refused";

/// The card for the agent's form, or why it cannot be shown.
pub fn card(request: &CreateElicitationRequest) -> Result<Elicited, Refused> {
    let ElicitationMode::Form(mode) = &request.mode else {
        return Err(Refused {
            reason: "Fronde shows forms only".into(),
            secret: false,
        });
    };
    let schema = serde_json::to_value(&mode.requested_schema).map_err(|error| Refused {
        reason: format!("the requested schema cannot be read: {error}"),
        secret: false,
    })?;
    form(&request.message, &schema)
}

/// The agent's answer once its card ended.
pub fn response(resolution: &Resolution, elicited: &Elicited) -> CreateElicitationResponse {
    match resolution {
        Resolution::Answered(Response::Form { values }) => {
            let content: BTreeMap<String, ElicitationContentValue> =
                serde_json::from_value(serde_json::Value::Object(elicited.content(values)))
                    .unwrap_or_else(|error| {
                        // `content` gives only strings, numbers, booleans and string arrays.
                        tracing::error!("cannot give the agent its form's answers: {error}");
                        BTreeMap::new()
                    });
            CreateElicitationResponse::new(ElicitationAction::Accept(
                ElicitationAcceptAction::new().content(content),
            ))
        }
        Resolution::Declined => CreateElicitationResponse::new(ElicitationAction::Decline),
        // An answer that is not a form's cannot pass `check`.
        Resolution::Answered(_)
        | Resolution::Saved(_)
        | Resolution::Expired
        | Resolution::Cancelled => CreateElicitationResponse::new(ElicitationAction::Cancel),
    }
}

/// The answer to a form Fronde refuses to show: a decline, with the reason in `_meta`, since the
/// response has no other place for it.
pub fn refusal(refused: &Refused) -> CreateElicitationResponse {
    let mut meta = serde_json::Map::new();
    meta.insert(
        REFUSED_META.into(),
        serde_json::Value::String(refused.reason.clone()),
    );
    CreateElicitationResponse::new(ElicitationAction::Decline).meta(meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::v1::schema::{ElicitationFormMode, ElicitationSessionScope};
    use fronde_protocol::{ChoiceAnswer, FieldValue};
    use serde_json::json;

    /// The adapter's form for one question, as it sends it on the wire.
    fn ask_one() -> CreateElicitationRequest {
        serde_json::from_value(json!({
            "mode": "form",
            "sessionId": "s1",
            "toolCallId": "toolu_1",
            "message": "Quelle appli installer ?",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "question_0": {"type": "string", "title": "Appli", "oneOf": [
                        {"const": "Mealie", "title": "Mealie", "description": "Recettes"},
                        {"const": "Seerr", "title": "Seerr"},
                    ]},
                    "question_0_custom": {"type": "string", "title": "Other"},
                },
            },
        }))
        .unwrap()
    }

    #[test]
    fn answer_types_are_v1() {
        let request = CreateElicitationRequest::new(
            ElicitationFormMode::new(
                ElicitationSessionScope::new("s1"),
                serde_json::from_value(json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "count": {"type": "integer"},
                        "ratio": {"type": "number"},
                        "ok": {"type": "boolean"},
                        "rooms": {"type": "array", "items": {"type": "string", "enum": ["a", "b"]}},
                    },
                }))
                .unwrap(),
            ),
            "Remplir",
        );
        let elicited = card(&request).unwrap();
        let values = [
            ("name", FieldValue::Text("Alix".into())),
            ("count", FieldValue::Number(3.0)),
            ("ratio", FieldValue::Number(0.5)),
            ("ok", FieldValue::Toggle(true)),
            (
                "rooms",
                FieldValue::Choice(ChoiceAnswer {
                    selected: vec!["a".into(), "b".into()],
                    ..ChoiceAnswer::default()
                }),
            ),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
        let response = response(&Resolution::Answered(Response::Form { values }), &elicited);
        let ElicitationAction::Accept(accept) = &response.action else {
            panic!("{response:?}");
        };
        let content = accept.content.as_ref().unwrap();
        assert_eq!(
            content["name"],
            ElicitationContentValue::String("Alix".into())
        );
        assert_eq!(content["count"], ElicitationContentValue::Integer(3));
        assert_eq!(content["ratio"], ElicitationContentValue::Number(0.5));
        assert_eq!(content["ok"], ElicitationContentValue::Boolean(true));
        assert_eq!(
            content["rooms"],
            ElicitationContentValue::StringArray(vec!["a".into(), "b".into()])
        );
        assert_eq!(
            serde_json::to_value(&response).unwrap(),
            json!({"action": "accept", "content": {"count": 3, "name": "Alix", "ok": true, "ratio": 0.5, "rooms": ["a", "b"]}})
        );
    }

    #[test]
    fn ends_become_decline_or_cancel() {
        let elicited = card(&ask_one()).unwrap();
        let action = |resolution| response(&resolution, &elicited).action;
        assert_eq!(action(Resolution::Declined), ElicitationAction::Decline);
        for resolution in [Resolution::Expired, Resolution::Cancelled] {
            assert_eq!(action(resolution), ElicitationAction::Cancel);
        }
        let refused = card(
            &serde_json::from_value(json!({
                "mode": "form", "sessionId": "s1", "message": "m",
                "requestedSchema": {"type": "object", "properties": {"password": {"type": "string"}}},
            }))
            .unwrap(),
        )
        .unwrap_err();
        assert!(refused.secret);
        let answer = serde_json::to_value(refusal(&refused)).unwrap();
        assert_eq!(answer["action"], "decline");
        assert!(
            answer["_meta"][REFUSED_META]
                .as_str()
                .unwrap()
                .contains("takes no secret in a form")
        );
    }

    #[test]
    fn url_mode_is_not_a_card() {
        let request: CreateElicitationRequest = serde_json::from_value(json!({
            "mode": "url", "sessionId": "s1", "message": "Sign in",
            "url": "https://example.com/login", "elicitationId": "e1",
        }))
        .unwrap();
        assert!(card(&request).is_err());
    }
}
