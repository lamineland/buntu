//! Finding the themes of a catalogue item (PRD-33 § 4.2). Does not look at countries, except to
//! skip a leading country code before reading the label.

use crate::db::profile_repo::is_adult_category;
use crate::taxonomy::country::prefix_meaning;
use crate::taxonomy::data::themes::{EPG_KEYWORDS, GENRE_KEYWORDS, KEYWORDS, NAME_KEYWORDS, QUALIFIER_SUBTHEMES};
use crate::taxonomy::tokenize::{leading_code, meaningful_words, normalize_phrase, split_segments};
use crate::taxonomy::{ClassifyInput, Theme};
use std::collections::HashMap;
use std::sync::OnceLock;

/// Where a theme came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeSource {
    User,
    Group,
    Kind,
    Name,
    /// Borrowed from another channel with the same `tvg-id` (stored by the database layer).
    TvgId,
    /// The `<category>` of the channel's programmes in the guide (lot 33.5).
    Epg,
    Fallback,
}

impl ThemeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ThemeSource::User => "user",
            ThemeSource::Group => "group",
            ThemeSource::Kind => "kind",
            ThemeSource::Name => "name",
            ThemeSource::TvgId => "tvg_id",
            ThemeSource::Epg => "epg",
            ThemeSource::Fallback => "fallback",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeTag {
    pub theme: Theme,
    pub subtheme: Option<&'static str>,
    pub source: ThemeSource,
}

type Entry = (Theme, Option<&'static str>);

struct Dictionary {
    exact: HashMap<String, Entry>,
    stems: Vec<(String, Entry)>,
    phrases: Vec<(String, Entry)>,
}

impl Dictionary {
    fn build(keywords: &[(&str, Theme, Option<&'static str>)]) -> Self {
        let mut dict = Dictionary { exact: HashMap::new(), stems: Vec::new(), phrases: Vec::new() };
        for (keyword, theme, sub) in keywords {
            let entry = (*theme, *sub);
            if let Some(stem) = keyword.strip_suffix('*') {
                dict.stems.push((normalize_phrase(stem), entry));
            } else {
                let key = normalize_phrase(keyword);
                if key.contains(' ') {
                    dict.phrases.push((key, entry));
                } else {
                    dict.exact.insert(key, entry);
                }
            }
        }
        // Longest stems first: "documentaire" before "docu".
        dict.stems.sort_by_key(|(stem, _)| std::cmp::Reverse(stem.len()));
        dict
    }

    fn word(&self, word: &str) -> Option<Entry> {
        self.exact
            .get(word)
            .copied()
            .or_else(|| self.stems.iter().find(|(stem, _)| word.starts_with(stem.as_str())).map(|(_, e)| *e))
    }

    /// Themes of one segment, in reading order.
    fn segment(&self, phrase: &str) -> Vec<Entry> {
        let mut padded = format!(" {phrase} ");
        let mut out: Vec<Entry> = Vec::new();
        for (p, entry) in &self.phrases {
            let needle = format!(" {p} ");
            if padded.contains(&needle) {
                out.push(*entry);
                // The words of a phrase are not read again alone: `science fiction` is not `science`.
                padded = padded.replace(&needle, "  ");
            }
        }
        out.extend(meaningful_words(padded.trim()).into_iter().filter_map(|w| self.word(w)));
        out
    }
}

fn label_dictionary() -> &'static Dictionary {
    static DICT: OnceLock<Dictionary> = OnceLock::new();
    DICT.get_or_init(|| Dictionary::build(KEYWORDS))
}

fn name_dictionary() -> &'static Dictionary {
    static DICT: OnceLock<Dictionary> = OnceLock::new();
    DICT.get_or_init(|| Dictionary::build(NAME_KEYWORDS))
}

fn vod_dictionary() -> &'static Dictionary {
    static DICT: OnceLock<Dictionary> = OnceLock::new();
    DICT.get_or_init(|| {
        let all: Vec<_> = KEYWORDS.iter().chain(GENRE_KEYWORDS).copied().collect();
        Dictionary::build(&all)
    })
}

fn epg_dictionary() -> &'static Dictionary {
    static DICT: OnceLock<Dictionary> = OnceLock::new();
    DICT.get_or_init(|| {
        let all: Vec<_> = KEYWORDS.iter().chain(EPG_KEYWORDS).copied().collect();
        Dictionary::build(&all)
    })
}

