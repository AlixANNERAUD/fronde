//! A request's card: the agent's permission to use a tool, questions with options, or one
//! secret, answered once from any page. The page checks the answer with the server's own rules
//! before sending it; the server stays the judge. A secret is typed in a masked box, emptied once
//! sent, and never shown again.

use dioxus::html::Key;
use dioxus::prelude::*;
use fronde_client::RequestStatus;
use fronde_protocol::request::{check, show_number};
use fronde_protocol::{
    CallBy, Card, ChoiceAnswer, ChoiceOption, ClientMsg, Field, FieldValue, Invalid, Keep,
    KeyPlace, Outcome, PermissionChoice, Remember, RequestId, Response, SecretValue, TextFormat,
    ToolKind,
};
use std::collections::BTreeMap;

use crate::{
    components::icons::{ExternalLink, KeyRound},
    components::ui::{
        badge::{Badge, BadgeSize, BadgeVariant},
        button::{Button, ButtonSize, ButtonVariant},
        card::{Card as CardBox, CardContent, CardDescription, CardFooter, CardHeader, CardSize},
        checkbox::Checkbox,
        field::{
            Field as FieldRow, FieldContent, FieldDescription, FieldError, FieldLabel, FieldLegend,
            FieldSet, FieldTitle, FieldVariant,
        },
        input::{Input, InputType},
        radio_button::{RadioGroup, RadioGroupItem},
        textarea::Textarea,
    },
    socket::Outgoing,
};

/// The value of the "Autre…" radio: it starts with a control character, which the server
/// refuses in labels.
const OTHER: &str = "\u{1f}autre";
/// Full width on a phone, beside the conversation's margin from `sm:` up.
const CARD_CLASS: &str = "w-full sm:max-w-[85%]";
/// Tap targets of at least 44 px on a phone.
const BUTTON_CLASS: &str = "h-11 w-full sm:h-9 sm:w-auto";
/// How many lines of a permission's body show before the rest is folded away.
const BODY_LINES: usize = 6;
/// A permission's texts read left to right, character after character: right-to-left letters
/// cannot reorder a command's arguments, whatever the agent wrote.
const IN_ORDER: &str = "[unicode-bidi:bidi-override]";

/// What the user did with one field so far.
#[derive(Clone, Debug, Default, PartialEq)]
struct Draft {
    /// The options ticked, for a question with several answers, in the order they were ticked.
    ticked: Vec<String>,
    /// "Autre…" ticked, for a question with several answers.
    other_ticked: bool,
    other: String,
    note: String,
    /// What a text or number field's box holds.
    text: String,
    /// Whether a toggle is on.
    on: bool,
}

impl Draft {
    /// A field's draft before the user does anything: its default.
    fn of(field: &Field) -> Self {
        match field {
            Field::Text {
                default: Some(text),
                format,
                ..
            } => Self {
                text: match format {
                    Some(TextFormat::DateTime) => local_date_time(text),
                    _ => text.clone(),
                },
                ..Self::default()
            },
            Field::Number {
                default: Some(number),
                ..
            } => Self {
                text: show_number(*number),
                ..Self::default()
            },
            Field::Toggle { default, .. } => Self {
                on: *default,
                ..Self::default()
            },
            _ => Self::default(),
        }
    }
}

#[component]
pub fn RequestCard(
    id: RequestId,
    card: Card,
    from: Option<String>,
    /// `from` is one of Fronde's upstream MCP servers, not a client.
    #[props(default)]
    server: bool,
    status: RequestStatus,
    refusal: Option<String>,
    /// Who answered, on a closed card, when it was not this page's user.
    by: Option<String>,
    /// The socket is open: an answer can go out.
    open: bool,
    /// The page's user is an admin, who may keep a secret for good.
    #[props(default)]
    admin: bool,
    on_send: EventHandler<Outgoing>,
) -> Element {
    let (message, fields, submit, truncated, replaced, during) = match card {
        Card::Permission {
            title,
            tool,
            body,
            options,
            truncated,
            replaced,
        } => {
            return rsx! {
                PermissionCard {
                    id,
                    title,
                    tool,
                    body,
                    options,
                    truncated,
                    replaced,
                    status,
                    refusal,
                    by,
                    open,
                    on_send,
                }
            };
        }
        Card::Form {
            message,
            fields,
            submit,
            truncated,
            replaced,
            during,
        } => (message, fields, submit, truncated, replaced, during),
        Card::Secret {
            label,
            reason,
            placeholder,
            url,
            remember,
        } => {
            let title = secret_asker(from.as_deref(), server);
            return rsx! {
                SecretCard {
                    id,
                    title,
                    label,
                    reason,
                    placeholder,
                    url,
                    remember,
                    admin,
                    status,
                    refusal,
                    by,
                    open,
                    on_send,
                }
            };
        }
    };
    let title = asker(from.as_deref(), server, fields.len());
    match status {
        RequestStatus::Open => rsx! {
            FormCard {
                id,
                title,
                message,
                fields,
                submit,
                truncated,
                replaced,
                during,
                // Who gets what is typed here, when it is not Fronde.
                goes_to: from.as_deref().map(|name| format!("« {name} » {}", outside(server))),
                refusal,
                open,
                on_send,
            }
        },
        RequestStatus::Closed(outcome) => rsx! {
            CardBox { size: CardSize::Sm, class: CARD_CLASS,
                CardHeader {
                    CardDescription { class: "text-xs font-medium", "{title}" }
                }
                CardContent { class: "flex flex-col gap-1",
                    for field in fields.iter() {
                        p { class: "text-muted-foreground text-sm wrap-break-word", {field.label()} }
                    }
                }
                CardFooter { class: "flex-wrap gap-2",
                    {closed_badge(&outcome)}
                    if let Some(by) = by {
                        span { class: "text-muted-foreground text-xs", "par {by}" }
                    }
                }
            }
        },
    }
}

/// The agent asks to use a tool: what it would do, then one button per option it offers,
/// labelled by kind, its own words for the option under the button. Its texts are shown as
/// text, never markup; the server already capped them and replaced hidden characters.
#[component]
fn PermissionCard(
    id: RequestId,
    title: String,
    tool: Option<ToolKind>,
    body: Option<String>,
    options: Vec<PermissionChoice>,
    /// Part of the title or the body was left out.
    truncated: bool,
    /// Hidden characters were replaced.
    replaced: bool,
    status: RequestStatus,
    refusal: Option<String>,
    by: Option<String>,
    open: bool,
    on_send: EventHandler<Outgoing>,
) -> Element {
    let mut sent = use_signal(|| false);
    let delivered = use_callback(move |()| sent.set(true));
    // Sent: wait for the server to close the card, unless it refused the answer.
    let disabled = !open || sent() && refusal.is_none();
    let header = rsx! {
        CardHeader {
            CardDescription { class: "flex flex-wrap items-center gap-2 text-xs font-medium",
                "Fronde demande l'autorisation"
                if let Some(tool) = tool {
                    Badge { variant: BadgeVariant::Outline, size: BadgeSize::Sm, {tool.label()} }
                }
            }
        }
    };
    let content = rsx! {
        p {
            id: "r{id}-title",
            dir: "ltr",
            class: "text-sm font-medium wrap-break-word {IN_ORDER}",
            "{title}"
        }
        if let Some(body) = &body {
            {permission_body(id, body)}
        }
        if truncated {
            p { id: "r{id}-truncated", role: "note", class: "text-destructive text-xs font-medium",
                "Texte tronqué : le milieu est omis, vérifie le début et la fin."
            }
        }
        if replaced {
            p { id: "r{id}-replaced", role: "note", class: "text-destructive text-xs font-medium",
                "Caractères cachés remplacés par �."
            }
        }
    };
    match status {
        RequestStatus::Closed(outcome) => rsx! {
            CardBox { size: CardSize::Sm, class: CARD_CLASS,
                {header}
                CardContent { class: "flex flex-col gap-2", {content} }
                CardFooter { class: "flex-wrap gap-2",
                    {permission_badge(&outcome)}
                    if let Some(by) = by {
                        span { class: "text-muted-foreground text-xs", "par {by}" }
                    }
                }
            }
        },
        RequestStatus::Open => rsx! {
            CardBox { size: CardSize::Sm, class: CARD_CLASS,
                {header}
                CardContent { class: "flex flex-col gap-2",
                    {content}
                    if let Some(refusal) = refusal {
                        FieldError { "{refusal}" }
                    }
                }
                CardFooter { class: "flex-col items-stretch gap-2 sm:flex-row sm:flex-wrap sm:items-start sm:justify-end",
                    for (index , option) in options.into_iter().enumerate() {
                        div { key: "{index}", class: "flex flex-col gap-1",
                            Button {
                                id: "r{id}-o{index}",
                                variant: if option.kind.allows() { ButtonVariant::Default } else { ButtonVariant::Outline },
                                class: BUTTON_CLASS,
                                disabled,
                                onclick: {
                                    let choice = option.id.clone();
                                    move |_| {
                                        on_send.call(Outgoing {
                                            message: ClientMsg::Answer {
                                                id,
                                                response: Response::Permission {
                                                    choice: choice.clone(),
                                                },
                                            },
                                            sent: Some(delivered),
                                            dropped: None,
                                        })
                                    }
                                },
                                {option.kind.label()}
                            }
                            if let Some(detail) = &option.detail {
                                p { class: "text-muted-foreground max-w-xs text-xs wrap-break-word",
                                    "« {detail} »"
                                }
                            }
                        }
                    }
                }
            }
        },
    }
}

/// What the tool would do, in monospace: its first lines, the rest folded away.
fn permission_body(id: RequestId, body: &str) -> Element {
    const PRE: &str = "bg-muted max-h-96 overflow-auto rounded-md p-2 font-mono text-xs whitespace-pre-wrap wrap-break-word [unicode-bidi:bidi-override]";
    let lines: Vec<&str> = body.lines().collect();
    if lines.len() <= BODY_LINES {
        return rsx! {
            pre { id: "r{id}-body", dir: "ltr", class: PRE, "{body}" }
        };
    }
    let head = format!("{}\n…", lines[..BODY_LINES].join("\n"));
    let count = lines.len();
    // The first lines stand for the whole while folded; open, the whole replaces them.
    rsx! {
        details { id: "r{id}-body", class: "group",
            summary { class: "cursor-pointer list-none",
                pre { dir: "ltr", class: "{PRE} group-open:hidden", "{head}" }
                span { class: "text-muted-foreground inline-flex min-h-11 items-center text-xs sm:min-h-0 sm:py-1",
                    "Afficher tout ({count} lignes)"
                }
            }
            pre { dir: "ltr", class: PRE, "{body}" }
        }
    }
}

/// How a permission card ended: the option chosen, or why none was.
fn permission_badge(outcome: &Outcome) -> Element {
    match outcome {
        Outcome::Answered { summary } => rsx! {
            Badge { class: "whitespace-normal", "{summary}" }
        },
        Outcome::Declined => rsx! {
            Badge { variant: BadgeVariant::Destructive, "Refusée" }
        },
        // No answer means no: the agent was refused.
        Outcome::Expired => rsx! {
            Badge { variant: BadgeVariant::Muted, "Expirée, refusée" }
        },
        Outcome::Cancelled => rsx! {
            Badge { variant: BadgeVariant::Muted, "Annulée" }
        },
        Outcome::Saved { .. } => rsx! {
            Badge { "Répondu" }
        },
    }
}

/// Who asks, as the card's header says it. An outside client, or an upstream MCP server
/// (`server`), is quoted and marked as one: its name must never read as Fronde's own.
fn asker(from: Option<&str>, server: bool, questions: usize) -> String {
    let what = if questions > 1 {
        "Questions"
    } else {
        "Question"
    };
    match from {
        None => format!("{what} de Fronde"),
        Some(name) => format!("{what} de « {name} » {}", outside(server)),
    }
}

