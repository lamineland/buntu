//! Storage and queries of the normalised categories (PRD-33 § 5): classification of stored items,
//! theme borrowing through the `tvg-id`, facets by country / theme, and the user's corrections.
//! No classification rule lives here: they are in `crate::taxonomy`.

use crate::db::catalog_repo::FacetCount;
use crate::db::profile_repo::normalize_group_key;
use crate::db::{chrono_now_ts, DatabaseManager, DbError};
use crate::taxonomy::{
    classify_with_override, theme_from_epg_categories, theme_terms, ClassifyInput, TaxonomyOverride, Theme, ThemeSource,
    TAXONOMY_VERSION,
};
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

const TAXONOMY_VERSION_KEY: &str = "taxonomy_version";
const PAGE_SIZE: i64 = 5_000;

/// Country and theme restrictions added to a catalogue query (PRD-33 § 5.4). All `None` = no filter.
#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct TaxonomyFilter {
    /// ISO code, `INT`, or an empty string for "country unknown".
    pub country: Option<String>,
    pub theme: Option<String>,
    /// Only with `theme`. A series keeps the sub-theme of its main theme (its genre).
    pub subtheme: Option<String>,
}

impl TaxonomyFilter {
    pub fn is_empty(&self) -> bool {
        self.country.is_none() && self.theme.is_none()
    }

    /// SQL conditions (each starting with `AND`) for items (`series == false`, alias with `id`,
    /// `tx_country`) or series (`tx_country`, `tx_theme`). Values are validated and inlined: a value
    /// that is not a known code makes the condition false instead of reaching the SQL text.
    pub(crate) fn sql(&self, alias: &str, series: bool) -> String {
        let mut out = String::new();
        if let Some(country) = &self.country {
            let country = country.trim().to_ascii_uppercase();
            if country.is_empty() {
                out.push_str(&format!(" AND {alias}.tx_country IS NULL"));
            } else if country == "INT" || (country.len() == 2 && country.chars().all(|c| c.is_ascii_uppercase())) {
                out.push_str(&format!(" AND {alias}.tx_country = '{country}'"));
            } else {
                out.push_str(" AND 1 = 0");
            }
        }
        if let Some(theme) = &self.theme {
            match Theme::parse(theme.trim()) {
                None => out.push_str(" AND 1 = 0"),
                Some(theme) if series => {
                    out.push_str(&format!(" AND {alias}.tx_theme = '{}'", theme.as_str()));
                    match self.subtheme.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                        None => {}
                        Some(s) if s.chars().all(|c| c.is_ascii_lowercase()) => {
                            out.push_str(&format!(" AND {alias}.tx_subtheme = '{s}'"));
                        }
                        Some(_) => out.push_str(" AND 1 = 0"),
                    }
                }
                Some(theme) => {
                    let sub = match self.subtheme.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                        None => String::new(),
                        Some(s) if s.chars().all(|c| c.is_ascii_lowercase()) => format!(" AND t.subtheme = '{s}'"),
                        Some(_) => " AND 1 = 0".to_string(),
                    };
                    out.push_str(&format!(
                        " AND EXISTS (SELECT 1 FROM item_themes t WHERE t.item_id = {alias}.id AND t.theme = '{}'{sub})",
                        theme.as_str()
                    ));
                }
            }
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CategoryAxis {
    Country,
    Theme,
    Provider,
}

/// One entry of the category column: a country, a theme or a provider category.
#[derive(Debug, Clone, uniffi::Record)]
pub struct CategoryFacet {
    /// `FR` / `INT` / `` (unknown country), a theme id, or the raw provider label.
    pub key: String,
    pub count: u32,
    /// Sub-themes of a theme (axis `Theme`, live and movies only).
    pub subkeys: Vec<FacetCount>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct TaxonomyStatus {
    pub version: u32,
    pub stored_version: u32,
    pub live_total: u32,
    pub live_without_country: u32,
    pub live_without_theme: u32,
}

/// A correction as stored: `None` keeps the computed value.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TaxonomyCorrection {
    pub country: Option<String>,
    pub themes: Vec<String>,
}

/// What the classifier decided for one item, and whether the user corrected it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ItemTaxonomy {
    /// ISO code, `INT`, or `None` when unknown.
    pub country: Option<String>,
    pub themes: Vec<String>,
    /// The item has its own correction.
    pub corrected: bool,
    /// A rule on its provider category applies to it.
    pub category_rule: bool,
}

/// Corrections made by the user (PRD-33 § 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct TaxonomyOverrideSummary {
    pub items: u32,
    pub groups: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClassifyReport {
    pub items: u64,
    pub series: u64,
    pub borrowed: u64,
    /// Channels that took a theme from the categories of their guide (lot 33.5).
    pub hinted: u64,
}

/// The columns the classifier reads from a stored item.
struct StoredItem {
    id: i64,
    stable_hash: String,
    kind: String,
    group_title: Option<String>,
    tvg_id: Option<String>,
    name: String,
    country: Option<String>,
}

enum Scope {
    All,
    Source(i64),
    SeriesEpisodes(i64),
    Hash(String),
    /// Every item of a provider category, by its normalised label.
    Group(String),
}

/// `(source, series, hash, group_key)`: the `NULL` ones do not restrict the scope.
type ScopeParams<'a> = (Option<i64>, Option<i64>, Option<&'a str>, Option<&'a str>);

impl Scope {
    fn params(&self) -> ScopeParams<'_> {
        match self {
            Scope::All => (None, None, None, None),
            Scope::Source(id) => (Some(*id), None, None, None),
            Scope::SeriesEpisodes(id) => (None, Some(*id), None, None),
            Scope::Hash(h) => (None, None, Some(h), None),
            Scope::Group(g) => (None, None, None, Some(g)),
        }
    }
}

/// Same visibility rules as `get_groups` / `query_catalog` (hidden bouquets, adult, kids profile),
/// for a table aliased `alias` and a catalogue kind written as a trusted literal.
fn visibility_sql(alias: &str, kind_literal: &str) -> String {
    format!(
        " AND NOT EXISTS (SELECT 1 FROM hidden_groups h WHERE h.profile_id = (SELECT id FROM temp.active_profile) AND h.kind = {kind_literal} AND h.group_key = LOWER(TRIM({alias}.group_title))) \
         AND (NOT EXISTS (SELECT 1 FROM hidden_groups tx WHERE tx.profile_id = (SELECT id FROM temp.active_profile) AND (tx.kind LIKE 'country:%' OR tx.kind LIKE 'theme:%')) OR NOT EXISTS (SELECT 1 FROM hidden_groups tg WHERE tg.profile_id = (SELECT id FROM temp.active_profile) AND ((tg.kind = 'country:' || {kind_literal} AND tg.group_key = LOWER(COALESCE({alias}.tx_country, ''))) OR (tg.kind = 'theme:' || {kind_literal} AND tg.group_key = LOWER(COALESCE({alias}.tx_theme, '')))))) \
         AND (((SELECT show_adult FROM temp.catalog_flags) = 1 AND (SELECT kids_only FROM temp.catalog_flags) = 0) OR NOT is_adult_group({alias}.group_title)) \
         AND ((SELECT kids_only FROM temp.catalog_flags) = 0 OR EXISTS (SELECT 1 FROM kids_allowed_groups k WHERE k.profile_id = (SELECT id FROM temp.active_profile) AND ((k.kind = {kind_literal} AND k.group_key = LOWER(TRIM({alias}.group_title))) OR (k.kind = 'theme:' || {kind_literal} AND k.group_key = LOWER(COALESCE({alias}.tx_theme, '')) AND COALESCE({alias}.tx_theme, '') != 'adult'))))"
    )
}

/// Corrections of single items, by `stable_hash`.
fn load_overrides(conn: &Connection) -> Result<HashMap<String, TaxonomyOverride>, DbError> {
    load_override_table(conn, "SELECT stable_hash, country, themes FROM taxonomy_overrides")
}

/// Corrections of a whole provider category, by normalised label. An item's own correction wins.
fn load_group_overrides(conn: &Connection) -> Result<HashMap<String, TaxonomyOverride>, DbError> {
    load_override_table(conn, "SELECT group_key, country, themes FROM taxonomy_group_overrides")
}

fn load_override_table(conn: &Connection, sql: &str) -> Result<HashMap<String, TaxonomyOverride>, DbError> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (hash, country, themes) = row?;
        let themes = themes.map(|t| t.split(',').filter_map(|s| Theme::parse(s.trim())).collect::<Vec<_>>());
        out.insert(hash, TaxonomyOverride { country, themes });
    }
    Ok(out)
}

fn correction_record(c: TaxonomyOverride) -> TaxonomyCorrection {
    TaxonomyCorrection {
        country: c.country,
        themes: c.themes.unwrap_or_default().into_iter().map(|t| t.as_str().to_string()).collect(),
    }
}

fn stored_version(conn: &Connection) -> u32 {
    conn.query_row("SELECT value FROM app_settings WHERE key = ?1", params![TAXONOMY_VERSION_KEY], |r| {
        r.get::<_, String>(0)
    })
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(0)
}

/// Key under which two channels are the same channel: `DHA.tr@SD` and `DHA.tr` match.
fn tvg_key(tvg_id: &str) -> Option<String> {
    let key = tvg_id.split('@').next()?.trim().to_lowercase();
    (!key.is_empty()).then_some(key)
}

