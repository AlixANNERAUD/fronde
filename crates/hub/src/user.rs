//! The `user_*` tools: ask the user, through a card in the chat, and wait for the answer.

use crate::config::is_valid_name;
use crate::origin::Origin;
use crate::source::Tool;
use crate::tools::{CallContext, ToolOutput, typed_tool_with_context};
use fronde_protocol::{Card, ChoiceOption, Field, Remember, Response};
use fronde_requests::{Ask, Caller, Requests, Resolution, Reused, Saved, Waiter};
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
/// Card texts stay short enough for a phone, and a card small enough for the chat's messages.
const MAX_QUESTION: usize = 300;
const MAX_LABEL: usize = 60;
const MAX_DESCRIPTION: usize = 200;
const MAX_PREVIEW: usize = 4000;
/// The longest name an outside client may show on a card.
const MAX_CLIENT_NAME: usize = 40;
/// Names an outside client may not take: a card headed with them would pass for Fronde's own.
const RESERVED_CLIENT_NAMES: [&str; 2] = ["fronde", "assistant"];

/// A secret card's texts.
const MAX_SECRET_LABEL: usize = 60;
const MAX_REASON: usize = 300;
const MAX_PLACEHOLDER: usize = 60;
const MAX_URL: usize = 2000;

const SECRET_DESCRIPTION: &str = "Ask the user for one secret (password, token, key) in a private card and wait. The value never reaches you: you get a one-use secret_ref for this conversation, or, if the owner keeps it, the name it is stored under. One secret per call; never ask for a secret in chat or in user_ask. Blocks up to timeout_ms (default 10 min).";

const DESCRIPTION: &str = "Ask the user 1 to 4 multiple-choice questions and wait for the answers. Use when a choice is theirs, not to confirm what you can decide. Recommended option first, with recommended: true. The user can always answer in their own words. Never ask for a password or key here. Blocks up to timeout_ms (default 10 min).";