/// Who asks for a secret, as [`asker`] says it for questions.
fn secret_asker(from: Option<&str>, server: bool) -> String {
    match from {
        None => "Secret demandé par Fronde".into(),
        Some(name) => format!("Secret demandé par « {name} » {}", outside(server)),
    }
}

/// Whose calls an upstream server was serving when it asked, as the card says it.
fn calls_text(during: &[CallBy]) -> Option<String> {
    let names: Vec<String> = during
        .iter()
        .map(|by| match by {
            CallBy::Assistant => "Fronde".to_owned(),
            CallBy::Client { name } => format!("« {name} » (client MCP)"),
        })
        .collect();
    match names.as_slice() {
        [] => None,
        [one] => Some(format!("Pendant un appel de {one}")),
        many => Some(format!("Pendant des appels de {}", many.join(", "))),
    }
}

/// What an asker other than Fronde's assistant is.
fn outside(server: bool) -> &'static str {
    if server {
        "(serveur MCP)"
    } else {
        "(client MCP)"
    }
}

fn closed_badge(outcome: &Outcome) -> Element {
    match outcome {
        Outcome::Answered { summary } if summary.is_empty() => rsx! {
            Badge { "Répondu" }
        },
        Outcome::Answered { summary } => rsx! {
            Badge { class: "whitespace-normal", "Réponse : {summary}" }
        },
        Outcome::Saved { .. } => rsx! {
            Badge { "Enregistré" }
        },
        Outcome::Declined => rsx! {
            Badge { variant: BadgeVariant::Destructive, "Refusée" }
        },
        Outcome::Expired => rsx! {
            Badge { variant: BadgeVariant::Muted, "Expirée" }
        },
        Outcome::Cancelled => rsx! {
            Badge { variant: BadgeVariant::Muted, "Annulée" }
        },
    }
}

/// How a secret card ended. Never the secret.
fn secret_badge(outcome: &Outcome, remember: Option<&Remember>) -> Element {
    match outcome {
        Outcome::Saved {
            kept: Keep::Conversation,
            ..
        } => rsx! {
            Badge { class: "whitespace-normal", "Enregistré, jamais montré à l'agent" }
        },
        Outcome::Saved {
            kept: Keep::Stored,
            new_key,
        } => rsx! {
            if let Some(remember) = remember {
                Badge { class: "whitespace-normal", "Gardé pour {remember.origin}" }
            } else {
                Badge { "Gardé" }
            }
            // The first secret kept made the master key: the owner learns where, to back it up.
            if let Some(place) = new_key.map(key_place) {
                span { class: "text-muted-foreground text-xs", "clé maîtresse créée : {place}" }
            }
        },
        Outcome::Answered { .. } => rsx! {
            Badge { "Répondu" }
        },
        Outcome::Declined => rsx! {
            Badge { variant: BadgeVariant::Destructive, "Refusé" }
        },
        Outcome::Expired => rsx! {
            Badge { variant: BadgeVariant::Muted, "Expirée" }
        },
        Outcome::Cancelled => rsx! {
            Badge { variant: BadgeVariant::Muted, "Annulée" }
        },
    }
}

/// Where a new master key was made, in a word.
fn key_place(place: KeyPlace) -> &'static str {
    match place {
        KeyPlace::Keychain => "trousseau",
        KeyPlace::File => "fichier",
        KeyPlace::Systemd => "identifiant systemd",
    }
}

/// The page a secret card links to and the host its button names, for an https address only:
/// anything else (another scheme, a user before the host) gets no link, whatever the server let
/// through.
fn link(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let clean = !host.is_empty()
        && !host.contains('@')
        && !url.chars().any(|c| c.is_control() || c.is_whitespace());
    clean.then(|| (url.to_owned(), host.to_owned()))
}

#[component]
fn SecretCard(
    id: RequestId,
    title: String,
    label: String,
    reason: String,
    placeholder: Option<String>,
    url: Option<String>,
    remember: Option<Remember>,
    admin: bool,
    status: RequestStatus,
    refusal: Option<String>,
    by: Option<String>,
    open: bool,
    on_send: EventHandler<Outgoing>,
) -> Element {
    match status {
        RequestStatus::Open => rsx! {
            OpenSecretCard {
                id,
                title,
                label,
                reason,
                placeholder,
                url,
                remember,
                admin,
                refusal,
                open,
                on_send,
            }
        },
        RequestStatus::Closed(outcome) => rsx! {
            CardBox { size: CardSize::Sm, class: CARD_CLASS,
                CardHeader {
                    CardDescription { class: "flex items-center gap-2 text-xs font-medium",
                        KeyRound { class: "size-4" }
                        "{title}"
                    }
                }
                CardContent {
                    p { class: "text-muted-foreground text-sm wrap-break-word", "{label}" }
                }
                CardFooter { class: "flex-wrap gap-2",
                    {secret_badge(&outcome, remember.as_ref())}
                    if let Some(by) = by {
                        span { class: "text-muted-foreground text-xs", "par {by}" }
                    }
                }
            }
        },
    }
}

#[component]
fn OpenSecretCard(
    id: RequestId,
    title: String,
    label: String,
    reason: String,
    placeholder: Option<String>,
    url: Option<String>,
    remember: Option<Remember>,
    admin: bool,
    refusal: Option<String>,
    open: bool,
    on_send: EventHandler<Outgoing>,
) -> Element {
    let mut shown = use_signal(|| false);
    let mut keep = use_signal(|| false);
    let mut invalid = use_signal(|| None::<Invalid>);
    let mut sent = use_signal(|| false);
    // The answer was dropped: the socket closed before it went out.
    let mut lost = use_signal(|| false);
    let input_id = format!("r{id}-secret");
    // The box keeps what was typed until the answer went out, and is emptied then: not kept in
    // the page, not even to send again, so a refused answer is typed anew. One that was dropped
    // stays in the box, to send again.
    let delivered = use_callback({
        let input_id = input_id.clone();
        move |()| {
            sent.set(true);
            secret_box::clear(&input_id);
        }
    });
    let dropped = use_callback(move |()| lost.set(true));
    let disabled = !open || sent() && refusal.is_none();
    // Only an admin keeps a secret for good, and only where the asker offered it.
    let keepable = remember.clone().filter(|_| admin);
    let card = Card::Secret {
        label: label.clone(),
        reason: reason.clone(),
        placeholder: placeholder.clone(),
        url: url.clone(),
        remember: remember.clone(),
    };
    let send = use_callback({
        let keepable = keepable.is_some();
        let input_id = input_id.clone();
        move |()| {
            let keep = if keepable && keep() {
                Keep::Stored
            } else {
                Keep::Conversation
            };
            // Read from the box only now: no signal holds it, and nothing renders it.
            let response = Response::Secret {
                value: SecretValue::new(secret_box::read(&input_id)),
                keep,
            };
            if let Err(error) = check(&card, &response) {
                invalid.set(Some(error));
                return;
            }
            invalid.set(None);
            lost.set(false);
            on_send.call(Outgoing {
                message: ClientMsg::Answer { id, response },
                sent: Some(delivered),
                dropped: Some(dropped),
            });
        }
    });
    let decline = {
        let input_id = input_id.clone();
        move |_| {
            invalid.set(None);
            lost.set(false);
            secret_box::clear(&input_id);
            on_send.call(Outgoing {
                message: ClientMsg::Answer {
                    id,
                    response: Response::Decline,
                },
                sent: Some(delivered),
                dropped: Some(dropped),
            });
        }
    };
    let error = invalid
        .read()
        .as_ref()
        .map(|error| error.message.clone())
        .or_else(|| lost().then(|| "Connexion perdue, réessaie.".to_owned()))
        .or(refusal);
    let keep_id = format!("r{id}-keep");
    let link_class = format!(
        "inline-flex items-center justify-center gap-2 rounded-md text-sm font-medium [&_svg]:size-4 {} {} {BUTTON_CLASS}",
        ButtonVariant::Outline.as_str(),
        ButtonSize::Default.as_str()
    );
    rsx! {
        CardBox { size: CardSize::Sm, class: CARD_CLASS,
            CardHeader {
                CardDescription { class: "flex items-center gap-2 text-xs font-medium",
                    KeyRound { class: "size-4" }
                    "{title}"
                }
            }
            CardContent { class: "flex flex-col gap-4",
                p { class: "text-sm whitespace-pre-wrap wrap-break-word", "{reason}" }
                if let Some((href, host)) = url.as_deref().and_then(link) {
                    a {
                        id: "r{id}-url",
                        href,
                        target: "_blank",
                        rel: "noopener noreferrer",
                        class: "{link_class}",
                        ExternalLink {}
                        "Ouvrir {host}"
                    }
                }
                FieldRow {
                    FieldLabel { html_for: input_id.clone(), "{label}" }
                    div {
                        class: "flex items-center gap-2",
                        onkeydown: move |event: KeyboardEvent| {
                            if event.key() == Key::Enter && !event.is_composing() && !disabled {
                                event.prevent_default();
                                send.call(());
                            }
                        },
                        // Uncontrolled: the page reads it only to send it. A new password, so
                        // that neither the browser nor a password manager offers one it knows.
                        Input {
                            id: input_id.clone(),
                            r#type: if shown() { InputType::Text } else { InputType::Password },
                            placeholder,
                            autocomplete: "new-password",
                            spellcheck: "false",
                            autocapitalize: "off",
                            disabled,
                            class: "h-11 md:h-9",
                        }
                        Button {
                            id: "r{id}-show",
                            variant: ButtonVariant::Ghost,
                            class: "h-11 shrink-0 px-3 md:h-9",
                            disabled,
                            onclick: move |_| shown.toggle(),
                            if shown() {
                                "Masquer"
                            } else {
                                "Afficher"
                            }
                        }
                    }
                    if let Some(remember) = &keepable {
                        if keep() {
                            FieldDescription {
                                "Scellé sous le nom « {remember.name} », envoyé seulement à {remember.origin}."
                            }
                        } else {
                            FieldDescription { "Pour cette conversation seulement, jamais montré à l'agent." }
                        }
                    } else {
                        FieldDescription { "Pour cette conversation seulement, jamais montré à l'agent." }
                    }
                }
                if let Some(remember) = &keepable {
                    FieldLabel { html_for: keep_id.clone(), class: "min-h-11 w-full",
                        FieldRow { variant: FieldVariant::Horizontal,
                            Checkbox {
                                id: keep_id.clone(),
                                checked: keep(),
                                disabled,
                                aria_label: format!("Garder pour {}", remember.origin),
                                on_checked_change: move |on: bool| keep.set(on),
                            }
                            FieldContent {
                                FieldTitle { class: "wrap-break-word", "Garder pour {remember.origin}" }
                                if remember.replaces {
                                    FieldDescription { class: "wrap-break-word",
                                        "Remplace le secret « {remember.name} » déjà gardé pour {remember.origin}"
                                    }
                                }
                            }
                        }
                    }
                }
                if let Some(error) = error {
                    FieldError { "{error}" }
                }
            }
            CardFooter { class: "flex-col-reverse items-stretch sm:flex-row sm:items-center sm:justify-end",
                Button {
                    id: "r{id}-decline",
                    variant: ButtonVariant::Outline,
                    class: BUTTON_CLASS,
                    disabled,
                    onclick: decline,
                    "Refuser"
                }
                Button {
                    id: "r{id}-send",
                    class: BUTTON_CLASS,
                    disabled,
                    onclick: move |_| send.call(()),
                    "Enregistrer"
                }
            }
        }
    }
}

