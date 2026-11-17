//! The agent's permission requests as cards: what the card shows of the tool call, and how the
//! request's end answers the agent.
//!
//! Everything a card shows here comes from the agent, which may have read it from anywhere (a
//! web page, a file): each text is capped, and control and bidirectional characters, which could
//! hide part of a command or make it read backwards, are replaced by a visible U+FFFD.

use agent_client_protocol::v1::schema::{
    ContentBlock, PermissionOption, PermissionOptionKind, RequestPermissionOutcome,
    SelectedPermissionOutcome, ToolCallContent, ToolCallUpdateFields, ToolKind as AcpToolKind,
};
use fronde_protocol::request::{MAX_PERMISSION_BODY, MAX_PERMISSION_TITLE};
use fronde_protocol::{Card, PermissionChoice, PermissionKind, Response, ToolKind};
use fronde_requests::Resolution;
use std::time::Duration;

/// How long a permission card waits for an answer before the agent is refused.
pub const PERMISSION_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The longest option detail (the agent's own words for an option), in characters.
const MAX_DETAIL: usize = 160;

/// The longest option id kept: the agent's ids are short words such as `allow-once`.
const MAX_OPTION_ID: usize = 128;

/// What the agent calls an option when it says nothing more than the option's kind.
const PLAIN_NAMES: &[&str] = &[
    "yes",
    "no",
    "allow",
    "allow once",
    "always allow",
    "allow always",
    "reject",
    "reject once",
    "deny",
    "always reject",
    "reject always",
];

/// The card for a permission request on the tool call `tool`, with the agent's `options`.
/// Options that last (`*_always`) are left out unless the user is an admin. `None` when no
/// option is left to offer.
pub fn card(
    tool: &ToolCallUpdateFields,
    options: &[PermissionOption],
    admin: bool,
) -> Option<Card> {
    let mut choices: Vec<PermissionChoice> = Vec::new();
    for option in options {
        let Some(kind) = kind(option.kind) else {
            continue;
        };
        let id = option.option_id.0.to_string();
        if kind.lasts() && !admin
            || id.is_empty()
            || id.chars().count() > MAX_OPTION_ID
            || choices.iter().any(|choice| choice.id == id)
        {
            continue;
        }
        let detail = line(&option.name, MAX_DETAIL);
        let plain = PLAIN_NAMES.contains(&detail.to_lowercase().as_str());
        choices.push(PermissionChoice {
            id,
            kind,
            detail: (!detail.is_empty() && !plain).then_some(detail),
        });
    }
    if choices.is_empty() {
        return None;
    }
    let title = tool
        .title
        .as_deref()
        .map(|title| line(title, MAX_PERMISSION_TITLE))
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| "Utiliser un outil".into());
    let body = body(tool);
    Some(Card::Permission {
        title,
        tool: tool.kind.map(tool_kind),
        body: (!body.is_empty()).then_some(body),
        options: choices,
    })
}

/// What the agent gets once its request ended: the option chosen, a refusal when the card was
/// declined or expired (no answer means no), or `Cancelled` when the turn was stopped or ended.
pub fn outcome(resolution: &Resolution, options: &[PermissionOption]) -> RequestPermissionOutcome {
    match resolution {
        Resolution::Answered(Response::Permission { choice })
            if options.iter().any(|option| *option.option_id.0 == **choice) =>
        {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(choice.clone()))
        }
        Resolution::Answered(_) | Resolution::Declined | Resolution::Expired => denial(options),
        Resolution::Cancelled | Resolution::Saved(_) => RequestPermissionOutcome::Cancelled,
    }
}

/// A refusal that grants nothing and remembers nothing: the agent's one-time reject, else a
/// cancel.
pub fn denial(options: &[PermissionOption]) -> RequestPermissionOutcome {
    options
        .iter()
        .find(|option| option.kind == PermissionOptionKind::RejectOnce)
        .map_or(RequestPermissionOutcome::Cancelled, |option| {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                option.option_id.clone(),
            ))
        })
}

