//! Finding the country of a catalogue item (PRD-33 § 4.1). Does not look at themes.

use crate::taxonomy::data::countries::{
    CODE_ALIASES, COUNTRIES, IGNORED_TVG_SUFFIXES, INTERNATIONAL, INTERNATIONAL_WORDS, LANGUAGE_PREFIXES,
};
use crate::taxonomy::tokenize::{leading_code, normalize_phrase};
use crate::taxonomy::ClassifyInput;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// Where a country came from, strongest first. Stored with the item so wrong guesses can be traced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountrySource {
    User,
    Attribute,
    GroupPrefix,
    GroupName,
    TvgId,
    NamePrefix,
    International,
}

impl CountrySource {
    pub fn as_str(self) -> &'static str {
        match self {
            CountrySource::User => "user",
            CountrySource::Attribute => "attribute",
            CountrySource::GroupPrefix => "group_prefix",
            CountrySource::GroupName => "group_name",
            CountrySource::TvgId => "tvg_id",
            CountrySource::NamePrefix => "name_prefix",
            CountrySource::International => "international",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountryMatch {
    /// ISO 3166-1 alpha-2 code, or `INT`.
    pub code: String,
    pub source: CountrySource,
}

/// What a leading code of a label means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrefixMeaning {
    Country(&'static str),
    Language,
    International,
}

struct Tables {
    codes: HashSet<&'static str>,
    aliases: HashMap<String, &'static str>,
    longest_alias_words: usize,
    international: HashSet<String>,
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut aliases = HashMap::new();
        let mut longest_alias_words = 1;
        for (code, names) in COUNTRIES {
            for name in *names {
                let key = normalize_phrase(name);
                longest_alias_words = longest_alias_words.max(key.split(' ').count());
                aliases.insert(key, *code);
            }
        }
        Tables {
            codes: COUNTRIES.iter().map(|(code, _)| *code).collect(),
            aliases,
            longest_alias_words,
            international: INTERNATIONAL_WORDS.iter().map(|w| normalize_phrase(w)).collect(),
        }
    })
}

/// ISO code for a short code as catalogues write it (`fr`, `UK`, `usa`), or `None`.
pub fn code_from_token(token: &str) -> Option<&'static str> {
    let lower = token.trim().to_ascii_lowercase();
    if let Some((_, iso)) = CODE_ALIASES.iter().find(|(alias, _)| *alias == lower) {
        return Some(iso);
    }
    if lower.len() != 2 {
        return None;
    }
    let upper = lower.to_ascii_uppercase();
    tables().codes.get(upper.as_str()).copied()
}

/// Meaning of the folded leading code of a label (`fr`, `ar`, `exyu`).
pub fn prefix_meaning(code: &str) -> Option<PrefixMeaning> {
    if LANGUAGE_PREFIXES.contains(&code) {
        return Some(PrefixMeaning::Language);
    }
    if tables().international.contains(code) {
        return Some(PrefixMeaning::International);
    }
    code_from_token(code).map(PrefixMeaning::Country)
}

/// Country named anywhere in a label, longest names first: `VOD Italy` -> `IT`,
/// `Bosnia and Herzegovina` -> `BA`, `日本 / Japan` -> `JP`.
pub fn country_in_phrase(text: &str) -> Option<&'static str> {
    let phrase = normalize_phrase(text);
    let words: Vec<&str> = phrase.split(' ').filter(|w| !w.is_empty()).collect();
    let t = tables();
    for size in (1..=t.longest_alias_words.min(words.len())).rev() {
        for window in words.windows(size) {
            if let Some(code) = t.aliases.get(&window.join(" ")) {
                return Some(code);
            }
        }
    }
    None
}

/// Names under which a country is searched: its first two names (English, French), or the
/// international words for `INT`.
pub fn country_terms(code: &str) -> String {
    if code == INTERNATIONAL {
        return "international monde".to_string();
    }
    COUNTRIES
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, names)| names.iter().take(2).copied().collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
}

fn is_international_phrase(text: &str) -> bool {
    let phrase = normalize_phrase(text);
    let words: Vec<&str> = phrase.split(' ').collect();
    let t = tables();
    words.iter().any(|w| t.international.contains(*w))
        || words.windows(2).any(|pair| t.international.contains(&pair.join(" ")))
}