/// The answer the drafts make: fields left blank are left out, for [`check`] to name. A number
/// box that holds no number is refused here, since no answer can say it.
fn response(fields: &[Field], radios: &[String], drafts: &[Draft]) -> Result<Response, Invalid> {
    let mut values = BTreeMap::new();
    for ((field, radio), draft) in fields.iter().zip(radios).zip(drafts) {
        let value = match field {
            Field::Choice { options, multi, .. } => {
                let (selected, other) = if *multi {
                    let selected = options
                        .iter()
                        .filter(|option| draft.ticked.contains(&option.label))
                        .map(|option| option.label.clone())
                        .collect();
                    (selected, draft.other_ticked.then(|| draft.other.clone()))
                } else {
                    match radio.as_str() {
                        "" => (Vec::new(), None),
                        OTHER => (Vec::new(), Some(draft.other.clone())),
                        label => (vec![label.to_owned()], None),
                    }
                };
                let note = Some(draft.note.trim().to_owned()).filter(|note| !note.is_empty());
                if selected.is_empty() && other.is_none() && note.is_none() {
                    continue;
                }
                FieldValue::Choice(ChoiceAnswer {
                    selected,
                    other,
                    note,
                })
            }
            Field::Text { format, .. } => {
                let text = draft.text.trim();
                if text.is_empty() {
                    continue;
                }
                FieldValue::Text(match format {
                    Some(TextFormat::DateTime) => with_offset(text),
                    _ => text.to_owned(),
                })
            }
            Field::Number { key, .. } => {
                let text = draft.text.trim();
                if text.is_empty() {
                    continue;
                }
                // A French keyboard types a decimal comma; "1,000" or "1.000" may be a thousand or
                // one, so the page asks rather than guess.
                let separators: Vec<usize> = text
                    .char_indices()
                    .filter(|(_, c)| matches!(c, ',' | '.'))
                    .map(|(at, _)| at)
                    .collect();
                let ambiguous = match separators.as_slice() {
                    [] => false,
                    [at] => {
                        let decimals = &text[at + 1..];
                        decimals.len() == 3 && decimals.chars().all(|c| c.is_ascii_digit())
                    }
                    _ => true,
                };
                if ambiguous {
                    return Err(Invalid {
                        key: Some(key.clone()),
                        message: "Nombre ambigu : écris-le sans séparateur de milliers, comme 1000 ou 3,5."
                            .into(),
                    });
                }
                match text.replace(',', ".").parse::<f64>() {
                    Ok(number) if number.is_finite() => FieldValue::Number(number),
                    _ => {
                        return Err(Invalid {
                            key: Some(key.clone()),
                            message: "Saisis un nombre, comme 12 ou 3,5.".into(),
                        });
                    }
                }
            }
            Field::Toggle { .. } => FieldValue::Toggle(draft.on),
        };
        values.insert(field.key().to_owned(), value);
    }
    Ok(Response::Form { values })
}

/// A `datetime-local` box's value (`2026-11-17T14:30`) as RFC 3339, with the browser's offset
/// for that moment.
fn with_offset(local: &str) -> String {
    let seconds = if local.len() == 16 { ":00" } else { "" };
    let minutes = local_offset_minutes(local);
    let sign = if minutes < 0 { '-' } else { '+' };
    let minutes = minutes.unsigned_abs();
    format!(
        "{local}{seconds}{sign}{:02}:{:02}",
        minutes / 60,
        minutes % 60
    )
}

/// An RFC 3339 date and time as a `datetime-local` box shows it, `2026-11-17T14:30`: the same
/// instant in the page's own time. One that does not read as RFC 3339 is left blank.
fn local_date_time(text: &str) -> String {
    let Ok(instant) = chrono::DateTime::parse_from_rfc3339(text) else {
        return String::new();
    };
    let minutes = instant_offset_minutes(text);
    let local = instant.naive_utc() + chrono::Duration::minutes(i64::from(minutes));
    local.format("%Y-%m-%dT%H:%M").to_string()
}

/// The browser's offset from UTC at the instant `rfc3339`, in minutes east.
#[cfg(target_arch = "wasm32")]
fn instant_offset_minutes(rfc3339: &str) -> i32 {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(rfc3339));
    let west = date.get_timezone_offset();
    if west.is_finite() { -(west as i32) } else { 0 }
}

/// Natively there is no browser: UTC.
#[cfg(not(target_arch = "wasm32"))]
fn instant_offset_minutes(_rfc3339: &str) -> i32 {
    0
}

/// The browser's offset from UTC at the local time `local`, in minutes east.
#[cfg(target_arch = "wasm32")]
fn local_offset_minutes(local: &str) -> i32 {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(local));
    let west = date.get_timezone_offset();
    if west.is_finite() { -(west as i32) } else { 0 }
}

/// Natively there is no browser: UTC.
#[cfg(not(target_arch = "wasm32"))]
fn local_offset_minutes(_local: &str) -> i32 {
    0
}

#[component]
fn FormCard(
    id: RequestId,
    title: String,
    message: String,
    fields: Vec<Field>,
    submit: Option<String>,
    /// Part of a text was left out.
    truncated: bool,
    /// Hidden characters were replaced.
    replaced: bool,
    /// For an upstream server's form, whose calls the server was serving.
    during: Vec<CallBy>,
    /// Who gets the answer, quoted and marked, when it is not Fronde.
    goes_to: Option<String>,
    refusal: Option<String>,
    open: bool,
    on_send: EventHandler<Outgoing>,
) -> Element {
    // One radio value per question, the drafts of the rest; a card's fields never change.
    let radios: Vec<Signal<String>> =
        use_hook(|| fields.iter().map(|_| Signal::new(String::new())).collect());
    let drafts = use_signal(|| fields.iter().map(Draft::of).collect::<Vec<_>>());
    let mut invalid = use_signal(|| None::<Invalid>);
    let mut sent = use_signal(|| false);
    let delivered = use_callback(move |()| sent.set(true));
    // Sent: wait for the server to close the card, unless it refused the answer.
    let disabled = !open || sent() && refusal.is_none();
    let card = Card::Form {
        message: message.clone(),
        fields: fields.clone(),
        submit: submit.clone(),
        truncated,
        replaced,
        during: during.clone(),
    };
    let send = use_callback({
        let radios = radios.clone();
        let fields = fields.clone();
        move |()| {
            let values: Vec<String> = radios.iter().map(|radio| radio()).collect();
            let response = response(&fields, &values, &drafts.read());
            if let Err(error) = response
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|response| check(&card, response))
            {
                invalid.set(Some(error));
                return;
            }
            invalid.set(None);
            let Ok(response) = response else {
                return;
            };
            // A dropped answer leaves the card as it was, to send again.
            on_send.call(Outgoing {
                message: ClientMsg::Answer { id, response },
                sent: Some(delivered),
                dropped: None,
            });
        }
    });
    let decline = move |_| {
        invalid.set(None);
        on_send.call(Outgoing {
            message: ClientMsg::Answer {
                id,
                response: Response::Decline,
            },
            sent: Some(delivered),
            dropped: None,
        });
    };
    let error_for = |key: &str| {
        invalid
            .read()
            .as_ref()
            .filter(|error| error.key.as_deref() == Some(key))
            .map(|error| error.message.clone())
    };
    let card_error = invalid
        .read()
        .as_ref()
        .filter(|error| error.key.is_none())
        .map(|error| error.message.clone())
        .or(refusal);
    rsx! {
        CardBox { size: CardSize::Sm, class: CARD_CLASS,
            CardHeader {
                CardDescription { class: "text-xs font-medium", "{title}" }
            }
            CardContent { class: "flex flex-col gap-6",
                if !message.is_empty() {
                    p { class: "text-sm whitespace-pre-wrap wrap-break-word", "{message}" }
                }
                if let Some(calls) = calls_text(&during) {
                    p { id: "r{id}-during", class: "text-muted-foreground text-xs", "{calls}" }
                }
                if truncated {
                    p { id: "r{id}-truncated", role: "note", class: "text-destructive text-xs font-medium",
                        "Texte tronqué : le milieu est omis."
                    }
                }
                if replaced {
                    p { id: "r{id}-replaced", role: "note", class: "text-destructive text-xs font-medium",
                        "Caractères cachés remplacés par �."
                    }
                }
                for (index , field) in fields.iter().enumerate() {
                    if matches!(field, Field::Choice { .. }) {
                        ChoiceField {
                            key: "{index}",
                            request: id,
                            index,
                            field: field.clone(),
                            radio: radios[index],
                            drafts,
                            error: error_for(field.key()),
                            disabled,
                            on_submit: send,
                        }
                    } else {
                        InputField {
                            key: "{index}",
                            request: id,
                            index,
                            field: field.clone(),
                            drafts,
                            error: error_for(field.key()),
                            disabled,
                            on_submit: send,
                        }
                    }
                }
                if let Some(error) = card_error {
                    FieldError { "{error}" }
                }
                // Right above *Envoyer*: an outside asker gets the answer as it is.
                if let Some(goes_to) = goes_to {
                    p { id: "r{id}-goes-to", role: "note", class: "text-xs font-medium wrap-break-word",
                        "Ce que tu saisis est envoyé à {goes_to}. N'y mets jamais de mot de passe ni de code."
                    }
                }
            }
            CardFooter { class: "flex-col-reverse items-stretch sm:flex-row sm:items-center sm:justify-end",
                Button {
                    id: "r{id}-decline",
                    variant: ButtonVariant::Outline,
                    class: BUTTON_CLASS,
                    disabled,
                    onclick: decline,
                    "Refuser"
                }
                Button {
                    id: "r{id}-send",
                    class: BUTTON_CLASS,
                    disabled,
                    onclick: move |_| send.call(()),
                    {submit.unwrap_or_else(|| "Envoyer".into())}
                }
            }
        }
    }
}