fn kind(kind: PermissionOptionKind) -> Option<PermissionKind> {
    Some(match kind {
        PermissionOptionKind::AllowOnce => PermissionKind::AllowOnce,
        PermissionOptionKind::AllowAlways => PermissionKind::AllowAlways,
        PermissionOptionKind::RejectOnce => PermissionKind::RejectOnce,
        PermissionOptionKind::RejectAlways => PermissionKind::RejectAlways,
        // A kind this version does not know: what it grants cannot be told on the card.
        _ => return None,
    })
}

fn tool_kind(kind: AcpToolKind) -> ToolKind {
    match kind {
        AcpToolKind::Read => ToolKind::Read,
        AcpToolKind::Edit => ToolKind::Edit,
        AcpToolKind::Delete => ToolKind::Delete,
        AcpToolKind::Move => ToolKind::Move,
        AcpToolKind::Search => ToolKind::Search,
        AcpToolKind::Execute => ToolKind::Execute,
        AcpToolKind::Think => ToolKind::Think,
        AcpToolKind::Fetch => ToolKind::Fetch,
        AcpToolKind::SwitchMode => ToolKind::SwitchMode,
        _ => ToolKind::Other,
    }
}

/// What the tool call would do, line by line: the command, the files and addresses it names,
/// its text content (a plan) and a summary of each change.
fn body(tool: &ToolCallUpdateFields) -> String {
    let mut lines: Vec<String> = Vec::new();
    fn add(lines: &mut Vec<String>, line: String) {
        if !line.trim().is_empty() && !lines.contains(&line) {
            lines.push(line);
        }
    }
    if let Some(input) = tool.raw_input.as_ref().and_then(|input| input.as_object()) {
        let text = |key: &str| input.get(key).and_then(|value| value.as_str());
        if let Some(command) = text("command") {
            add(&mut lines, format!("$ {command}"));
        }
        for key in ["file_path", "notebook_path", "path", "url"] {
            if let Some(value) = text(key) {
                add(&mut lines, value.to_owned());
            }
        }
    }
    for content in tool.content.iter().flatten() {
        match content {
            ToolCallContent::Content(content) => {
                if let ContentBlock::Text(text) = &content.content {
                    add(&mut lines, text.text.clone());
                }
            }
            ToolCallContent::Diff(diff) => {
                let path = diff.path.display();
                let after = diff.new_text.lines().count();
                add(
                    &mut lines,
                    match &diff.old_text {
                        None => format!("{path} : nouveau fichier, {after} ligne(s)"),
                        Some(old) => {
                            let (removed, added) = changed_lines(old, &diff.new_text);
                            format!("{path} : -{removed} +{added} ligne(s)")
                        }
                    },
                );
            }
            _ => {}
        }
    }
    for location in tool.locations.iter().flatten() {
        let path = location.path.display().to_string();
        if !lines.iter().any(|line| line.contains(&path)) {
            add(
                &mut lines,
                match location.line {
                    Some(at) => format!("{path}:{at}"),
                    None => path,
                },
            );
        }
    }
    text(&lines.join("\n"), MAX_PERMISSION_BODY)
}

/// The lines only in `old` and only in `new`, as a count of each: a summary, not a diff.
fn changed_lines(old: &str, new: &str) -> (usize, usize) {
    let mut old: Vec<&str> = old.lines().collect();
    let mut added = 0;
    for line in new.lines() {
        match old.iter().position(|kept| *kept == line) {
            Some(at) => {
                old.swap_remove(at);
            }
            None => added += 1,
        }
    }
    (old.len(), added)
}

/// A control or bidirectional character, which the card never shows as it is.
fn hidden(c: char) -> bool {
    let bidi = matches!(
        c,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    );
    c.is_control() || bidi
}

/// `text` on one line: line breaks and tabs become spaces, other hidden characters U+FFFD;
/// capped to `max` characters.
fn line(text: &str, max: usize) -> String {
    let flat: String = text
        .trim()
        .chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            c if hidden(c) => '\u{fffd}',
            c => c,
        })
        .collect();
    cap(&flat, max)
}

