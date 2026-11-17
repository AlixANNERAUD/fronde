//! Which characters a card never shows as they are. Every text of a card may come from an agent,
//! which may have read it anywhere: a character that renders as nothing, or that moves the text
//! around it, could make a command, a path or an option read as another one.
//!
//! Hidden characters become a visible U+FFFD when a text is cleaned for a card ([`line`],
//! [`block`]). A text too long keeps its head and its tail around a marker saying how much was
//! left out, so that padding cannot push its end out of sight, and long runs of blank space are
//! shown as such. [`Notes`] says when either happened, for the card to say so.

use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};

/// Whether `c` is hidden: a control, format, line or paragraph separator, private-use or
/// unassigned character, or a default-ignorable code point (soft hyphen, zero-width characters,
/// variation selectors, tag characters, fillers). Line breaks and tabs are controls too: a text
/// that allows them lets them through itself.
pub fn hidden(c: char) -> bool {
    matches!(
        c.general_category(),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator
            | GeneralCategory::PrivateUse
            | GeneralCategory::Unassigned
            | GeneralCategory::Surrogate
    ) || default_ignorable(c)
}

/// The first hidden character of `text`, line breaks and tabs allowed when `multiline`.
pub fn first_hidden(text: &str, multiline: bool) -> Option<char> {
    text.chars()
        .find(|&c| hidden(c) && !(multiline && matches!(c, '\n' | '\t')))
}

/// Unicode's `Default_Ignorable_Code_Point` (DerivedCoreProperties.txt, stable since 6.0 save
/// for unassigned points it already covered): code points a renderer may show as nothing at all.
fn default_ignorable(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}'
            | '\u{034f}'
            | '\u{061c}'
            | '\u{115f}'..='\u{1160}'
            | '\u{17b4}'..='\u{17b5}'
            | '\u{180b}'..='\u{180f}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{3164}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{feff}'
            | '\u{ffa0}'
            | '\u{fff0}'..='\u{fff8}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0000}'..='\u{e0fff}'
    )
}

/// A run of blank space longer than this shows as a marker: padding could push what follows
/// out of sight.
pub const MAX_SPACES: usize = 8;

/// More blank lines in a row than this show as a marker.
pub const MAX_BLANK_LINES: usize = 2;

/// Room kept for a "[… N caractères omis …]" marker.
pub const MARKER: usize = 32;

/// What cleaning a card's texts did, for the card to say so.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Notes {
    pub truncated: bool,
    pub replaced: bool,
}

/// The character the card shows for `c`: U+FFFD for a hidden one.
fn visible(c: char, notes: &mut Notes) -> char {
    if hidden(c) {
        notes.replaced = true;
        '\u{fffd}'
    } else {
        c
    }
}

/// Long runs of blank space within a line as a marker naming their length.
fn spaces(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut run = 0;
    let flush = |out: &mut String, run: usize| {
        if run > MAX_SPACES {
            out.push_str(&format!(" [… {run} espaces …] "));
        } else {
            out.extend(std::iter::repeat_n(' ', run));
        }
    };
    for c in line.chars() {
        if c == ' ' || c == '\t' || (c.is_whitespace() && c != '\n') {
            run += 1;
            continue;
        }
        flush(&mut out, run);
        run = 0;
        out.push(c);
    }
    flush(&mut out, run);
    out
}

/// `text` on one line: line breaks and tabs become spaces, hidden characters U+FFFD, long runs
/// of space a marker; at most `max` characters, its head and tail kept.
pub fn line(text: &str, max: usize, notes: &mut Notes) -> String {
    let flat: String = text
        .trim()
        .chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            c => visible(c, notes),
        })
        .collect();
    elide(spaces(&flat).trim(), max, notes)
}

