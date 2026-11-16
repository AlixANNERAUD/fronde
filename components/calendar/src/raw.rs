//! Writes a parsed calendar back to text without the two spots where calcard 0.3's writer changes
//! data: it backslash-escapes `,` and `;` in URI values, which RFC 5545 §3.3.13 does not do, and
//! its parser stores an inline `ENCODING=BASE64;VALUE=BINARY` value that decodes to UTF-8 as
//! text and drops `ENCODING`, so the writer would emit the decoded text.
//!
//! Every other line is calcard's own output. Only the lines whose values are all URIs are written
//! here, from calcard's rendering of their name and parameters.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use calcard::icalendar::{
    ICalendar, ICalendarEntry, ICalendarParameterName, ICalendarParameterValue, ICalendarValue,
    ICalendarValueType, Uri,
};

const MAX_LINE: usize = 75;

/// The text with folded lines joined, and every line ending as `\n`.
pub(crate) fn unfold(ics: &str) -> String {
    ics.replace("\r\n", "\n")
        .replace("\n ", "")
        .replace("\n\t", "")
}

/// `cal` as text, with values restored to the form they had in `original`. Refuses when a value
/// cannot be restored exactly, rather than write it back changed.
pub(crate) fn write(mut cal: ICalendar, original: &str) -> anyhow::Result<String> {
    restore(&mut cal, original)?;
    let mut out = String::new();
    if !cal.components.is_empty() {
        write_component(&mut out, &cal, 0);
    }
    Ok(out)
}

/// The refusal for a property whose value cannot be written back as it was read.
fn cannot_keep(property: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "cannot edit the stored item: {property} holds inline data Fronde cannot keep exactly; \
         change it in your calendar app"
    )
}

/// The property name and parameters (name, upper-cased) of a content line, split outside quotes.
fn line_params(line: &str) -> Option<(&str, Vec<(String, String)>)> {
    let mut parts = Vec::new();
    let (mut start, mut quoted) = (0, false);
    let mut end = None;
    for (i, ch) in line.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            ';' if !quoted => {
                parts.push(&line[start..i]);
                start = i + 1;
            }
            ':' if !quoted => {
                end = Some(i);
                break;
            }
            _ => {}
        }
    }
    parts.push(&line[start..end?]);
    let name = parts.remove(0);
    let params = parts
        .into_iter()
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (
                k.to_ascii_uppercase(),
                v.trim_matches('"').to_ascii_uppercase(),
            ),
            None => (p.to_ascii_uppercase(), String::new()),
        })
        .collect();
    Some((name, params))
}

/// calcard decodes any value with an ENCODING (or bare BASE64) parameter and drops the
/// parameter. Only `VALUE=BINARY` ones are restored (below), so any other is refused.
fn refuse_unrestorable_encodings(unfolded: &str) -> anyhow::Result<()> {
    for line in unfolded.lines() {
        let Some((name, params)) = line_params(line) else {
            continue;
        };
        let encoded = params.iter().any(|(k, _)| k == "ENCODING" || k == "BASE64");
        let binary = params.iter().any(|(k, v)| k == "VALUE" && v == "BINARY");
        if encoded && !binary {
            return Err(cannot_keep(&name.to_ascii_uppercase()));
        }
    }
    Ok(())
}