/// Theme of a channel from the `(category, number of programmes)` pairs of its guide (PRD-33 lot 33.5).
/// The theme most programmes agree on, when at least three programmes name one and they are at
/// least half of the programmes that name any. `general`, `other` and `adult` are never proposed:
/// a guide cannot make a channel adult.
pub fn theme_from_epg_categories(categories: &[(String, u32)]) -> Option<Theme> {
    let dict = epg_dictionary();
    let mut counts: HashMap<Theme, u32> = HashMap::new();
    for (category, n) in categories {
        let mut seen: Vec<Theme> = Vec::new();
        for segment in split_segments(category) {
            for (theme, _) in dict.segment(&normalize_phrase(segment)) {
                if !matches!(theme, Theme::General | Theme::Other | Theme::Adult) && !seen.contains(&theme) {
                    seen.push(theme);
                }
            }
        }
        for theme in seen {
            *counts.entry(theme).or_default() += n;
        }
    }
    let total: u32 = counts.values().sum();
    let (theme, top) = counts.into_iter().max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.as_str().cmp(a.0.as_str())))?;
    (top >= 3 && top * 2 >= total).then_some(theme)
}

/// Theme named by one word of a spoken request ("info", "sport", "films"), for the assistant
/// (PRD-33 lot 33.4). `general`, `other` and `adult` are never guessed from a word.
pub fn theme_for_word(word: &str) -> Option<(Theme, Option<&'static str>)> {
    let (theme, subtheme) = label_dictionary().word(&normalize_phrase(word))?;
    (!matches!(theme, Theme::General | Theme::Other | Theme::Adult)).then_some((theme, subtheme))
}

/// The label without a leading country, language or region code: `FR| SPORTS` -> `SPORTS`.
fn without_leading_code(label: &str) -> &str {
    match leading_code(label) {
        Some((code, rest)) if prefix_meaning(&code).is_some() => rest,
        _ => label,
    }
}

fn push(tags: &mut Vec<ThemeTag>, (theme, subtheme): Entry, source: ThemeSource) {
    if let Some(existing) = tags.iter_mut().find(|t| t.theme == theme) {
        // A genre or a precise sub-theme replaces a qualifier: `Classic;Comedy;Movies` is movies/comedy.
        let existing_is_qualifier = existing.subtheme.is_some_and(|s| QUALIFIER_SUBTHEMES.contains(&s));
        let new_is_qualifier = subtheme.is_some_and(|s| QUALIFIER_SUBTHEMES.contains(&s));
        if existing.subtheme.is_none() || (existing_is_qualifier && subtheme.is_some() && !new_is_qualifier) {
            existing.subtheme = subtheme;
        }
    } else {
        tags.push(ThemeTag { theme, subtheme, source });
    }
}

fn is_qualifier((_, sub): &Entry) -> bool {
    sub.is_some_and(|s| QUALIFIER_SUBTHEMES.contains(&s))
}

fn is_weak((theme, _): &Entry) -> bool {
    matches!(theme, Theme::General | Theme::Other)
}

fn from_group(label: &str, dict: &Dictionary) -> Vec<ThemeTag> {
    let mut entries: Vec<Entry> = Vec::new();
    for segment in split_segments(without_leading_code(label)) {
        entries.extend(dict.segment(&normalize_phrase(segment)));
    }
    // A qualifier (`Classic`, `Family`, `Interactive`) only stays when nothing else names a theme,
    // or when it refines a theme the label also names (`Classic;Movies` -> movies/classic).
    let core: Vec<Theme> = entries.iter().filter(|e| !is_weak(e) && !is_qualifier(e)).map(|e| e.0).collect();
    if !core.is_empty() {
        entries.retain(|e| !is_qualifier(e) || core.contains(&e.0));
    }
    let explicit_kids = entries.contains(&(Theme::Kids, None));
    let mut tags = Vec::new();
    for entry in entries {
        push(&mut tags, entry, ThemeSource::Group);
    }
    // `Animation;Comedy` without `Kids` is adult animation (PRD-33 § 14): entertainment, not kids.
    let has_comedy = tags.iter().any(|t| t.subtheme == Some("comedy"));
    if has_comedy && !explicit_kids {
        tags.retain(|t| !(t.theme == Theme::Kids && t.subtheme == Some("animation")));
    }
    if is_adult_category(label) {
        push(&mut tags, (Theme::Adult, None), ThemeSource::Group);
    }
    tags
}

fn only_weak(tags: &[ThemeTag]) -> bool {
    tags.iter().all(|t| matches!(t.theme, Theme::General | Theme::Other))
}

/// A film is never a "series" theme and the other way round: `Movies;Series` on an episode is series.
fn align_with_kind(tags: &mut Vec<ThemeTag>, kind_theme: Theme) {
    let wrong = if kind_theme == Theme::Movies { Theme::Series } else { Theme::Movies };
    if !tags.iter().any(|t| t.theme == wrong) {
        return;
    }
    let mut merged: Vec<ThemeTag> = Vec::new();
    for tag in tags.drain(..) {
        let theme = if tag.theme == wrong { kind_theme } else { tag.theme };
        push(&mut merged, (theme, tag.subtheme), tag.source);
    }
    *tags = merged;
}

