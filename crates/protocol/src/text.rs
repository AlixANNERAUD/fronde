//! Which characters a card never shows as they are. Every text of a card may come from an agent,
//! which may have read it anywhere: a character that renders as nothing, or that moves the text
//! around it, could make a command, a path or an option read as another one.

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
