pub mod catalog_repo;
pub mod download_repo;
pub mod epg_repo;
pub mod ingestion_writer;
pub mod library_repo;
pub mod profile_repo;
pub mod recording_repo;
pub mod recommend_repo;
pub mod schema;
pub mod settings_repo;
pub mod sync_repo;
pub mod taxonomy_repo;

pub use profile_repo::{is_adult_category, FavoriteFolderRecord, ProfileRecord};
pub use recording_repo::{NewRecordingRequest, RecordingRecord, SeriesRuleRecord};
pub use recommend_repo::{CachedItemMetadata, SagaMemberRecord, SagaRecord};

use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum DbError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("Database lock error: {0}")]
    Lock(String),
    #[error("Record not found: {0}")]
    NotFound(String),
    #[error("Serialization error: {0}")]
    Serialization(String),
    #[error("Query error: {0}")]
    Query(String),
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct DbStats {
    pub total_sources: u32,
    pub total_live_channels: u32,
    pub total_movies: u32,
    pub total_series: u32,
    pub total_episodes: u32,
    pub total_favorites: u32,
    pub total_broken: u32,
    pub total_downloads: u32,
    pub db_size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct StreamHealthRecord {
    pub stable_hash: String,
    pub status: String,
    pub latency_ms: Option<u32>,
    pub checked_at: i64,
    pub consecutive_failures: u32,
    pub last_ok_at: Option<i64>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct IptvSourceRecord {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub source_type: String,
    pub url: Option<String>,
    pub server: Option<String>,
    pub username: Option<String>,
    pub password_ref: Option<String>,
    pub epg_url: Option<String>,
    pub exp_date: Option<i64>,
    pub max_connections: Option<u32>,
    pub active_cons: Option<u32>,
    pub account_status: Option<String>,
    pub item_count: u32,
    pub updated_at: i64,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct IptvItemRecord {
    pub id: i64,
    pub source_id: i64,
    pub series_id: Option<i64>,
    pub stable_hash: String,
    pub kind: String,
    pub season_num: Option<u32>,
    pub episode_num: Option<u32>,
    pub name: String,
    pub name_clean: String,
    pub year: Option<u32>,
    pub group_title: Option<String>,
    pub tvg_id: Option<String>,
    pub logo_url: Option<String>,
    pub chno: Option<u32>,
    pub host: String,
    pub container_ext: Option<String>,
    pub stream_url: String,
    pub user_agent: Option<String>,
    pub referer: Option<String>,
    pub headers_json: Option<String>,
    pub catchup_type: Option<String>,
    pub catchup_source: Option<String>,
    pub catchup_days: Option<u32>,
    pub plot: Option<String>,
    pub imdb_id: Option<String>,
    pub imdb_rating: Option<f64>,
    pub is_favorite: bool,
    pub is_broken: bool,
    pub resume_position_ms: i64,
    pub duration_ms: i64,
    pub is_completed: bool,
}

pub struct DatabaseManager {
    conn: Mutex<Connection>,
    db_path: String,
}

impl DatabaseManager {
    pub fn open<P: AsRef<Path>>(path: P) -> std::result::Result<Self, DbError> {
        let path_str = path.as_ref().to_string_lossy().to_string();
        let conn = Connection::open(&path)?;
        
        // Execute PRAGMAs and Schema creation
        conn.execute_batch(schema::INIT_SQL)?;
        Self::apply_incremental_migrations(&conn)?;
        Self::init_active_profile(&conn)?;

        Ok(Self {
            conn: Mutex::new(conn),
            db_path: path_str,
        })
    }

    pub fn open_in_memory() -> std::result::Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(schema::INIT_SQL)?;
        Self::apply_incremental_migrations(&conn)?;
        Self::init_active_profile(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            db_path: ":memory:".to_string(),
        })
    }

    /// The profile whose hidden lists and custom order the catalog queries apply. It lives in a
    /// connection-local table so the SQL can read it (`(SELECT id FROM temp.active_profile)`) without
    /// threading a parameter through every query. Profile 1 ("Principal") is the default.
    fn init_active_profile(conn: &Connection) -> std::result::Result<(), DbError> {
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS active_profile (id INTEGER NOT NULL);
             DELETE FROM temp.active_profile;
             INSERT INTO temp.active_profile (id) VALUES (1);
             CREATE TEMP TABLE IF NOT EXISTS catalog_flags (show_adult INTEGER NOT NULL, kids_only INTEGER NOT NULL DEFAULT 0);
             DELETE FROM temp.catalog_flags;
             INSERT INTO temp.catalog_flags (show_adult, kids_only) VALUES (0, 0);",
        )?;
        // The same word-boundary keyword rule as `is_adult_category`, callable from the catalog SQL.
        conn.create_scalar_function(
            "is_adult_group",
            1,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8 | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let group: Option<String> = ctx.get(0)?;
                Ok(group.is_some_and(|g| profile_repo::is_adult_category(&g)))
            },
        )?;
        Ok(())
    }

    /// Whether categories recognised as adult (keywords on the name) appear in the catalog. Off by
    /// default; the app turns it on for a session once the parental PIN has been entered.
    pub fn set_adult_content_visible(&self, visible: bool) -> std::result::Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute("UPDATE temp.catalog_flags SET show_adult = ?1", params![visible as i64])?;
            Ok(())
        })
    }

    /// The source of an item and its subscription's connection limit, when the provider reported one.
    pub(crate) fn source_limit_for_item(&self, item_id: i64) -> Option<(i64, Option<u32>)> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT s.id, s.max_connections FROM iptv_items i
                 JOIN iptv_sources s ON s.id = i.source_id WHERE i.id = ?1",
            )?;
            let mut rows = stmt.query(params![item_id])?;
            if let Some(r) = rows.next()? {
                let id: i64 = r.get(0)?;
                let max: Option<i64> = r.get(1)?;
                return Ok(Some((id, max.map(|m| m.max(0) as u32))));
            }
            Ok(None)
        })
        .ok()
        .flatten()
    }

    /// The sync identity of the active profile: `None` for the main profile (it keeps the original
    /// record names), its `uuid` for any other.
    fn active_profile_uuid(conn: &Connection) -> Option<String> {
        conn.query_row(
            "SELECT CASE WHEN a.id = 1 THEN NULL ELSE (SELECT uuid FROM profiles WHERE id = a.id) END FROM temp.active_profile a",
            [],
            |r| r.get::<_, Option<String>>(0),
        )
        .unwrap_or(None)
    }

    pub fn set_active_profile(&self, id: i64) -> std::result::Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute("UPDATE temp.active_profile SET id = ?1", params![id])?;
            Ok(())
        })?;
        self.refresh_kids_flag()
    }

    /// A Kids profile only sees the categories it was explicitly given, and never adult ones. The flag is
    /// derived from the active profile, so it follows a profile switch or an edit of the profile.
    pub(crate) fn refresh_kids_flag(&self) -> std::result::Result<(), DbError> {
        self.with_conn(|conn| {
            let kids: i64 = conn
                .query_row(
                    "SELECT COALESCE((SELECT is_kids FROM profiles WHERE id = (SELECT id FROM temp.active_profile)), 0)",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            conn.execute("UPDATE temp.catalog_flags SET kids_only = ?1", params![kids])?;
            Ok(())
        })
    }

    fn apply_incremental_migrations(conn: &Connection) -> std::result::Result<(), DbError> {
        let alter_stmts = [
            "ALTER TABLE iptv_sources ADD COLUMN exp_date INTEGER",
            "ALTER TABLE iptv_sources ADD COLUMN max_connections INTEGER",
            "ALTER TABLE iptv_sources ADD COLUMN server_utc_offset_secs INTEGER",
            "ALTER TABLE profiles ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0",
            "UPDATE profiles SET updated_at = created_at WHERE updated_at = 0",
            "ALTER TABLE iptv_sources ADD COLUMN active_cons INTEGER",
            "ALTER TABLE iptv_sources ADD COLUMN account_status TEXT",
            "ALTER TABLE series_catalog ADD COLUMN stable_hash TEXT NOT NULL DEFAULT ''",
            "CREATE INDEX IF NOT EXISTS idx_series_stable_hash ON series_catalog(stable_hash)",
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_epg_unique_slot ON epg_programs(channel_tvg_id, start_ts)",
            "ALTER TABLE download_tasks ADD COLUMN headers_json TEXT",
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_download_parts_task_idx ON download_parts(task_id, part_index)",
            "CREATE INDEX IF NOT EXISTS idx_download_logs_task ON download_logs(task_id, id DESC)",
            "ALTER TABLE broken_flags ADD COLUMN origin TEXT NOT NULL DEFAULT 'user'",
            "CREATE TABLE IF NOT EXISTS stream_health (
                stable_hash TEXT PRIMARY KEY,
                status TEXT NOT NULL,
                latency_ms INTEGER,
                checked_at INTEGER NOT NULL,
                consecutive_failures INTEGER NOT NULL DEFAULT 0,
                last_ok_at INTEGER
            )",
            "CREATE INDEX IF NOT EXISTS idx_stream_health_checked ON stream_health(checked_at)",
            "CREATE TABLE IF NOT EXISTS profiles (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                uuid TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                avatar TEXT NOT NULL DEFAULT 'person.crop.circle',
                is_kids INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            )",
            "INSERT OR IGNORE INTO profiles(id, uuid, name, avatar, is_kids, created_at) VALUES (1, '00000000-0000-0000-0000-000000000001', 'Principal', 'person.crop.circle', 0, 1700000000)",
            "CREATE TABLE IF NOT EXISTS kids_allowed_groups (
                profile_id INTEGER NOT NULL,
                kind TEXT NOT NULL,
                group_key TEXT NOT NULL,
                PRIMARY KEY(profile_id, kind, group_key)
            )",
            "CREATE TABLE IF NOT EXISTS hidden_groups (
                profile_id INTEGER NOT NULL DEFAULT 1,
                kind TEXT NOT NULL,
                group_key TEXT NOT NULL,
                PRIMARY KEY(profile_id, kind, group_key)
            )",
            "CREATE TABLE IF NOT EXISTS hidden_items (
                profile_id INTEGER NOT NULL DEFAULT 1,
                stable_hash TEXT NOT NULL,
                PRIMARY KEY(profile_id, stable_hash)
            )",
            "CREATE TABLE IF NOT EXISTS custom_order (
                profile_id INTEGER NOT NULL DEFAULT 1,
                scope TEXT NOT NULL,
                key TEXT NOT NULL,
                position INTEGER NOT NULL,
                PRIMARY KEY(profile_id, scope, key)
            )",
            "CREATE TABLE IF NOT EXISTS favorite_folders (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                profile_id INTEGER NOT NULL DEFAULT 1,
                name TEXT NOT NULL,
                position INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            )",
            "CREATE TABLE IF NOT EXISTS favorite_folder_items (
                folder_id INTEGER NOT NULL,
                stable_hash TEXT NOT NULL,
                position INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(folder_id, stable_hash),
                FOREIGN KEY(folder_id) REFERENCES favorite_folders(id) ON DELETE CASCADE
            )",
            "ALTER TABLE favorites ADD COLUMN profile_id INTEGER NOT NULL DEFAULT 1",
            "ALTER TABLE watch_history ADD COLUMN profile_id INTEGER NOT NULL DEFAULT 1",
            "CREATE TABLE IF NOT EXISTS sagas (
                saga_key TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                level TEXT NOT NULL DEFAULT 'saga',
                source TEXT NOT NULL DEFAULT 'local',
                confidence REAL NOT NULL DEFAULT 1.0,
                fetched_at INTEGER NOT NULL,
                missing_titles TEXT
            )",
            "CREATE TABLE IF NOT EXISTS saga_members (
                saga_key TEXT NOT NULL,
                work_key TEXT NOT NULL,
                release_index INTEGER NOT NULL,
                story_index INTEGER,
                PRIMARY KEY (saga_key, work_key),
                FOREIGN KEY (saga_key) REFERENCES sagas(saga_key) ON DELETE CASCADE
            )",
            "CREATE INDEX IF NOT EXISTS idx_saga_members_work ON saga_members(work_key)",
            "CREATE TABLE IF NOT EXISTS recommendation_dismissals (
                profile_id INTEGER NOT NULL DEFAULT 1,
                stable_hash TEXT NOT NULL,
                dismissed_at INTEGER NOT NULL,
                PRIMARY KEY(profile_id, stable_hash)
            )",
            "CREATE TABLE IF NOT EXISTS library_folders (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                display_name TEXT NOT NULL,
                path TEXT NOT NULL,
                bookmark BLOB,
                include_subfolders INTEGER NOT NULL DEFAULT 1,
                kinds TEXT NOT NULL DEFAULT 'video,audio,image',
                state TEXT NOT NULL DEFAULT 'ok',
                item_count INTEGER NOT NULL DEFAULT 0,
                last_scan_at INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            )",
            "CREATE TABLE IF NOT EXISTS library_items (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                folder_id INTEGER NOT NULL,
                rel_path TEXT NOT NULL,
                kind TEXT NOT NULL,
                size INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                fingerprint TEXT NOT NULL,
                title TEXT,
                duration_ms INTEGER,
                width INTEGER,
                height INTEGER,
                codec TEXT,
                artist TEXT,
                album TEXT,
                album_artist TEXT,
                track_no INTEGER,
                disc_no INTEGER,
                genre TEXT,
                year INTEGER,
                taken_at INTEGER,
                thumbnail_path TEXT,
                subtitles_json TEXT,
                metadata_state TEXT NOT NULL DEFAULT 'pending',
                added_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                FOREIGN KEY(folder_id) REFERENCES library_folders(id) ON DELETE CASCADE,
                UNIQUE(folder_id, rel_path)
            )",
            "CREATE INDEX IF NOT EXISTS idx_library_items_folder ON library_items(folder_id)",
            "CREATE INDEX IF NOT EXISTS idx_library_items_kind ON library_items(kind)",
            "CREATE INDEX IF NOT EXISTS idx_library_items_fingerprint ON library_items(fingerprint)",
            "CREATE INDEX IF NOT EXISTS idx_library_items_meta_state ON library_items(metadata_state)",
            "CREATE INDEX IF NOT EXISTS idx_library_items_artist ON library_items(artist)",
            "CREATE INDEX IF NOT EXISTS idx_library_items_album ON library_items(album)",
            "CREATE VIRTUAL TABLE IF NOT EXISTS library_items_fts USING fts5(
                title,
                artist,
                album,
                rel_path,
                content='library_items',
                content_rowid='id'
            )",
            "CREATE TRIGGER IF NOT EXISTS trg_library_items_ai AFTER INSERT ON library_items BEGIN
                INSERT INTO library_items_fts(rowid, title, artist, album, rel_path)
                VALUES (new.id, new.title, new.artist, new.album, new.rel_path);
            END",
            "CREATE TRIGGER IF NOT EXISTS trg_library_items_ad AFTER DELETE ON library_items BEGIN
                INSERT INTO library_items_fts(library_items_fts, rowid, title, artist, album, rel_path)
                VALUES ('delete', old.id, old.title, old.artist, old.album, old.rel_path);
            END",
            "CREATE TRIGGER IF NOT EXISTS trg_library_items_au AFTER UPDATE ON library_items BEGIN
                INSERT INTO library_items_fts(library_items_fts, rowid, title, artist, album, rel_path)
                VALUES ('delete', old.id, old.title, old.artist, old.album, old.rel_path);
                INSERT INTO library_items_fts(rowid, title, artist, album, rel_path)
                VALUES (new.id, new.title, new.artist, new.album, new.rel_path);
            END",
            "CREATE TABLE IF NOT EXISTS library_progress (
                profile_id INTEGER NOT NULL DEFAULT 1,
                fingerprint TEXT NOT NULL,
                position_ms INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL,
                completed INTEGER NOT NULL DEFAULT 0,
                last_played_at INTEGER NOT NULL,
                PRIMARY KEY(profile_id, fingerprint)
            )",
            "CREATE TABLE IF NOT EXISTS library_playlists (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )",
            "CREATE TABLE IF NOT EXISTS library_playlist_items (
                playlist_id INTEGER NOT NULL,
                item_id INTEGER NOT NULL,
                position INTEGER NOT NULL,
                PRIMARY KEY(playlist_id, item_id),
                FOREIGN KEY(playlist_id) REFERENCES library_playlists(id) ON DELETE CASCADE,
                FOREIGN KEY(item_id) REFERENCES library_items(id) ON DELETE CASCADE
            )",
            // PRD-33: normalised categories (country + themes of a closed list).
            "ALTER TABLE iptv_items ADD COLUMN tx_country TEXT",
            "ALTER TABLE iptv_items ADD COLUMN tx_country_src TEXT",
            "ALTER TABLE iptv_items ADD COLUMN tx_theme TEXT",
            "ALTER TABLE series_catalog ADD COLUMN tx_country TEXT",
            "ALTER TABLE series_catalog ADD COLUMN tx_theme TEXT",
            "CREATE TABLE IF NOT EXISTS item_themes (
                item_id INTEGER NOT NULL REFERENCES iptv_items(id) ON DELETE CASCADE,
                theme TEXT NOT NULL,
                subtheme TEXT,
                src TEXT NOT NULL,
                PRIMARY KEY (item_id, theme)
            ) WITHOUT ROWID",
            "CREATE TABLE IF NOT EXISTS taxonomy_overrides (
                stable_hash TEXT PRIMARY KEY,
                country TEXT,
                themes TEXT,
                updated_at INTEGER NOT NULL
            )",
            "CREATE INDEX IF NOT EXISTS idx_items_kind_country_theme ON iptv_items(kind, tx_country, tx_theme, name)",
            "CREATE INDEX IF NOT EXISTS idx_item_themes_theme ON item_themes(theme, item_id)",
            "CREATE INDEX IF NOT EXISTS idx_series_country_theme ON series_catalog(tx_country, tx_theme)",
            // PRD-33 lot 33.4: words of the country and themes, indexed for search; rules per provider category.
            "ALTER TABLE iptv_items ADD COLUMN tx_terms TEXT",
            // PRD-33 lot 33.5: genre of a series (sub-theme of its main theme).
            "ALTER TABLE series_catalog ADD COLUMN tx_subtheme TEXT",
            "CREATE TABLE IF NOT EXISTS taxonomy_group_overrides (
                group_key TEXT PRIMARY KEY,
                country TEXT,
                themes TEXT,
                updated_at INTEGER NOT NULL
            )",
        ];
        for sql in alter_stmts {
            // Ignore duplicate column error on already-migrated databases
            let _ = conn.execute(sql, []);
        }
        Self::scope_user_data_to_profiles(conn)?;
        Self::upgrade_search_index(conn)?;
        Ok(())
    }

    /// PRD-33 lot 33.4: the full-text index also covers the words of a channel's country and themes
    /// (`tx_terms`). An FTS5 table cannot gain a column, so an index without it is rebuilt once.
    fn upgrade_search_index(conn: &Connection) -> std::result::Result<(), DbError> {
        let has_terms: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('iptv_items_fts') WHERE name = 'tx_terms'",
            [],
            |r| r.get(0),
        )?;
        if has_terms > 0 {
            return Ok(());
        }
        conn.execute_batch(
            "BEGIN;
             DROP TRIGGER IF EXISTS trg_items_ai;
             DROP TRIGGER IF EXISTS trg_items_ad;
             DROP TRIGGER IF EXISTS trg_items_au;
             DROP TABLE IF EXISTS iptv_items_fts;
             CREATE VIRTUAL TABLE iptv_items_fts USING fts5(
                 name, group_title, tvg_name, tx_terms,
                 content='iptv_items', content_rowid='id',
                 tokenize='unicode61 remove_diacritics 2'
             );
             CREATE TRIGGER trg_items_ai AFTER INSERT ON iptv_items BEGIN
                 INSERT INTO iptv_items_fts(rowid, name, group_title, tvg_name, tx_terms)
                 VALUES (new.id, new.name, new.group_title, new.tvg_name, new.tx_terms);
             END;
             CREATE TRIGGER trg_items_ad AFTER DELETE ON iptv_items BEGIN
                 INSERT INTO iptv_items_fts(iptv_items_fts, rowid, name, group_title, tvg_name, tx_terms)
                 VALUES ('delete', old.id, old.name, old.group_title, old.tvg_name, old.tx_terms);
             END;
             CREATE TRIGGER trg_items_au AFTER UPDATE ON iptv_items BEGIN
                 INSERT INTO iptv_items_fts(iptv_items_fts, rowid, name, group_title, tvg_name, tx_terms)
                 VALUES ('delete', old.id, old.name, old.group_title, old.tvg_name, old.tx_terms);
                 INSERT INTO iptv_items_fts(rowid, name, group_title, tvg_name, tx_terms)
                 VALUES (new.id, new.name, new.group_title, new.tvg_name, new.tx_terms);
             END;
             INSERT INTO iptv_items_fts(iptv_items_fts) VALUES ('rebuild');
             COMMIT;",
        )?;
        Ok(())
    }

    /// Favorites and watch history used a primary key on `stable_hash` alone, so two profiles could not
    /// each have the same channel as a favorite. Rebuilds both tables with `(profile_id, stable_hash)`,
    /// keeping every existing row (they all belong to profile 1). Does nothing once migrated.
    fn scope_user_data_to_profiles(conn: &Connection) -> std::result::Result<(), DbError> {
        let key_columns = |table: &str| -> std::result::Result<Vec<String>, DbError> {
            let mut stmt = conn.prepare(&format!("SELECT name FROM pragma_table_info('{table}') WHERE pk > 0 ORDER BY pk"))?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        };
        if key_columns("favorites")? != ["profile_id", "stable_hash"] {
            conn.execute_batch(
                "BEGIN;
                 CREATE TABLE favorites_scoped (
                     profile_id INTEGER NOT NULL DEFAULT 1,
                     stable_hash TEXT NOT NULL,
                     item_kind TEXT NOT NULL,
                     created_at INTEGER NOT NULL,
                     PRIMARY KEY(profile_id, stable_hash)
                 );
                 INSERT INTO favorites_scoped (profile_id, stable_hash, item_kind, created_at)
                     SELECT profile_id, stable_hash, item_kind, created_at FROM favorites;
                 DROP TABLE favorites;
                 ALTER TABLE favorites_scoped RENAME TO favorites;
                 COMMIT;",
            )?;
        }
        if key_columns("watch_history")? != ["profile_id", "stable_hash"] {
            conn.execute_batch(
                "BEGIN;
                 CREATE TABLE watch_history_scoped (
                     profile_id INTEGER NOT NULL DEFAULT 1,
                     stable_hash TEXT NOT NULL,
                     position_ms INTEGER NOT NULL,
                     duration_ms INTEGER NOT NULL,
                     completed INTEGER DEFAULT 0,
                     last_watched_at INTEGER NOT NULL,
                     PRIMARY KEY(profile_id, stable_hash)
                 );
                 INSERT INTO watch_history_scoped (profile_id, stable_hash, position_ms, duration_ms, completed, last_watched_at)
                     SELECT profile_id, stable_hash, position_ms, duration_ms, completed, last_watched_at FROM watch_history;
                 DROP TABLE watch_history;
                 ALTER TABLE watch_history_scoped RENAME TO watch_history;
                 COMMIT;",
            )?;
        }
        Ok(())
    }

    pub(crate) fn with_conn<F, T>(&self, f: F) -> std::result::Result<T, DbError>
    where
        F: FnOnce(&Connection) -> std::result::Result<T, DbError>,
    {
        let guard = self.conn.lock().map_err(|e| DbError::Lock(e.to_string()))?;
        f(&guard)
    }

    pub(crate) fn with_conn_mut<F, T>(&self, f: F) -> std::result::Result<T, DbError>
    where
        F: FnOnce(&mut Connection) -> std::result::Result<T, DbError>,
    {
        let mut guard = self.conn.lock().map_err(|e| DbError::Lock(e.to_string()))?;
        f(&mut guard)
    }

    pub fn get_stats(&self) -> std::result::Result<DbStats, DbError> {
        let conn = self.conn.lock().map_err(|e| DbError::Lock(e.to_string()))?;

        let total_sources: u32 = conn.query_row(
            "SELECT COUNT(*) FROM iptv_sources",
            [],
            |r| r.get(0),
        ).unwrap_or(0);

        let total_live_channels: u32 = conn.query_row(
            "SELECT COUNT(*) FROM iptv_items WHERE kind = 'live'",
            [],
            |r| r.get(0),
        ).unwrap_or(0);

        let total_movies: u32 = conn.query_row(
            "SELECT COUNT(*) FROM iptv_items WHERE kind = 'movie'",
            [],
            |r| r.get(0),
        ).unwrap_or(0);

        let total_series: u32 = conn.query_row(
            "SELECT COUNT(*) FROM series_catalog",
            [],
            |r| r.get(0),
        ).unwrap_or(0);

        let total_episodes: u32 = conn.query_row(
            "SELECT COUNT(*) FROM iptv_items WHERE kind = 'episode'",
            [],
            |r| r.get(0),
        ).unwrap_or(0);

        let total_favorites: u32 = conn.query_row(
            "SELECT COUNT(*) FROM favorites WHERE profile_id = (SELECT id FROM temp.active_profile)",
            [],
            |r| r.get(0),
        ).unwrap_or(0);

        let total_broken: u32 = conn.query_row(
            "SELECT COUNT(*) FROM broken_flags",
            [],
            |r| r.get(0),
        ).unwrap_or(0);

        let total_downloads: u32 = conn.query_row(
            "SELECT COUNT(*) FROM download_tasks",
            [],
            |r| r.get(0),
        ).unwrap_or(0);

        let db_size_bytes = if self.db_path != ":memory:" {
            std::fs::metadata(&self.db_path)
                .map(|m| m.len())
                .unwrap_or(0)
        } else {
            0
        };

        Ok(DbStats {
            total_sources,
            total_live_channels,
            total_movies,
            total_series,
            total_episodes,
            total_favorites,
            total_broken,
            total_downloads,
            db_size_bytes,
        })
    }

    pub fn add_source(&self, name: &str, source_type: &str, url: Option<&str>, server: Option<&str>, username: Option<&str>, password_ref: Option<&str>) -> std::result::Result<i64, DbError> {
        let conn = self.conn.lock().map_err(|e| DbError::Lock(e.to_string()))?;
        let now = chrono_now_ts();
        let uuid = new_source_uuid(name, now);

        conn.execute(
            "INSERT INTO iptv_sources (uuid, name, source_type, url, server, username, password_ref, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            params![uuid, name, source_type, url, server, username, password_ref, now],
        )?;

        Ok(conn.last_insert_rowid())
    }

    pub fn list_sources(&self) -> std::result::Result<Vec<IptvSourceRecord>, DbError> {
        let conn = self.conn.lock().map_err(|e| DbError::Lock(e.to_string()))?;
        let mut stmt = conn.prepare(
            "SELECT id, uuid, name, source_type, url, server, username, password_ref, epg_url,
                    exp_date, max_connections, active_cons, account_status, item_count, updated_at
             FROM iptv_sources ORDER BY updated_at DESC"
        )?;

        let rows = stmt.query_map([], |r| {
            Ok(IptvSourceRecord {
                id: r.get(0)?,
                uuid: r.get(1)?,
                name: r.get(2)?,
                source_type: r.get(3)?,
                url: r.get(4)?,
                server: r.get(5)?,
                username: r.get(6)?,
                password_ref: r.get(7)?,
                epg_url: r.get(8)?,
                exp_date: r.get(9)?,
                max_connections: r.get(10)?,
                active_cons: r.get(11)?,
                account_status: r.get(12)?,
                item_count: r.get(13)?,
                updated_at: r.get(14)?,
            })
        })?;

        let mut list = Vec::new();
        for item in rows {
            list.push(item?);
        }
        Ok(list)
    }

    pub fn toggle_favorite(&self, stable_hash: &str, item_kind: &str) -> std::result::Result<bool, DbError> {
        let conn = self.conn.lock().map_err(|e| DbError::Lock(e.to_string()))?;
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM favorites WHERE profile_id = (SELECT id FROM temp.active_profile) AND stable_hash = ?1)",
            params![stable_hash],
            |r| r.get(0),
        ).unwrap_or(false);

        if exists {
            conn.execute(
                "DELETE FROM favorites WHERE profile_id = (SELECT id FROM temp.active_profile) AND stable_hash = ?1",
                params![stable_hash],
            )?;
            // The tombstone names the profile, so removing a channel from one profile never deletes it
            // from another profile on the other devices.
            let key = sync_repo::favorite_key(Self::active_profile_uuid(&conn).as_deref(), stable_hash);
            sync_repo::upsert_tombstone(&conn, sync_repo::TOMBSTONE_FAVORITE, &key, chrono_now_ts())?;
            Ok(false)
        } else {
            let now = chrono_now_ts();
            conn.execute(
                "INSERT INTO favorites (profile_id, stable_hash, item_kind, created_at)
                 VALUES ((SELECT id FROM temp.active_profile), ?1, ?2, ?3)",
                params![stable_hash, item_kind, now],
            )?;
            let key = sync_repo::favorite_key(Self::active_profile_uuid(&conn).as_deref(), stable_hash);
            conn.execute(
                "DELETE FROM sync_tombstones WHERE kind = ?1 AND key = ?2",
                params![sync_repo::TOMBSTONE_FAVORITE, key],
            )?;
            Ok(true)
        }
    }

    pub fn toggle_broken(&self, stable_hash: &str, item_kind: &str) -> std::result::Result<bool, DbError> {
        let conn = self.conn.lock().map_err(|e| DbError::Lock(e.to_string()))?;
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM broken_flags WHERE stable_hash = ?1)",
            params![stable_hash],
            |r| r.get(0),
        ).unwrap_or(false);

        if exists {
            conn.execute("DELETE FROM broken_flags WHERE stable_hash = ?1", params![stable_hash])?;
            Ok(false)
        } else {
            let now = chrono_now_ts();
            conn.execute(
                "INSERT INTO broken_flags (stable_hash, item_kind, flagged_at, origin) VALUES (?1, ?2, ?3, 'user')",
                params![stable_hash, item_kind, now],
            )?;
            Ok(true)
        }
    }

    pub fn set_broken(&self, stable_hash: &str, item_kind: &str, broken: bool) -> std::result::Result<bool, DbError> {
        self.set_broken_with_origin(stable_hash, item_kind, broken, "user")
    }

    pub fn set_broken_with_origin(
        &self,
        stable_hash: &str,
        item_kind: &str,
        broken: bool,
        origin: &str,
    ) -> std::result::Result<bool, DbError> {
        let conn = self.conn.lock().map_err(|e| DbError::Lock(e.to_string()))?;
        if broken {
            let now = chrono_now_ts();
            conn.execute(
                "INSERT INTO broken_flags (stable_hash, item_kind, flagged_at, origin)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(stable_hash) DO UPDATE SET
                     flagged_at = excluded.flagged_at,
                     origin = CASE WHEN broken_flags.origin = 'user' THEN 'user' ELSE excluded.origin END",
                params![stable_hash, item_kind, now, origin],
            )?;
            Ok(true)
        } else {
            if origin == "auto" {
                conn.execute(
                    "DELETE FROM broken_flags WHERE stable_hash = ?1 AND origin = 'auto'",
                    params![stable_hash],
                )?;
            } else {
                conn.execute("DELETE FROM broken_flags WHERE stable_hash = ?1", params![stable_hash])?;
            }
            Ok(false)
        }
    }

    pub fn is_broken(&self, stable_hash: &str) -> std::result::Result<bool, DbError> {
        let conn = self.conn.lock().map_err(|e| DbError::Lock(e.to_string()))?;
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM broken_flags WHERE stable_hash = ?1)",
            params![stable_hash],
            |r| r.get(0),
        ).unwrap_or(false);
        Ok(exists)
    }

    pub fn save_stream_health(&self, record: &StreamHealthRecord) -> std::result::Result<(), DbError> {
        let conn = self.conn.lock().map_err(|e| DbError::Lock(e.to_string()))?;
        conn.execute(
            "INSERT INTO stream_health (stable_hash, status, latency_ms, checked_at, consecutive_failures, last_ok_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(stable_hash) DO UPDATE SET
                 status = excluded.status,
                 latency_ms = excluded.latency_ms,
                 checked_at = excluded.checked_at,
                 consecutive_failures = excluded.consecutive_failures,
                 last_ok_at = COALESCE(excluded.last_ok_at, stream_health.last_ok_at)",
            params![
                record.stable_hash,
                record.status,
                record.latency_ms,
                record.checked_at,
                record.consecutive_failures,
                record.last_ok_at
            ],
        )?;
        Ok(())
    }

    pub fn get_stream_health(&self, stable_hash: &str) -> std::result::Result<Option<StreamHealthRecord>, DbError> {
        let conn = self.conn.lock().map_err(|e| DbError::Lock(e.to_string()))?;
        let mut stmt = conn.prepare(
            "SELECT stable_hash, status, latency_ms, checked_at, consecutive_failures, last_ok_at
             FROM stream_health WHERE stable_hash = ?1",
        )?;
        let mut rows = stmt.query(params![stable_hash])?;
        if let Some(r) = rows.next()? {
            Ok(Some(StreamHealthRecord {
                stable_hash: r.get(0)?,
                status: r.get(1)?,
                latency_ms: r.get(2)?,
                checked_at: r.get(3)?,
                consecutive_failures: r.get(4)?,
                last_ok_at: r.get(5)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn get_stream_health_batch(&self, hashes: &[String]) -> std::result::Result<Vec<StreamHealthRecord>, DbError> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().map_err(|e| DbError::Lock(e.to_string()))?;
        let placeholders = hashes.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT stable_hash, status, latency_ms, checked_at, consecutive_failures, last_ok_at
             FROM stream_health WHERE stable_hash IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(hashes.iter()), |r| {
            Ok(StreamHealthRecord {
                stable_hash: r.get(0)?,
                status: r.get(1)?,
                latency_ms: r.get(2)?,
                checked_at: r.get(3)?,
                consecutive_failures: r.get(4)?,
                last_ok_at: r.get(5)?,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn query_items(
        &self,
        kind: &str,
        query: &str,
        group: Option<&str>,
        host: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> std::result::Result<Vec<IptvItemRecord>, DbError> {
        self.query_catalog(&catalog_repo::CatalogFilter {
            kind: kind.to_string(),
            source_id: None,
            query: query.to_string(),
            group_title: group.map(|s| s.to_string()),
            host: host.map(|s| s.to_string()),
            favorites_only: false,
            include_broken: false,
            include_hidden: false,
            sort_by: catalog_repo::SortField::Name,
            sort_asc: true,
            limit,
            offset,
        })
    }
}

/// `src_<unix seconds>_<hash>`. The hash mixes the name with a nanosecond clock and a process-wide
/// counter, so two sources with the same name created in the same second never collide, and the
/// uuid stays a safe identity across devices.
fn new_source_uuid(name: &str, now: i64) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let digest = blake3::hash(format!("{name}|{nanos}|{seq}").as_bytes());
    let short: String = digest.to_hex().chars().take(12).collect();
    format!("src_{now}_{short}")
}

pub(crate) fn chrono_now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_db_init_and_stats() {
        let db = DatabaseManager::open_in_memory().expect("open memory db");
        let stats = db.get_stats().expect("get stats");
        assert_eq!(stats.total_sources, 0);
        assert_eq!(stats.total_live_channels, 0);

        let src_id = db.add_source(
            "Test Source",
            "m3u",
            Some("http://example.com/test.m3u"),
            None,
            None,
            None,
        ).expect("add source");

        assert!(src_id > 0);
        let sources = db.list_sources().expect("list sources");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, "Test Source");
    }

    #[test]
    fn test_fts5_triggers_and_query() {
        let db = DatabaseManager::open_in_memory().expect("open memory db");
        let src_id = db.add_source("TV Source", "m3u", None, None, None, None).unwrap();

        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO iptv_items (source_id, stable_hash, kind, name, name_clean, group_title, host, stream_url, created_at, updated_at)
                 VALUES (?1, 'hash_france24', 'live', 'France 24 HD', 'France 24', 'News', 'stream.tv', 'http://stream.tv/f24.m3u8', 100, 100)",
                params![src_id],
            ).unwrap();
        }

        // Test normal list
        let items = db.query_items("live", "", None, None, 10, 0).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "France 24 HD");
        assert!(!items[0].is_favorite);

        // Test toggle favorite
        let is_fav = db.toggle_favorite("hash_france24", "live").unwrap();
        assert!(is_fav);
        let items_fav = db.query_items("live", "", None, None, 10, 0).unwrap();
        assert!(items_fav[0].is_favorite);

        // Test FTS5 search
        let search_res = db.query_items("live", "France", None, None, 10, 0).unwrap();
        assert_eq!(search_res.len(), 1);

        let search_none = db.query_items("live", "Canal", None, None, 10, 0).unwrap();
        assert_eq!(search_none.len(), 0);
    }

    #[test]
    fn old_favorites_and_history_move_to_profile_scoped_tables_without_loss() {
        let conn = Connection::open_in_memory().unwrap();
        // The shape before profiles: primary key on stable_hash alone, plus the late profile_id column.
        conn.execute_batch(
            "CREATE TABLE favorites (stable_hash TEXT PRIMARY KEY, item_kind TEXT NOT NULL, created_at INTEGER NOT NULL);
             CREATE TABLE watch_history (stable_hash TEXT PRIMARY KEY, position_ms INTEGER NOT NULL, duration_ms INTEGER NOT NULL, completed INTEGER DEFAULT 0, last_watched_at INTEGER NOT NULL);
             ALTER TABLE favorites ADD COLUMN profile_id INTEGER NOT NULL DEFAULT 1;
             ALTER TABLE watch_history ADD COLUMN profile_id INTEGER NOT NULL DEFAULT 1;
             INSERT INTO favorites (stable_hash, item_kind, created_at) VALUES ('a', 'live', 10), ('b', 'movie', 20);
             INSERT INTO watch_history (stable_hash, position_ms, duration_ms, completed, last_watched_at) VALUES ('a', 5, 100, 0, 30);",
        )
        .unwrap();

        DatabaseManager::scope_user_data_to_profiles(&conn).unwrap();
        DatabaseManager::scope_user_data_to_profiles(&conn).expect("running it again is harmless");

        let favorites: Vec<(i64, String)> = conn
            .prepare("SELECT profile_id, stable_hash FROM favorites ORDER BY stable_hash").unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect();
        assert_eq!(favorites, vec![(1, "a".to_string()), (1, "b".to_string())], "every old row belongs to profile 1");
        let history: i64 = conn.query_row("SELECT position_ms FROM watch_history WHERE profile_id = 1 AND stable_hash = 'a'", [], |r| r.get(0)).unwrap();
        assert_eq!(history, 5);

        // The same channel can now be a favorite for another profile.
        conn.execute("INSERT INTO favorites (profile_id, stable_hash, item_kind, created_at) VALUES (2, 'a', 'live', 40)", []).unwrap();
        let pk: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('favorites') WHERE pk > 0 ORDER BY pk").unwrap()
            .query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        assert_eq!(pk, vec!["profile_id", "stable_hash"]);
    }

    /// Manual check on a copy of a real database: `CND_TEST_DB=/path/to/copy.db cargo test -- --ignored real_database`.
    #[test]
    #[ignore]
    fn real_database_survives_the_profile_scoping_migration() {
        let Ok(path) = std::env::var("CND_TEST_DB") else { return };
        let count = |p: &str, table: &str| -> i64 {
            Connection::open(p).unwrap().query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
        };
        let (favs, history) = (count(&path, "favorites"), count(&path, "watch_history"));
        let db = DatabaseManager::open(&path).expect("opens and migrates");
        drop(db);
        assert_eq!(count(&path, "favorites"), favs);
        assert_eq!(count(&path, "watch_history"), history);
    }
}

