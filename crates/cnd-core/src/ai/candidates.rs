//! Short list of channels sent to the model. Size is capped (channels, title length) so a request
//! stays around two thousand tokens whatever the size of the catalog.

use crate::ai::intent::AiCommandContext;
use crate::ai::local_matcher::{
    channel_name_tokens, channels_airing, distinctive_tokens, fts_variants, significant_tokens,
};
use crate::db::catalog_repo::{CatalogFilter, SortField};
use crate::db::taxonomy_repo::TaxonomyFilter;
use crate::taxonomy::country::country_in_phrase;
use crate::taxonomy::theme::theme_for_word;
use crate::db::{DatabaseManager, DbError, IptvItemRecord};
use std::collections::HashSet;

pub const MAX_CANDIDATES: usize = 40;
pub const MAX_TITLE_CHARS: usize = 60;
const PER_QUERY: u32 = 10;

#[derive(Debug, Clone)]
pub struct Candidate {
    pub item: IptvItemRecord,
    pub now: Option<String>,
    pub next: Option<String>,
}

/// Words that name a topic or a language, with the words a playlist uses for them.
const TOPICS: &[(&str, &[&str])] = &[
    ("info", &["info", "news"]),
    ("infos", &["info", "news"]),
    ("actualites", &["info", "news"]),
    ("actu", &["info", "news"]),
    ("news", &["news", "info"]),
    ("sport", &["sport", "sports"]),
    ("sports", &["sport", "sports"]),
    ("cinema", &["cinema", "movies", "film"]),
    ("films", &["cinema", "movies", "film"]),
    ("musique", &["music", "musique"]),
    ("music", &["music", "musique"]),
    ("enfants", &["kids", "jeunesse", "enfants"]),
    ("jeunesse", &["kids", "jeunesse", "enfants"]),
    ("dessins", &["kids", "cartoon"]),
    ("documentaire", &["documentary", "docu", "discovery"]),
    ("documentaires", &["documentary", "docu", "discovery"]),
    ("meteo", &["meteo", "weather"]),
];

const LANGUAGES: &[(&str, &[&str])] = &[
    ("anglais", &["uk", "us", "en"]),
    ("english", &["uk", "us", "en"]),
    ("francais", &["fr"]),
    ("french", &["fr"]),
    ("espagnol", &["es"]),
    ("spanish", &["es"]),
    ("allemand", &["de"]),
    ("german", &["de"]),
    ("italien", &["it"]),
    ("italian", &["it"]),
    ("arabe", &["ar"]),
    ("portugais", &["pt"]),
];

/// Countries where a language is mostly spoken, for "en anglais" (PRD-33 lot 33.4).
const LANGUAGE_COUNTRIES: &[(&str, &[&str])] = &[
    ("anglais", &["GB", "US"]),
    ("english", &["GB", "US"]),
    ("francais", &["FR"]),
    ("french", &["FR"]),
    ("espagnol", &["ES", "MX"]),
    ("spanish", &["ES", "MX"]),
    ("allemand", &["DE", "AT"]),
    ("german", &["DE", "AT"]),
    ("italien", &["IT"]),
    ("italian", &["IT"]),
    ("portugais", &["PT", "BR"]),
];