/// `tvg-country` / `country` attribute: `FR`, `FR;BE`, `France`, `INT`, `UK`.
fn from_attribute(value: &str) -> Option<String> {
    let first = value.split([';', ',', '|']).map(str::trim).find(|s| !s.is_empty())?;
    if let Some(code) = code_from_token(first) {
        return Some(code.to_string());
    }
    if let Some(code) = country_in_phrase(first) {
        return Some(code.to_string());
    }
    is_international_phrase(first).then(|| INTERNATIONAL.to_string())
}

/// iptv-org style ids: `DHA.tr@SD` -> `TR`, `BBCOne.uk` -> `GB`. `Foo.tv` is not Tuvalu.
fn from_tvg_id(tvg_id: &str) -> Option<&'static str> {
    let id = tvg_id.split('@').next()?.trim();
    let (name, suffix) = id.rsplit_once('.')?;
    if name.is_empty() || suffix.len() != 2 || !suffix.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let suffix = suffix.to_ascii_lowercase();
    if IGNORED_TVG_SUFFIXES.contains(&suffix.as_str()) {
        return None;
    }
    code_from_token(&suffix)
}

fn found(code: &str, source: CountrySource) -> Option<CountryMatch> {
    Some(CountryMatch { code: code.to_string(), source })
}