impl DatabaseManager {
    /// Classifies the stored items of `scope` page by page (one transaction per page, so the
    /// connection is shared with the rest of the app between pages).
    fn classify_items(
        &self,
        scope: &Scope,
        progress: &dyn Fn(u64, u64),
        cancel: &AtomicBool,
    ) -> Result<u64, DbError> {
        let (source, series, hash, group) = scope.params();
        let total: i64 = self.with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM iptv_items
                 WHERE (?1 IS NULL OR source_id = ?1) AND (?2 IS NULL OR series_id = ?2) AND (?3 IS NULL OR stable_hash = ?3)
                   AND (?4 IS NULL OR LOWER(TRIM(group_title)) = ?4)",
                params![source, series, hash, group],
                |r| r.get(0),
            )?)
        })?;
        let overrides = self.with_conn(load_overrides)?;
        let group_overrides = self.with_conn(load_group_overrides)?;
        let (mut last_id, mut done) = (0i64, 0u64);
        loop {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let page = self.with_conn_mut(|conn| {
                let tx = conn.transaction()?;
                let rows: Vec<StoredItem> = {
                    let mut stmt = tx.prepare_cached(
                        "SELECT id, stable_hash, kind, group_title, tvg_id, name, country FROM iptv_items
                         WHERE id > ?4 AND (?1 IS NULL OR source_id = ?1) AND (?2 IS NULL OR series_id = ?2) AND (?3 IS NULL OR stable_hash = ?3)
                           AND (?6 IS NULL OR LOWER(TRIM(group_title)) = ?6)
                         ORDER BY id LIMIT ?5",
                    )?;
                    let mapped = stmt.query_map(params![source, series, hash, last_id, PAGE_SIZE, group], |r| {
                        Ok(StoredItem {
                            id: r.get(0)?,
                            stable_hash: r.get(1)?,
                            kind: r.get(2)?,
                            group_title: r.get(3)?,
                            tvg_id: r.get(4)?,
                            name: r.get(5)?,
                            country: r.get(6)?,
                        })
                    })?;
                    mapped.collect::<Result<_, _>>()?
                };
                {
                    let mut delete = tx.prepare_cached("DELETE FROM item_themes WHERE item_id = ?1")?;
                    let mut update = tx.prepare_cached(
                        "UPDATE iptv_items SET tx_country = ?2, tx_country_src = ?3, tx_theme = ?4, tx_terms = ?5 WHERE id = ?1",
                    )?;
                    let mut insert = tx.prepare_cached(
                        "INSERT INTO item_themes (item_id, theme, subtheme, src) VALUES (?1, ?2, ?3, ?4)",
                    )?;
                    for item in &rows {
                        let id = &item.id;
                        let c = classify_with_override(
                            &ClassifyInput {
                                kind: &item.kind,
                                group_title: item.group_title.as_deref(),
                                tvg_id: item.tvg_id.as_deref(),
                                name: &item.name,
                                country_attr: item.country.as_deref().filter(|c| !c.trim().is_empty()),
                            },
                            overrides.get(&item.stable_hash).or_else(|| {
                                item.group_title.as_deref().and_then(|g| group_overrides.get(&normalize_group_key(g)))
                            }),
                        );
                        let terms = c.search_terms();
                        update.execute(params![
                            id,
                            c.country.as_ref().map(|m| m.code.as_str()),
                            c.country.as_ref().map(|m| m.source.as_str()),
                            c.primary_theme().as_str(),
                            (!terms.is_empty()).then_some(terms)
                        ])?;
                        delete.execute(params![id])?;
                        for tag in &c.themes {
                            insert.execute(params![id, tag.theme.as_str(), tag.subtheme, tag.source.as_str()])?;
                        }
                    }
                }
                tx.commit()?;
                Ok(rows.last().map(|r| (r.id, rows.len() as u64)))
            })?;
            let Some((max_id, n)) = page else { break };
            last_id = max_id;
            done += n;
            progress(done, total.max(0) as u64);
            if (n as i64) < PAGE_SIZE {
                break;
            }
        }
        Ok(done)
    }

    /// Series rows carry a country, a main theme and its sub-theme (the genre).
    fn classify_series(&self, scope: &Scope) -> Result<u64, DbError> {
        let (source, _, hash, group) = scope.params();
        if matches!(scope, Scope::SeriesEpisodes(_)) {
            return Ok(0);
        }
        let overrides = self.with_conn(load_overrides)?;
        let group_overrides = self.with_conn(load_group_overrides)?;
        self.with_conn_mut(|conn| {
            let tx = conn.transaction()?;
            let rows: Vec<(i64, String, Option<String>, String)> = {
                let mut stmt = tx.prepare_cached(
                    "SELECT id, stable_hash, group_title, title FROM series_catalog
                     WHERE (?1 IS NULL OR source_id = ?1) AND (?2 IS NULL OR stable_hash = ?2)
                       AND (?3 IS NULL OR LOWER(TRIM(group_title)) = ?3)",
                )?;
                let mapped = stmt.query_map(params![source, hash, group], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
                mapped.collect::<Result<_, _>>()?
            };
            {
                let mut update =
                    tx.prepare_cached("UPDATE series_catalog SET tx_country = ?2, tx_theme = ?3, tx_subtheme = ?4 WHERE id = ?1")?;
                for (id, stable_hash, group, title) in &rows {
                    let c = classify_with_override(
                        &ClassifyInput { kind: "series", group_title: group.as_deref(), tvg_id: None, name: title, country_attr: None },
                        overrides
                            .get(stable_hash)
                            .or_else(|| group.as_deref().and_then(|g| group_overrides.get(&normalize_group_key(g)))),
                    );
                    let main = c.primary_theme();
                    let genre = c.themes.iter().find(|t| t.theme == main).and_then(|t| t.subtheme);
                    update.execute(params![id, c.country.as_ref().map(|m| m.code.as_str()), main.as_str(), genre])?;
                }
            }
            tx.commit()?;
            Ok(rows.len() as u64)
        })
    }

    /// A channel with no theme of its own (`other`) takes the theme of another stored channel with
    /// the same `tvg-id` (PRD-33 lot 33.2): `Italy` playlists carry no theme, the same channel in a
    /// themed playlist does. Items the user corrected are left alone.
    pub fn borrow_themes_by_tvg_id(&self) -> Result<u64, DbError> {
        self.with_conn_mut(|conn| {
            let mut donors: HashMap<String, HashMap<String, u32>> = HashMap::new();
            {
                let mut stmt = conn.prepare(
                    "SELECT tvg_id, tx_theme FROM iptv_items
                     WHERE kind = 'live' AND tvg_id IS NOT NULL AND tvg_id != ''
                       AND tx_theme IS NOT NULL AND tx_theme NOT IN ('other', 'general', 'adult')",
                )?;
                let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
                for row in rows {
                    let (tvg_id, theme) = row?;
                    if let Some(key) = tvg_key(&tvg_id) {
                        *donors.entry(key).or_default().entry(theme).or_default() += 1;
                    }
                }
            }
            if donors.is_empty() {
                return Ok(0);
            }
            let receivers: Vec<(i64, String)> = {
                let mut stmt = conn.prepare(
                    "SELECT id, tvg_id FROM iptv_items
                     WHERE kind = 'live' AND tx_theme = 'other' AND tvg_id IS NOT NULL AND tvg_id != ''
                       AND stable_hash NOT IN (SELECT stable_hash FROM taxonomy_overrides)",
                )?;
                let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                mapped.collect::<Result<_, _>>()?
            };
            let tx = conn.transaction()?;
            let mut borrowed = 0u64;
            {
                let mut update = tx.prepare_cached(
                    "UPDATE iptv_items SET tx_theme = ?2, tx_terms = TRIM(COALESCE(tx_terms, '') || ' ' || ?3) WHERE id = ?1",
                )?;
                let mut delete = tx.prepare_cached("DELETE FROM item_themes WHERE item_id = ?1")?;
                let mut insert = tx.prepare_cached(
                    "INSERT INTO item_themes (item_id, theme, subtheme, src) VALUES (?1, ?2, NULL, ?3)",
                )?;
                for (id, tvg_id) in receivers {
                    let Some(counts) = tvg_key(&tvg_id).and_then(|k| donors.get(&k)) else { continue };
                    // Most frequent theme; ties broken by name so the result never depends on map order.
                    let Some((theme, _)) = counts.iter().max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0))) else { continue };
                    update.execute(params![id, theme, Theme::parse(theme).map_or("", theme_terms)])?;
                    delete.execute(params![id])?;
                    insert.execute(params![id, theme, ThemeSource::TvgId.as_str()])?;
                    borrowed += 1;
                }
            }
            tx.commit()?;
            Ok(borrowed)
        })
    }

    /// A channel still without a theme (`other`) takes the one its programmes agree on in the guide
    /// (PRD-33 lot 33.5), e.g. "Informazione" on most of its programmes -> news. Runs after the
    /// guide is imported and after every classification. Never gives `adult`; items the user
    /// corrected are left alone.
    pub fn apply_epg_theme_hints(&self) -> Result<u64, DbError> {
        let receivers: Vec<(i64, String)> = self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, tvg_id FROM iptv_items
                 WHERE kind = 'live' AND tx_theme = 'other' AND tvg_id IS NOT NULL AND tvg_id != ''
                   AND stable_hash NOT IN (SELECT stable_hash FROM taxonomy_overrides)",
            )?;
            let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(mapped.collect::<Result<_, _>>()?)
        })?;
        if receivers.is_empty() {
            return Ok(0);
        }
        let wanted: std::collections::HashSet<String> = receivers.iter().filter_map(|(_, t)| tvg_key(t)).collect();
        let mut categories: HashMap<String, Vec<(String, u32)>> = HashMap::new();
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT channel_tvg_id, category, COUNT(*) FROM epg_programs
                 WHERE category IS NOT NULL AND category != '' GROUP BY channel_tvg_id, category",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, u32>(2)?)))?;
            for row in rows {
                let (channel, category, n) = row?;
                if let Some(key) = tvg_key(&channel).filter(|k| wanted.contains(k)) {
                    categories.entry(key).or_default().push((category, n));
                }
            }
            Ok(())
        })?;
        if categories.is_empty() {
            return Ok(0);
        }
        self.with_conn_mut(|conn| {
            let tx = conn.transaction()?;
            let mut hinted = 0u64;
            {
                let mut update = tx.prepare_cached(
                    "UPDATE iptv_items SET tx_theme = ?2, tx_terms = TRIM(COALESCE(tx_terms, '') || ' ' || ?3) WHERE id = ?1",
                )?;
                let mut delete = tx.prepare_cached("DELETE FROM item_themes WHERE item_id = ?1")?;
                let mut insert = tx.prepare_cached(
                    "INSERT INTO item_themes (item_id, theme, subtheme, src) VALUES (?1, ?2, NULL, ?3)",
                )?;
                for (id, tvg_id) in receivers {
                    let Some(theme) = tvg_key(&tvg_id)
                        .and_then(|k| categories.get(&k))
                        .and_then(|c| theme_from_epg_categories(c))
                    else {
                        continue;
                    };
                    update.execute(params![id, theme.as_str(), theme_terms(theme)])?;
                    delete.execute(params![id])?;
                    insert.execute(params![id, theme.as_str(), ThemeSource::Epg.as_str()])?;
                    hinted += 1;
                }
            }
            tx.commit()?;
            Ok(hinted)
        })
    }

    fn classify_scope(&self, scope: Scope, progress: &dyn Fn(u64, u64), cancel: &AtomicBool) -> Result<ClassifyReport, DbError> {
        let items = self.classify_items(&scope, progress, cancel)?;
        let series = self.classify_series(&scope)?;
        let (borrowed, hinted) = if matches!(scope, Scope::Hash(_) | Scope::SeriesEpisodes(_)) {
            (0, 0)
        } else {
            (self.borrow_themes_by_tvg_id()?, self.apply_epg_theme_hints()?)
        };
        Ok(ClassifyReport { items, series, borrowed, hinted })
    }

    /// Called at the end of an M3U or Xtream refresh of `source_id`.
    pub fn classify_source(&self, source_id: i64) -> Result<ClassifyReport, DbError> {
        self.with_conn(|conn| {
            // Items replaced by a refresh leave their theme rows behind when foreign keys are off.
            conn.execute("DELETE FROM item_themes WHERE item_id NOT IN (SELECT id FROM iptv_items)", [])?;
            Ok(())
        })?;
        self.classify_scope(Scope::Source(source_id), &|_, _| {}, &AtomicBool::new(false))
    }

    /// Episodes added to the catalogue after the first Xtream refresh.
    pub fn classify_series_episodes(&self, series_id: i64) -> Result<u64, DbError> {
        Ok(self.classify_scope(Scope::SeriesEpisodes(series_id), &|_, _| {}, &AtomicBool::new(false))?.items)
    }

    /// Reclassifies the whole catalogue from the stored columns, without refreshing any source.
    pub fn reclassify_all(&self, progress: &dyn Fn(u64, u64), cancel: &AtomicBool) -> Result<ClassifyReport, DbError> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM item_themes WHERE item_id NOT IN (SELECT id FROM iptv_items)", [])?;
            Ok(())
        })?;
        let report = self.classify_scope(Scope::All, progress, cancel)?;
        if !cancel.load(Ordering::Relaxed) {
            self.set_stored_taxonomy_version(TAXONOMY_VERSION)?;
        }
        Ok(report)
    }

    /// Reclassifies when the stored rules are older than the code (first launch after the update
    /// included). Returns whether it ran.
    pub fn ensure_taxonomy_current(&self, progress: &dyn Fn(u64, u64), cancel: &AtomicBool) -> Result<bool, DbError> {
        if self.stored_taxonomy_version()? >= TAXONOMY_VERSION {
            return Ok(false);
        }
        // A new, empty catalogue has nothing to reclassify: record the version and stay silent.
        if !self.catalog_has_items() {
            self.set_stored_taxonomy_version(TAXONOMY_VERSION)?;
            return Ok(false);
        }
        self.reclassify_all(progress, cancel)?;
        Ok(true)
    }

    pub fn catalog_has_items(&self) -> bool {
        self.with_conn(|conn| {
            Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM iptv_items) OR EXISTS(SELECT 1 FROM series_catalog)", [], |r| r.get(0))?)
        })
        .unwrap_or(true)
    }

    pub fn stored_taxonomy_version(&self) -> Result<u32, DbError> {
        self.with_conn(|conn| Ok(stored_version(conn)))
    }

    fn set_stored_taxonomy_version(&self, version: u32) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO app_settings (key, value, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                params![TAXONOMY_VERSION_KEY, version.to_string(), chrono_now_ts()],
            )?;
            Ok(())
        })
    }

    pub fn taxonomy_status(&self) -> Result<TaxonomyStatus, DbError> {
        self.with_conn(|conn| {
            let (total, no_country, no_theme): (u32, u32, u32) = conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(tx_country IS NULL), 0), COALESCE(SUM(tx_theme IS NULL OR tx_theme = 'other'), 0)
                 FROM iptv_items WHERE kind = 'live'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            Ok(TaxonomyStatus {
                version: TAXONOMY_VERSION,
                stored_version: stored_version(conn),
                live_total: total,
                live_without_country: no_country,
                live_without_theme: no_theme,
            })
        })
    }

    /// Checks a correction and writes it in the storage format: an upper-case code or `INT`, and
    /// a comma-separated list of theme ids. Unknown values are refused so the closed list stays closed.
    fn validate_correction(country: Option<&str>, themes: Option<&[String]>) -> Result<(Option<String>, Option<String>), DbError> {
        let country = country.map(|c| c.trim().to_ascii_uppercase()).filter(|c| !c.is_empty());
        if let Some(c) = &country {
            if !(c == "INT" || (c.len() == 2 && c.chars().all(|ch| ch.is_ascii_uppercase()))) {
                return Err(DbError::Query(format!("Unknown country code '{c}'")));
            }
        }
        let themes = match themes {
            None => None,
            Some(list) => {
                let mut ids = Vec::new();
                for t in list {
                    let theme = Theme::parse(t.trim()).ok_or_else(|| DbError::Query(format!("Unknown theme '{t}'")))?;
                    ids.push(theme.as_str());
                }
                (!ids.is_empty()).then(|| ids.join(","))
            }
        };
        Ok((country, themes))
    }

    /// Stores (or replaces) a correction and applies it to the item at once. `themes` are theme ids.
    pub fn set_taxonomy_override(
        &self,
        stable_hash: &str,
        country: Option<&str>,
        themes: Option<&[String]>,
    ) -> Result<(), DbError> {
        let (country, themes) = Self::validate_correction(country, themes)?;
        if country.is_none() && themes.is_none() {
            return self.clear_taxonomy_override(stable_hash);
        }
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO taxonomy_overrides (stable_hash, country, themes, updated_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(stable_hash) DO UPDATE SET country = excluded.country, themes = excluded.themes, updated_at = excluded.updated_at",
                params![stable_hash, country, themes, chrono_now_ts()],
            )?;
            Ok(())
        })?;
        self.reclassify_hash(stable_hash)
    }

    pub fn clear_taxonomy_override(&self, stable_hash: &str) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM taxonomy_overrides WHERE stable_hash = ?1", params![stable_hash])?;
            Ok(())
        })?;
        self.reclassify_hash(stable_hash)
    }

    /// Classifies every item of a provider category the same way ("Classer cette catégorie…",
    /// PRD-33 § 7). The rule survives refreshes; an item's own correction still wins over it.
    pub fn set_taxonomy_group_override(
        &self,
        group_title: &str,
        country: Option<&str>,
        themes: Option<&[String]>,
    ) -> Result<(), DbError> {
        let key = normalize_group_key(group_title);
        if key.is_empty() {
            return Err(DbError::Query("A category rule needs a category name".to_string()));
        }
        let (country, themes) = Self::validate_correction(country, themes)?;
        if country.is_none() && themes.is_none() {
            return self.clear_taxonomy_group_override(group_title);
        }
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO taxonomy_group_overrides (group_key, country, themes, updated_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(group_key) DO UPDATE SET country = excluded.country, themes = excluded.themes, updated_at = excluded.updated_at",
                params![key, country, themes, chrono_now_ts()],
            )?;
            Ok(())
        })?;
        self.reclassify_group(&key)
    }

    pub fn clear_taxonomy_group_override(&self, group_title: &str) -> Result<(), DbError> {
        let key = normalize_group_key(group_title);
        self.with_conn(|conn| {
            conn.execute("DELETE FROM taxonomy_group_overrides WHERE group_key = ?1", params![key])?;
            Ok(())
        })?;
        self.reclassify_group(&key)
    }

    fn reclassify_group(&self, key: &str) -> Result<(), DbError> {
        self.classify_scope(Scope::Group(key.to_string()), &|_, _| {}, &AtomicBool::new(false))?;
        Ok(())
    }

    /// Country and themes stored for an item, with where they come from (for the correction sheet).
    pub fn get_item_taxonomy(&self, stable_hash: &str) -> Result<Option<ItemTaxonomy>, DbError> {
        self.with_conn(|conn| {
            let row: Option<(i64, Option<String>, Option<String>)> = conn
                .query_row(
                    "SELECT id, tx_country, group_title FROM iptv_items WHERE stable_hash = ?1",
                    params![stable_hash],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .ok();
            let Some((id, country, group)) = row else { return Ok(None) };
            let mut stmt = conn.prepare("SELECT theme FROM item_themes WHERE item_id = ?1 ORDER BY (theme = (SELECT tx_theme FROM iptv_items WHERE id = ?1)) DESC, theme")?;
            let themes = stmt.query_map(params![id], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            let corrected: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM taxonomy_overrides WHERE stable_hash = ?1)",
                params![stable_hash],
                |r| r.get(0),
            )?;
            let category_rule: bool = match group.as_deref().map(normalize_group_key) {
                Some(key) => conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM taxonomy_group_overrides WHERE group_key = ?1)",
                    params![key],
                    |r| r.get(0),
                )?,
                None => false,
            };
            Ok(Some(ItemTaxonomy { country, themes, corrected, category_rule }))
        })
    }

    /// The correction stored for one item, if any.
    pub fn get_taxonomy_override(&self, stable_hash: &str) -> Result<Option<TaxonomyCorrection>, DbError> {
        self.with_conn(|conn| Ok(load_overrides(conn)?.remove(stable_hash).map(correction_record)))
    }

    /// The rule stored for a provider category, if any.
    pub fn get_taxonomy_group_override(&self, group_title: &str) -> Result<Option<TaxonomyCorrection>, DbError> {
        let key = normalize_group_key(group_title);
        self.with_conn(|conn| Ok(load_group_overrides(conn)?.remove(&key).map(correction_record)))
    }

    /// How many corrections the user made: of single items, and of whole provider categories.
    pub fn taxonomy_override_summary(&self) -> Result<TaxonomyOverrideSummary, DbError> {
        self.with_conn(|conn| {
            let items: u32 = conn.query_row("SELECT COUNT(*) FROM taxonomy_overrides", [], |r| r.get(0))?;
            let groups: u32 = conn.query_row("SELECT COUNT(*) FROM taxonomy_group_overrides", [], |r| r.get(0))?;
            Ok(TaxonomyOverrideSummary { items, groups })
        })
    }

    /// Forgets every correction and classifies the whole catalogue again ("Réinitialiser mes
    /// corrections de catégories"). Returns how many corrections were removed.
    pub fn reset_taxonomy_overrides(&self) -> Result<u32, DbError> {
        let summary = self.taxonomy_override_summary()?;
        self.with_conn(|conn| {
            conn.execute("DELETE FROM taxonomy_overrides", [])?;
            conn.execute("DELETE FROM taxonomy_group_overrides", [])?;
            Ok(())
        })?;
        if summary.items + summary.groups > 0 {
            self.reclassify_all(&|_, _| {}, &AtomicBool::new(false))?;
        }
        Ok(summary.items + summary.groups)
    }

    fn reclassify_hash(&self, stable_hash: &str) -> Result<(), DbError> {
        self.classify_scope(Scope::Hash(stable_hash.to_string()), &|_, _| {}, &AtomicBool::new(false))?;
        // Without the donor pass an uncorrected channel would lose a borrowed theme here.
        self.borrow_themes_by_tvg_id()?;
        self.apply_epg_theme_hints()?;
        Ok(())
    }

    /// Entries of the category column for `axis`, with the filters of the other axis applied
    /// (PRD-33 § 5.4). `kind` is `live`, `movie` or `series`. Same visibility rules as `get_groups`.
    pub fn get_category_facets(
        &self,
        kind: &str,
        axis: CategoryAxis,
        source_id: Option<i64>,
        filter: &TaxonomyFilter,
    ) -> Result<Vec<CategoryFacet>, DbError> {
        if !matches!(kind, "live" | "movie" | "series") {
            return Err(DbError::Query(format!("Unknown catalogue kind '{kind}'")));
        }
        let series = kind == "series";
        let (table, alias) = if series { ("series_catalog", "sc") } else { ("iptv_items", "i") };
        let kind_filter = if series { String::new() } else { format!("{alias}.kind = '{kind}' AND ") };
        let visibility = visibility_sql(alias, &format!("'{kind}'"));
        match axis {
            CategoryAxis::Provider => Ok(self
                .get_groups(kind, source_id, false)?
                .into_iter()
                .map(|g| CategoryFacet { key: g.value, count: g.count, subkeys: Vec::new() })
                .collect()),
            CategoryAxis::Country => {
                // The country list is narrowed by the chosen theme only.
                let narrowed = TaxonomyFilter { country: None, theme: filter.theme.clone(), subtheme: filter.subtheme.clone() };
                let extra = narrowed.sql(alias, series);
                let sql = format!(
                    "SELECT COALESCE({alias}.tx_country, ''), COUNT(*) FROM {table} {alias}
                     WHERE {kind_filter}(?1 IS NULL OR {alias}.source_id = ?1){extra}{visibility}
                     GROUP BY 1 ORDER BY 2 DESC, 1"
                );
                self.with_conn(|conn| {
                    let mut stmt = conn.prepare(&sql)?;
                    let rows = stmt.query_map(params![source_id], |r| {
                        Ok(CategoryFacet { key: r.get(0)?, count: r.get(1)?, subkeys: Vec::new() })
                    })?;
                    Ok(rows.collect::<Result<Vec<_>, _>>()?)
                })
            }
            CategoryAxis::Theme => {
                // The theme list is narrowed by the chosen country only.
                let narrowed = TaxonomyFilter { country: filter.country.clone(), theme: None, subtheme: None };
                let extra = narrowed.sql(alias, series);
                self.with_conn(|conn| {
                    if series {
                        let sql = format!(
                            "SELECT COALESCE({alias}.tx_theme, 'other'), {alias}.tx_subtheme, COUNT(*) FROM {table} {alias}
                             WHERE (?1 IS NULL OR {alias}.source_id = ?1){extra}{visibility}
                             GROUP BY 1, 2"
                        );
                        let mut stmt = conn.prepare(&sql)?;
                        let rows = stmt.query_map(params![source_id], |r| {
                            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, u32>(2)?))
                        })?;
                        return Ok(theme_facets(rows.collect::<Result<Vec<_>, _>>()?));
                    }
                    // A theme the profile hid is not listed, even for channels that carry it as a second theme.
                    let sql = format!(
                        "SELECT t.theme, t.subtheme, COUNT(*) FROM iptv_items {alias}
                         JOIN item_themes t ON t.item_id = {alias}.id
                         WHERE {kind_filter}(?1 IS NULL OR {alias}.source_id = ?1){extra}{visibility}
                           AND (NOT EXISTS (SELECT 1 FROM hidden_groups tx WHERE tx.profile_id = (SELECT id FROM temp.active_profile) AND tx.kind LIKE 'theme:%')
                                OR NOT EXISTS (SELECT 1 FROM hidden_groups ht WHERE ht.profile_id = (SELECT id FROM temp.active_profile)
                                               AND ht.kind = 'theme:{kind}' AND ht.group_key = t.theme))
                         GROUP BY t.theme, t.subtheme"
                    );
                    let mut stmt = conn.prepare(&sql)?;
                    let rows = stmt.query_map(params![source_id], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, u32>(2)?))
                    })?;
                    let mut counted = Vec::new();
                    for row in rows {
                        counted.push(row?);
                    }
                    Ok(theme_facets(counted))
                })
            }
        }
    }
}