#[derive(Deserialize, JsonSchema)]
struct AskArgs {
    /// 1 to 4 questions, shown together on one card.
    questions: Vec<QuestionArg>,
    /// How long to wait for the answers, in milliseconds: 1000 to 3600000 (default 600000).
    timeout_ms: Option<u64>,
    /// Give the same value, with the same questions, when retrying this call in the same turn,
    /// so the user sees one card, not two. A new question needs a new value.
    client_request_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct QuestionArg {
    /// The full question, on one line, at most 300 characters.
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
    /// What the user picks, a few words (at most 60 characters).
    label: String,
    /// What picking it means, at most 200 characters.
    description: String,
    /// Text shown in monospace while the option is picked: a sample, a layout, a diff (at most
    /// 4000 characters).
    preview: Option<String>,
    /// The option you advise; at most one, and first.
    #[serde(default)]
    recommended: bool,
}

#[derive(Deserialize, JsonSchema)]
struct SecretArgs {
    /// What to type, a few words (at most 60 characters), such as "Mot de passe Nextcloud".
    label: String,
    /// Why you need it, one sentence (at most 300 characters).
    reason: String,
    /// A hint shown in the empty box (at most 60 characters), such as "xxxx-xxxx-xxxx".
    placeholder: Option<String>,
    /// The sign-in or setup page where the user finds or makes the secret: an https:// address,
    /// shown as a button.
    url: Option<String>,
    /// Lets the owner keep the secret for good, sealed, sent only to this origin, under this
    /// name; a component can then use it as a { stored = "name" } credential. Leave it out for
    /// a secret needed in this conversation only.
    remember: Option<RememberArg>,
    /// How long to wait for the answer, in milliseconds: 1000 to 3600000 (default 600000).
    timeout_ms: Option<u64>,
    /// Give the same value, with the same arguments, when retrying this call in the same turn,
    /// so the user sees one card, not two. A new secret needs a new value.
    client_request_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct RememberArg {
    /// The name to store it under: lower-case letters, digits and _, starting with a letter, at
    /// most 64 characters, such as "nextcloud_password".
    name: String,
    /// The only origin it may be sent to, scheme://host[:port], such as
    /// "https://cloud.example.com": over https, or http to this machine.
    origin: String,
}

/// The `user_*` tools, asking through `requests`; `user_secret` only when `requests` keeps
/// secrets.
pub fn user_tools(requests: Arc<Requests>) -> Vec<Tool> {
    let ask = typed_tool_with_context("user_ask", DESCRIPTION, false, {
        let requests = requests.clone();
        move |context: CallContext, args: AskArgs| {
            let requests = requests.clone();
            async move { user_ask(&requests, context, args).await }
        }
    });
    let mut tools = vec![ask];
    if requests.secrets().is_some() {
        tools.push(typed_tool_with_context(
            "user_secret",
            SECRET_DESCRIPTION,
            false,
            move |context: CallContext, args: SecretArgs| {
                let requests = requests.clone();
                async move { user_secret(&requests, context, args).await }
            },
        ));
    }
    tools
}

/// Waits for the request's end, or cancels it when the caller gives the call up: the card must
/// not stay open for an answer nobody reads.
async fn wait(requests: &Requests, context: &CallContext, waiter: Waiter) -> Resolution {
    let id = waiter.id();
    tokio::select! {
        resolution = waiter.wait() => resolution,
        () = context.cancelled.cancelled() => {
            if let Err(error) = requests.cancel(id).await {
                tracing::error!("cannot cancel request {id}: {error:#}");
            }
            Resolution::Cancelled
        }
    }
}

async fn user_secret(requests: &Requests, context: CallContext, args: SecretArgs) -> ToolOutput {
    if let Err(message) = check_asker(&context.caller) {
        return ToolOutput::error(message);
    }
    let (card, timeout) = match check_secret(&args) {
        Ok(checked) => checked,
        Err(message) => return ToolOutput::error(message),
    };
    if let Card::Secret {
        remember: Some(remember),
        ..
    } = &card
    {
        match requests.stored_origin(&remember.name).await {
            Ok(Some(other)) if other != remember.origin => {
                return ToolOutput::error(format!(
                    "remember.name {:?} is already stored for {other}; give another name, or {other} as remember.origin to replace it",
                    remember.name
                ));
            }
            Ok(_) => {}
            Err(error) => {
                tracing::error!("user_secret cannot read the stored secrets: {error:#}");
                return ToolOutput::error(
                    "the stored secrets cannot be read; ask again without remember",
                );
            }
        }
    }
    let ask = Ask {
        topic: context.topic.clone(),
        turn: context.turn,
        caller: context.caller.clone(),
        client_request_id: args.client_request_id,
        card,
        for_user: context.user.clone(),
        admin_only: false,
        timeout,
    };
    let waiter = match requests.ask(ask).await {
        Ok(waiter) => waiter,
        Err(error) if error.is::<Reused>() => {
            return ToolOutput::error(format!(
                "{error}; give a new client_request_id for a new secret, or retry with exactly the same arguments"
            ));
        }
        Err(error) => {
            tracing::error!("user_secret cannot open its request: {error:#}");
            return ToolOutput::error(
                "the card could not be shown to the user; say in your reply that you need this secret, and never ask for it in chat",
            );
        }
    };
    match wait(requests, &context, waiter).await {
        Resolution::Saved(Saved::Conversation {
            secret_ref,
            expires_at,
        }) => ToolOutput::ok(json!({
            "status": "saved",
            "secret_ref": secret_ref,
            "expires_at": expires_at,
        }))
        .with_next(
            "Kept for this conversation only, usable once until expires_at. No tool accepts a secret_ref yet: it expires unused. For a component's credential, ask again with remember so the owner can store it.",
        ),
        Resolution::Saved(Saved::Stored { name, origin }) => ToolOutput::ok(json!({
            "status": "saved",
            "stored": name,
            "origin": origin,
        }))
        .with_next(format!(
            "Stored for {origin} only. A component sends it there when its [[sources.credentials]] entry for {origin} names it as {{ stored = \"{name}\" }}; the owner edits the configuration."
        )),
        Resolution::Answered(_) => {
            ToolOutput::error("the answer is not a secret; ask again with user_secret")
        }
        Resolution::Declined => ToolOutput::ok(json!({"status": "declined"}))
            .with_next("The user declined to give it; do not ask again this turn."),
        Resolution::Expired => ToolOutput::ok(json!({"status": "expired"}))
            .with_next("No answer in time; go on without it if you can, or say what you need in your reply."),
        Resolution::Cancelled => ToolOutput::ok(json!({"status": "cancelled"}))
            .with_next("The request was cancelled; stop and wait for the user's next message."),
    }
}

/// The secret card the arguments describe, or what to fix in them.
fn check_secret(args: &SecretArgs) -> Result<(Card, Duration), String> {
    let timeout = check_timeout(args.timeout_ms)?;
    check_client_request_id(args.client_request_id.as_deref())?;
    let label = args.label.trim();
    if label.is_empty() {
        return Err(
            "label is empty; say what to type in a few words, such as \"Mot de passe Nextcloud\""
                .into(),
        );
    }
    check_text(
        "label",
        label,
        MAX_SECRET_LABEL,
        false,
        "and put the details in reason",
    )?;
    let reason = args.reason.trim();
    if reason.is_empty() {
        return Err("reason is empty; say in one sentence why you need the secret".into());
    }
    check_text("reason", reason, MAX_REASON, false, "one sentence at most")?;
    let placeholder = args
        .placeholder
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    if let Some(placeholder) = placeholder {
        check_text(
            "placeholder",
            placeholder,
            MAX_PLACEHOLDER,
            false,
            "a short hint",
        )?;
    }
    let url = args.url.as_deref().map(check_url).transpose()?;
    let remember = args.remember.as_ref().map(check_remember).transpose()?;
    let card = Card::Secret {
        label: label.to_owned(),
        reason: reason.to_owned(),
        placeholder: placeholder.map(str::to_owned),
        url,
        remember,
    };
    Ok((card, timeout))
}

/// A sign-in or setup page the card may link to: https, with a host, and no user name or
/// password before it, which would make the button's host read as another. Normalised.
fn check_url(url: &str) -> Result<String, String> {
    let url = url.trim();
    check_text("url", url, MAX_URL, false, "give the page's address alone")?;
    let parsed = url::Url::parse(url).map_err(|error| {
        format!("url {url:?} is not a URL ({error}); give a full https:// address, or leave it out")
    })?;
    if parsed.scheme() != "https" {
        return Err(format!(
            "url must be an https:// address, not {}:; give the sign-in or setup page over https, or leave it out",
            parsed.scheme()
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(
            "url must not carry a user name or password before its host; give the page's address alone"
                .into(),
        );
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err("url has no host; give a full https:// address, or leave it out".into());
    }
    Ok(parsed.to_string())
}

/// Where the card may offer to keep the secret: a valid name, and an origin that may carry it,
/// canonical.
fn check_remember(remember: &RememberArg) -> Result<Remember, String> {
    if !is_valid_name(&remember.name) {
        return Err(format!(
            "remember.name {:?} is not allowed; use lower-case letters, digits and _, starting with a letter, at most 64 characters, such as \"nextcloud_password\"",
            remember.name
        ));
    }
    let origin = Origin::parse(remember.origin.trim()).map_err(|error| {
        format!("remember.origin: {error:#}; give scheme://host[:port], such as \"https://cloud.example.com\"")
    })?;
    if !origin.carries_credentials() {
        return Err(format!(
            "remember.origin {origin} is plain http to another machine, where a secret would cross the network readable; use its https:// origin"
        ));
    }
    Ok(Remember {
        name: remember.name.clone(),
        origin: origin.to_string(),
    })
}

fn check_timeout(timeout_ms: Option<u64>) -> Result<Duration, String> {
    let timeout_ms = timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(format!(
            "timeout_ms {timeout_ms} is out of range; give {MIN_TIMEOUT_MS} to {MAX_TIMEOUT_MS}, or leave it out for {DEFAULT_TIMEOUT_MS}"
        ));
    }
    Ok(Duration::from_millis(timeout_ms))
}

fn check_client_request_id(id: Option<&str>) -> Result<(), String> {
    if let Some(id) = id
        && (id.is_empty() || id.chars().count() > MAX_CLIENT_REQUEST_ID)
    {
        return Err(format!(
            "client_request_id must have 1 to {MAX_CLIENT_REQUEST_ID} characters; leave it out if you do not retry"
        ));
    }
    Ok(())
}

async fn user_ask(requests: &Requests, context: CallContext, args: AskArgs) -> ToolOutput {
    if let Err(message) = check_asker(&context.caller) {
        return ToolOutput::error(message);
    }
    let (card, timeout) = match check(&args) {
        Ok(checked) => checked,
        Err(message) => return ToolOutput::error(message),
    };
    let questions: Vec<String> = args.questions.into_iter().map(|q| q.question).collect();
    let ask = Ask {
        topic: context.topic.clone(),
        turn: context.turn,
        caller: context.caller.clone(),
        client_request_id: args.client_request_id,
        card,
        for_user: context.user.clone(),
        admin_only: false,
        timeout,
    };
    let waiter = match requests.ask(ask).await {
        Ok(waiter) => waiter,
        Err(error) if error.is::<Reused>() => {
            return ToolOutput::error(format!(
                "{error}; give a new client_request_id for a new question, or retry with exactly the same questions"
            ));
        }
        Err(error) => {
            tracing::error!("user_ask cannot open its request: {error:#}");
            return ToolOutput::error(
                "the question could not be shown to the user; ask it in your reply instead",
            );
        }
    };
    match wait(requests, &context, waiter).await {
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
        Resolution::Answered(_) | Resolution::Saved(_) => {
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

/// An outside client asks under the name it chose: one the card can show without passing for
/// Fronde, or what to fix.
fn check_asker(caller: &Caller) -> Result<(), String> {
    let Caller::Mcp(name) = caller else {
        return Ok(());
    };
    let fix = format!(
        "connect with a clientInfo.name of 1 to {MAX_CLIENT_NAME} characters, without control characters, other than \"Fronde\" or \"assistant\", such as \"Codex\""
    );
    let length = name.chars().count();
    if length > MAX_CLIENT_NAME {
        return Err(format!(
            "your MCP client's name has {length} characters, too long for the card; {fix}"
        ));
    }
    if let Some(control) = control_char(name, false) {
        return Err(format!(
            "your MCP client's name contains the control character {control}; {fix}"
        ));
    }
    if RESERVED_CLIENT_NAMES
        .iter()
        .any(|reserved| name.trim().eq_ignore_ascii_case(reserved))
    {
        return Err(format!(
            "your MCP client's name {name:?} is Fronde's own, and a card must not pass for Fronde; {fix}"
        ));
    }
    Ok(())
}

/// The first control character of `text`, as `U+XXXX`; line breaks and tabs are allowed when
/// `multiline`. Bidirectional controls count too: they can make a name or a label read as
/// another.
fn control_char(text: &str, multiline: bool) -> Option<String> {
    let bidi = |c: char| matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
    text.chars()
        .find(|&c| (c.is_control() || bidi(c)) && !(multiline && matches!(c, '\n' | '\t')))
        .map(|c| format!("U+{:04X}", u32::from(c)))
}

/// What to fix in one text of the card: too long, or holding a control character.
fn check_text(
    what: &str,
    text: &str,
    max: usize,
    multiline: bool,
    shorten: &str,
) -> Result<(), String> {
    let length = text.chars().count();
    if length > max {
        return Err(format!(
            "{what} has {length} characters; use at most {max}, {shorten}"
        ));
    }
    if let Some(control) = control_char(text, multiline) {
        let allowed = if multiline {
            "only line breaks and tabs are allowed"
        } else {
            "write it on one line, as plain text"
        };
        return Err(format!(
            "{what} contains the control character {control}; remove it ({allowed})"
        ));
    }
    Ok(())
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
    let timeout = check_timeout(args.timeout_ms)?;
    check_client_request_id(args.client_request_id.as_deref())?;
    let mut fields = Vec::with_capacity(count);
    for (index, q) in args.questions.iter().enumerate() {
        let n = index + 1;
        if q.question.trim().is_empty() {
            return Err(format!(
                "question {n}: question is empty; write the full question"
            ));
        }
        check_text(
            &format!("question {n}: question"),
            q.question.trim(),
            MAX_QUESTION,
            false,
            "and put the details in the options' descriptions",
        )?;
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
        check_text(
            &format!("question {n}: header"),
            header,
            MAX_HEADER,
            false,
            "such as \"Fournisseurs\"",
        )?;
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
            let option_n = format!("question {n}, option {}", index + 1);
            check_text(
                &format!("{option_n}: label"),
                label,
                MAX_LABEL,
                false,
                "and put the details in description",
            )?;
            check_text(
                &format!("{option_n}: description"),
                option.description.trim(),
                MAX_DESCRIPTION,
                false,
                "a sentence at most",
            )?;
            if let Some(preview) = &option.preview {
                check_text(
                    &format!("{option_n}: preview"),
                    preview,
                    MAX_PREVIEW,
                    true,
                    "and show only the part that matters",
                )?;
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
    Ok((card, timeout))
}