fn restore(cal: &mut ICalendar, original: &str) -> anyhow::Result<()> {
    let unfolded = unfold(original);
    refuse_unrestorable_encodings(&unfolded)?;
    for entry in cal.components.iter_mut().flat_map(|c| c.entries.iter_mut()) {
        let binary = entry.params.iter().any(|p| {
            p.name == ICalendarParameterName::Value
                && p.value == ICalendarParameterValue::Value(ICalendarValueType::Binary)
        });
        for value in &mut entry.values {
            match value {
                // The parser decoded the base64 and kept the bytes as text. When decoding failed,
                // the text is the undecodable payload: its base64 is then not in the original.
                ICalendarValue::Text(text) if binary => {
                    let bytes = std::mem::take(text).into_bytes();
                    if !bytes.is_empty() && !unfolded.contains(&BASE64.encode(&bytes)) {
                        return Err(cannot_keep(entry.name.as_str()));
                    }
                    *value = ICalendarValue::Binary(bytes);
                }
                // calcard keeps only the media type and the decoded bytes of a data: URI, so a
                // URI with other parameters (charset) or a percent-encoded payload cannot be
                // rebuilt: only the base64 form found in the original is written back.
                ICalendarValue::Uri(Uri::Data(data)) => {
                    let media_type = data.content_type.as_deref().unwrap_or_default();
                    let payload = BASE64.encode(&data.data);
                    let uri = format!("data:{media_type};base64,{payload}");
                    let escaped = format!("data:{media_type};base64\\,{payload}");
                    if !unfolded.contains(&uri) && !unfolded.contains(&escaped) {
                        return Err(cannot_keep(entry.name.as_str()));
                    }
                    *value = ICalendarValue::Uri(Uri::Location(uri));
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Walks the tree with an explicit stack, so that a deeply nested resource cannot overflow the
/// stack. A component's entries come before its children, which keep their `component_ids` order.
fn write_component(out: &mut String, cal: &ICalendar, id: usize) {
    enum Step {
        Open(usize),
        Close(String),
    }
    let mut stack = vec![Step::Open(id)];
    while let Some(step) = stack.pop() {
        match step {
            Step::Close(name) => push_folded(out, &format!("END:{name}")),
            Step::Open(id) => {
                let Some(component) = cal.components.get(id) else {
                    continue;
                };
                let name = component.component_type.as_str();
                push_folded(out, &format!("BEGIN:{name}"));
                for entry in &component.entries {
                    write_entry(out, entry);
                }
                stack.push(Step::Close(name.to_string()));
                stack.extend(
                    component
                        .component_ids
                        .iter()
                        .rev()
                        .map(|child| Step::Open(*child as usize)),
                );
            }
        }
    }
}

fn write_entry(out: &mut String, entry: &ICalendarEntry) {
    let all_uris = !entry.values.is_empty()
        && entry
            .values
            .iter()
            .all(|v| matches!(v, ICalendarValue::Uri(_)));
    if !all_uris {
        entry
            .write_to(out)
            .expect("writing to a String cannot fail");
        return;
    }
    // calcard renders the name and parameters (with their quoting); the values follow unescaped.
    let mut line = rendered(&ICalendarEntry {
        name: entry.name.clone(),
        params: entry.params.clone(),
        values: Vec::new(),
    });
    let separator = separator(entry);
    for (i, value) in entry.values.iter().enumerate() {
        if i > 0 {
            line.push_str(&separator);
        }
        if let ICalendarValue::Uri(uri) = value {
            push_uri(&mut line, &uri.to_unwrapped_string());
        }
    }
    push_folded(out, &line);
}

/// The entry as calcard writes it, on one line and without the line ending.
fn rendered(entry: &ICalendarEntry) -> String {
    let mut text = String::new();
    entry
        .write_to(&mut text)
        .expect("writing to a String cannot fail");
    let text = unfold(&text);
    text.strip_suffix('\n').unwrap_or(&text).to_string()
}

/// What calcard puts between the values of this property: read from its own output, so that it
/// stays in step with the parser that split them.
fn separator(entry: &ICalendarEntry) -> String {
    if entry.values.len() < 2 {
        return String::new();
    }
    let header = rendered(&ICalendarEntry {
        name: entry.name.clone(),
        params: entry.params.clone(),
        values: Vec::new(),
    });
    let two = rendered(&ICalendarEntry {
        name: entry.name.clone(),
        params: entry.params.clone(),
        values: vec![ICalendarValue::Integer(0), ICalendarValue::Integer(1)],
    });
    two.strip_prefix(&header)
        .and_then(|v| v.strip_prefix('0'))
        .and_then(|v| v.strip_suffix('1'))
        .unwrap_or(";")
        .to_string()
}

/// A URI value as RFC 5545 writes it: no escaping. A backslash or line break cannot be part of a
/// URI; they come from the source's own escapes, which are restored so the line stays one value.
fn push_uri(line: &mut String, uri: &str) {
    for ch in uri.chars() {
        match ch {
            '\\' => line.push_str("\\\\"),
            '\n' => line.push_str("\\n"),
            '\r' => line.push_str("\\r"),
            _ => line.push(ch),
        }
    }
}

/// Appends `line` folded at 75 octets, never inside a character, and its CRLF.
fn push_folded(out: &mut String, line: &str) {
    let mut width = 0;
    for ch in line.chars() {
        if width + ch.len_utf8() > MAX_LINE {
            out.push_str("\r\n ");
            width = 1;
        }
        out.push(ch);
        width += ch.len_utf8();
    }
    out.push_str("\r\n");
}