/// `text` with its line breaks and tabs, other hidden characters U+FFFD; capped to `max`
/// characters.
fn text(text: &str, max: usize) -> String {
    let clean: String = text
        .trim()
        .replace("\r\n", "\n")
        .chars()
        .map(|c| match c {
            '\n' | '\t' => c,
            c if hidden(c) => '\u{fffd}',
            c => c,
        })
        .collect();
    cap(&clean, max)
}

/// The first `max` characters of `text`, the last one an ellipsis when some were cut.
fn cap(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut capped: String = text.chars().take(max.saturating_sub(1)).collect();
    capped.push('…');
    capped
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::v1::schema::{Diff, ToolCallLocation};
    use serde_json::json;

    fn option(id: &str, name: &str, kind: PermissionOptionKind) -> PermissionOption {
        PermissionOption::new(id.to_owned(), name.to_owned(), kind)
    }

    fn claude_options() -> Vec<PermissionOption> {
        vec![
            option("allow-once", "Yes", PermissionOptionKind::AllowOnce),
            option(
                "allow-with-updates",
                "Yes, and don't ask again for git status commands",
                PermissionOptionKind::AllowAlways,
            ),
            option("reject", "No", PermissionOptionKind::RejectOnce),
        ]
    }

    fn choices(card: &Card) -> Vec<(String, PermissionKind, Option<String>)> {
        let Card::Permission { options, .. } = card else {
            panic!("not a permission card: {card:?}");
        };
        options
            .iter()
            .map(|o| (o.id.clone(), o.kind, o.detail.clone()))
            .collect()
    }

    #[test]
    fn options_follow_the_agent_and_lasting_ones_need_an_admin() {
        let tool = ToolCallUpdateFields::new().title("git status".to_owned());
        let card = card(&tool, &claude_options(), true).unwrap();
        assert_eq!(
            choices(&card),
            [
                ("allow-once".into(), PermissionKind::AllowOnce, None),
                (
                    "allow-with-updates".into(),
                    PermissionKind::AllowAlways,
                    Some("Yes, and don't ask again for git status commands".into())
                ),
                ("reject".into(), PermissionKind::RejectOnce, None),
            ]
        );
        let family = super::card(&tool, &claude_options(), false).unwrap();
        assert_eq!(
            choices(&family),
            [
                ("allow-once".into(), PermissionKind::AllowOnce, None),
                ("reject".into(), PermissionKind::RejectOnce, None),
            ]
        );
        // Nothing left to offer: no card.
        let always = [option("a", "Always", PermissionOptionKind::AllowAlways)];
        assert_eq!(super::card(&tool, &always, false), None);
    }

    #[test]
    fn preview_shows_the_command_paths_and_changes() {
        let tool = ToolCallUpdateFields::new()
            .title("Edit src/main.rs".to_owned())
            .kind(AcpToolKind::Edit)
            .raw_input(json!({"command": "cargo fmt", "file_path": "/w/src/main.rs"}))
            .content(vec![
                ToolCallContent::Diff(
                    Diff::new("/w/src/main.rs", "fn main() {}\n// done\n")
                        .old_text("fn main() {}\n// todo\n".to_owned()),
                ),
                ToolCallContent::Diff(Diff::new("/w/NEW.md", "a\nb\nc\n")),
                ToolCallContent::from("Le plan :\n1. lire\n2. écrire"),
            ])
            .locations(vec![
                ToolCallLocation::new("/w/src/main.rs"),
                ToolCallLocation::new("/w/lib.rs").line(12),
            ]);
        let Some(Card::Permission {
            title, tool, body, ..
        }) = card(&tool, &claude_options(), true)
        else {
            panic!()
        };
        assert_eq!(title, "Edit src/main.rs");
        assert_eq!(tool, Some(ToolKind::Edit));
        assert_eq!(
            body.as_deref(),
            Some(
                "$ cargo fmt\n/w/src/main.rs\n/w/src/main.rs : -1 +1 ligne(s)\n\
                 /w/NEW.md : nouveau fichier, 3 ligne(s)\nLe plan :\n1. lire\n2. écrire\n\
                 /w/lib.rs:12"
            )
        );
        // No title, nothing to preview.
        let bare = card(&ToolCallUpdateFields::new(), &claude_options(), true).unwrap();
        let Card::Permission { title, body, .. } = bare else {
            panic!()
        };
        assert_eq!(title, "Utiliser un outil");
        assert_eq!(body, None);
    }

    #[test]
    fn preview_is_capped_and_sanitised() {
        // A command that hides its end behind a right-to-left override, a newline in the title
        // and an escape sequence.
        let sneaky = "rm -rf ~/\u{202e}txt.notes\u{202c} \u{1b}[2Kok";
        let tool = ToolCallUpdateFields::new()
            .title(format!("Bash\n{sneaky}"))
            .raw_input(json!({"command": format!("{sneaky}\n\u{2066}x\u{2069}\techo")}));
        let options = [option(
            "allow-once",
            "Yes \u{200f}always\u{0007}",
            PermissionOptionKind::AllowOnce,
        )];
        let Some(Card::Permission {
            title,
            body,
            options,
            ..
        }) = card(&tool, &options, true)
        else {
            panic!()
        };
        assert_eq!(
            title,
            "Bash rm -rf ~/\u{fffd}txt.notes\u{fffd} \u{fffd}[2Kok"
        );
        assert_eq!(
            body.as_deref(),
            Some("$ rm -rf ~/\u{fffd}txt.notes\u{fffd} \u{fffd}[2Kok\n\u{fffd}x\u{fffd}\techo")
        );
        assert_eq!(
            options[0].detail.as_deref(),
            Some("Yes \u{fffd}always\u{fffd}")
        );

        let long = ToolCallUpdateFields::new()
            .title("t".repeat(5000))
            .content(vec![ToolCallContent::from("x\n".repeat(5000))]);
        let options = [option(
            "allow-once",
            &"n".repeat(500),
            PermissionOptionKind::AllowOnce,
        )];
        let Some(Card::Permission {
            title,
            body,
            options,
            ..
        }) = card(&long, &options, true)
        else {
            panic!()
        };
        assert_eq!(title.chars().count(), MAX_PERMISSION_TITLE);
        assert!(title.ends_with('…'));
        let body = body.unwrap();
        assert_eq!(body.chars().count(), MAX_PERMISSION_BODY);
        assert!(body.ends_with('…'));
        assert_eq!(
            options[0].detail.as_ref().unwrap().chars().count(),
            MAX_DETAIL
        );
        // An id no agent uses is dropped rather than sent back.
        let odd = [option(
            &"i".repeat(500),
            "Yes",
            PermissionOptionKind::AllowOnce,
        )];
        assert_eq!(card(&long, &odd, true), None);
    }

    #[test]
    fn the_end_of_the_request_answers_the_agent() {
        let options = claude_options();
        let selected = |id: &str| {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id.to_owned()))
        };
        let answered = |choice: &str| {
            Resolution::Answered(Response::Permission {
                choice: choice.into(),
            })
        };
        assert_eq!(
            outcome(&answered("allow-once"), &options),
            selected("allow-once")
        );
        assert_eq!(
            outcome(&answered("allow-with-updates"), &options),
            selected("allow-with-updates")
        );
        assert_eq!(outcome(&answered("reject"), &options), selected("reject"));
        // Never an option the agent did not offer.
        assert_eq!(outcome(&answered("other"), &options), selected("reject"));
        // No answer means no.
        assert_eq!(outcome(&Resolution::Expired, &options), selected("reject"));
        assert_eq!(outcome(&Resolution::Declined, &options), selected("reject"));
        assert_eq!(
            outcome(&Resolution::Cancelled, &options),
            RequestPermissionOutcome::Cancelled
        );
        // Without a one-time refusal, a cancel grants nothing lasting.
        let always = [option("a", "Always", PermissionOptionKind::RejectAlways)];
        assert_eq!(
            outcome(&Resolution::Expired, &always),
            RequestPermissionOutcome::Cancelled
        );
    }
}