fn aliases(table: &[(&str, &[&'static str])], token: &str) -> Vec<&'static str> {
    table
        .iter()
        .find(|(word, _)| *word == token)
        .map(|(_, list)| list.to_vec())
        .unwrap_or_default()
}

fn search(db: &DatabaseManager, query: &str, group: Option<&str>) -> Result<Vec<IptvItemRecord>, DbError> {
    db.query_catalog(&CatalogFilter {
        kind: "live".to_string(),
        source_id: None,
        query: query.to_string(),
        group_title: group.map(str::to_string),
        host: None,
        favorites_only: false,
        include_broken: false,
        include_hidden: false,
        sort_by: SortField::ChannelNumber,
        sort_asc: true,
        limit: PER_QUERY,
        offset: 0,
    })
}

/// Channels of a theme in one or several countries, from the stored classification.
fn search_taxonomy(db: &DatabaseManager, country: &str, theme: &str, subtheme: Option<&str>) -> Result<Vec<IptvItemRecord>, DbError> {
    db.query_catalog_with(
        &CatalogFilter {
            kind: "live".to_string(),
            source_id: None,
            query: String::new(),
            group_title: None,
            host: None,
            favorites_only: false,
            include_broken: false,
            include_hidden: false,
            sort_by: SortField::ChannelNumber,
            sort_asc: true,
            limit: PER_QUERY,
            offset: 0,
        },
        &TaxonomyFilter { country: Some(country.to_string()), theme: Some(theme.to_string()), subtheme: subtheme.map(str::to_string) },
    )
}

/// "le sport en Espagne", "une chaîne d'info en anglais": a theme and a country (named, or implied by
/// a language) together narrow the catalogue far better than words searched in a label.
fn taxonomy_matches(db: &DatabaseManager, command: &str, words: &[String]) -> Result<Vec<IptvItemRecord>, DbError> {
    let Some((theme, subtheme)) = words.iter().find_map(|w| theme_for_word(w)) else { return Ok(Vec::new()) };
    let mut countries: Vec<&str> = Vec::new();
    if let Some(code) = country_in_phrase(command) {
        countries.push(code);
    }
    for word in words {
        if let Some((_, list)) = LANGUAGE_COUNTRIES.iter().find(|(w, _)| w == word) {
            countries.extend(list.iter());
        }
    }
    let mut seen = HashSet::new();
    countries.retain(|c| seen.insert(*c));
    let mut out = Vec::new();
    for country in countries {
        out.extend(search_taxonomy(db, country, theme.as_str(), subtheme)?);
    }
    Ok(out)
}

fn truncate(text: &str, max: usize) -> String {
    let cleaned: String = text.chars().filter(|c| !c.is_control()).collect();
    if cleaned.chars().count() <= max {
        cleaned
    } else {
        let cut: String = cleaned.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", cut.trim_end())
    }
}

/// Union, best first: what the local matcher already found, programmes on air that match, then
/// searches by word (with topic and language synonyms), the category on screen, favorites and recents.
pub fn gather(
    db: &DatabaseManager,
    command: &str,
    context: &AiCommandContext,
    seed: Vec<IptvItemRecord>,
    now: i64,
) -> Result<Vec<Candidate>, DbError> {
    let words = significant_tokens(command);
    let mut pool: Vec<IptvItemRecord> = seed;
    let seed_len = pool.len();

    let distinctive = distinctive_tokens(command);
    if !distinctive.is_empty() {
        pool.extend(channels_airing(db, &distinctive, now).unwrap_or_default());
    }

    let mut singles: Vec<String> = Vec::new();
    let mut topic_aliases: Vec<&'static str> = Vec::new();
    let mut language_aliases: Vec<&'static str> = Vec::new();
    for word in &words {
        singles.push(word.clone());
        let topic = aliases(TOPICS, word);
        let language = aliases(LANGUAGES, word);
        singles.extend(topic.iter().chain(language.iter()).map(|a| a.to_string()));
        topic_aliases.extend(topic);
        language_aliases.extend(language);
    }
    // Narrowest first: a topic together with a language ("news uk") before either alone.
    let mut queries: Vec<String> = Vec::new();
    for topic in &topic_aliases {
        for language in &language_aliases {
            queries.push(format!("{topic} {language}"));
        }
    }
    queries.extend(singles);
    let mut seen_queries = HashSet::new();
    queries.retain(|q| seen_queries.insert(q.clone()));
    queries.truncate(12);
    for query in &queries {
        pool.extend(search(db, query, None)?);
    }
    // A theme with a country is the narrowest request: right after what the matcher already found.
    let narrow = taxonomy_matches(db, command, &words)?;
    if !narrow.is_empty() {
        let at = seed_len.min(pool.len());
        pool.splice(at..at, narrow);
    }
    // Several words together first: they narrow the list the most.
    if words.len() > 1 {
        let mut narrowed = Vec::new();
        for variant in fts_variants(command) {
            narrowed.extend(search(db, &variant, None)?);
        }
        narrowed.extend(std::mem::take(&mut pool));
        pool = narrowed;
    }

    if let Some(group) = context.group_title.as_deref().filter(|g| !g.trim().is_empty()) {
        pool.extend(search(db, "", Some(group))?);
    }
    pool.extend(db.list_favorite_items("live")?.into_iter().take(10));
    pool.extend(db.list_recent_items("live", 10)?);

    // One entry per channel, in order, capped.
    let mut seen_ids = HashSet::new();
    let mut seen_names = HashSet::new();
    let mut unique: Vec<IptvItemRecord> = Vec::new();
    for item in pool {
        if !seen_ids.insert(item.id) {
            continue;
        }
        if !seen_names.insert(channel_name_tokens(&item.name).join(" ")) {
            continue;
        }
        unique.push(item);
        if unique.len() >= MAX_CANDIDATES {
            break;
        }
    }

    let tvg_ids: Vec<String> = unique
        .iter()
        .filter_map(|i| i.tvg_id.clone())
        .filter(|id| !id.trim().is_empty())
        .collect();
    let programmes = db.get_epg_now_next_at(&tvg_ids, now)?;
    Ok(unique
        .into_iter()
        .map(|item| {
            let entry = item
                .tvg_id
                .as_ref()
                .and_then(|id| programmes.iter().find(|p| &p.channel_tvg_id == id));
            Candidate {
                now: entry.and_then(|p| p.now.as_ref()).map(|p| truncate(&p.title, MAX_TITLE_CHARS)),
                next: entry.and_then(|p| p.next.as_ref()).map(|p| truncate(&p.title, MAX_TITLE_CHARS)),
                item,
            }
        })
        .collect())
}

pub fn clean_name(name: &str) -> String {
    truncate(name, MAX_TITLE_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::local_matcher::test_catalog::*;

    #[test]
    fn topic_and_language_synonyms_reach_the_right_channels() {
        let s = seed();
        let found = gather(&s.db, "une chaîne d'info en anglais", &ctx(None), Vec::new(), s.now).unwrap();
        let names: Vec<&str> = found.iter().map(|c| c.item.name.as_str()).collect();
        assert!(names.contains(&"UK: BBC News"), "{names:?}");
        assert!(names.contains(&"US: CNN International"), "{names:?}");
        let bbc = found.iter().position(|c| c.item.name == "UK: BBC News").unwrap();
        let france24 = found.iter().position(|c| c.item.name == "FR: France 24");
        if let Some(f24) = france24 {
            assert!(bbc < f24, "topic + language narrows first");
        }
    }

    #[test]
    fn a_theme_with_a_country_uses_the_stored_classification() {
        let s = seed();
        let source: i64 = s.db.with_conn(|c| Ok(c.query_row("SELECT id FROM iptv_sources LIMIT 1", [], |r| r.get(0))?)).unwrap();
        s.db.classify_source(source).unwrap();
        let found = gather(&s.db, "le sport en France", &ctx(None), Vec::new(), s.now).unwrap();
        let names: Vec<&str> = found.iter().map(|c| c.item.name.as_str()).collect();
        let first_three: Vec<&str> = names.iter().take(3).copied().collect();
        assert_eq!(first_three, vec!["FR: Canal+ Sport", "FR: beIN Sports 1", "FR: beIN Sports 2"], "{names:?}");

        let found = gather(&s.db, "une chaîne d'info en anglais", &ctx(None), Vec::new(), s.now).unwrap();
        let names: Vec<&str> = found.iter().map(|c| c.item.name.as_str()).collect();
        assert_eq!(names[..2], ["UK: BBC News", "US: CNN International"], "{names:?}");
    }

    #[test]
    fn a_theme_alone_or_a_country_alone_adds_no_structured_search() {
        let s = seed();
        let source: i64 = s.db.with_conn(|c| Ok(c.query_row("SELECT id FROM iptv_sources LIMIT 1", [], |r| r.get(0))?)).unwrap();
        s.db.classify_source(source).unwrap();
        assert!(taxonomy_matches(&s.db, "du sport", &significant_tokens("du sport")).unwrap().is_empty());
        assert!(taxonomy_matches(&s.db, "la France", &significant_tokens("la France")).unwrap().is_empty());
    }

    #[test]
    fn current_programmes_are_attached_and_truncated() {
        let s = seed();
        let found = gather(&s.db, "match psg", &ctx(None), Vec::new(), s.now).unwrap();
        let bein = found.iter().find(|c| c.item.name == "FR: beIN Sports 1").expect("bein 1");
        assert_eq!(bein.now.as_deref(), Some("Ligue 1 : PSG - Marseille"));
        assert_eq!(truncate(&"é".repeat(100), 10).chars().count(), 10);
        assert!(truncate("a\nb\u{7}c", 10).chars().all(|c| !c.is_control()));
    }

    #[test]
    fn the_list_is_capped_deduplicated_by_channel_and_keeps_seeds_first() {
        let s = seed();
        let seed_item = s.db.get_item_by_id(id_of(&s.db, "FR: M6 HD")).unwrap().unwrap();
        let found = gather(&s.db, "chaîne", &ctx(None), vec![seed_item.clone()], s.now).unwrap();
        assert!(found.len() <= MAX_CANDIDATES);
        assert_eq!(found[0].item.id, seed_item.id);
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        for c in &found {
            assert!(ids.insert(c.item.id));
            assert!(names.insert(channel_name_tokens(&c.item.name).join(" ")), "duplicate channel {}", c.item.name);
        }
    }

    #[test]
    fn a_large_catalog_still_gives_a_bounded_list() {
        let db = DatabaseManager::open_in_memory().unwrap();
        let source = db.add_source("Big", "m3u", Some("http://x/l.m3u"), None, None, None).unwrap();
        db.with_conn(|conn| {
            for i in 0..3000 {
                conn.execute(
                    "INSERT INTO iptv_items (source_id, stable_hash, kind, name, name_clean, group_title, host, stream_url, created_at, updated_at)
                     VALUES (?1, ?2, 'live', ?3, ?3, 'FR | INFO', 'h', 'http://h/s', 1, 1)",
                    rusqlite::params![source, format!("h{i}"), format!("Info Channel {i}")],
                )?;
            }
            Ok(())
        })
        .unwrap();
        let found = gather(&db, "info", &ctx(None), Vec::new(), 0).unwrap();
        assert!(!found.is_empty() && found.len() <= MAX_CANDIDATES);
    }
}