/// All themes of an item, first one = order of the label (PRD-33 § 4.2).
/// User overrides are applied by `taxonomy::classify`.
pub fn resolve_themes(input: &ClassifyInput<'_>) -> Vec<ThemeTag> {
    let kind_theme = match input.kind {
        "movie" => Some(Theme::Movies),
        "episode" | "series" => Some(Theme::Series),
        _ => None,
    };
    // Films and series also read the genre words (lot 33.5); live channels do not.
    let dict = if kind_theme.is_some() { vod_dictionary() } else { label_dictionary() };
    let mut tags = input.group_title.map(|label| from_group(label, dict)).unwrap_or_default();
    if let Some(theme) = kind_theme {
        align_with_kind(&mut tags, theme);
    }
    if let Some(theme) = kind_theme {
        if only_weak(&tags) {
            tags.clear();
            tags.push(ThemeTag { theme, subtheme: None, source: ThemeSource::Kind });
        }
    }

    if tags.iter().all(|t| t.theme == Theme::Other) {
        let dict = name_dictionary();
        let mut from_name = Vec::new();
        for entry in dict.segment(&normalize_phrase(input.name)) {
            push(&mut from_name, entry, ThemeSource::Name);
        }
        if !from_name.is_empty() {
            tags = from_name;
        }
    }

    if tags.is_empty() {
        tags.push(ThemeTag { theme: Theme::Other, subtheme: None, source: ThemeSource::Fallback });
    }
    tags
}

