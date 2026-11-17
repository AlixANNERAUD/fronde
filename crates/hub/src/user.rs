//! The `user_*` tools: ask the user, through a card in the chat, and wait for the answer.

use crate::source::Tool;
use crate::tools::{CallContext, ToolOutput, typed_tool_with_context};
use fronde_protocol::{Card, ChoiceOption, Field, Response};
use fronde_requests::{Ask, Requests, Resolution};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

/// How long `user_ask` waits by default: 10 minutes.
pub const DEFAULT_TIMEOUT_MS: u64 = 600_000;
const MIN_TIMEOUT_MS: u64 = 1_000;
const MAX_TIMEOUT_MS: u64 = 3_600_000;
const MAX_QUESTIONS: usize = 4;
const MIN_OPTIONS: usize = 2;
const MAX_OPTIONS: usize = 4;
const MAX_HEADER: usize = 12;
const MAX_CLIENT_REQUEST_ID: usize = 128;

const DESCRIPTION: &str = "Ask the user 1 to 4 multiple-choice questions and wait for the answers. Use when a choice is theirs, not to confirm what you can decide. Recommended option first, with recommended: true. The user can always answer in their own words. Never ask for a password or key here. Blocks up to timeout_ms (default 10 min).";

#[derive(Deserialize, JsonSchema)]
struct AskArgs {
    /// 1 to 4 questions, shown together on one card.
    questions: Vec<QuestionArg>,
    /// How long to wait for the answers, in milliseconds: 1000 to 3600000 (default 600000).
    timeout_ms: Option<u64>,
    /// Give the same value when retrying this call, so the user sees one card, not two.
    client_request_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct QuestionArg {
    /// The full question.
    question: String,
    /// A short chip shown above the question, at most 12 characters, such as "Appli".
    header: String,
    /// Several options may be picked.
    #[serde(default)]
    multi_select: bool,
    /// 2 to 4 options.
    options: Vec<OptionArg>,
}

#[derive(Deserialize, JsonSchema)]
struct OptionArg {
    /// What the user picks, a few words.
    label: String,
    /// What picking it means.
    description: String,
    /// Text shown in monospace while the option is picked: a sample, a layout, a diff.
    preview: Option<String>,
    /// The option you advise; at most one, and first.
    #[serde(default)]
    recommended: bool,
}

/// The `user_*` tools, asking through `requests`.
pub fn user_tools(requests: Arc<Requests>) -> Vec<Tool> {
    let ask = typed_tool_with_context(
        "user_ask",
        DESCRIPTION,
        false,
        move |context: CallContext, args: AskArgs| {
            let requests = requests.clone();
            async move { user_ask(&requests, context, args).await }
        },
    );
    vec![ask]
}

async fn user_ask(requests: &Requests, context: CallContext, args: AskArgs) -> ToolOutput {
    let (card, timeout) = match check(&args) {
        Ok(checked) => checked,
        Err(message) => return ToolOutput::error(message),
    };
    let questions: Vec<String> = args.questions.into_iter().map(|q| q.question).collect();
    let ask = Ask {
        topic: context.topic,
        turn: context.turn,
        caller: context.caller,
        client_request_id: args.client_request_id,
        card,
        for_user: context.user,
        admin_only: false,
        timeout,
    };
    let waiter = match requests.ask(ask).await {
        Ok(waiter) => waiter,
        Err(error) => {
            tracing::error!("user_ask cannot open its request: {error:#}");
            return ToolOutput::error(
                "the question could not be shown to the user; ask it in your reply instead",
            );
        }
    };
    match waiter.wait().await {
        Resolution::Answered(Response::Form { values }) => {
            let answers: Vec<Value> = questions
                .iter()
                .enumerate()
                .map(|(index, question)| {
                    let answer = values.get(&key(index)).cloned().unwrap_or_default();
                    json!({
                        "question": question,
                        "selected": answer.selected,
                        "other": answer.other.map(|other| other.trim().to_owned()),
                        "note": answer.note.map(|note| note.trim().to_owned()).filter(|note| !note.is_empty()),
                    })
                })
                .collect();
            ToolOutput::ok(json!({"status": "answered", "answers": answers}))
        }
        Resolution::Answered(_) => {
            ToolOutput::error("the answer does not fit the question; ask it in your reply instead")
        }
        Resolution::Declined => ToolOutput::ok(json!({"status": "declined"}))
            .with_next("The user declined to answer; do not ask again this turn."),
        Resolution::Expired => ToolOutput::ok(json!({"status": "expired"}))
            .with_next("No answer in time; go on without it if you can, or ask in your reply."),
        Resolution::Cancelled => ToolOutput::ok(json!({"status": "cancelled"}))
            .with_next("The question was cancelled; stop and wait for the user's next message."),
    }
}

fn key(index: usize) -> String {
    format!("q{}", index + 1)
}

/// The card the arguments describe, or what to fix in them.
fn check(args: &AskArgs) -> Result<(Card, Duration), String> {
    let count = args.questions.len();
    if !(1..=MAX_QUESTIONS).contains(&count) {
        return Err(format!(
            "questions has {count} items; give 1 to {MAX_QUESTIONS}, and ask more in a later call"
        ));
    }
    let timeout_ms = args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(format!(
            "timeout_ms {timeout_ms} is out of range; give {MIN_TIMEOUT_MS} to {MAX_TIMEOUT_MS}, or leave it out for {DEFAULT_TIMEOUT_MS}"
        ));
    }
    if let Some(id) = &args.client_request_id
        && (id.is_empty() || id.chars().count() > MAX_CLIENT_REQUEST_ID)
    {
        return Err(format!(
            "client_request_id must have 1 to {MAX_CLIENT_REQUEST_ID} characters; leave it out if you do not retry"
        ));
    }
    let mut fields = Vec::with_capacity(count);
    for (index, q) in args.questions.iter().enumerate() {
        let n = index + 1;
        if q.question.trim().is_empty() {
            return Err(format!(
                "question {n}: question is empty; write the full question"
            ));
        }
        if let Some(earlier) = args.questions[..index]
            .iter()
            .position(|other| other.question.trim() == q.question.trim())
        {
            return Err(format!(
                "question {n} repeats question {}; ask each question once",
                earlier + 1
            ));
        }
        let header = q.header.trim();
        let length = header.chars().count();
        if length == 0 {
            return Err(format!(
                "question {n}: header is empty; give a chip of at most {MAX_HEADER} characters, such as \"Appli\""
            ));
        }
        if length > MAX_HEADER {
            return Err(format!(
                "question {n}: header {header:?} has {length} characters; use at most {MAX_HEADER}, such as \"Fournisseurs\""
            ));
        }
        let options = q.options.len();
        if !(MIN_OPTIONS..=MAX_OPTIONS).contains(&options) {
            return Err(format!(
                "question {n} has {options} options; give {MIN_OPTIONS} to {MAX_OPTIONS} (the user can always answer in their own words)"
            ));
        }
        let mut labels = HashSet::new();
        for (index, option) in q.options.iter().enumerate() {
            let label = option.label.trim();
            if label.is_empty() {
                return Err(format!(
                    "question {n}, option {}: label is empty; give a few words",
                    index + 1
                ));
            }
            if !labels.insert(label) {
                return Err(format!(
                    "question {n}: label {label:?} appears twice; make each option's label unique"
                ));
            }
            if option.recommended && index > 0 {
                return Err(format!(
                    "question {n}: option {} {label:?} is recommended but not first; put the one recommended option first",
                    index + 1
                ));
            }
        }
        fields.push(Field::Choice {
            key: key(index),
            header: Some(header.to_owned()),
            question: q.question.trim().to_owned(),
            options: q
                .options
                .iter()
                .map(|option| ChoiceOption {
                    label: option.label.trim().to_owned(),
                    description: Some(option.description.trim().to_owned())
                        .filter(|text| !text.is_empty()),
                    preview: option
                        .preview
                        .clone()
                        .filter(|text| !text.trim().is_empty()),
                    recommended: option.recommended,
                })
                .collect(),
            multi: q.multi_select,
            other: true,
            required: true,
        });
    }
    let card = Card::Form {
        message: String::new(),
        fields,
        submit: None,
    };
    Ok((card, Duration::from_millis(timeout_ms)))
}