/// Facets of the theme axis from `(theme, sub-theme, count)` rows: one entry per theme, its
/// sub-themes inside, most populated first. An item has at most one sub-theme per theme, so the
/// rows of one theme add up to its total.
fn theme_facets(rows: Vec<(String, Option<String>, u32)>) -> Vec<CategoryFacet> {
    let mut totals: HashMap<String, u32> = HashMap::new();
    let mut subs: HashMap<String, Vec<FacetCount>> = HashMap::new();
    for (theme, sub, count) in rows {
        *totals.entry(theme.clone()).or_default() += count;
        if let Some(sub) = sub {
            subs.entry(theme).or_default().push(FacetCount { value: sub, count });
        }
    }
    let mut out: Vec<CategoryFacet> = totals
        .into_iter()
        .map(|(key, count)| {
            let mut subkeys = subs.remove(&key).unwrap_or_default();
            subkeys.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.value.cmp(&b.value)));
            CategoryFacet { key, count, subkeys }
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.key.cmp(&b.key)));
    out
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::catalog_repo::{CatalogFilter, SortField};

    struct Row<'a> {
        hash: &'a str,
        kind: &'a str,
        name: &'a str,
        group: &'a str,
        tvg_id: &'a str,
        country: &'a str,
    }

    fn live<'a>(hash: &'a str, name: &'a str, group: &'a str, tvg_id: &'a str) -> Row<'a> {
        Row { hash, kind: "live", name, group, tvg_id, country: "" }
    }

    fn db_with(rows: &[Row<'_>]) -> (DatabaseManager, i64) {
        let db = DatabaseManager::open_in_memory().expect("in-memory db");
        let source = db.add_source("S", "m3u", Some("http://x/list.m3u"), None, None, None).expect("source");
        insert(&db, source, rows);
        (db, source)
    }

    fn insert(db: &DatabaseManager, source: i64, rows: &[Row<'_>]) {
        db.with_conn(|conn| {
            for r in rows {
                conn.execute(
                    "INSERT INTO iptv_items (source_id, stable_hash, kind, name, name_clean, group_title, tvg_id, country, host, stream_url, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?4, ?5, NULLIF(?6, ''), NULLIF(?7, ''), 'x', 'http://x/1.ts', 0, 0)",
                    params![source, r.hash, r.kind, r.name, r.group, r.tvg_id, r.country],
                )?;
            }
            Ok(())
        })
        .expect("items");
    }

    fn facets(db: &DatabaseManager, axis: CategoryAxis, filter: &TaxonomyFilter) -> Vec<(String, u32)> {
        db.get_category_facets("live", axis, None, filter).expect("facets").into_iter().map(|f| (f.key, f.count)).collect()
    }

    fn catalog_filter(kind: &str) -> CatalogFilter {
        CatalogFilter {
            kind: kind.into(),
            source_id: None,
            query: String::new(),
            group_title: None,
            host: None,
            favorites_only: false,
            include_broken: true,
            include_hidden: false,
            sort_by: SortField::Name,
            sort_asc: true,
            limit: 100,
            offset: 0,
        }
    }

    fn names(db: &DatabaseManager, taxonomy: &TaxonomyFilter) -> Vec<String> {
        db.query_catalog_with(&catalog_filter("live"), taxonomy).expect("query").into_iter().map(|i| i.name).collect()
    }

    fn sample() -> (DatabaseManager, i64) {
        db_with(&[
            live("a", "TF1", "FR| GENERALISTES", ""),
            live("b", "BFM", "FR| INFOS", ""),
            live("c", "L'Equipe", "FR| SPORTS", ""),
            live("d", "Sky News", "|UK| NEWS", ""),
            live("e", "Kids UK", "Animation;Kids;Music", "kids.uk"),
            live("f", "Rai 1", "Italy", ""),
            live("g", "Mystery", "Undefined", ""),
        ])
    }

    #[test]
    fn classifying_a_source_fills_country_theme_and_theme_rows() {
        let (db, source) = sample();
        let report = db.classify_source(source).expect("classify");
        assert_eq!(report.items, 7);
        let (country, src, theme): (Option<String>, Option<String>, String) = db
            .with_conn(|c| {
                Ok(c.query_row("SELECT tx_country, tx_country_src, tx_theme FROM iptv_items WHERE stable_hash = 'e'", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?)
            })
            .unwrap();
        assert_eq!((country.as_deref(), src.as_deref(), theme.as_str()), (Some("GB"), Some("tvg_id"), "kids"));
        let themes: i64 = db
            .with_conn(|c| Ok(c.query_row("SELECT COUNT(*) FROM item_themes WHERE item_id = (SELECT id FROM iptv_items WHERE stable_hash = 'e')", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(themes, 2, "kids + music");
    }

    #[test]
    fn country_and_theme_facets_cross_filter() {
        let (db, source) = sample();
        db.classify_source(source).unwrap();

        let by_country = facets(&db, CategoryAxis::Country, &TaxonomyFilter::default());
        assert_eq!(by_country, vec![("FR".into(), 3), ("GB".into(), 2), ("".into(), 1), ("IT".into(), 1)]);

        let fr = TaxonomyFilter { country: Some("FR".into()), ..Default::default() };
        let by_theme = facets(&db, CategoryAxis::Theme, &fr);
        assert_eq!(by_theme.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), vec!["general", "news", "sports"]);

        let sports = TaxonomyFilter { theme: Some("sports".into()), ..Default::default() };
        assert_eq!(facets(&db, CategoryAxis::Country, &sports), vec![("FR".into(), 1)]);

        assert_eq!(names(&db, &fr), vec!["BFM", "L'Equipe", "TF1"]);
        let fr_sports = TaxonomyFilter { country: Some("FR".into()), theme: Some("sports".into()), subtheme: None };
        assert_eq!(names(&db, &fr_sports), vec!["L'Equipe"]);
        let unknown = TaxonomyFilter { country: Some("".into()), ..Default::default() };
        assert_eq!(names(&db, &unknown), vec!["Mystery"]);
        let kids = TaxonomyFilter { theme: Some("kids".into()), subtheme: Some("animation".into()), country: None };
        assert_eq!(names(&db, &kids), vec!["Kids UK"]);
        let music = TaxonomyFilter { theme: Some("music".into()), ..Default::default() };
        assert_eq!(names(&db, &music), vec!["Kids UK"], "an item with two themes is found under both");
    }

    #[test]
    fn sub_themes_are_listed_under_their_theme() {
        let (db, source) = sample();
        db.classify_source(source).unwrap();
        let facets = db.get_category_facets("live", CategoryAxis::Theme, None, &TaxonomyFilter::default()).unwrap();
        let kids = facets.iter().find(|f| f.key == "kids").expect("kids");
        assert_eq!(kids.count, 1);
        assert_eq!(kids.subkeys.iter().map(|s| s.value.as_str()).collect::<Vec<_>>(), vec!["animation"]);
    }

    #[test]
    fn the_provider_axis_is_the_unchanged_group_list() {
        let (db, source) = sample();
        db.classify_source(source).unwrap();
        let provider = facets(&db, CategoryAxis::Provider, &TaxonomyFilter::default());
        let groups: Vec<(String, u32)> = db.get_groups("live", None, false).unwrap().into_iter().map(|g| (g.value, g.count)).collect();
        assert_eq!(provider, groups);
        assert!(provider.iter().any(|(k, _)| k == "Animation;Kids;Music"));
    }

    #[test]
    fn a_bare_country_channel_borrows_the_theme_of_the_same_tvg_id_elsewhere() {
        let (db, source_a) = db_with(&[live("a", "DHA", "News", "DHA.tr@SD"), live("b", "Other", "General", "Z.tr@SD")]);
        let source_b = db.add_source("B", "m3u", Some("http://y/list.m3u"), None, None, None).unwrap();
        insert(&db, source_b, &[live("c", "DHA TR", "Turkey", "DHA.tr"), live("d", "Lonely", "Turkey", "Lonely.tr"), live("e", "Z TR", "Turkey", "Z.tr")]);
        db.classify_source(source_a).unwrap();
        let report = db.classify_source(source_b).unwrap();
        assert_eq!(report.borrowed, 1, "only DHA has a themed twin; `general` is not a donor");
        let theme_of = |hash: &str| -> (String, String) {
            db.with_conn(|c| {
                Ok(c.query_row(
                    "SELECT i.tx_theme, t.src FROM iptv_items i JOIN item_themes t ON t.item_id = i.id WHERE i.stable_hash = ?1",
                    params![hash],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .unwrap()
        };
        assert_eq!(theme_of("c"), ("news".into(), "tvg_id".into()));
        assert_eq!(theme_of("d").0, "other");
        assert_eq!(theme_of("e").0, "other");
    }

    #[test]
    fn corrections_apply_at_once_survive_a_reclassification_and_can_be_cleared() {
        let (db, source) = sample();
        db.classify_source(source).unwrap();
        db.set_taxonomy_override("g", Some("be"), Some(&["sports".to_string()])).unwrap();
        let sports = TaxonomyFilter { theme: Some("sports".into()), country: Some("BE".into()), subtheme: None };
        assert_eq!(names(&db, &sports), vec!["Mystery"]);

        db.reclassify_all(&|_, _| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(names(&db, &sports), vec!["Mystery"], "kept after a full reclassification");

        db.clear_taxonomy_override("g").unwrap();
        assert!(names(&db, &sports).is_empty());
        assert_eq!(names(&db, &TaxonomyFilter { country: Some("".into()), ..Default::default() }), vec!["Mystery"]);

        assert!(db.set_taxonomy_override("g", None, Some(&["weather".to_string()])).is_err(), "closed list");
        assert!(db.set_taxonomy_override("g", Some("France"), None).is_err());
    }

    #[test]
    fn hidden_bouquets_and_adult_groups_stay_out_of_every_axis() {
        let (db, source) = db_with(&[
            live("a", "TF1", "FR| GENERALISTES", ""),
            live("b", "Sport FR", "FR| SPORTS", ""),
            live("c", "Night", "FR| ADULTS +18", ""),
        ]);
        db.classify_source(source).unwrap();
        let all = TaxonomyFilter::default();
        assert_eq!(facets(&db, CategoryAxis::Country, &all), vec![("FR".into(), 2)], "adult hidden by default");
        assert!(!facets(&db, CategoryAxis::Theme, &all).iter().any(|(k, _)| k == "adult"));

        db.hide_group(2, "live", "FR| SPORTS").unwrap();
        db.set_active_profile(2).unwrap();
        assert_eq!(facets(&db, CategoryAxis::Country, &all), vec![("FR".into(), 1)]);
        assert_eq!(names(&db, &all), vec!["TF1"]);
        db.set_active_profile(1).unwrap();
        assert_eq!(facets(&db, CategoryAxis::Country, &all), vec![("FR".into(), 2)]);
    }

    #[test]
    fn a_kids_profile_only_sees_what_it_was_given_on_every_axis() {
        let (db, source) = sample();
        db.classify_source(source).unwrap();
        let kids = db.create_profile("Kid", "person.crop.circle", true).unwrap();
        db.set_active_profile(kids.id).unwrap();
        assert!(facets(&db, CategoryAxis::Country, &TaxonomyFilter::default()).is_empty());
        assert!(facets(&db, CategoryAxis::Theme, &TaxonomyFilter::default()).is_empty());
        assert!(names(&db, &TaxonomyFilter::default()).is_empty());
        db.allow_group(kids.id, "live", "Animation;Kids;Music").unwrap();
        assert_eq!(names(&db, &TaxonomyFilter::default()), vec!["Kids UK"]);
        assert_eq!(facets(&db, CategoryAxis::Theme, &TaxonomyFilter::default()).len(), 2);
    }

    #[test]
    fn filter_values_never_reach_the_sql_text() {
        let (db, source) = sample();
        db.classify_source(source).unwrap();
        for bad in ["FR'; DROP TABLE iptv_items; --", "fr or 1=1", "FRA"] {
            let f = TaxonomyFilter { country: Some(bad.into()), ..Default::default() };
            assert!(names(&db, &f).is_empty(), "{bad}");
        }
        let f = TaxonomyFilter { theme: Some("sports' OR '1'='1".into()), ..Default::default() };
        assert!(names(&db, &f).is_empty());
        let f = TaxonomyFilter { theme: Some("sports".into()), subtheme: Some("x' OR '1'='1".into()), country: None };
        assert!(names(&db, &f).is_empty());
        assert!(db.get_category_facets("live; DROP", CategoryAxis::Country, None, &TaxonomyFilter::default()).is_err());
        assert_eq!(names(&db, &TaxonomyFilter::default()).len(), 7, "table intact");
    }

    #[test]
    fn series_are_classified_with_a_country_and_a_main_theme() {
        let db = DatabaseManager::open_in_memory().unwrap();
        let source = db.add_source("S", "m3u", Some("http://x/list.m3u"), None, None, None).unwrap();
        db.with_conn(|c| {
            for (i, (title, group)) in [("Dix pour cent", "FR| SERIES"), ("Cartoon", "|UK| KIDS"), ("Mystery", "Undefined")].iter().enumerate() {
                c.execute(
                    "INSERT INTO series_catalog (source_id, canonical_key, stable_hash, title, group_title, episode_count, season_count, updated_at)
                     VALUES (?1, ?2, ?2, ?3, ?4, 0, 0, 0)",
                    params![source, format!("s{i}"), title, group],
                )?;
            }
            Ok(())
        })
        .unwrap();
        let report = db.classify_source(source).unwrap();
        assert_eq!(report.series, 3);
        let by_theme: Vec<(String, u32)> = db
            .get_category_facets("series", CategoryAxis::Theme, None, &TaxonomyFilter { country: Some("GB".into()), ..Default::default() })
            .unwrap()
            .into_iter()
            .map(|f| (f.key, f.count))
            .collect();
        assert_eq!(by_theme, vec![("kids".to_string(), 1)]);
        let found = db
            .query_series_with(&catalog_filter("series"), &TaxonomyFilter { theme: Some("series".into()), ..Default::default() })
            .unwrap();
        // A series whose label gives no theme is a `series` by kind.
        assert_eq!(found.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(), vec!["Dix pour cent", "Mystery"]);
    }

    fn movie<'a>(hash: &'a str, name: &'a str, group: &'a str) -> Row<'a> {
        Row { hash, kind: "movie", name, group, tvg_id: "", country: "" }
    }

    #[test]
    fn films_and_series_are_browsed_by_genre() {
        let (db, source) = db_with(&[
            movie("m1", "Heat", "FR| ACTION"),
            movie("m2", "Die Hard", "Action"),
            movie("m3", "Scream", "Horreur"),
            movie("m4", "Amélie", "Movies"),
            movie("m5", "Planet", "Documentary"),
        ]);
        db.with_conn(|c| {
            for (i, (title, group)) in [("Lost", "Drama;Series"), ("Friends", "Comedy;Series"), ("Dallas", "Drama")].iter().enumerate() {
                c.execute(
                    "INSERT INTO series_catalog (source_id, canonical_key, stable_hash, title, group_title, episode_count, season_count, updated_at)
                     VALUES (?1, ?2, ?2, ?3, ?4, 0, 0, 0)",
                    params![source, format!("s{i}"), title, group],
                )?;
            }
            Ok(())
        })
        .unwrap();
        db.classify_source(source).unwrap();

        // Movies: the "movies" theme unfolds into genres, "documentary" stays its own theme.
        let themes = db.get_category_facets("movie", CategoryAxis::Theme, None, &TaxonomyFilter::default()).unwrap();
        let movies = themes.iter().find(|f| f.key == "movies").expect("movies");
        assert_eq!(movies.count, 4);
        assert_eq!(
            movies.subkeys.iter().map(|s| (s.value.as_str(), s.count)).collect::<Vec<_>>(),
            vec![("action", 2), ("horror", 1)]
        );
        assert!(themes.iter().any(|f| f.key == "documentary"));
        let action = db
            .query_catalog_with(
                &catalog_filter("movie"),
                &TaxonomyFilter { theme: Some("movies".into()), subtheme: Some("action".into()), ..Default::default() },
            )
            .unwrap();
        assert_eq!(action.len(), 2);

        // Series carry their genre too, and a profile-independent filter finds them.
        let series = db.get_category_facets("series", CategoryAxis::Theme, None, &TaxonomyFilter::default()).unwrap();
        let main = series.iter().find(|f| f.key == "series").expect("series");
        assert_eq!(main.count, 3);
        assert_eq!(
            main.subkeys.iter().map(|s| (s.value.as_str(), s.count)).collect::<Vec<_>>(),
            vec![("drama", 2), ("comedy", 1)]
        );
        let drama = db
            .query_series_with(
                &catalog_filter("series"),
                &TaxonomyFilter { theme: Some("series".into()), subtheme: Some("drama".into()), ..Default::default() },
            )
            .unwrap();
        assert_eq!(drama.len(), 2);
        let hostile = db
            .query_series_with(
                &catalog_filter("series"),
                &TaxonomyFilter { theme: Some("series".into()), subtheme: Some("x' OR 1=1 --".into()), ..Default::default() },
            )
            .unwrap();
        assert!(hostile.is_empty());
    }

    fn add_guide(db: &DatabaseManager, tvg_id: &str, category: &str, programmes: i64) {
        db.with_conn(|c| {
            for n in 0..programmes {
                c.execute(
                    "INSERT INTO epg_programs (channel_tvg_id, start_ts, end_ts, title, category) VALUES (?1, ?2, ?3, 'P', ?4)",
                    params![tvg_id, (category.len() as i64 * 1_000 + n) * 100, (category.len() as i64 * 1_000 + n) * 100 + 50, category],
                )?;
            }
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_channel_without_a_theme_takes_the_one_its_guide_agrees_on() {
        let (db, source) = db_with(&[
            live("a", "Canale 5", "Italy", "c5.it@SD"),
            live("b", "Mystery TV", "Italy", "mystery.it"),
            live("c", "Mixed", "Italy", "mixed.it"),
            live("d", "Corrected", "Italy", "corrected.it"),
            live("e", "Bare", "Italy", "bare.it"),
        ]);
        add_guide(&db, "c5.it", "Informazione", 20);
        add_guide(&db, "mystery.it", "Magazine", 20);
        add_guide(&db, "mixed.it", "Sport", 5);
        add_guide(&db, "mixed.it", "Informazione", 5);
        add_guide(&db, "mixed.it", "Cartoni Animati", 5);
        add_guide(&db, "corrected.it", "Informazione", 20);
        db.set_taxonomy_override("d", None, Some(&["music".to_string()])).unwrap();
        db.classify_source(source).unwrap();

        let theme_of = |hash: &str| db.get_item_taxonomy(hash).unwrap().unwrap().themes[0].clone();
        assert_eq!(theme_of("a"), "news", "most programmes are news");
        assert_eq!(theme_of("b"), "other", "'Magazine' names no theme");
        assert_eq!(theme_of("c"), "other", "no theme has a majority");
        assert_eq!(theme_of("d"), "music", "the user's correction wins");
        assert_eq!(theme_of("e"), "other", "no guide, no hint");

        // The hint is a real theme: listed, filterable, searchable, and kept by a full reclassification.
        let found = db
            .query_catalog_with(&catalog_filter("live"), &TaxonomyFilter { theme: Some("news".into()), ..Default::default() })
            .unwrap();
        assert_eq!(found.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), vec!["Canale 5"]);
        db.reclassify_all(&|_, _| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(theme_of("a"), "news");
        let mut searched = catalog_filter("live");
        searched.query = "infos".into();
        assert_eq!(db.query_catalog_with(&searched, &TaxonomyFilter::default()).unwrap().len(), 1);
    }

    #[test]
    fn the_stored_version_triggers_one_background_reclassification() {
        let (db, _) = sample();
        assert_eq!(db.stored_taxonomy_version().unwrap(), 0);
        assert!(db.ensure_taxonomy_current(&|_, _| {}, &AtomicBool::new(false)).unwrap());
        assert_eq!(db.stored_taxonomy_version().unwrap(), TAXONOMY_VERSION);
        assert!(!db.ensure_taxonomy_current(&|_, _| {}, &AtomicBool::new(false)).unwrap(), "up to date: nothing to do");
        let status = db.taxonomy_status().unwrap();
        assert_eq!((status.live_total, status.live_without_country, status.stored_version), (7, 1, TAXONOMY_VERSION));
    }

    #[test]
    fn an_empty_catalogue_is_marked_current_without_a_reclassification() {
        let db = DatabaseManager::open_in_memory().unwrap();
        assert!(!db.ensure_taxonomy_current(&|_, _| {}, &AtomicBool::new(false)).unwrap());
        assert_eq!(db.stored_taxonomy_version().unwrap(), TAXONOMY_VERSION);
    }

    #[test]
    fn a_cancelled_reclassification_does_not_claim_to_be_up_to_date() {
        let (db, _) = sample();
        db.reclassify_all(&|_, _| {}, &AtomicBool::new(true)).unwrap();
        assert_eq!(db.stored_taxonomy_version().unwrap(), 0);
    }

    #[test]
    fn orphan_theme_rows_of_replaced_items_are_removed() {
        let (db, source) = sample();
        db.classify_source(source).unwrap();
        db.with_conn(|c| {
            c.execute("PRAGMA foreign_keys = OFF", [])?;
            c.execute("DELETE FROM iptv_items WHERE stable_hash = 'a'", [])?;
            c.execute("PRAGMA foreign_keys = ON", [])?;
            Ok(())
        })
        .unwrap();
        db.classify_source(source).unwrap();
        let orphans: i64 = db
            .with_conn(|c| Ok(c.query_row("SELECT COUNT(*) FROM item_themes WHERE item_id NOT IN (SELECT id FROM iptv_items)", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(orphans, 0);
    }

    #[tokio::test]
    async fn an_m3u_refresh_classifies_what_it_stores_and_keeps_the_provider_label() {
        let m3u = "#EXTM3U\n\
#EXTINF:-1 tvg-id=\"DHA.tr@SD\" group-title=\"News\",DHA\n\
http://iptv.local/live/1.ts\n\
#EXTINF:-1 tvg-id=\"B4U.in@SD\" group-title=\"Animation;Kids;Music\",B4U Kids\n\
http://iptv.local/live/2.ts\n\
#EXTINF:-1 group-title=\"Italy\",Rai Sport\n\
http://iptv.local/live/3.ts\n";
        let engine = crate::CndEngine::in_memory().expect("engine");
        let source = engine.add_source("T".into(), "m3u".into(), Some("http://iptv.local/p.m3u".into()), None, None, None).expect("source");
        let uuid = engine.list_sources().expect("sources").into_iter().find(|s| s.id == source).expect("source").uuid;
        engine
            .ingest_m3u_reader(source, &uuid, tokio::io::BufReader::new(m3u.as_bytes()), std::time::Instant::now())
            .await
            .expect("ingest");

        let countries: Vec<(String, u32)> = engine
            .get_category_facets("live".into(), CategoryAxis::Country, None, TaxonomyFilter::default())
            .expect("facets")
            .into_iter()
            .map(|f| (f.key, f.count))
            .collect();
        assert_eq!(countries, vec![("IN".to_string(), 1), ("IT".to_string(), 1), ("TR".to_string(), 1)]);
        let themes: Vec<String> = engine
            .get_category_facets("live".into(), CategoryAxis::Theme, None, TaxonomyFilter::default())
            .expect("facets")
            .into_iter()
            .map(|f| f.key)
            .collect();
        assert!(themes.contains(&"kids".to_string()) && themes.contains(&"music".to_string()) && themes.contains(&"news".to_string()));
        let groups = engine.get_groups("live".into(), None).expect("groups");
        assert!(groups.iter().any(|g| g.value == "Animation;Kids;Music"), "provider label untouched");

        // A refresh replaces the items; the new ones are classified again and no theme row is left behind.
        engine
            .ingest_m3u_reader(source, &uuid, tokio::io::BufReader::new(m3u.as_bytes()), std::time::Instant::now())
            .await
            .expect("second ingest");
        let status = engine.taxonomy_status().expect("status");
        assert_eq!((status.live_total, status.live_without_country), (3, 0));
    }

    // ---- Lot 33.4 -------------------------------------------------------------------------

    fn query_names(db: &DatabaseManager, query: &str) -> Vec<String> {
        let mut f = catalog_filter("live");
        f.query = query.to_string();
        db.query_catalog_with(&f, &TaxonomyFilter::default()).expect("search").into_iter().map(|i| i.name).collect()
    }

    #[test]
    fn a_country_or_a_theme_can_be_hidden_per_profile_on_every_axis() {
        let (db, source) = sample();
        db.classify_source(source).unwrap();
        let all = TaxonomyFilter::default();
        db.hide_group(2, "country:live", "FR").unwrap();
        db.hide_group(2, "theme:live", "music").unwrap();
        assert_eq!(names(&db, &all).len(), 7, "profile 1 still sees everything");

        db.set_active_profile(2).unwrap();
        assert_eq!(names(&db, &all), vec!["Kids UK", "Mystery", "Rai 1", "Sky News"], "France is gone; Kids UK stays: its main theme is kids, music is only its second");
        let countries = facets(&db, CategoryAxis::Country, &all);
        assert!(!countries.iter().any(|(k, _)| k == "FR"), "{countries:?}");
        let themes = facets(&db, CategoryAxis::Theme, &all);
        assert!(!themes.iter().any(|(k, _)| k == "music" || k == "general"), "{themes:?}");
        assert!(db.get_groups("live", None, false).unwrap().iter().all(|g| !g.value.starts_with("FR|")), "provider categories of a hidden country are gone too");
        db.set_active_profile(1).unwrap();
        assert_eq!(names(&db, &all).len(), 7);

        db.unhide_group(2, "country:live", "FR").unwrap();
        db.set_active_profile(2).unwrap();
        assert!(names(&db, &all).contains(&"TF1".to_string()));
    }

    #[test]
    fn a_hidden_country_also_hides_channels_without_a_provider_label() {
        let (db, source) = db_with(&[live("a", "Orphan FR", "", "x.fr"), live("b", "Other", "General", "y.de")]);
        db.with_conn(|c| Ok(c.execute("UPDATE iptv_items SET group_title = NULL WHERE stable_hash = 'a'", [])?)).unwrap();
        db.classify_source(source).unwrap();
        db.hide_group(1, "country:live", "FR").unwrap();
        assert_eq!(names(&db, &TaxonomyFilter::default()), vec!["Other"]);
    }

    #[test]
    fn a_kids_profile_can_be_given_a_whole_theme_but_never_the_adult_one() {
        let (db, source) = db_with(&[
            live("a", "Cartoon", "|UK| KIDS", ""),
            live("b", "Docs", "|UK| DOCUMENTARY", ""),
            live("c", "News", "|UK| NEWS", ""),
            live("d", "Night", "|UK| ADULTS +18", ""),
        ]);
        db.classify_source(source).unwrap();
        let kid = db.create_profile("Kid", "person.crop.circle", true).unwrap();
        db.set_active_profile(kid.id).unwrap();
        assert!(names(&db, &TaxonomyFilter::default()).is_empty());

        db.allow_group(kid.id, "theme:live", "kids").unwrap();
        db.allow_group(kid.id, "theme:live", "documentary").unwrap();
        assert_eq!(names(&db, &TaxonomyFilter::default()), vec!["Cartoon", "Docs"]);
        assert_eq!(facets(&db, CategoryAxis::Theme, &TaxonomyFilter::default()).len(), 2);
        assert_eq!(facets(&db, CategoryAxis::Country, &TaxonomyFilter::default()), vec![("GB".into(), 2)]);

        assert!(db.allow_group(kid.id, "theme:live", "adult").is_err());
        assert!(db.allow_group(kid.id, "theme:live", " ADULT ").is_err());
        // An adult channel is out of reach whatever else is allowed.
        db.set_active_profile(1).unwrap();
        db.allow_group(kid.id, "live", "|UK| ADULTS +18").unwrap();
        db.set_active_profile(kid.id).unwrap();
        assert!(!names(&db, &TaxonomyFilter::default()).contains(&"Night".to_string()));
    }

    #[test]
    fn a_provider_category_can_be_classified_once_for_all_its_channels() {
        let (db, source) = db_with(&[
            live("a", "Rai 1", "Italy", ""),
            live("b", "Rai 2", "Italy", ""),
            live("c", "TF1", "FR| GENERALISTES", ""),
        ]);
        db.classify_source(source).unwrap();
        let italy = TaxonomyFilter { country: Some("IT".into()), theme: Some("sports".into()), subtheme: None };
        assert!(names(&db, &italy).is_empty());

        db.set_taxonomy_group_override("  ITALY ", None, Some(&["sports".to_string()])).unwrap();
        assert_eq!(names(&db, &italy), vec!["Rai 1", "Rai 2"], "the rule applies to the whole category, whatever its spelling");
        assert_eq!(names(&db, &TaxonomyFilter { theme: Some("sports".into()), ..Default::default() }).len(), 2);

        // An item's own correction wins over the category rule.
        db.set_taxonomy_override("b", None, Some(&["music".to_string()])).unwrap();
        assert_eq!(names(&db, &italy), vec!["Rai 1"]);

        // The rule survives a refresh of the source and a full reclassification.
        db.classify_source(source).unwrap();
        db.reclassify_all(&|_, _| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(names(&db, &italy), vec!["Rai 1"]);
        assert_eq!(db.taxonomy_override_summary().unwrap(), TaxonomyOverrideSummary { items: 1, groups: 1 });

        db.clear_taxonomy_group_override("Italy").unwrap();
        assert!(names(&db, &italy).is_empty());
        assert!(db.set_taxonomy_group_override("Italy", Some("Italia"), None).is_err());
        assert!(db.set_taxonomy_group_override("  ", None, Some(&["sports".to_string()])).is_err());
        assert!(db.set_taxonomy_group_override("Italy", None, Some(&["nope".to_string()])).is_err());
    }

    #[test]
    fn resetting_the_corrections_restores_the_computed_values() {
        let (db, source) = sample();
        db.classify_source(source).unwrap();
        assert_eq!(db.reset_taxonomy_overrides().unwrap(), 0, "nothing to reset");
        db.set_taxonomy_override("g", Some("BE"), Some(&["sports".to_string()])).unwrap();
        db.set_taxonomy_group_override("Italy", None, Some(&["sports".to_string()])).unwrap();
        assert_eq!(db.taxonomy_override_summary().unwrap(), TaxonomyOverrideSummary { items: 1, groups: 1 });

        assert_eq!(db.reset_taxonomy_overrides().unwrap(), 2);
        assert_eq!(db.taxonomy_override_summary().unwrap(), TaxonomyOverrideSummary { items: 0, groups: 0 });
        assert!(names(&db, &TaxonomyFilter { country: Some("BE".into()), ..Default::default() }).is_empty());
        assert_eq!(names(&db, &TaxonomyFilter { country: Some("".into()), ..Default::default() }), vec!["Mystery"]);
        assert_eq!(names(&db, &TaxonomyFilter { country: Some("IT".into()), theme: Some("sports".into()), subtheme: None }).len(), 0);
    }

    #[test]
    fn search_finds_channels_by_the_words_of_their_country_and_theme() {
        let (db, source) = db_with(&[
            live("a", "L'Equipe", "FR| SPORTS", ""),
            live("b", "Sky Sports", "|UK| SPORTS", ""),
            live("c", "BFM", "FR| INFOS", ""),
            live("d", "Night", "FR| ADULTS +18", ""),
        ]);
        db.classify_source(source).unwrap();
        assert_eq!(query_names(&db, "sport france"), vec!["L'Equipe"]);
        assert_eq!(query_names(&db, "sport Royaume-Uni"), vec!["Sky Sports"]);
        assert_eq!(query_names(&db, "infos france"), vec!["BFM"]);
        assert_eq!(query_names(&db, "news"), vec!["BFM"], "English theme words work too");
        let both = query_names(&db, "sport");
        assert!(both.contains(&"L'Equipe".to_string()) && both.contains(&"Sky Sports".to_string()));
        assert!(!query_names(&db, "adult").iter().any(|n| n == "Night"), "adult words are not indexed as theme terms");
        // Name and provider label still work.
        assert_eq!(query_names(&db, "Equipe"), vec!["L'Equipe"]);
    }

    #[test]
    fn corrections_are_searchable_at_once() {
        let (db, source) = sample();
        db.classify_source(source).unwrap();
        assert!(query_names(&db, "Mystery sport").is_empty());
        db.set_taxonomy_override("g", Some("BE"), Some(&["sports".to_string()])).unwrap();
        assert_eq!(query_names(&db, "sport belgique"), vec!["Mystery"]);
    }

    #[test]
    fn an_index_without_the_taxonomy_column_is_rebuilt_once_and_keeps_its_content() {
        let dir = std::env::temp_dir().join(format!("cnd-fts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        {
            let db = DatabaseManager::open(&path).unwrap();
            let source = db.add_source("S", "m3u", Some("http://x/l.m3u"), None, None, None).unwrap();
            insert(&db, source, &[live("a", "L'Equipe", "FR| SPORTS", ""), live("b", "BFM", "FR| INFOS", "")]);
            db.classify_source(source).unwrap();
        }
        // Back to the previous index definition, as an older version left it.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "DROP TRIGGER trg_items_ai; DROP TRIGGER trg_items_ad; DROP TRIGGER trg_items_au; DROP TABLE iptv_items_fts;
                 CREATE VIRTUAL TABLE iptv_items_fts USING fts5(name, group_title, tvg_name, content='iptv_items', content_rowid='id', tokenize='unicode61 remove_diacritics 2');
                 INSERT INTO iptv_items_fts(iptv_items_fts) VALUES ('rebuild');",
            )
            .unwrap();
        }
        let db = DatabaseManager::open(&path).unwrap();
        assert_eq!(query_names(&db, "Equipe"), vec!["L'Equipe"], "existing names are still searchable");
        assert_eq!(query_names(&db, "sport france"), vec!["L'Equipe"], "the words stored before the upgrade are indexed");
        // And a later open does not rebuild again nor break anything.
        drop(db);
        let db = DatabaseManager::open(&path).unwrap();
        assert_eq!(query_names(&db, "infos"), vec!["BFM"]);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_correction_sheet_can_read_what_is_stored_and_where_it_comes_from() {
        let (db, source) = sample();
        db.classify_source(source).unwrap();
        let t = db.get_item_taxonomy("e").unwrap().expect("known item");
        assert_eq!((t.country.as_deref(), t.corrected, t.category_rule), (Some("GB"), false, false));
        assert_eq!(t.themes, vec!["kids", "music"]);
        assert!(db.get_item_taxonomy("nope").unwrap().is_none());
        assert!(db.get_taxonomy_override("e").unwrap().is_none());

        db.set_taxonomy_override("e", Some("be"), Some(&["sports".to_string()])).unwrap();
        db.set_taxonomy_group_override("Animation;Kids;Music", None, Some(&["education".to_string()])).unwrap();
        let t = db.get_item_taxonomy("e").unwrap().unwrap();
        assert_eq!((t.country.as_deref(), t.corrected, t.category_rule), (Some("BE"), true, true));
        assert_eq!(t.themes, vec!["sports"]);
        assert_eq!(
            db.get_taxonomy_override("e").unwrap(),
            Some(TaxonomyCorrection { country: Some("BE".into()), themes: vec!["sports".into()] })
        );
        assert_eq!(
            db.get_taxonomy_group_override(" animation;kids;music ").unwrap(),
            Some(TaxonomyCorrection { country: None, themes: vec!["education".into()] })
        );
        assert!(db.get_taxonomy_group_override("Italy").unwrap().is_none());
    }

    /// Migrates and reclassifies a real catalogue and prints what the category column would show.
    /// Writes to the file it is given: pass a COPY, e.g.
    /// `CND_TAXONOMY_DB_COPY=/tmp/copy.db cargo test -p cnd-core --lib --release migrates_a_copy -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a copy of a real catalogue: set CND_TAXONOMY_DB_COPY"]
    fn migrates_a_copy_of_a_real_catalogue() {
        let Some(path) = std::env::var_os("CND_TAXONOMY_DB_COPY") else { return };
        let db = DatabaseManager::open(&path).expect("open the copy");
        let started = std::time::Instant::now();
        let ran = db.ensure_taxonomy_current(&|_, _| {}, &AtomicBool::new(false)).expect("reclassify");
        println!("reclassified: {ran} in {:?}", started.elapsed());
        println!("status: {:?}", db.taxonomy_status().expect("status"));
        for axis in [CategoryAxis::Country, CategoryAxis::Theme] {
            let started = std::time::Instant::now();
            let facets = db.get_category_facets("live", axis, None, &TaxonomyFilter::default()).expect("facets");
            let top: Vec<String> = facets.iter().take(12).map(|f| format!("{}={}", f.key, f.count)).collect();
            println!("{axis:?}: {} entries in {:?}: {}", facets.len(), started.elapsed(), top.join(", "));
        }
        let provider = db.get_category_facets("live", CategoryAxis::Provider, None, &TaxonomyFilter::default()).expect("facets");
        println!("Provider: {} entries (as before)", provider.len());
    }

    /// SLAs of PRD-33 (reclassification < 2 s and facets < 50 ms on 50 000 items), timed in
    /// optimised builds only (`cargo test --release`).
    #[test]
    fn reclassifying_and_listing_fifty_thousand_items_is_fast() {
        let db = DatabaseManager::open_in_memory().unwrap();
        let source = db.add_source("S", "m3u", Some("http://x/list.m3u"), None, None, None).unwrap();
        let groups = ["Animation;Kids;Music", "FR| SPORTS FHD", "Italy", "General", "Undefined", "|UK| ENTERTAINMENT", "News"];
        db.with_conn_mut(|conn| {
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO iptv_items (source_id, stable_hash, kind, name, name_clean, group_title, tvg_id, host, stream_url, created_at, updated_at)
                     VALUES (?1, ?2, 'live', ?3, ?3, ?4, ?5, 'x', 'http://x/1.ts', 0, 0)",
                )?;
                for i in 0..50_000 {
                    stmt.execute(params![source, format!("h{i}"), format!("Channel {i}"), groups[i % groups.len()], format!("c{}.fr@SD", i % 30_000)])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        let started = std::time::Instant::now();
        db.reclassify_all(&|_, _| {}, &AtomicBool::new(false)).unwrap();
        let reclassify = started.elapsed();
        let started = std::time::Instant::now();
        let filter = TaxonomyFilter { country: Some("FR".into()), ..Default::default() };
        let themes = db.get_category_facets("live", CategoryAxis::Theme, None, &filter).unwrap();
        let countries = db.get_category_facets("live", CategoryAxis::Country, None, &TaxonomyFilter::default()).unwrap();
        let facet_time = started.elapsed();
        println!("reclassify 50 000: {reclassify:?}, facets: {facet_time:?}");
        assert!(!themes.is_empty() && !countries.is_empty());
        if !cfg!(debug_assertions) {
            assert!(reclassify.as_millis() < 2_000, "reclassification took {reclassify:?}");
            assert!(facet_time.as_millis() < 100, "two facet queries took {facet_time:?}");
        }
    }
}