/// One question: its options as radios (one answer) or boxes (several), "Autre…" with its
/// text, a note, and the picked option's preview: under it on a phone, beside the options from
/// `md:`.
#[component]
fn ChoiceField(
    request: RequestId,
    index: usize,
    field: Field,
    radio: Signal<String>,
    drafts: Signal<Vec<Draft>>,
    error: Option<String>,
    disabled: bool,
    /// Enter in the options or in "Autre…": the card's "Envoyer".
    on_submit: Callback<()>,
) -> Element {
    let Field::Choice {
        header,
        question,
        options,
        multi,
        other,
        note,
        ..
    } = field
    else {
        return rsx! {};
    };
    let mut drafts = drafts;
    let mut radio = radio;
    let mut noting = use_signal(|| false);
    let draft = drafts.read()[index].clone();
    let prefix = format!("r{request}-q{index}");
    let option_id = |n: usize| format!("{prefix}-o{n}");
    let other_id = format!("{prefix}-other");
    let picked = |option: &ChoiceOption| {
        if multi {
            draft.ticked.contains(&option.label)
        } else {
            radio() == option.label
        }
    };
    // The preview of the option picked last that has one.
    let preview = if multi {
        draft
            .ticked
            .iter()
            .rev()
            .find_map(|label| options.iter().find(|option| option.label == *label))
            .and_then(|option| option.preview.clone())
    } else {
        options
            .iter()
            .find(|option| option.label == radio())
            .and_then(|option| option.preview.clone())
    };
    let previews = options.iter().any(|option| option.preview.is_some());
    let other_open = if multi {
        draft.other_ticked
    } else {
        radio() == OTHER
    };
    // Arrow keys move the pick within a question with one answer, as in a native radio group;
    // Enter sends the card, as in a form. The "Autre…" text box sits outside the options, so
    // its own arrow keys move its caret.
    let values: Vec<String> = options
        .iter()
        .map(|option| option.label.clone())
        .chain(other.then(|| OTHER.to_owned()))
        .collect();
    let ids: Vec<String> = (0..options.len())
        .map(option_id)
        .chain(other.then(|| other_id.clone()))
        .collect();
    let on_key = move |event: KeyboardEvent| {
        if disabled || event.is_composing() {
            return;
        }
        if event.key() == Key::Enter {
            event.prevent_default();
            on_submit.call(());
            return;
        }
        if multi {
            return;
        }
        let step: isize = match event.key() {
            Key::ArrowDown | Key::ArrowRight => 1,
            Key::ArrowUp | Key::ArrowLeft => -1,
            _ => return,
        };
        event.prevent_default();
        let count = values.len() as isize;
        let next = match values.iter().position(|value| *value == radio()) {
            Some(at) => (at as isize + step).rem_euclid(count),
            None if step > 0 => 0,
            None => count - 1,
        } as usize;
        radio.set(values[next].clone());
        focus(&ids[next]);
    };
    let rows = rsx! {
        for (n , option) in options.iter().enumerate() {
            div { key: "{n}", class: "flex flex-col gap-2",
                FieldLabel { html_for: option_id(n), class: "min-h-11 w-full",
                    FieldRow { variant: FieldVariant::Horizontal,
                        if multi {
                            Checkbox {
                                id: option_id(n),
                                checked: picked(option),
                                disabled,
                                aria_label: option.label.clone(),
                                on_checked_change: {
                                    let label = option.label.clone();
                                    move |on: bool| {
                                        let ticked = &mut drafts.write()[index].ticked;
                                        ticked.retain(|other| *other != label);
                                        if on {
                                            ticked.push(label.clone());
                                        }
                                    }
                                },
                            }
                        } else {
                            RadioGroupItem { id: option_id(n), value: option.label.clone(), disabled }
                        }
                        FieldContent {
                            FieldTitle { class: "wrap-break-word",
                                "{option.label}"
                                if option.recommended {
                                    Badge { variant: BadgeVariant::Outline, size: BadgeSize::Sm, "Recommandé" }
                                }
                            }
                            if let Some(description) = &option.description {
                                FieldDescription { "{description}" }
                            }
                        }
                    }
                }
                if let Some(text) = option.preview.as_ref().filter(|_| picked(option)) {
                    pre { class: "bg-muted overflow-x-auto rounded-md p-3 font-mono text-xs md:hidden",
                        "{text}"
                    }
                }
            }
        }
        if other {
            FieldLabel { html_for: other_id.clone(), class: "min-h-11 w-full",
                FieldRow { variant: FieldVariant::Horizontal,
                    if multi {
                        Checkbox {
                            id: other_id.clone(),
                            checked: draft.other_ticked,
                            disabled,
                            aria_label: "Autre",
                            on_checked_change: move |on: bool| drafts.write()[index].other_ticked = on,
                        }
                    } else {
                        RadioGroupItem { id: other_id.clone(), value: OTHER, disabled }
                    }
                    FieldContent {
                        FieldTitle { "Autre…" }
                    }
                }
            }
        }
    };
    let other_text = rsx! {
        if other && other_open {
            // The box's keys bubble here, never to the options' arrow keys.
            div {
                onkeydown: move |event: KeyboardEvent| {
                    if event.key() == Key::Enter && !event.is_composing() && !disabled {
                        event.prevent_default();
                        on_submit.call(());
                    }
                },
                Input {
                    id: "{other_id}-text",
                    value: draft.other.clone(),
                    placeholder: "Ta réponse",
                    aria_label: "Autre réponse",
                    autocomplete: "off",
                    disabled,
                    class: "h-11 md:h-9",
                    oninput: move |event: FormEvent| drafts.write()[index].other = event.value(),
                }
            }
        }
    };
    let note_id = format!("{prefix}-note");
    rsx! {
        FieldSet { class: "gap-3",
            FieldLegend { class: "mb-0 flex flex-col items-start gap-2",
                if let Some(header) = header {
                    Badge { variant: BadgeVariant::Secondary, "{header}" }
                }
                span { class: "wrap-break-word", "{question}" }
            }
            div { class: if previews { "grid gap-3 md:grid-cols-2" } else { "flex flex-col gap-3" },
                div { class: "flex flex-col gap-2",
                    div { id: "{prefix}-options", class: "flex flex-col gap-2", onkeydown: on_key,
                        if multi {
                            div { role: "group", class: "flex flex-col gap-2", {rows} }
                        } else {
                            RadioGroup { value: radio, class: "gap-2", {rows} }
                        }
                    }
                    {other_text}
                }
                if previews {
                    pre { class: "bg-muted hidden overflow-x-auto rounded-md p-3 font-mono text-xs md:block",
                        {preview.unwrap_or_else(|| "Choisis une option pour voir l'aperçu.".into())}
                    }
                }
            }
            if let Some(error) = error {
                FieldError { "{error}" }
            }
            if note {
                Button {
                    id: "{note_id}-toggle",
                    variant: ButtonVariant::Ghost,
                    class: "text-muted-foreground h-11 self-start px-2 sm:h-8",
                    onclick: move |_| noting.toggle(),
                    if noting() {
                        "Retirer la précision"
                    } else {
                        "Ajouter une précision"
                    }
                }
            }
            if note && noting() {
                label { r#for: "{note_id}", class: "sr-only", "Précision" }
                Textarea {
                    id: note_id.clone(),
                    value: draft.note.clone(),
                    placeholder: "Une précision pour Fronde",
                    rows: 2,
                    disabled,
                    oninput: move |event: FormEvent| drafts.write()[index].note = event.value(),
                }
            }
        }
    }
}

/// A text, number or toggle field: its title, its description, then a box sized for a thumb
/// (the keyboard a phone shows follows what the box takes), or a box to tick on a row as tall.
/// Enter in a box sends the card.
#[component]
fn InputField(
    request: RequestId,
    index: usize,
    field: Field,
    drafts: Signal<Vec<Draft>>,
    error: Option<String>,
    disabled: bool,
    on_submit: Callback<()>,
) -> Element {
    let mut drafts = drafts;
    let draft = drafts.read()[index].clone();
    let id = format!("r{request}-q{index}");
    let on_key = move |event: KeyboardEvent| {
        if event.key() == Key::Enter && !event.is_composing() && !disabled {
            event.prevent_default();
            on_submit.call(());
        }
    };
    let (title, description, required) = match &field {
        Field::Text {
            title,
            description,
            required,
            ..
        }
        | Field::Number {
            title,
            description,
            required,
            ..
        } => (title.clone(), description.clone(), *required),
        Field::Toggle {
            title, description, ..
        } => {
            let (title, description) = (title.clone(), description.clone());
            return rsx! {
                FieldLabel { html_for: id.clone(), class: "min-h-11 w-full",
                    FieldRow { variant: FieldVariant::Horizontal,
                        Checkbox {
                            id: id.clone(),
                            checked: draft.on,
                            disabled,
                            aria_label: title.clone(),
                            on_checked_change: move |on: bool| drafts.write()[index].on = on,
                        }
                        FieldContent {
                            FieldTitle { class: "wrap-break-word", "{title}" }
                            if let Some(description) = description {
                                FieldDescription { class: "wrap-break-word", "{description}" }
                            }
                        }
                    }
                }
                if let Some(error) = error {
                    FieldError { "{error}" }
                }
            };
        }
        Field::Choice { .. } => return rsx! {},
    };
    let (kind, mode, placeholder) = match &field {
        Field::Text {
            format,
            placeholder,
            ..
        } => (
            match format {
                Some(TextFormat::Email) => InputType::Email,
                Some(TextFormat::Uri) => InputType::Url,
                Some(TextFormat::Date) => InputType::Date,
                Some(TextFormat::DateTime) => InputType::DatetimeLocal,
                None => InputType::Text,
            },
            None,
            placeholder.clone(),
        ),
        // A number box as text: a browser's own number box loses "3,5" on a French keyboard.
        Field::Number { integer, .. } => (
            InputType::Text,
            Some(if *integer { "numeric" } else { "decimal" }.to_owned()),
            None,
        ),
        _ => (InputType::Text, None, None),
    };
    let input_id = format!("{id}-input");
    rsx! {
        div { class: "flex flex-col gap-2", onkeydown: on_key,
            label { r#for: "{input_id}", class: "text-sm font-medium wrap-break-word",
                "{title}"
                if !required {
                    span { class: "text-muted-foreground font-normal", " (facultatif)" }
                }
            }
            if let Some(description) = description {
                p { class: "text-muted-foreground text-sm wrap-break-word", "{description}" }
            }
            Input {
                id: input_id.clone(),
                r#type: kind,
                inputmode: mode,
                value: draft.text.clone(),
                placeholder,
                autocomplete: "off",
                disabled,
                class: "h-11 md:h-9",
                oninput: move |event: FormEvent| drafts.write()[index].text = event.value(),
            }
            if let Some(error) = error {
                FieldError { "{error}" }
            }
        }
    }
}

/// A secret card's box, which the page reads only to send it and empties once it went out.
#[cfg(target_arch = "wasm32")]
mod secret_box {
    use wasm_bindgen::JsCast;

    fn input(id: &str) -> Option<web_sys::HtmlInputElement> {
        web_sys::window()?
            .document()?
            .get_element_by_id(id)?
            .dyn_into()
            .ok()
    }

    /// What is typed in the box `id`.
    pub fn read(id: &str) -> String {
        input(id).map(|input| input.value()).unwrap_or_default()
    }

    /// Empties the box `id`.
    pub fn clear(id: &str) {
        if let Some(input) = input(id) {
            input.set_value("");
        }
    }
}

/// Natively there is no page: tests type in boxes of their own.
#[cfg(not(target_arch = "wasm32"))]
mod secret_box {
    use std::{cell::RefCell, collections::HashMap};

    thread_local! {
        static BOXES: RefCell<HashMap<String, String>> = RefCell::default();
    }

    pub fn read(id: &str) -> String {
        BOXES.with_borrow(|boxes| boxes.get(id).cloned().unwrap_or_default())
    }

    pub fn clear(id: &str) {
        BOXES.with_borrow_mut(|boxes| boxes.remove(id));
    }

    #[cfg(test)]
    pub fn type_in(id: &str, text: &str) {
        BOXES.with_borrow_mut(|boxes| boxes.insert(id.to_owned(), text.to_owned()));
    }
}

/// Moves the keyboard focus to the element `id`.
#[cfg(target_arch = "wasm32")]
fn focus(id: &str) {
    use wasm_bindgen::JsCast;
    let element = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id(id))
        .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok());
    if let Some(element) = element {
        let _ = element.focus();
    }
}

