use crate::db::catalog_repo::{map_item_row, ITEM_SELECT_COLUMNS};
use crate::db::{chrono_now_ts, DatabaseManager, DbError, IptvItemRecord};
use rusqlite::params;
use std::collections::HashSet;

#[derive(Debug, Clone)]
pub struct SagaRecord {
    pub saga_key: String,
    pub name: String,
    pub level: String, // 'saga' | 'universe'
    pub source: String, // 'local' | 'tmdb' | 'llm'
    pub confidence: f64,
    pub fetched_at: i64,
    pub missing_titles: Option<String>, // JSON array
}

#[derive(Debug, Clone)]
pub struct SagaMemberRecord {
    pub saga_key: String,
    pub work_key: String,
    pub release_index: i32,
    pub story_index: Option<i32>,
}

impl DatabaseManager {
    // MARK: - Dismissals

    pub fn add_recommendation_dismissal(&self, profile_id: i64, stable_hash: &str) -> Result<(), DbError> {
        let now = chrono_now_ts();
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO recommendation_dismissals (profile_id, stable_hash, dismissed_at)
                 VALUES (?1, ?2, ?3)",
                params![profile_id, stable_hash, now],
            )?;
            Ok(())
        })
    }

    pub fn reset_recommendation_dismissals(&self, profile_id: i64) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM recommendation_dismissals WHERE profile_id = ?1",
                params![profile_id],
            )?;
            Ok(())
        })
    }

    pub fn get_dismissed_hashes(&self, profile_id: i64) -> Result<HashSet<String>, DbError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT stable_hash FROM recommendation_dismissals WHERE profile_id = ?1",
            )?;
            let rows = stmt.query_map(params![profile_id], |r| r.get::<_, String>(0))?;
            let mut set = HashSet::new();
            for hash in rows {
                set.insert(hash?);
            }
            Ok(set)
        })
    }

    // MARK: - Sagas Storage

    pub fn save_saga(&self, saga: &SagaRecord, members: &[SagaMemberRecord]) -> Result<(), DbError> {
        self.with_conn_mut(|conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT OR REPLACE INTO sagas (saga_key, name, level, source, confidence, fetched_at, missing_titles)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    saga.saga_key,
                    saga.name,
                    saga.level,
                    saga.source,
                    saga.confidence,
                    saga.fetched_at,
                    saga.missing_titles
                ],
            )?;

            // Remove existing members for this saga and reinsert
            tx.execute("DELETE FROM saga_members WHERE saga_key = ?1", params![saga.saga_key])?;

            {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO saga_members (saga_key, work_key, release_index, story_index)
                     VALUES (?1, ?2, ?3, ?4)",
                )?;
                for m in members {
                    stmt.execute(params![m.saga_key, m.work_key, m.release_index, m.story_index])?;
                }
            }

            tx.commit()?;
            Ok(())
        })
    }

    pub fn get_saga(&self, saga_key: &str) -> Result<Option<(SagaRecord, Vec<SagaMemberRecord>)>, DbError> {
        self.with_conn(|conn| {
            use rusqlite::OptionalExtension;
            let mut saga_stmt = conn.prepare_cached(
                "SELECT saga_key, name, level, source, confidence, fetched_at, missing_titles
                 FROM sagas WHERE saga_key = ?1",
            )?;
            let saga = saga_stmt
                .query_row(params![saga_key], |r| {
                    Ok(SagaRecord {
                        saga_key: r.get(0)?,
                        name: r.get(1)?,
                        level: r.get(2)?,
                        source: r.get(3)?,
                        confidence: r.get(4)?,
                        fetched_at: r.get(5)?,
                        missing_titles: r.get(6)?,
                    })
                })
                .optional()?;

            let Some(saga) = saga else { return Ok(None) };

            let mut members_stmt = conn.prepare_cached(
                "SELECT saga_key, work_key, release_index, story_index
                 FROM saga_members WHERE saga_key = ?1
                 ORDER BY release_index ASC",
            )?;
            let member_rows = members_stmt.query_map(params![saga_key], |r| {
                Ok(SagaMemberRecord {
                    saga_key: r.get(0)?,
                    work_key: r.get(1)?,
                    release_index: r.get(2)?,
                    story_index: r.get(3)?,
                })
            })?;

            let mut members = Vec::new();
            for m in member_rows {
                members.push(m?);
            }

            Ok(Some((saga, members)))
        })
    }

    pub fn find_saga_for_work(&self, work_key: &str) -> Result<Option<(SagaRecord, Vec<SagaMemberRecord>)>, DbError> {
        let saga_key: Option<String> = self.with_conn(|conn| {
            use rusqlite::OptionalExtension;
            let mut stmt = conn.prepare_cached(
                "SELECT saga_key FROM saga_members WHERE work_key = ?1 LIMIT 1",
            )?;
            Ok(stmt.query_row(params![work_key], |r| r.get(0)).optional()?)
        })?;

        match saga_key {
            Some(key) => self.get_saga(&key),
            None => Ok(None),
        }
    }

    pub fn invalidate_sagas_for_root(&self, root: &str) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM sagas WHERE saga_key = ?1", params![root])?;
            Ok(())
        })
    }

    /// Finds a single item by its saga work key (either imdb_id like 'tt0848228' or 'root:year').
    pub fn find_item_by_work_key(&self, work_key: &str) -> Result<Option<IptvItemRecord>, DbError> {
        self.with_conn(|conn| {
            use rusqlite::OptionalExtension;
            // 1. If work_key looks like an imdb_id (starts with 'tt')
            if work_key.starts_with("tt") {
                let sql = format!(
                    "SELECT {ITEM_SELECT_COLUMNS}
                     FROM iptv_items i
                     LEFT JOIN favorites f ON f.profile_id = (SELECT id FROM temp.active_profile) AND f.stable_hash = i.stable_hash
                     LEFT JOIN broken_flags bf ON bf.stable_hash = i.stable_hash
                     LEFT JOIN watch_history wh ON wh.profile_id = (SELECT id FROM temp.active_profile) AND wh.stable_hash = i.stable_hash
                     WHERE i.kind = 'movie' AND i.imdb_id = ?1
                       AND bf.stable_hash IS NULL
                     LIMIT 1"
                );
                let mut stmt = conn.prepare_cached(&sql)?;
                let row = stmt.query_row(params![work_key], map_item_row).optional()?;
                if row.is_some() {
                    return Ok(row);
                }
            }

            // 2. Otherwise root:year or root
            let parts: Vec<&str> = work_key.split(':').collect();
            let root = parts[0];
            let year: Option<u32> = parts.get(1).and_then(|y| y.parse().ok()).filter(|&y| y > 0);

            let sql = format!(
                "SELECT {ITEM_SELECT_COLUMNS}
                 FROM iptv_items i
                 LEFT JOIN favorites f ON f.profile_id = (SELECT id FROM temp.active_profile) AND f.stable_hash = i.stable_hash
                 LEFT JOIN broken_flags bf ON bf.stable_hash = i.stable_hash
                 LEFT JOIN watch_history wh ON wh.profile_id = (SELECT id FROM temp.active_profile) AND wh.stable_hash = i.stable_hash
                 WHERE i.kind = 'movie'
                   AND (?1 IS NULL OR i.year = ?1)
                   AND (i.name_clean LIKE ?2 OR i.name LIKE ?2)
                   AND bf.stable_hash IS NULL
                 ORDER BY i.imdb_rating DESC
                 LIMIT 1"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let like_pattern = format!("%{}%", root);
            let row = stmt.query_row(params![year, like_pattern], map_item_row).optional()?;
            Ok(row)
        })
    }

    /// Searches active profile's movie candidates matching a title pattern.
    pub fn search_movie_candidates_like(&self, pattern: &str, limit: u32) -> Result<Vec<IptvItemRecord>, DbError> {
        self.with_conn(|conn| {
            let sql = format!(
                "SELECT {ITEM_SELECT_COLUMNS}
                 FROM iptv_items i
                 LEFT JOIN favorites f ON f.profile_id = (SELECT id FROM temp.active_profile) AND f.stable_hash = i.stable_hash
                 LEFT JOIN broken_flags bf ON bf.stable_hash = i.stable_hash
                 LEFT JOIN watch_history wh ON wh.profile_id = (SELECT id FROM temp.active_profile) AND wh.stable_hash = i.stable_hash
                 WHERE i.kind = 'movie'
                   AND (i.name_clean LIKE ?1 OR i.name LIKE ?1)
                   AND (bf.stable_hash IS NULL)
                   AND NOT EXISTS (
                       SELECT 1 FROM hidden_items hi
                       WHERE hi.profile_id = (SELECT id FROM temp.active_profile)
                         AND hi.stable_hash = i.stable_hash
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM hidden_groups hg
                       WHERE hg.profile_id = (SELECT id FROM temp.active_profile)
                         AND hg.kind = i.kind
                         AND hg.group_key = COALESCE(i.group_title, '')
                   )
                   AND (
                       NOT EXISTS (
                           SELECT 1 FROM hidden_groups tx
                           WHERE tx.profile_id = (SELECT id FROM temp.active_profile)
                             AND (tx.kind LIKE 'country:%' OR tx.kind LIKE 'theme:%')
                       )
                       OR NOT EXISTS (
                           SELECT 1 FROM hidden_groups tg
                           WHERE tg.profile_id = (SELECT id FROM temp.active_profile)
                             AND ((tg.kind = 'country:' || i.kind AND tg.group_key = LOWER(COALESCE(i.tx_country, '')))
                                  OR (tg.kind = 'theme:' || i.kind AND tg.group_key = LOWER(COALESCE(i.tx_theme, ''))))
                       )
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM recommendation_dismissals rd
                       WHERE rd.profile_id = (SELECT id FROM temp.active_profile)
                         AND rd.stable_hash = i.stable_hash
                   )
                   AND (
                       (SELECT kids_only FROM temp.catalog_flags) = 0
                       OR EXISTS (
                           SELECT 1 FROM kids_allowed_groups kag
                           WHERE kag.profile_id = (SELECT id FROM temp.active_profile)
                             AND (
                                 (kag.kind = i.kind AND kag.group_key = COALESCE(i.group_title, ''))
                                 OR (kag.kind = 'theme:' || i.kind AND kag.group_key = LOWER(COALESCE(i.tx_theme, '')) AND COALESCE(i.tx_theme, '') != 'adult')
                             )
                       )
                   )
                   AND (
                       (SELECT show_adult FROM temp.catalog_flags) = 1
                       OR NOT is_adult_group(i.group_title)
                   )
                 ORDER BY i.imdb_rating DESC, i.year ASC
                 LIMIT ?2"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![pattern, limit], map_item_row)?;
            let mut items = Vec::new();
            for r in rows {
                items.push(r?);
            }
            Ok(items)
        })
    }

    /// Loads candidates for a movie, preselecting matching saga titles, universe keywords,
    /// same category/group, and top rated movies.
    pub fn load_movie_candidates_for_item(
        &self,
        current: &IptvItemRecord,
        root: &str,
        universe_keywords: &[&str],
        limit: u32,
    ) -> Result<Vec<IptvItemRecord>, DbError> {
        let mut candidates = Vec::new();
        let mut seen_ids = HashSet::new();
        seen_ids.insert(current.id);

        // 1. Franchise root pattern
        if root.len() >= 3 {
            let root_pattern = format!("%{}%", root);
            if let Ok(root_items) = self.search_movie_candidates_like(&root_pattern, 40) {
                for item in root_items {
                    if seen_ids.insert(item.id) {
                        candidates.push(item);
                    }
                }
            }
        }

        // 2. Universe keywords
        for kw in universe_keywords {
            if candidates.len() >= limit as usize {
                break;
            }
            let kw_pattern = format!("%{}%", kw);
            if let Ok(kw_items) = self.search_movie_candidates_like(&kw_pattern, 20) {
                for item in kw_items {
                    if seen_ids.insert(item.id) {
                        candidates.push(item);
                    }
                }
            }
        }

        // 3. Same category / group_title
        if let Some(ref group) = current.group_title {
            if candidates.len() < limit as usize {
                let remaining = limit.saturating_sub(candidates.len() as u32);
                if let Ok(group_items) = self.load_recommendation_candidates("movie", Some(group), remaining.min(40)) {
                    for item in group_items {
                        if seen_ids.insert(item.id) {
                            candidates.push(item);
                        }
                    }
                }
            }
        }

        // 4. Fill remaining slots with general top-rated movies
        if candidates.len() < limit as usize {
            let remaining = limit.saturating_sub(candidates.len() as u32);
            if let Ok(top_items) = self.load_recommendation_candidates("movie", None, remaining) {
                for item in top_items {
                    if seen_ids.insert(item.id) {
                        candidates.push(item);
                    }
                }
            }
        }

        Ok(candidates)
    }

    // MARK: - Recommendation Candidates Query

    /// Loads active profile's candidate items of the specified kind ('movie', 'episode', or 'live').
    /// Applies active profile restrictions (kids whitelist, adult groups hidden if parental lock active,
    /// hidden groups, hidden items, broken items).
    pub fn load_recommendation_candidates(
        &self,
        kind: &str,
        category: Option<&str>,
        limit: u32,
    ) -> Result<Vec<IptvItemRecord>, DbError> {
        self.with_conn(|conn| {
            let sql = format!(
                "SELECT {ITEM_SELECT_COLUMNS}
                 FROM iptv_items i
                 LEFT JOIN favorites f ON f.profile_id = (SELECT id FROM temp.active_profile) AND f.stable_hash = i.stable_hash
                 LEFT JOIN broken_flags bf ON bf.stable_hash = i.stable_hash
                 LEFT JOIN watch_history wh ON wh.profile_id = (SELECT id FROM temp.active_profile) AND wh.stable_hash = i.stable_hash
                 WHERE i.kind = ?1
                   AND (bf.stable_hash IS NULL)
                   AND (?2 IS NULL OR i.group_title = ?2)
                   AND NOT EXISTS (
                       SELECT 1 FROM hidden_items hi
                       WHERE hi.profile_id = (SELECT id FROM temp.active_profile)
                         AND hi.stable_hash = i.stable_hash
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM hidden_groups hg
                       WHERE hg.profile_id = (SELECT id FROM temp.active_profile)
                         AND hg.kind = i.kind
                         AND hg.group_key = COALESCE(i.group_title, '')
                   )
                   AND (
                       NOT EXISTS (
                           SELECT 1 FROM hidden_groups tx
                           WHERE tx.profile_id = (SELECT id FROM temp.active_profile)
                             AND (tx.kind LIKE 'country:%' OR tx.kind LIKE 'theme:%')
                       )
                       OR NOT EXISTS (
                           SELECT 1 FROM hidden_groups tg
                           WHERE tg.profile_id = (SELECT id FROM temp.active_profile)
                             AND ((tg.kind = 'country:' || i.kind AND tg.group_key = LOWER(COALESCE(i.tx_country, '')))
                                  OR (tg.kind = 'theme:' || i.kind AND tg.group_key = LOWER(COALESCE(i.tx_theme, ''))))
                       )
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM recommendation_dismissals rd
                       WHERE rd.profile_id = (SELECT id FROM temp.active_profile)
                         AND rd.stable_hash = i.stable_hash
                   )
                   AND (
                       (SELECT kids_only FROM temp.catalog_flags) = 0
                       OR EXISTS (
                           SELECT 1 FROM kids_allowed_groups kag
                           WHERE kag.profile_id = (SELECT id FROM temp.active_profile)
                             AND (
                                 (kag.kind = i.kind AND kag.group_key = COALESCE(i.group_title, ''))
                                 OR (kag.kind = 'theme:' || i.kind AND kag.group_key = LOWER(COALESCE(i.tx_theme, '')) AND COALESCE(i.tx_theme, '') != 'adult')
                             )
                       )
                   )
                   AND (
                       (SELECT show_adult FROM temp.catalog_flags) = 1
                       OR NOT is_adult_group(i.group_title)
                   )
                 ORDER BY i.imdb_rating DESC
                 LIMIT ?3"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![kind, category, limit], map_item_row)?;
            let mut items = Vec::new();
            for r in rows {
                items.push(r?);
            }
            Ok(items)
        })
    }

    /// Loads items in-progress (resume candidates) for active profile.
    pub fn load_in_progress_candidates(&self, kind: &str, limit: u32) -> Result<Vec<IptvItemRecord>, DbError> {
        self.with_conn(|conn| {
            let sql = format!(
                "SELECT {ITEM_SELECT_COLUMNS}
                 FROM watch_history wh
                 INNER JOIN iptv_items i ON i.stable_hash = wh.stable_hash
                 LEFT JOIN favorites f ON f.profile_id = (SELECT id FROM temp.active_profile) AND f.stable_hash = i.stable_hash
                 LEFT JOIN broken_flags bf ON bf.stable_hash = i.stable_hash
                 WHERE wh.profile_id = (SELECT id FROM temp.active_profile)
                   AND i.kind = ?1
                   AND (bf.stable_hash IS NULL)
                   AND wh.completed = 0
                   AND wh.position_ms > 0
                   AND wh.duration_ms > 0
                   AND wh.position_ms < (wh.duration_ms * 9 / 10)
                   AND NOT EXISTS (
                       SELECT 1 FROM recommendation_dismissals rd
                       WHERE rd.profile_id = (SELECT id FROM temp.active_profile)
                         AND rd.stable_hash = i.stable_hash
                   )
                 ORDER BY wh.last_watched_at DESC
                 LIMIT ?2"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![kind, limit], map_item_row)?;
            let mut items = Vec::new();
            for r in rows {
                items.push(r?);
            }
            Ok(items)
        })
    }

    /// Loads cached metadata from `media_metadata_cache` by imdb_id or cleaned title.
    pub fn get_metadata_for_item(&self, item: &IptvItemRecord) -> Result<Option<CachedItemMetadata>, DbError> {
        self.with_conn(|conn| {
            use rusqlite::OptionalExtension;
            let mut stmt = conn.prepare_cached(
                "SELECT genres, director, actors, plot, imdb_rating
                 FROM media_metadata_cache
                 WHERE (?1 IS NOT NULL AND imdb_id = ?1)
                    OR lookup_key = ?2
                    OR lookup_key = ?3
                 LIMIT 1",
            )?;

            let key_with_year = match item.year {
                Some(y) => format!("{}:{}:{}", item.kind, item.name_clean.trim().to_lowercase(), y),
                None => format!("{}:{}", item.kind, item.name_clean.trim().to_lowercase()),
            };
            let key_simple = format!("{}:{}", item.kind, item.name_clean.trim().to_lowercase());

            let row = stmt
                .query_row(
                    params![item.imdb_id, key_with_year, key_simple],
                    |r| {
                        Ok(CachedItemMetadata {
                            genres: r.get(0)?,
                            director: r.get(1)?,
                            actors: r.get(2)?,
                            plot: r.get(3)?,
                            imdb_rating: r.get(4)?,
                        })
                    },
                )
                .optional()?;
            Ok(row)
        })
    }
}

#[derive(Debug, Clone)]
pub struct CachedItemMetadata {
    pub genres: Option<String>,
    pub director: Option<String>,
    pub actors: Option<String>,
    pub plot: Option<String>,
    pub imdb_rating: Option<f64>,
}