/// First rule that answers wins (PRD-33 § 4.1). User overrides are applied by `taxonomy::classify`.
pub fn resolve_country(input: &ClassifyInput<'_>) -> Option<CountryMatch> {
    if let Some(code) = input.country_attr.and_then(from_attribute) {
        let source = if code == INTERNATIONAL { CountrySource::International } else { CountrySource::Attribute };
        return Some(CountryMatch { code, source });
    }

    let group = input.group_title.map(str::trim).filter(|g| !g.is_empty());
    let mut international_hint = false;

    if let Some(group) = group {
        match leading_code(group).and_then(|(code, _)| prefix_meaning(&code)) {
            Some(PrefixMeaning::Country(code)) => return found(code, CountrySource::GroupPrefix),
            Some(PrefixMeaning::International) => international_hint = true,
            Some(PrefixMeaning::Language) | None => {}
        }
        if let Some(code) = country_in_phrase(group) {
            return found(code, CountrySource::GroupName);
        }
        international_hint |= is_international_phrase(group);
    }

    if let Some(code) = input.tvg_id.and_then(from_tvg_id) {
        return found(code, CountrySource::TvgId);
    }

    if let Some((code, _)) = leading_code(input.name) {
        match prefix_meaning(&code) {
            Some(PrefixMeaning::Country(code)) => return found(code, CountrySource::NamePrefix),
            Some(PrefixMeaning::International) => international_hint = true,
            _ => {}
        }
    }

    international_hint.then(|| CountryMatch { code: INTERNATIONAL.to_string(), source: CountrySource::International })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(group: Option<&'a str>, tvg_id: Option<&'a str>, name: &'a str) -> ClassifyInput<'a> {
        ClassifyInput { kind: "live", group_title: group, tvg_id, name, country_attr: None }
    }

    fn code(i: ClassifyInput<'_>) -> Option<(String, CountrySource)> {
        resolve_country(&i).map(|m| (m.code, m.source))
    }

    #[test]
    fn every_alias_points_to_a_known_code_and_no_alias_is_shared() {
        let mut seen: HashMap<String, &str> = HashMap::new();
        for (code, names) in COUNTRIES {
            assert_eq!(code.len(), 2, "{code}");
            for name in *names {
                let key = normalize_phrase(name);
                if let Some(other) = seen.insert(key.clone(), code) {
                    assert_eq!(other, *code, "alias {key:?} used by {other} and {code}");
                }
            }
        }
        for (_, iso) in CODE_ALIASES {
            assert!(tables().codes.contains(iso), "{iso}");
        }
    }

    #[test]
    fn attribute_wins_and_accepts_codes_names_and_lists() {
        let mut i = input(Some("Italy"), Some("X.de"), "TF1");
        i.country_attr = Some("FR;BE");
        assert_eq!(code(i), Some(("FR".into(), CountrySource::Attribute)));
        let mut i = input(None, None, "x");
        i.country_attr = Some("UK");
        assert_eq!(code(i), Some(("GB".into(), CountrySource::Attribute)));
        let mut i = input(None, None, "x");
        i.country_attr = Some("International");
        assert_eq!(code(i), Some(("INT".into(), CountrySource::International)));
    }

    #[test]
    fn xtream_prefixes() {
        assert_eq!(code(input(Some("FR| SPORTS FHD"), None, "x")), Some(("FR".into(), CountrySource::GroupPrefix)));
        assert_eq!(code(input(Some("|UK| ENTERTAINMENT"), None, "x")), Some(("GB".into(), CountrySource::GroupPrefix)));
        assert_eq!(code(input(Some("[DE] Sport"), None, "x")), Some(("DE".into(), CountrySource::GroupPrefix)));
        assert_eq!(code(input(Some("USA | NBC"), None, "x")), Some(("US".into(), CountrySource::GroupPrefix)));
        assert_eq!(code(input(Some("IN | ZEE"), None, "x")), Some(("IN".into(), CountrySource::GroupPrefix)));
    }

    #[test]
    fn language_prefixes_never_set_a_country() {
        assert_eq!(code(input(Some("AR - MBC"), None, "MBC 1")), None);
        assert_eq!(code(input(Some("AR | NEWS"), Some("AlJazeera.qa"), "x")), Some(("QA".into(), CountrySource::TvgId)));
        assert_eq!(code(input(Some("EN| MOVIES"), None, "x")), None);
    }

    #[test]
    fn country_names_in_labels() {
        assert_eq!(code(input(Some("Italy"), None, "x")), Some(("IT".into(), CountrySource::GroupName)));
        assert_eq!(code(input(Some("UK"), None, "x")), Some(("GB".into(), CountrySource::GroupName)));
        assert_eq!(code(input(Some("USA"), None, "x")), Some(("US".into(), CountrySource::GroupName)));
        assert_eq!(code(input(Some("VOD Italy"), None, "x")), Some(("IT".into(), CountrySource::GroupName)));
        assert_eq!(code(input(Some("日本 / Japan"), None, "x")), Some(("JP".into(), CountrySource::GroupName)));
        assert_eq!(code(input(Some("Bosnia and Herzegovina"), None, "x")), Some(("BA".into(), CountrySource::GroupName)));
        assert_eq!(code(input(Some("North Korea"), None, "x")), Some(("KP".into(), CountrySource::GroupName)));
        assert_eq!(code(input(Some("Korea"), None, "x")), Some(("KR".into(), CountrySource::GroupName)));
        assert_eq!(code(input(Some("Royaume-Uni"), None, "x")), Some(("GB".into(), CountrySource::GroupName)));
    }

    #[test]
    fn tvg_id_suffixes() {
        assert_eq!(code(input(Some("News"), Some("DHA.tr@SD"), "DHA")), Some(("TR".into(), CountrySource::TvgId)));
        assert_eq!(code(input(Some("General"), Some("BBCOne.uk"), "BBC One")), Some(("GB".into(), CountrySource::TvgId)));
        assert_eq!(code(input(Some("Music"), Some("B4UMusic.in@India"), "B4U")), Some(("IN".into(), CountrySource::TvgId)));
        assert_eq!(code(input(Some("General"), Some("Foo.tv"), "Foo")), None);
        assert_eq!(code(input(Some("General"), Some("tf1"), "TF1")), None);
        assert_eq!(code(input(Some("General"), Some(".fr"), "x")), None);
    }

    #[test]
    fn name_prefix_then_international() {
        assert_eq!(code(input(Some("Sports"), None, "|FR| beIN Sports 1")), Some(("FR".into(), CountrySource::NamePrefix)));
        assert_eq!(code(input(Some("International"), None, "x")), Some(("INT".into(), CountrySource::International)));
        assert_eq!(code(input(Some("EXYU | RTS"), None, "x")), Some(("INT".into(), CountrySource::International)));
        assert_eq!(code(input(Some("Undefined"), None, "Some channel")), None);
    }
}