/// Theme used to sort and display an item: adult first (safety), then the first real theme,
/// then `general`, then `other`.
pub fn primary_theme(tags: &[ThemeTag]) -> Theme {
    if tags.iter().any(|t| t.theme == Theme::Adult) {
        return Theme::Adult;
    }
    tags.iter()
        .map(|t| t.theme)
        .find(|d| !matches!(d, Theme::General | Theme::Other))
        .or_else(|| tags.iter().map(|t| t.theme).find(|d| *d == Theme::General))
        .unwrap_or(Theme::Other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn themes(kind: &str, group: Option<&str>, name: &str) -> Vec<(String, Option<&'static str>)> {
        let input = ClassifyInput { kind, group_title: group, tvg_id: None, name, country_attr: None };
        resolve_themes(&input).into_iter().map(|t| (t.theme.as_str().to_string(), t.subtheme)).collect()
    }

    fn live(group: &str) -> Vec<(String, Option<&'static str>)> {
        themes("live", Some(group), "Channel")
    }

    fn t(theme: &str, sub: Option<&'static str>) -> (String, Option<&'static str>) {
        (theme.to_string(), sub)
    }

    #[test]
    fn combined_iptv_org_labels_give_several_themes() {
        assert_eq!(live("Animation;Kids;Music"), vec![t("kids", Some("animation")), t("music", None)]);
        assert_eq!(live("Animation;Classic;Entertainment"), vec![t("kids", Some("animation")), t("entertainment", None)]);
        assert_eq!(live("Classic;News"), vec![t("news", None)]);
        assert_eq!(live("Classic;Movies"), vec![t("movies", Some("classic"))]);
        assert_eq!(live("Classic"), vec![t("movies", Some("classic"))]);
        assert_eq!(live("Family;Religious"), vec![t("religious", None)]);
        assert_eq!(live("Public;Sports"), vec![t("general", Some("public")), t("sports", None)]);
        assert_eq!(live("Business;News"), vec![t("news", Some("business"))]);
    }

    #[test]
    fn adult_animation_is_not_kids() {
        assert_eq!(live("Animation;Comedy"), vec![t("entertainment", Some("comedy"))]);
        assert_eq!(live("Animation;Comedy;Kids"), vec![t("kids", Some("animation")), t("entertainment", Some("comedy"))]);
    }

    #[test]
    fn xtream_labels_lose_their_country_code_and_noise() {
        assert_eq!(live("FR| SPORTS FHD"), vec![t("sports", None)]);
        assert_eq!(live("|UK| ENTERTAINMENT"), vec![t("entertainment", None)]);
        assert_eq!(live("AR | Documentaires"), vec![t("documentary", None)]);
        assert_eq!(live("Documentaries (AR)"), vec![t("documentary", None)]);
        assert_eq!(live("FR| Téléachat"), vec![t("shopping", None)]);
    }

    #[test]
    fn country_only_labels_fall_back_to_the_name() {
        assert_eq!(themes("live", Some("Italy"), "Rai Sport HD"), vec![t("sports", None)]);
        assert_eq!(themes("live", Some("Italy"), "Rai 1"), vec![t("other", None)]);
    }

    #[test]
    fn kind_fills_vod_without_a_theme() {
        assert_eq!(themes("movie", Some("VOD Italy"), "Film"), vec![t("movies", None)]);
        assert_eq!(themes("episode", Some("Undefined"), "S01E01"), vec![t("series", None)]);
        assert_eq!(themes("movie", Some("Comedy;Movies"), "x"), vec![t("movies", Some("comedy"))]);
    }

    #[test]
    fn vod_genres_are_sub_themes_of_the_kind() {
        assert_eq!(themes("movie", Some("Action"), "x"), vec![t("movies", Some("action"))]);
        assert_eq!(themes("movie", Some("FR| FILMS THRILLER"), "x"), vec![t("movies", Some("thriller"))]);
        assert_eq!(themes("movie", Some("Sci-Fi & Fantasy"), "x"), vec![t("movies", Some("scifi"))]);
        assert_eq!(themes("movie", Some("Science Fiction"), "x"), vec![t("movies", Some("scifi"))]);
        assert_eq!(themes("movie", Some("Horreur"), "x"), vec![t("movies", Some("horror"))]);
        assert_eq!(themes("movie", Some("Family;Movies"), "x"), vec![t("movies", Some("family"))]);
        assert_eq!(themes("movie", Some("Classic;Movies"), "x"), vec![t("movies", Some("classic"))]);
        // A series genre is a series sub-theme, and "Movies;Series" on an episode is series.
        assert_eq!(themes("episode", Some("Drama;Series"), "x"), vec![t("series", Some("drama"))]);
        assert_eq!(themes("series", Some("Comedy"), "x"), vec![t("series", Some("comedy"))]);
        assert_eq!(themes("episode", Some("Movies;Series"), "x"), vec![t("series", None)]);
        // Other themes stay themes.
        assert_eq!(themes("movie", Some("Documentary"), "x"), vec![t("documentary", None)]);
        assert_eq!(themes("movie", Some("Kids;Animation"), "x"), vec![t("kids", Some("animation"))]);
        // Live channels do not read genres.
        assert_eq!(live("Action"), vec![t("other", None)]);
        assert_eq!(live("Comedy"), vec![t("entertainment", Some("comedy"))]);
    }

    #[test]
    fn a_guide_names_a_channel_theme_only_when_programmes_agree() {
        let cats = |v: &[(&str, u32)]| v.iter().map(|(c, n)| (c.to_string(), *n)).collect::<Vec<_>>();
        assert_eq!(theme_from_epg_categories(&cats(&[("Informazione", 40), ("Magazine", 10)])), Some(Theme::News));
        assert_eq!(theme_from_epg_categories(&cats(&[("Cartoni Animati", 30), ("Per i bimbi piccoli", 20)])), Some(Theme::Kids));
        assert_eq!(theme_from_epg_categories(&cats(&[("Nieuws", 5)])), Some(Theme::News));
        assert_eq!(theme_from_epg_categories(&cats(&[("Sport", 2)])), None, "too few programmes");
        assert_eq!(theme_from_epg_categories(&cats(&[("Sport", 10), ("Informazione", 10), ("Sarjat", 10)])), None, "no majority");
        assert_eq!(theme_from_epg_categories(&cats(&[("Magazine", 99)])), None);
        assert_eq!(theme_from_epg_categories(&cats(&[("Adult", 50), ("XXX", 50)])), None);
    }

    #[test]
    fn undefined_and_general() {
        assert_eq!(live("Undefined"), vec![t("other", None)]);
        assert_eq!(live("General"), vec![t("general", None)]);
        assert_eq!(themes("live", None, "Nothing to see"), vec![t("other", None)]);
    }

    #[test]
    fn adult_uses_the_parental_rule_and_wins_the_primary_theme() {
        let input = ClassifyInput { kind: "live", group_title: Some("Movies +18"), tvg_id: None, name: "x", country_attr: None };
        let tags = resolve_themes(&input);
        assert!(tags.iter().any(|t| t.theme == Theme::Adult));
        assert_eq!(primary_theme(&tags), Theme::Adult);
    }

    #[test]
    fn primary_skips_general_and_other() {
        let input = ClassifyInput { kind: "live", group_title: Some("General;News"), tvg_id: None, name: "x", country_attr: None };
        assert_eq!(primary_theme(&resolve_themes(&input)), Theme::News);
        let input = ClassifyInput { kind: "live", group_title: Some("General"), tvg_id: None, name: "x", country_attr: None };
        assert_eq!(primary_theme(&resolve_themes(&input)), Theme::General);
    }
}
