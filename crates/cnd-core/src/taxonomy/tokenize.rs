//! Cutting category labels and channel names into comparable pieces (PRD-33 § 2).
//!
//! Knows nothing about countries or themes: it only folds case and accents, splits labels into
//! segments and words, drops quality noise, and reads a leading `FR|` / `[DE]` / `AR -` code.

use crate::iptv::normalizer::fold_diacritics_ascii;

/// Characters that separate the segments of one label: `Animation;Kids`, `FR| SPORTS`,
/// `日本 / Japan`, `Documentaries (AR)`.
const SEGMENT_SEPARATORS: &[char] = &[';', '|', '/', ',', '•', '(', ')', '[', ']', '{', '}', ':'];

/// Words that say nothing about a country or a theme.
const NOISE_WORDS: &[&str] = &[
    "hd", "fhd", "uhd", "sd", "hq", "lq", "4k", "8k", "hevc", "h265", "h264", "1080p", "720p",
    "50fps", "60fps", "vip", "raw", "backup", "multi", "plus", "24", "7", "247", "tv", "channels",
    "channel", "chaines", "chaine", "canales", "canal", "canali", "kanal", "kanali", "live", "vod",
    "the", "and", "et", "de", "des", "du", "la", "le", "les", "y", "e", "di",
];

/// Lower case, without Latin accents.
pub fn fold(text: &str) -> String {
    fold_diacritics_ascii(&text.to_lowercase())
}

/// Folded, every non-alphanumeric character turned into one space: `Côte d'Ivoire` -> `cote d ivoire`.
pub fn normalize_phrase(text: &str) -> String {
    let folded = fold(text);
    let mut out = String::with_capacity(folded.len());
    let mut pending_space = false;
    for c in folded.chars() {
        if c.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(c);
        } else {
            pending_space = true;
        }
    }
    out
}

/// Segments of a label, trimmed, empty ones dropped. ` - ` and ` – ` also separate segments, but a
/// hyphen inside a word (`Royaume-Uni`, `Bosnie-Herzégovine`) does not.
pub fn split_segments(label: &str) -> Vec<&str> {
    label
        .split(|c: char| SEGMENT_SEPARATORS.contains(&c))
        .flat_map(|part| part.split(" - ").flat_map(|p| p.split(" – ")))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Words of a phrase without the noise words.
pub fn meaningful_words(phrase: &str) -> Vec<&str> {
    phrase.split(' ').filter(|w| !w.is_empty() && !NOISE_WORDS.contains(w)).collect()
}

/// A short leading code followed by a separator: `FR| SPORTS`, `|UK| ENTERTAINMENT`, `[DE] Sport`,
/// `AR - MBC`, `FR: M6`, `EXYU | RTS`. Returns the folded code and the rest of the label.
/// Plain words followed by a space (`UK ENTERTAINMENT`) are not codes: too ambiguous.
pub fn leading_code(text: &str) -> Option<(String, &str)> {
    let trimmed = text.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '|' | '[' | '('));
    let letters_end = trimmed
        .char_indices()
        .find(|(_, c)| !c.is_ascii_alphabetic())
        .map(|(i, _)| i)
        .unwrap_or(trimmed.len());
    if !(2..=7).contains(&letters_end) {
        return None;
    }
    let (code, after) = trimmed.split_at(letters_end);
    let after_spaces = after.trim_start();
    let separator = after_spaces.chars().next()?;
    if !matches!(separator, '|' | ']' | ')' | ':' | '-' | '–') {
        return None;
    }
    // `Sports-Live` is one word with a hyphen, not a code: a hyphen must be spaced or be the end.
    if matches!(separator, '-' | '–') && after.len() == after_spaces.len() {
        let next = after_spaces[separator.len_utf8()..].chars().next();
        if next.is_some_and(|c| c.is_alphanumeric()) {
            return None;
        }
    }
    let rest = after_spaces.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '|' | ']' | ')' | ':' | '-' | '–'));
    Some((code.to_ascii_lowercase(), rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phrases_are_folded_and_spaced() {
        assert_eq!(normalize_phrase("Côte d'Ivoire"), "cote d ivoire");
        assert_eq!(normalize_phrase("  Royaume-Uni  "), "royaume uni");
        assert_eq!(normalize_phrase("日本 / Japan"), "日本 japan");
    }

    #[test]
    fn segments_split_on_label_separators_but_not_inner_hyphens() {
        assert_eq!(split_segments("Animation;Kids;Music"), vec!["Animation", "Kids", "Music"]);
        assert_eq!(split_segments("Documentaries (AR)"), vec!["Documentaries", "AR"]);
        assert_eq!(split_segments("日本 / Japan"), vec!["日本", "Japan"]);
        assert_eq!(split_segments("AR - MBC"), vec!["AR", "MBC"]);
        assert_eq!(split_segments("Bosnie-Herzégovine"), vec!["Bosnie-Herzégovine"]);
    }

    #[test]
    fn noise_words_are_dropped() {
        assert_eq!(meaningful_words("sports fhd 24 7"), vec!["sports"]);
        assert_eq!(meaningful_words("italy hd"), vec!["italy"]);
    }

    #[test]
    fn leading_codes_need_a_separator() {
        assert_eq!(leading_code("FR| SPORTS FHD"), Some(("fr".into(), "SPORTS FHD")));
        assert_eq!(leading_code("|UK| ENTERTAINMENT"), Some(("uk".into(), "ENTERTAINMENT")));
        assert_eq!(leading_code("[DE] Sport"), Some(("de".into(), "Sport")));
        assert_eq!(leading_code("AR - MBC"), Some(("ar".into(), "MBC")));
        assert_eq!(leading_code("FR: M6"), Some(("fr".into(), "M6")));
        assert_eq!(leading_code("UK ENTERTAINMENT"), None);
        assert_eq!(leading_code("Animation;Kids"), None);
        assert_eq!(leading_code("Sports-Live"), None);
        assert_eq!(leading_code("X| foo"), None);
    }
}