/// `text` with its line breaks, tabs as spaces, hidden characters U+FFFD, long runs of space or
/// of blank lines a marker, each line at most `max_line` characters and the whole at most `max`,
/// their heads and tails kept.
pub fn block(text: &str, max_line: usize, max: usize, notes: &mut Notes) -> String {
    let clean: String = text
        .trim()
        .replace("\r\n", "\n")
        .chars()
        .map(|c| match c {
            '\n' => c,
            '\t' => ' ',
            c => visible(c, notes),
        })
        .collect();
    let mut lines: Vec<String> = Vec::new();
    let mut blank = 0;
    for line in clean.split('\n') {
        if line.trim().is_empty() {
            blank += 1;
            continue;
        }
        push_blank(&mut lines, blank);
        blank = 0;
        lines.push(elide(spaces(line).trim_end(), max_line, notes));
    }
    elide_lines(lines, max, notes)
}

/// `blank` empty lines, or one marker for more than [`MAX_BLANK_LINES`].
fn push_blank(lines: &mut Vec<String>, blank: usize) {
    if blank > MAX_BLANK_LINES {
        lines.push(format!("[… {blank} lignes vides …]"));
    } else {
        lines.extend(std::iter::repeat_n(String::new(), blank));
    }
}

/// `text` if it has at most `max` characters; else its head and its tail around a marker
/// counting what was left out.
pub fn elide(text: &str, max: usize, notes: &mut Notes) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_owned();
    }
    notes.truncated = true;
    let room = max.saturating_sub(MARKER);
    let head = room / 2;
    let tail = room - head;
    let omitted = chars.len() - head - tail;
    let head: String = chars[..head].iter().collect();
    let tail: String = chars[chars.len() - tail..].iter().collect();
    format!("{head}[… {omitted} caractères omis …]{tail}")
}

/// `lines` joined, if that has at most `max` characters; else the first and the last lines
/// that fit around a marker counting what was left out.
fn elide_lines(lines: Vec<String>, max: usize, notes: &mut Notes) -> String {
    let joined = lines.join("\n");
    let total = joined.chars().count();
    if total <= max {
        return joined;
    }
    notes.truncated = true;
    let budget = max.saturating_sub(MARKER + 2) / 2;
    let size = |line: &String| line.chars().count() + 1;
    let (mut head, mut used) = (0, 0);
    while head < lines.len() && used + size(&lines[head]) <= budget {
        used += size(&lines[head]);
        head += 1;
    }
    let (mut tail, mut tail_used) = (lines.len(), 0);
    while tail > head && tail_used + size(&lines[tail - 1]) <= budget {
        tail_used += size(&lines[tail - 1]);
        tail -= 1;
    }
    let omitted: usize = lines[head..tail].iter().map(size).sum();
    let mut kept: Vec<String> = lines[..head].to_vec();
    kept.push(format!("[… {omitted} caractères omis …]"));
    kept.extend(lines[tail..].iter().cloned());
    kept.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invisible_and_reordering_characters_are_hidden() {
        let hidden_ones = [
            '\u{0}',      // NUL
            '\u{1b}',     // escape
            '\u{7f}',     // delete
            '\u{85}',     // next line
            '\u{ad}',     // soft hyphen
            '\u{200b}',   // zero-width space
            '\u{200d}',   // zero-width joiner
            '\u{200e}',   // left-to-right mark
            '\u{202e}',   // right-to-left override
            '\u{2028}',   // line separator
            '\u{2029}',   // paragraph separator
            '\u{2066}',   // left-to-right isolate
            '\u{2060}',   // word joiner
            '\u{3164}',   // Hangul filler
            '\u{fe0f}',   // variation selector 16
            '\u{feff}',   // byte order mark
            '\u{e0041}',  // tag letter A
            '\u{e0100}',  // variation selector 17
            '\u{e000}',   // private use
            '\u{10ffff}', // unassigned
        ];
        for c in hidden_ones {
            assert!(hidden(c), "U+{:04X}", u32::from(c));
        }
        for c in ['a', 'é', ' ', '…', '«', 'ש', '中', '😀', '-', '\u{fffd}'] {
            assert!(!hidden(c), "U+{:04X}", u32::from(c));
        }
        assert_eq!(first_hidden("a\nb\tc", true), None);
        assert_eq!(first_hidden("a\nb", false), Some('\n'));
        assert_eq!(first_hidden("ok\u{200b}", true), Some('\u{200b}'));
    }
}