/// Natively there is no focus to move.
#[cfg(not(target_arch = "wasm32"))]
fn focus(_id: &str) {}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::{HashMap, HashSet},
        rc::Rc,
    };

    use dioxus::{
        dioxus_core::{AttributeValue, ElementId, Mutation, Mutations},
        html::{
            Code, Location, Modifiers, PlatformEventData, SerializedFormData,
            SerializedKeyboardData, SerializedMouseData,
        },
    };

    use super::*;
    use fronde_protocol::PermissionKind;

    const ID: RequestId = 7;

    thread_local! {
        /// What the card under test sent.
        static SENT: RefCell<Vec<ClientMsg>> = const { RefCell::new(Vec::new()) };
    }

    fn sent() -> Vec<ClientMsg> {
        SENT.with_borrow(Clone::clone)
    }

    fn option(label: &str, description: &str) -> ChoiceOption {
        ChoiceOption {
            label: label.into(),
            description: Some(description.into()),
            preview: None,
            recommended: false,
        }
    }

    fn choice(key: &str, multi: bool, options: Vec<ChoiceOption>) -> Field {
        Field::Choice {
            key: key.into(),
            header: Some("Appli".into()),
            question: "Quelle appli installer ?".into(),
            options,
            multi,
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

    /// One question with one answer: Mealie (recommended, with a preview) or Seerr.
    fn single() -> Card {
        form(vec![choice(
            "q1",
            false,
            vec![
                ChoiceOption {
                    recommended: true,
                    preview: Some("[ Mealie ]".into()),
                    ..option("Mealie", "Recettes")
                },
                option("Seerr", "Demandes de films"),
            ],
        )])
    }

    fn multi() -> Card {
        form(vec![choice(
            "q1",
            true,
            vec![
                option("Jellyfin", "Films"),
                option("Immich", "Photos"),
                option("Paperless", "Papiers"),
            ],
        )])
    }

    fn answer(selected: &[&str], other: Option<&str>, note: Option<&str>) -> ClientMsg {
        ClientMsg::Answer {
            id: ID,
            response: Response::Form {
                values: [(
                    "q1".to_owned(),
                    ChoiceAnswer {
                        selected: selected.iter().map(|s| (*s).to_owned()).collect(),
                        other: other.map(str::to_owned),
                        note: note.map(str::to_owned),
                    }
                    .into(),
                )]
                .into(),
            },
        }
    }

    /// One card. `deliver`: whether the socket says each message went out, or dropped it.
    #[component]
    fn Harness(
        card: Card,
        status: RequestStatus,
        refusal: Option<String>,
        open: bool,
        admin: bool,
        deliver: bool,
    ) -> Element {
        rsx! {
            RequestCard {
                id: ID,
                card,
                from: None,
                status,
                refusal,
                open,
                admin,
                on_send: move |outgoing: Outgoing| {
                    SENT.with_borrow_mut(|sent| sent.push(outgoing.message));
                    let done = if deliver { outgoing.sent } else { outgoing.dropped };
                    if let Some(done) = done {
                        done.call(());
                    }
                },
            }
        }
    }

    fn props(card: Card) -> HarnessProps {
        HarnessProps {
            card,
            status: RequestStatus::Open,
            refusal: None,
            open: true,
            admin: false,
            deliver: true,
        }
    }

    fn render(props: HarnessProps) -> String {
        let mut dom = VirtualDom::new_with_props(Harness, props);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// The card, live: events go to elements by their `id`.
    struct Live {
        dom: VirtualDom,
        ids: HashMap<String, ElementId>,
        listeners: HashSet<(String, ElementId)>,
    }

    impl Live {
        fn new(props: HarnessProps) -> Self {
            dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
            SENT.with_borrow_mut(Vec::clear);
            let mut live = Self {
                dom: VirtualDom::new_with_props(Harness, props),
                ids: HashMap::new(),
                listeners: HashSet::new(),
            };
            let mut mutations = Mutations::default();
            live.dom.rebuild(&mut mutations);
            live.absorb(mutations);
            live
        }

        /// Learns which element has which `id` and listens to what.
        fn absorb(&mut self, mutations: Mutations) {
            for edit in mutations.edits {
                match edit {
                    Mutation::NewEventListener { name, id } => {
                        self.listeners.insert((name, id));
                    }
                    Mutation::RemoveEventListener { name, id } => {
                        self.listeners.remove(&(name, id));
                    }
                    Mutation::SetAttribute {
                        name: "id",
                        value: AttributeValue::Text(text),
                        id,
                        ..
                    } => {
                        self.ids.insert(text, id);
                    }
                    _ => {}
                }
            }
        }

        fn send(&mut self, element: &str, name: &str, data: impl std::any::Any) {
            let id = *self
                .ids
                .get(element)
                .unwrap_or_else(|| panic!("no element {element}:\n{}", self.html()));
            assert!(
                self.listeners.contains(&(name.to_owned(), id)),
                "{element} does not listen to {name}"
            );
            let data: Rc<dyn std::any::Any> = Rc::new(PlatformEventData::new(Box::new(data)));
            self.dom
                .runtime()
                .handle_event(name, Event::new(data, true), id);
            let mut mutations = Mutations::default();
            self.dom.render_immediate(&mut mutations);
            self.absorb(mutations);
        }

        fn click(&mut self, element: &str) {
            self.send(element, "click", SerializedMouseData::default());
        }

        fn type_text(&mut self, element: &str, text: &str) {
            self.send(
                element,
                "input",
                SerializedFormData::new(text.to_owned(), Vec::new()),
            );
        }

        fn key(&mut self, element: &str, key: Key, code: Code) {
            self.send(element, "keydown", keyboard(key, code));
        }

        /// A key pressed in `element`, which need not listen itself: the event bubbles up to
        /// whoever does.
        fn key_in(&mut self, element: &str, key: Key, code: Code) {
            let id = *self
                .ids
                .get(element)
                .unwrap_or_else(|| panic!("no element {element}:\n{}", self.html()));
            let data: Rc<dyn std::any::Any> =
                Rc::new(PlatformEventData::new(Box::new(keyboard(key, code))));
            self.dom
                .runtime()
                .handle_event("keydown", Event::new(data, true), id);
            let mut mutations = Mutations::default();
            self.dom.render_immediate(&mut mutations);
            self.absorb(mutations);
        }

        fn html(&self) -> String {
            dioxus_ssr::render(&self.dom)
        }
    }

    fn keyboard(key: Key, code: Code) -> SerializedKeyboardData {
        SerializedKeyboardData::new(
            key,
            code,
            Location::Standard,
            false,
            Modifiers::empty(),
            false,
        )
    }

    /// The opening tag of the element `id`.
    fn tag<'a>(html: &'a str, id: &str) -> Option<&'a str> {
        let at = html.find(&format!(r#"id="{id}""#))?;
        let start = html[..at].rfind('<')?;
        let end = at + html[at..].find('>')?;
        Some(&html[start..end])
    }

    fn checked(html: &str, id: &str) -> bool {
        tag(html, id)
            .unwrap_or_else(|| panic!("no {id}"))
            .contains(r#"aria-checked="true""#)
    }

    fn disabled(html: &str, id: &str) -> bool {
        tag(html, id)
            .unwrap_or_else(|| panic!("no {id}"))
            .contains("disabled=true")
    }

    fn class_of<'a>(html: &'a str, id: &str) -> &'a str {
        let tag = tag(html, id).unwrap_or_else(|| panic!("no {id}"));
        tag.split(r#"class=""#)
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
    }

    #[test]
    fn question_card_renders_options_and_other() {
        let page = render(props(single()));
        for text in [
            "Question de Fronde",
            "Appli",
            "Quelle appli installer ?",
            "Mealie",
            "Recettes",
            "Seerr",
            "Demandes de films",
            "Autre…",
            "Ajouter une précision",
            "Envoyer",
            "Refuser",
        ] {
            assert!(page.contains(text), "{text}: {page}");
        }
        assert!(page.contains(r#"role="radiogroup""#), "{page}");
        // Each row names its radio, so a tap anywhere on it picks the option.
        assert!(page.contains(r#"for="r7-q0-o1""#), "{page}");
        assert!(page.contains(r#"for="r7-q0-other""#), "{page}");
        assert!(!disabled(&page, "r7-send"));
        assert!(!disabled(&page, "r7-decline"));
        // The text box shows once "Autre…" is picked.
        assert!(tag(&page, "r7-q0-other-text").is_none(), "{page}");

        // Asked by an outside client, the card says so.
        let mut dom = VirtualDom::new_with_props(
            |props: HarnessProps| {
                rsx! {
                    RequestCard {
                        id: ID,
                        card: props.card,
                        from: Some("Codex".into()),
                        status: props.status,
                        refusal: props.refusal,
                        open: props.open,
                        on_send: |_| {},
                    }
                }
            },
            props(single()),
        );
        dom.rebuild_in_place();
        assert!(
            dioxus_ssr::render(&dom).contains("Question de « Codex » (client MCP)"),
            "{}",
            dioxus_ssr::render(&dom)
        );
    }

    /// The card, asked by `from`, rendered.
    fn render_from(from: &str, status: RequestStatus, by: Option<&str>) -> String {
        #[derive(Props, Clone, PartialEq)]
        struct FromProps {
            from: String,
            status: RequestStatus,
            by: Option<String>,
        }
        let mut dom = VirtualDom::new_with_props(
            |props: FromProps| {
                rsx! {
                    RequestCard {
                        id: ID,
                        card: single(),
                        from: Some(props.from),
                        status: props.status,
                        refusal: None,
                        by: props.by,
                        open: true,
                        on_send: |_| {},
                    }
                }
            },
            FromProps {
                from: from.into(),
                status,
                by: by.map(str::to_owned),
            },
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn an_outside_client_never_reads_as_fronde() {
        for name in ["Fronde", "fronde", "assistant"] {
            for status in [
                RequestStatus::Open,
                RequestStatus::Closed(Outcome::Declined),
            ] {
                let page = render_from(name, status, None);
                assert!(
                    page.contains(&format!("Question de « {name} » (client MCP)")),
                    "{page}"
                );
                assert!(!page.contains("Question de Fronde"), "{page}");
            }
        }
    }

    #[test]
    fn closed_card_names_who_answered_when_someone_else_did() {
        let answered = RequestStatus::Closed(Outcome::Answered {
            summary: "Seerr".into(),
        });
        let page = render_from("Codex", answered.clone(), Some("Sam"));
        assert!(page.contains("Réponse : Seerr"), "{page}");
        assert!(page.contains("par Sam"), "{page}");
        let page = render_from("Codex", answered, None);
        assert!(!page.contains("par "), "{page}");
    }

    #[test]
    fn single_select_sends_one_choice() {
        let mut card = Live::new(props(single()));
        card.click("r7-q0-o1");
        assert!(checked(&card.html(), "r7-q0-o1"));
        card.click("r7-q0-o0");
        let page = card.html();
        assert!(checked(&page, "r7-q0-o0"));
        assert!(!checked(&page, "r7-q0-o1"), "one answer only");
        card.click("r7-send");
        assert_eq!(sent(), [answer(&["Mealie"], None, None)]);
        // Sent: nothing more goes until the server closes the card or refuses the answer.
        let page = card.html();
        assert!(disabled(&page, "r7-send"));
        assert!(disabled(&page, "r7-decline"));
    }

    #[test]
    fn multi_select_sends_several() {
        let mut card = Live::new(props(multi()));
        assert!(card.html().contains(r#"role="group""#));
        card.click("r7-q0-o2");
        card.click("r7-q0-o0");
        card.click("r7-q0-o1");
        // Unticked again.
        card.click("r7-q0-o0");
        let page = card.html();
        assert!(!checked(&page, "r7-q0-o0"));
        assert!(checked(&page, "r7-q0-o1") && checked(&page, "r7-q0-o2"));
        card.click("r7-q0-note-toggle");
        card.type_text("r7-q0-note", " le soir ");
        card.click("r7-send");
        // In the card's order, whatever the order they were ticked in.
        assert_eq!(
            sent(),
            [answer(&["Immich", "Paperless"], None, Some("le soir"))]
        );
    }

    #[test]
    fn other_reveals_text() {
        let mut card = Live::new(props(single()));
        card.click("r7-q0-other");
        assert!(checked(&card.html(), "r7-q0-other"));
        // Picked but empty: the card says what to fix and sends nothing.
        card.click("r7-send");
        assert_eq!(sent(), []);
        assert!(
            card.html()
                .contains("Écris ta réponse, ou choisis une option."),
            "{}",
            card.html()
        );
        card.type_text("r7-q0-other-text", "Tandoor");
        card.click("r7-send");
        assert_eq!(sent(), [answer(&[], Some("Tandoor"), None)]);
        assert!(!card.html().contains("Écris ta réponse"));

        // Several answers: "Autre…" is a box of its own, beside the options.
        let mut card = Live::new(props(multi()));
        card.click("r7-q0-o0");
        card.click("r7-q0-other");
        card.type_text("r7-q0-other-text", "Nextcloud");
        card.click("r7-send");
        assert_eq!(sent(), [answer(&["Jellyfin"], Some("Nextcloud"), None)]);
    }

    #[test]
    fn nothing_picked_is_not_sent() {
        let mut card = Live::new(props(single()));
        card.click("r7-send");
        assert_eq!(sent(), []);
        assert!(card.html().contains("Réponds à cette question."));
        assert!(!disabled(&card.html(), "r7-send"));
    }

    #[test]
    fn decline_sends_decline() {
        let mut card = Live::new(props(single()));
        card.click("r7-decline");
        assert_eq!(
            sent(),
            [ClientMsg::Answer {
                id: ID,
                response: Response::Decline
            }]
        );
    }

    #[test]
    fn recommended_badge_first_not_selected() {
        let page = render(props(single()));
        let badge = page.find("Recommandé").expect("the badge");
        // On the first option's row, before the second.
        assert!(page.find("Mealie").unwrap() < badge, "{page}");
        assert!(badge < page.find("Seerr").unwrap(), "{page}");
        // Never picked for the user.
        assert!(!page.contains(r#"aria-checked="true""#), "{page}");
    }

    #[test]
    fn preview_beside_on_wide_under_on_narrow() {
        let mut card = Live::new(props(single()));
        let page = card.html();
        // Beside the options from md: up, waiting for a pick.
        assert!(page.contains("md:grid-cols-2"), "{page}");
        assert!(
            page.contains("Choisis une option pour voir l&#39;aperçu."),
            "{page}"
        );
        card.click("r7-q0-o0");
        let page = card.html();
        let pres: Vec<&str> = page.split("<pre").skip(1).collect();
        assert_eq!(pres.len(), 2, "{page}");
        let under = pres.iter().find(|pre| pre.contains("md:hidden")).unwrap();
        let beside = pres.iter().find(|pre| pre.contains("md:block")).unwrap();
        assert!(under.contains("[ Mealie ]"), "{page}");
        assert!(
            beside.contains("hidden") && beside.contains("[ Mealie ]"),
            "{page}"
        );
        // Without a preview, no column for it.
        card.click("r7-q0-o1");
        let page = card.html();
        assert!(!page.contains("[ Mealie ]"), "{page}");
        let page = render(props(multi()));
        assert!(
            !page.contains("md:grid-cols-2") && !page.contains("<pre"),
            "{page}"
        );
    }

    #[test]
    fn card_is_full_width_on_phone() {
        let page = render(props(single()));
        let card = page
            .split(r#"data-name="Card""#)
            .nth(1)
            .and_then(|rest| rest.split(r#"class=""#).nth(1))
            .and_then(|rest| rest.split('"').next())
            .unwrap();
        assert!(
            card.contains("w-full") && card.contains("sm:max-w-[85%]"),
            "{card}"
        );
        for button in ["r7-send", "r7-decline"] {
            let class = class_of(&page, button);
            // 44 px tall and full width on a phone; compact from sm: up.
            for part in ["h-11", "w-full", "sm:h-9", "sm:w-auto"] {
                assert!(class.contains(part), "{button}: {class}");
            }
        }
        // Each option row is a tap target of 44 px at least.
        let rows = page.matches(r#"data-slot="field-label""#).count();
        assert_eq!(rows, 3, "{page}");
        assert_eq!(page.matches("min-h-11").count(), 3, "{page}");
    }

    #[test]
    fn keyboard_answers_a_question() {
        let mut card = Live::new(props(single()));
        let options = "r7-q0-options";
        card.key(options, Key::ArrowDown, Code::ArrowDown);
        assert!(checked(&card.html(), "r7-q0-o0"));
        card.key(options, Key::ArrowDown, Code::ArrowDown);
        assert!(checked(&card.html(), "r7-q0-o1"));
        card.key(options, Key::ArrowRight, Code::ArrowRight);
        assert!(checked(&card.html(), "r7-q0-other"));
        // Round the end, as a native radio group does.
        card.key(options, Key::ArrowDown, Code::ArrowDown);
        assert!(checked(&card.html(), "r7-q0-o0"));
        card.key(options, Key::ArrowUp, Code::ArrowUp);
        card.key(options, Key::ArrowLeft, Code::ArrowLeft);
        assert!(checked(&card.html(), "r7-q0-o1"));
        // Other keys do nothing.
        card.key(options, Key::Character("a".into()), Code::KeyA);
        assert!(checked(&card.html(), "r7-q0-o1"));
        assert_eq!(sent(), []);
        // Enter sends the card, as in a form.
        card.key(options, Key::Enter, Code::Enter);
        assert_eq!(sent(), [answer(&["Seerr"], None, None)]);
        assert!(disabled(&card.html(), "r7-send"));

        // Several answers: Enter sends too, and arrow keys leave the boxes alone.
        let mut card = Live::new(props(multi()));
        card.click("r7-q0-o1");
        card.key(options, Key::ArrowDown, Code::ArrowDown);
        assert!(checked(&card.html(), "r7-q0-o1"));
        card.key(options, Key::Enter, Code::Enter);
        assert_eq!(sent(), [answer(&["Immich"], None, None)]);

        // Nothing picked: Enter shows what is missing and sends nothing.
        let mut card = Live::new(props(single()));
        card.key(options, Key::Enter, Code::Enter);
        assert_eq!(sent(), []);
        assert!(card.html().contains("Réponds à cette question."));
    }

    #[test]
    fn arrow_keys_in_other_move_the_caret_not_the_pick() {
        let mut card = Live::new(props(single()));
        card.click("r7-q0-other");
        card.type_text("r7-q0-other-text", "Tandoor");
        for (key, code) in [
            (Key::ArrowLeft, Code::ArrowLeft),
            (Key::ArrowRight, Code::ArrowRight),
            (Key::ArrowUp, Code::ArrowUp),
            (Key::ArrowDown, Code::ArrowDown),
        ] {
            card.key_in("r7-q0-other-text", key, code);
            let page = card.html();
            assert!(checked(&page, "r7-q0-other"), "{page}");
            assert!(tag(&page, "r7-q0-other-text").is_some(), "{page}");
        }
        assert_eq!(sent(), []);
        // Enter in the box sends the card.
        card.key_in("r7-q0-other-text", Key::Enter, Code::Enter);
        assert_eq!(sent(), [answer(&[], Some("Tandoor"), None)]);
    }

    #[test]
    fn refusal_shows_and_lets_the_user_answer_again() {
        let refused = HarnessProps {
            refusal: Some("Cette demande est déjà close.".into()),
            ..props(single())
        };
        let page = render(refused);
        assert!(page.contains("Cette demande est déjà close."), "{page}");
        assert!(page.contains(r#"role="alert""#), "{page}");
        assert!(!disabled(&page, "r7-send"));

        // Nothing goes out while the socket is closed.
        let closed = HarnessProps {
            open: false,
            ..props(single())
        };
        let page = render(closed);
        assert!(disabled(&page, "r7-send") && disabled(&page, "r7-decline"));
        assert!(disabled(&page, "r7-q0-o0"));
    }

    #[test]
    fn closed_card_shows_how_it_ended() {
        let cases = [
            (
                Outcome::Answered {
                    summary: "Mealie, Seerr".into(),
                },
                "Réponse : Mealie, Seerr",
            ),
            (Outcome::Declined, "Refusée"),
            (Outcome::Expired, "Expirée"),
            (Outcome::Cancelled, "Annulée"),
        ];
        for (outcome, text) in cases {
            let page = render(HarnessProps {
                status: RequestStatus::Closed(outcome),
                ..props(single())
            });
            assert!(page.contains(text), "{text}: {page}");
            assert!(page.contains("Quelle appli installer ?"), "{page}");
            assert!(!page.contains("<button"), "{page}");
        }
    }

    const CANARY: &str = "canary-typed-secret-0b7f";

    fn secret_card(url: Option<&str>, remember: bool) -> Card {
        Card::Secret {
            label: "Mot de passe Nextcloud".into(),
            reason: "Pour lire ton agenda.".into(),
            placeholder: Some("xxxx-xxxx".into()),
            url: url.map(str::to_owned),
            remember: remember.then(|| Remember {
                name: "nextcloud".into(),
                origin: "https://cloud.example".into(),
                replaces: false,
            }),
        }
    }

    fn secret_answer(value: &str, keep: Keep) -> ClientMsg {
        ClientMsg::Answer {
            id: ID,
            response: Response::Secret {
                value: SecretValue::new(value.into()),
                keep,
            },
        }
    }

    fn admin(card: Card) -> HarnessProps {
        HarnessProps {
            admin: true,
            ..props(card)
        }
    }

    #[test]
    fn secret_card_input_is_a_password() {
        let mut card = Live::new(props(secret_card(None, false)));
        let page = card.html();
        for text in [
            "Secret demandé par Fronde",
            "Pour lire ton agenda.",
            "Mot de passe Nextcloud",
            "Pour cette conversation seulement, jamais montré à l&#39;agent.",
            "Enregistrer",
            "Refuser",
        ] {
            assert!(page.contains(text), "{text}: {page}");
        }
        let input = tag(&page, "r7-secret").unwrap();
        for attribute in [
            r#"type="password""#,
            r#"autocomplete="new-password""#,
            r#"spellcheck="false""#,
            r#"autocapitalize="off""#,
            r#"placeholder="xxxx-xxxx""#,
        ] {
            assert!(input.contains(attribute), "{attribute}: {input}");
        }
        // The label names the box.
        assert!(page.contains(r#"for="r7-secret""#), "{page}");
        // Shown only on demand, and masked again.
        card.click("r7-show");
        assert!(
            tag(&card.html(), "r7-secret")
                .unwrap()
                .contains(r#"type="text""#)
        );
        assert!(card.html().contains("Masquer"));
        card.click("r7-show");
        assert!(
            tag(&card.html(), "r7-secret")
                .unwrap()
                .contains(r#"type="password""#)
        );
    }

    #[test]
    fn secret_card_sends_once_and_echoes_nothing() {
        let mut card = Live::new(props(secret_card(None, false)));
        // Empty: nothing goes.
        card.click("r7-send");
        assert_eq!(sent(), []);
        assert!(
            card.html()
                .contains("Saisis le secret, ou refuse la demande."),
            "{}",
            card.html()
        );
        secret_box::type_in("r7-secret", CANARY);
        // Typed, it is in the box only: nothing in the page holds or renders it.
        assert!(!card.html().contains(CANARY));
        assert!(!tag(&card.html(), "r7-secret").unwrap().contains("value="));
        card.key_in("r7-secret", Key::Enter, Code::Enter);
        assert_eq!(sent(), [secret_answer(CANARY, Keep::Conversation)]);
        assert_eq!(
            secret_box::read("r7-secret"),
            "",
            "emptied once it went out"
        );
        let page = card.html();
        assert!(!page.contains(CANARY), "the page still shows it: {page}");
        assert!(!page.contains("Saisis le secret"), "{page}");
        assert!(disabled(&page, "r7-send") && disabled(&page, "r7-decline"));
        assert!(tag(&page, "r7-secret").unwrap().contains("disabled=true"));

        // Closed, it says where the secret went, never what it was.
        let cases = [
            (
                Outcome::Saved {
                    kept: Keep::Conversation,
                    new_key: None,
                },
                "Enregistré, jamais montré à l&#39;agent",
            ),
            (
                Outcome::Saved {
                    kept: Keep::Stored,
                    new_key: None,
                },
                "Gardé pour https://cloud.example",
            ),
            (
                Outcome::Saved {
                    kept: Keep::Stored,
                    new_key: Some(KeyPlace::Keychain),
                },
                "clé maîtresse créée : trousseau",
            ),
            (Outcome::Declined, "Refusé"),
            (Outcome::Expired, "Expirée"),
            (Outcome::Cancelled, "Annulée"),
        ];
        for (outcome, text) in cases {
            let page = render(HarnessProps {
                status: RequestStatus::Closed(outcome),
                ..admin(secret_card(Some("https://cloud.example/"), true))
            });
            assert!(page.contains(text), "{text}: {page}");
            assert!(page.contains("Mot de passe Nextcloud"), "{page}");
            assert!(
                !page.contains("<input") && !page.contains("<button"),
                "{page}"
            );
        }
    }

    #[test]
    fn remember_checkbox_only_for_admin() {
        // Offered by the asker, but the page's user is no admin.
        let page = render(props(secret_card(None, true)));
        assert!(tag(&page, "r7-keep").is_none(), "{page}");
        assert!(!page.contains("Garder pour"), "{page}");
        // An admin, but nothing offered.
        let page = render(admin(secret_card(None, false)));
        assert!(tag(&page, "r7-keep").is_none(), "{page}");

        let mut card = Live::new(admin(secret_card(None, true)));
        assert!(card.html().contains("Garder pour https://cloud.example"));
        assert!(!checked(&card.html(), "r7-keep"), "never kept by default");
        card.click("r7-keep");
        assert!(
            card.html().contains(
                "Scellé sous le nom « nextcloud », envoyé seulement à https://cloud.example."
            ),
            "{}",
            card.html()
        );
        secret_box::type_in("r7-secret", CANARY);
        card.click("r7-send");
        assert_eq!(sent(), [secret_answer(CANARY, Keep::Stored)]);

        // Without ticking it, for the conversation only.
        let mut card = Live::new(admin(secret_card(None, true)));
        secret_box::type_in("r7-secret", CANARY);
        card.click("r7-send");
        assert_eq!(sent(), [secret_answer(CANARY, Keep::Conversation)]);
    }

    #[test]
    fn secret_card_says_when_it_replaces_a_kept_secret() {
        let notice = "Remplace le secret « nextcloud » déjà gardé pour https://cloud.example";
        let page = render(admin(secret_card(None, true)));
        assert!(!page.contains("Remplace"), "{page}");
        let mut card = secret_card(None, true);
        if let Card::Secret {
            remember: Some(remember),
            ..
        } = &mut card
        {
            remember.replaces = true;
        }
        let page = render(admin(card.clone()));
        assert!(page.contains(notice), "{page}");
        // Only beside the box that keeps it, which only an admin gets.
        let page = render(props(card));
        assert!(!page.contains("Remplace"), "{page}");
    }

    #[test]
    fn secret_card_survives_a_lost_connection() {
        let mut card = Live::new(HarnessProps {
            deliver: false,
            ..props(secret_card(None, false))
        });
        secret_box::type_in("r7-secret", CANARY);
        card.click("r7-send");
        assert_eq!(sent(), [secret_answer(CANARY, Keep::Conversation)]);
        let page = card.html();
        assert!(page.contains("Connexion perdue, réessaie."), "{page}");
        assert!(!page.contains(CANARY), "{page}");
        // Still answerable, with what was typed still in the box.
        assert!(!disabled(&page, "r7-send") && !disabled(&page, "r7-decline"));
        assert!(!tag(&page, "r7-secret").unwrap().contains("disabled=true"));
        assert_eq!(secret_box::read("r7-secret"), CANARY);
        card.click("r7-send");
        assert_eq!(sent().len(), 2, "sent again");
        // A decline that is dropped says so too.
        card.click("r7-decline");
        assert!(card.html().contains("Connexion perdue, réessaie."));
        assert!(!disabled(&card.html(), "r7-send"));
    }

    #[test]
    fn secret_card_url_only_https() {
        let page = render(props(secret_card(
            Some("https://cloud.example/settings/security"),
            false,
        )));
        let link = tag(&page, "r7-url").unwrap_or_else(|| panic!("{page}"));
        assert!(
            link.contains(r#"href="https://cloud.example/settings/security""#),
            "{link}"
        );
        assert!(link.contains(r#"rel="noopener noreferrer""#), "{link}");
        assert!(link.contains(r#"target="_blank""#), "{link}");
        assert!(page.contains("Ouvrir cloud.example"), "{page}");
        for url in [
            "javascript:alert(1)",
            "http://cloud.example/",
            "https://cloud.example@evil.example/",
            "https:///nohost",
            "data:text/html,x",
            " https://cloud.example/",
        ] {
            let page = render(props(secret_card(Some(url), false)));
            assert!(tag(&page, "r7-url").is_none(), "{url}: {page}");
            assert!(!page.contains("Ouvrir"), "{url}: {page}");
        }
    }

    #[test]
    fn secret_card_from_an_outside_client_says_so() {
        let mut dom = VirtualDom::new_with_props(
            |props: HarnessProps| {
                rsx! {
                    RequestCard {
                        id: ID,
                        card: props.card,
                        from: Some("Fronde".into()),
                        status: props.status,
                        refusal: props.refusal,
                        open: props.open,
                        on_send: |_| {},
                    }
                }
            },
            props(secret_card(None, false)),
        );
        dom.rebuild_in_place();
        let page = dioxus_ssr::render(&dom);
        assert!(
            page.contains("Secret demandé par « Fronde » (client MCP)"),
            "{page}"
        );
    }

    fn permission(body: Option<&str>) -> Card {
        let choice = |id: &str, kind, detail: Option<&str>| PermissionChoice {
            id: id.into(),
            kind,
            detail: detail.map(str::to_owned),
        };
        Card::Permission {
            title: "Lancer <b>git status</b>".into(),
            tool: Some(ToolKind::Execute),
            body: body.map(str::to_owned),
            options: vec![
                choice("allow-once", PermissionKind::AllowOnce, None),
                choice(
                    "allow-with-updates",
                    PermissionKind::AllowAlways,
                    Some("Yes, and don't ask again for git status commands"),
                ),
                choice("reject", PermissionKind::RejectOnce, None),
            ],
            truncated: false,
            replaced: false,
        }
    }

    fn button_texts(html: &str) -> Vec<String> {
        html.split("<button")
            .skip(1)
            .map(|rest| {
                let inner = rest.split_once('>').unwrap().1;
                inner.split("</button>").next().unwrap().trim().to_owned()
            })
            .collect()
    }

    #[test]
    fn permission_card_shows_the_tool_call_and_the_agents_options() {
        let page = render(props(permission(Some("$ git status"))));
        assert!(page.contains("Fronde demande l&#39;autorisation"), "{page}");
        assert!(page.contains("Commande"), "{page}");
        // The agent's text is text, never markup.
        assert!(
            page.contains("Lancer &#60;b&#62;git status&#60;/b&#62;"),
            "{page}"
        );
        assert!(!page.contains("<b>"), "{page}");
        let body = tag(&page, "r7-body").expect("the body");
        assert!(body.starts_with("<pre"), "{body}");
        assert!(page.contains("$ git status"), "{page}");
        // One button per option, in the agent's order, named by kind; its words under it.
        assert_eq!(
            button_texts(&page),
            ["Autoriser une fois", "Toujours autoriser", "Refuser"]
        );
        assert!(
            page.contains("« Yes, and don&#39;t ask again for git status commands »"),
            "{page}"
        );
        for button in ["r7-o0", "r7-o1", "r7-o2"] {
            assert!(!disabled(&page, button), "{button}");
            let class = class_of(&page, button);
            for part in ["h-11", "w-full", "sm:h-9", "sm:w-auto"] {
                assert!(class.contains(part), "{button}: {class}");
            }
        }
        // Without a body or a kind, neither shows.
        let bare = Card::Permission {
            title: "Lire".into(),
            tool: None,
            body: None,
            options: vec![PermissionChoice {
                id: "allow-once".into(),
                kind: PermissionKind::AllowOnce,
                detail: None,
            }],
            truncated: false,
            replaced: false,
        };
        let page = render(props(bare));
        assert!(tag(&page, "r7-body").is_none(), "{page}");
        assert!(!page.contains("Commande"), "{page}");
        // Nothing was cut or replaced: no warning.
        assert!(tag(&page, "r7-truncated").is_none(), "{page}");
        assert!(tag(&page, "r7-replaced").is_none(), "{page}");
    }

    #[test]
    fn permission_texts_read_in_order_and_say_what_was_changed() {
        // Right-to-left letters cannot reorder the arguments of a command: the title and the
        // body are shown left to right, character after character.
        let mut card = permission(Some("$ mv א ב"));
        let Card::Permission {
            title,
            truncated,
            replaced,
            ..
        } = &mut card
        else {
            unreachable!()
        };
        *title = "mv א ב".into();
        *truncated = true;
        *replaced = true;
        let page = render(props(card.clone()));
        for id in ["r7-title", "r7-body"] {
            let tag = tag(&page, id).unwrap_or_else(|| panic!("no {id}"));
            assert!(tag.contains(r#"dir="ltr""#), "{tag}");
            assert!(
                class_of(&page, id).contains("[unicode-bidi:bidi-override]"),
                "{tag}"
            );
        }
        assert!(page.contains("mv א ב"), "{page}");
        let truncated = tag(&page, "r7-truncated").expect("the truncation warning");
        assert!(truncated.contains(r#"role="note""#), "{truncated}");
        assert!(page.contains("Texte tronqué"), "{page}");
        assert!(tag(&page, "r7-replaced").is_some(), "{page}");
        assert!(page.contains("Caractères cachés remplacés"), "{page}");
        // A folded body reads in order too, folded or not.
        let lines: Vec<String> = (1..=10).map(|n| format!("mv א{n} ב")).collect();
        let page = render(props(permission(Some(&lines.join("\n")))));
        let pres: Vec<&str> = page.split("<pre").skip(1).collect();
        assert_eq!(pres.len(), 2, "{page}");
        for pre in pres {
            assert!(pre.contains(r#"dir="ltr""#), "{pre}");
            assert!(pre.contains("[unicode-bidi:bidi-override]"), "{pre}");
        }
        // The warnings stay on the closed card.
        let page = render(HarnessProps {
            status: RequestStatus::Closed(Outcome::Cancelled),
            ..props(card)
        });
        assert!(page.contains("Texte tronqué"), "{page}");
        assert!(page.contains("Caractères cachés remplacés"), "{page}");
    }

    #[test]
    fn permission_buttons_send_the_chosen_option() {
        for (button, choice) in [
            ("r7-o0", "allow-once"),
            ("r7-o1", "allow-with-updates"),
            ("r7-o2", "reject"),
        ] {
            let mut card = Live::new(props(permission(None)));
            card.click(button);
            assert_eq!(
                sent(),
                [ClientMsg::Answer {
                    id: ID,
                    response: Response::Permission {
                        choice: choice.into()
                    }
                }],
                "{button}"
            );
            // Sent once: the card waits for the server to close it.
            let page = card.html();
            for other in ["r7-o0", "r7-o1", "r7-o2"] {
                assert!(disabled(&page, other), "{button} then {other}");
            }
        }
    }

    #[test]
    fn permission_card_follows_the_socket_and_closes() {
        // Nothing goes out while the socket is closed.
        let page = render(HarnessProps {
            open: false,
            ..props(permission(None))
        });
        for button in ["r7-o0", "r7-o1", "r7-o2"] {
            assert!(disabled(&page, button), "{button}");
        }
        // A refused answer says why, and the user may answer again.
        let page = render(HarnessProps {
            refusal: Some("Cette demande ne t'est pas adressée.".into()),
            ..props(permission(None))
        });
        assert!(
            page.contains("Cette demande ne t&#39;est pas adressée."),
            "{page}"
        );
        assert!(!disabled(&page, "r7-o0"));

        let cases = [
            (
                Outcome::Answered {
                    summary: "Toujours autoriser".into(),
                },
                "Toujours autoriser",
            ),
            (Outcome::Declined, "Refusée"),
            (Outcome::Expired, "Expirée, refusée"),
            (Outcome::Cancelled, "Annulée"),
        ];
        for (outcome, text) in cases {
            let page = render(HarnessProps {
                status: RequestStatus::Closed(outcome),
                ..props(permission(Some("$ git status")))
            });
            assert!(page.contains(text), "{text}: {page}");
            assert!(page.contains("git status"), "{page}");
            assert!(!page.contains("<button"), "{page}");
        }
    }

    #[test]
    fn long_permission_body_is_folded_after_six_lines() {
        let lines: Vec<String> = (1..=10).map(|n| format!("ligne {n}")).collect();
        let page = render(props(permission(Some(&lines.join("\n")))));
        let body = tag(&page, "r7-body").expect("the body");
        assert!(body.starts_with("<details"), "{body}");
        assert!(page.contains("Afficher tout (10 lignes)"), "{page}");
        // Folded, the first six lines stand for the rest.
        let head = page
            .split("<pre")
            .find(|pre| pre.contains("group-open:hidden"))
            .expect("the folded lines");
        let head = head.split("</pre>").next().unwrap();
        assert!(head.contains("ligne 6\n…"), "{head}");
        assert!(!head.contains("ligne 7"), "{head}");
        // The whole body is there to unfold.
        assert!(page.contains("ligne 10"), "{page}");
    }

    /// An e-mail (required), a whole number from 1 to 10, a decimal one (optional), a date and
    /// time (optional) and a toggle that starts on.
    fn inputs() -> Card {
        let text = |key: &str, title: &str, format, required| Field::Text {
            key: key.into(),
            title: title.into(),
            description: None,
            placeholder: None,
            format,
            min_length: None,
            max_length: None,
            default: None,
            required,
        };
        form(vec![
            text("mail", "Adresse e-mail", Some(TextFormat::Email), true),
            Field::Number {
                key: "count".into(),
                title: "Combien de places ?".into(),
                description: Some("De 1 à 10".into()),
                integer: true,
                min: Some(1.0),
                max: Some(10.0),
                default: None,
                required: true,
            },
            Field::Number {
                key: "price".into(),
                title: "Prix maximum".into(),
                description: None,
                integer: false,
                min: None,
                max: None,
                default: Some(12.5),
                required: false,
            },
            text("at", "Quand ?", Some(TextFormat::DateTime), false),
            Field::Toggle {
                key: "remind".into(),
                title: "Me le rappeler".into(),
                description: Some("La veille au soir".into()),
                default: true,
            },
        ])
    }

    #[test]
    fn text_number_and_toggle_fields_render_for_a_phone() {
        let page = render(props(inputs()));
        assert!(page.contains("Questions de Fronde"), "{page}");
        let mail = tag(&page, "r7-q0-input").unwrap();
        assert!(mail.contains(r#"type="email""#), "{mail}");
        assert!(page.contains(r#"for="r7-q0-input""#), "{page}");
        // A number is typed as text, with the phone's number keyboard.
        let count = tag(&page, "r7-q1-input").unwrap();
        assert!(count.contains(r#"type="text""#) && count.contains(r#"inputmode="numeric""#));
        let price = tag(&page, "r7-q2-input").unwrap();
        assert!(price.contains(r#"inputmode="decimal""#) && price.contains(r#"value="12.5""#));
        assert!(
            tag(&page, "r7-q3-input")
                .unwrap()
                .contains(r#"type="datetime-local""#)
        );
        assert!(page.contains("De 1 à 10") && page.contains("La veille au soir"));
        assert!(page.contains("Prix maximum<span") && page.contains(" (facultatif)"));
        assert!(checked(&page, "r7-q4"));
        assert!(page.contains(r#"for="r7-q4""#), "{page}");
        // 44 px tall on a phone.
        for box_id in ["r7-q0-input", "r7-q1-input", "r7-q2-input"] {
            assert!(class_of(&page, box_id).contains("h-11"), "{box_id}");
        }
        // No note where nothing can carry it, nor on these fields.
        assert!(!page.contains("Ajouter une précision"), "{page}");
    }

    #[test]
    fn text_number_and_toggle_fields_send_typed_answers() {
        let mut live = Live::new(props(inputs()));
        live.click("r7-send");
        assert!(live.html().contains("Remplis ce champ."), "{}", live.html());
        assert_eq!(sent(), []);
        live.type_text("r7-q0-input", "alix.example.com");
        live.type_text("r7-q1-input", "deux");
        live.click("r7-send");
        assert!(live.html().contains("Saisis un nombre, comme 12 ou 3,5."));
        live.type_text("r7-q1-input", "12");
        live.click("r7-send");
        assert!(
            live.html().contains("Adresse e-mail invalide."),
            "{}",
            live.html()
        );
        live.type_text("r7-q0-input", " alix@example.com ");
        live.click("r7-send");
        assert!(live.html().contains("Le nombre doit être au plus 10."));
        assert_eq!(sent(), []);
        live.type_text("r7-q1-input", "3");
        live.type_text("r7-q2-input", "9,90");
        live.type_text("r7-q3-input", "2026-11-17T14:30");
        live.click("r7-q4");
        // Enter in a box sends the card, as in a form.
        live.key_in("r7-q2-input", Key::Enter, Code::Enter);
        let expected = Response::Form {
            values: [
                ("mail", FieldValue::Text("alix@example.com".into())),
                ("count", FieldValue::Number(3.0)),
                ("price", FieldValue::Number(9.9)),
                // Natively the page's offset is UTC's.
                ("at", FieldValue::Text("2026-11-17T14:30:00+00:00".into())),
                ("remind", FieldValue::Toggle(false)),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
        };
        assert_eq!(
            sent(),
            [ClientMsg::Answer {
                id: ID,
                response: expected
            }]
        );
    }

    #[test]
    fn optional_fields_left_blank_are_left_out() {
        let mut live = Live::new(props(inputs()));
        live.type_text("r7-q0-input", "alix@example.com");
        live.type_text("r7-q1-input", "1");
        live.type_text("r7-q2-input", "  ");
        live.click("r7-send");
        let sent = sent();
        let [
            ClientMsg::Answer {
                response: Response::Form { values },
                ..
            },
        ] = sent.as_slice()
        else {
            panic!("{sent:?}");
        };
        assert_eq!(
            values.keys().map(String::as_str).collect::<Vec<_>>(),
            ["count", "mail", "remind"]
        );
        assert_eq!(values["remind"], FieldValue::Toggle(true));
    }

    #[test]
    fn date_and_time_carry_an_offset() {
        assert_eq!(with_offset("2026-11-17T14:30"), "2026-11-17T14:30:00+00:00");
        assert_eq!(
            with_offset("2026-11-17T14:30:05"),
            "2026-11-17T14:30:05+00:00"
        );
        // The same instant in the page's time (UTC natively), not the asker's wall clock.
        assert_eq!(
            local_date_time("2026-11-17T14:30:00+01:00"),
            "2026-11-17T13:30"
        );
        assert_eq!(
            local_date_time("2026-11-17T23:30:00-02:00"),
            "2026-11-18T01:30"
        );
        assert_eq!(local_date_time("demain"), "");
    }

    #[test]
    fn a_cleaned_form_says_so_and_a_choice_without_note_offers_none() {
        let mut card = single();
        if let Card::Form {
            fields,
            truncated,
            replaced,
            ..
        } = &mut card
        {
            *truncated = true;
            *replaced = true;
            if let Some(Field::Choice { note, .. }) = fields.first_mut() {
                *note = false;
            }
        }
        let page = render(props(card));
        assert!(
            page.contains("Texte tronqué : le milieu est omis."),
            "{page}"
        );
        assert!(
            page.contains("Caractères cachés remplacés par �."),
            "{page}"
        );
        assert!(!page.contains("Ajouter une précision"), "{page}");
        // The closed card names each field, whatever its kind.
        let mut closed = props(inputs());
        closed.status = RequestStatus::Closed(Outcome::Answered {
            summary: "alix@example.com ; 3".into(),
        });
        let page = render(closed);
        for text in ["Adresse e-mail", "Combien de places ?", "Me le rappeler"] {
            assert!(page.contains(text), "{text}: {page}");
        }
    }

    #[test]
    fn an_upstream_server_is_marked_as_one() {
        #[derive(Props, Clone, PartialEq)]
        struct ServerProps {
            card: Card,
        }
        let render = |card: Card| {
            let mut dom = VirtualDom::new_with_props(
                |props: ServerProps| {
                    rsx! {
                        RequestCard {
                            id: ID,
                            card: props.card,
                            from: Some("github".into()),
                            server: true,
                            status: RequestStatus::Open,
                            refusal: None,
                            by: None,
                            open: true,
                            admin: true,
                            on_send: |_| {},
                        }
                    }
                },
                ServerProps { card },
            );
            dom.rebuild_in_place();
            dioxus_ssr::render(&dom)
        };
        let page = render(inputs());
        assert!(
            page.contains("Questions de « github » (serveur MCP)"),
            "{page}"
        );
        assert!(!page.contains("client MCP"), "{page}");
        assert!(render(single()).contains("Question de « github » (serveur MCP)"));
        // Right above the buttons, where the answer goes.
        assert!(
            page.contains(
                "Ce que tu saisis est envoyé à « github » (serveur MCP). N&#39;y mets jamais de mot de passe ni de code."
            ),
            "{page}"
        );
        assert!(
            page.find("r7-goes-to").unwrap() < page.find("r7-send").unwrap(),
            "{page}"
        );
        // Whose call the server was serving.
        let mut card = inputs();
        if let Card::Form { during, .. } = &mut card {
            *during = vec![
                CallBy::Assistant,
                CallBy::Client {
                    name: "Codex".into(),
                },
            ];
        }
        let page = render(card);
        assert!(
            page.contains("Pendant des appels de Fronde, « Codex » (client MCP)"),
            "{page}"
        );
        assert_eq!(
            calls_text(&[CallBy::Assistant]).as_deref(),
            Some("Pendant un appel de Fronde")
        );
        assert_eq!(calls_text(&[]), None);
    }

    #[test]
    fn only_an_outside_askers_card_says_where_the_answer_goes() {
        let page = render_from("Codex", RequestStatus::Open, None);
        assert!(
            page.contains("Ce que tu saisis est envoyé à « Codex » (client MCP)."),
            "{page}"
        );
        // Fronde's own question goes to Fronde: no such line.
        let page = render(props(single()));
        assert!(tag(&page, "r7-goes-to").is_none(), "{page}");
        assert!(!page.contains("Ce que tu saisis"), "{page}");
    }

    #[test]
    fn an_ambiguous_number_is_asked_again() {
        for typed in ["1,000", "1.000", "1,000,000", "1.000,5", "2,5.0"] {
            let mut live = Live::new(props(inputs()));
            live.type_text("r7-q0-input", "alix@example.com");
            live.type_text("r7-q1-input", "3");
            live.type_text("r7-q2-input", typed);
            live.click("r7-send");
            assert!(
                live.html().contains(
                    "Nombre ambigu : écris-le sans séparateur de milliers, comme 1000 ou 3,5."
                ),
                "{typed}: {}",
                live.html()
            );
            assert_eq!(sent(), [], "{typed}");
        }
        for (typed, number) in [("1000", 1000.0), ("3,5", 3.5), ("0.25", 0.25), ("12", 12.0)] {
            let mut live = Live::new(props(inputs()));
            live.type_text("r7-q0-input", "alix@example.com");
            live.type_text("r7-q1-input", "3");
            live.type_text("r7-q2-input", typed);
            live.click("r7-send");
            let sent = sent();
            let [
                ClientMsg::Answer {
                    response: Response::Form { values },
                    ..
                },
            ] = sent.as_slice()
            else {
                panic!("{typed}: {sent:?}");
            };
            assert_eq!(values["price"], FieldValue::Number(number), "{typed}");
        }
    }
}
