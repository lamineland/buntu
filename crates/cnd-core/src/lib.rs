pub mod ai;
pub mod db;
pub mod downloader;
pub mod health;
pub mod http_client;
pub mod iptv;
pub mod library;
pub mod metadata;
pub mod recorder;
pub mod recommend;
pub mod sharing;
pub mod subtitles;
pub mod taxonomy;
pub mod telemetry;

pub use recommend::{
    Recommendation, RecommendationContext, RecommendationReasonKind, SagaInfo, SagaOrderPreference,
    SagaPosition,
};

pub use ai::{
    AiAction, AiCommandContext, AiCommandResult, AiProviderConfig, AiProviderKind, AiProviderStatus,
};
pub use sharing::{
    ShareDecision, ShareExport, ShareImportAction, ShareImportResult, ShareImportedSource, SharePreview,
    SharePreviewSource, ShareSourceOption, ShareWarning,
};
pub use subtitles::{SubtitleCue, SubtitleSearchQuery, SubtitleSearchResult};
pub use db::library_repo::{
    LibraryAlbumRecord, LibraryArtistRecord, LibraryBrowseEntry, LibraryFolderRecord,
    LibraryGenreRecord, LibraryItemMetadataInput, LibraryItemRecord, LibraryProgressRecord,
    LibraryScanReport,
};
pub use db::profile_repo::{FavoriteFolderRecord, ProfileRecord};
pub use db::recording_repo::{NewRecordingRequest, RecordingRecord, SeriesRuleRecord};
pub use db::catalog_repo::{CatalogFilter, FacetCount, SeasonSummary, SeriesRecord, SortField};
pub use db::taxonomy_repo::{
    CategoryAxis, CategoryFacet, ItemTaxonomy, TaxonomyCorrection, TaxonomyFilter, TaxonomyOverrideSummary, TaxonomyStatus,
};
pub use db::download_repo::{
    DownloadLogRecord, DownloadPartRecord, DownloadTaskRecord, NewDownloadRequest,
};
pub use db::epg_repo::{ChannelNowNext, EpgProgramRecord};
pub use db::ingestion_writer::SyncSummary;
pub use db::settings_repo::RuntimeSettings;
pub use db::sync_repo::{
    SyncDelta, SyncFavorite, SyncImportSummary, SyncSource, SyncTombstone, SyncWatchProgress,
};
pub use db::StreamHealthRecord;
pub use health::ProbeResultRecord;
pub use iptv::xtream::XtreamAccountInfo;
pub use metadata::MediaMetadata;
pub use telemetry::{LogLevel, NetworkLogEntry, TelemetryHub};

use futures_util::TryStreamExt;
use http_client::HttpClientPool;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Instant;
use thiserror::Error;
use tokio::io::BufReader;
use tokio_util::io::StreamReader;

uniffi::setup_scaffolding!();

#[derive(Error, Debug, uniffi::Error)]
pub enum CndError {
    #[error("Database error: {message}")]
    Database { message: String },
    #[error("Network error: {message}")]
    Network { message: String },
    #[error("Parse error: {message}")]
    Parse { message: String },
    #[error("IO error: {message}")]
    Io { message: String },
    #[error("Internal error: {message}")]
    Internal { message: String },
    #[error("Validation error: {message}")]
    Validation { message: String },
    #[error("Not found: {message}")]
    NotFound { message: String },
}

impl From<db::DbError> for CndError {
    fn from(err: db::DbError) -> Self {
        CndError::Database {
            message: err.to_string(),
        }
    }
}

impl From<std::io::Error> for CndError {
    fn from(err: std::io::Error) -> Self {
        CndError::Io {
            message: err.to_string(),
        }
    }
}

#[derive(uniffi::Object)]
pub struct CndEngine {
    db: Arc<db::DatabaseManager>,
    settings: Arc<RwLock<RuntimeSettings>>,
    telemetry: Arc<TelemetryHub>,
    http_pool: Arc<HttpClientPool>,
    downloads: Arc<downloader::DownloadManager>,
    health_service: Arc<health::HealthService>,
    subtitle_service: Arc<subtitles::SubtitleService>,
    recorder: Arc<recorder::PvrManager>,
    library: Arc<library::LibraryManager>,
    active_profile_id: std::sync::atomic::AtomicI64,
    taxonomy_busy: Arc<std::sync::atomic::AtomicBool>,
}

impl CndEngine {
    fn from_db(db: db::DatabaseManager) -> Result<Arc<Self>, CndError> {
        let initial_settings = db.load_settings()?;
        let settings = Arc::new(RwLock::new(initial_settings));
        let telemetry = Arc::new(TelemetryHub::new());
        let http_pool = Arc::new(HttpClientPool::new(
            Arc::clone(&settings),
            Arc::clone(&telemetry),
        )?);
        let recovered = db.reset_running_tasks_on_startup()?;
        let db = Arc::new(db);
        let downloads = downloader::DownloadManager::new(
            Arc::clone(&db),
            Arc::clone(&settings),
            Arc::clone(&telemetry),
            Arc::clone(&http_pool),
        );
        let health_service = Arc::new(health::HealthService::new(Arc::clone(&db)));
        let subtitle_service = Arc::new(subtitles::SubtitleService::default());
        let default_rec_dir = std::env::temp_dir().join("cnd_recordings");
        let recorder = recorder::PvrManager::new(
            Arc::clone(&db),
            Arc::clone(&http_pool),
            Arc::clone(&telemetry),
            default_rec_dir,
        );
        let library = Arc::new(library::LibraryManager::new());
        telemetry.info(
            "DB",
            "CndEngine initialized (SQLite WAL + FTS5 + HttpClientPool + PVR + Library)",
        );
        if recovered > 0 {
            telemetry.info(
                "DL",
                format!("Recovered {recovered} interrupted download(s) as paused"),
            );
        }

        // PRD-33: catalogues stored before the taxonomy existed, or classified by older rules, are
        // reclassified from their stored columns in the background; the app stays usable meanwhile.
        let taxonomy_busy = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if db.stored_taxonomy_version().map(|v| v < taxonomy::TAXONOMY_VERSION).unwrap_or(false) {
            // Runs on this thread when the catalogue is empty (instant, nothing logged), else in the background.
            if !db.catalog_has_items() {
                let _ = db.ensure_taxonomy_current(&|_, _| {}, &std::sync::atomic::AtomicBool::new(false));
            }
        }
        if db.stored_taxonomy_version().map(|v| v < taxonomy::TAXONOMY_VERSION).unwrap_or(false) {
            taxonomy_busy.store(true, std::sync::atomic::Ordering::Relaxed);
            let (db, telemetry, busy) = (Arc::clone(&db), Arc::clone(&telemetry), Arc::clone(&taxonomy_busy));
            std::thread::spawn(move || {
                let started = Instant::now();
                match db.ensure_taxonomy_current(&|_, _| {}, &std::sync::atomic::AtomicBool::new(false)) {
                    Ok(true) => {
                        telemetry.info(
                            "TAXONOMY",
                            format!("Categories reclassified in {} ms", started.elapsed().as_millis()),
                        );
                    }
                    Ok(false) => {}
                    Err(e) => {
                        telemetry.warn("TAXONOMY", format!("Reclassification failed: {e}"));
                    }
                }
                busy.store(false, std::sync::atomic::Ordering::Relaxed);
            });
        }

        Ok(Arc::new(Self {
            db,
            settings,
            telemetry,
            http_pool,
            downloads,
            health_service,
            subtitle_service,
            recorder,
            library,
            active_profile_id: std::sync::atomic::AtomicI64::new(1),
            taxonomy_busy,
        }))
    }

    pub fn http_pool(&self) -> &Arc<HttpClientPool> {
        &self.http_pool
    }

    pub fn telemetry(&self) -> &Arc<TelemetryHub> {
        &self.telemetry
    }

    pub async fn ingest_m3u_reader<R>(
        &self,
        source_id: i64,
        source_uuid: &str,
        reader: R,
        started: Instant,
    ) -> Result<SyncSummary, CndError>
    where
        R: tokio::io::AsyncBufRead + Unpin,
    {
        let mut session =
            db::ingestion_writer::SourceIngestionSession::new(source_id, source_uuid);
        let header = iptv::m3u_parser::parse_m3u_stream(reader, source_uuid, |batch| {
            self.db.write_m3u_batch(&mut session, batch)?;
            Ok(())
        })
        .await?;

        let duration_ms = started.elapsed().as_millis() as u64;
        let summary = self
            .db
            .finalize_m3u_ingestion(&mut session, &header, duration_ms)?;
        if let Err(e) = self.db.classify_source(source_id) {
            self.telemetry.warn("TAXONOMY", format!("Could not classify source id={source_id}: {e}"));
        }

        self.telemetry.info(
            "M3U",
            format!(
                "Synced source id={source_id}: {} items (live={}, movies={}, series={}, episodes={}) in {} ms",
                summary.total_items,
                summary.live_count,
                summary.movie_count,
                summary.series_count,
                summary.episode_count,
                summary.duration_ms
            ),
        );

        Ok(summary)
    }

    async fn ingest_m3u_from_file(
        &self,
        source_id: i64,
        source_uuid: &str,
        file_path: &Path,
        started: Instant,
    ) -> Result<SyncSummary, CndError> {
        let file = tokio::fs::File::open(file_path).await?;
        let reader = BufReader::with_capacity(64 * 1024, file);
        self.ingest_m3u_reader(source_id, source_uuid, reader, started)
            .await
    }

    async fn ingest_m3u_from_http(
        &self,
        source_id: i64,
        source_uuid: &str,
        url: &str,
        started: Instant,
    ) -> Result<SyncSummary, CndError> {
        let req = self.http_pool.get(url)?;
        let resp = self.http_pool.send_logged("M3U", req).await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(CndError::Network {
                message: format!("M3U HTTP error status {}", status.as_u16()),
            });
        }

        let byte_stream = resp
            .bytes_stream()
            .map_err(std::io::Error::other);
        let stream_reader = StreamReader::new(byte_stream);
        let buf_reader = BufReader::with_capacity(64 * 1024, stream_reader);

        self.ingest_m3u_reader(source_id, source_uuid, buf_reader, started)
            .await
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl CndEngine {
    #[uniffi::constructor]
    pub fn new(db_path: String) -> Result<Arc<Self>, CndError> {
        let db = db::DatabaseManager::open(db_path)?;
        Self::from_db(db)
    }

    #[uniffi::constructor]
    pub fn in_memory() -> Result<Arc<Self>, CndError> {
        let db = db::DatabaseManager::open_in_memory()?;
        Self::from_db(db)
    }

    pub fn version(&self) -> String {
        "1.0.0-native-rust".to_string()
    }

    pub fn get_stats(&self) -> Result<db::DbStats, CndError> {
        Ok(self.db.get_stats()?)
    }

    pub fn get_settings(&self) -> RuntimeSettings {
        self.settings.read().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn update_settings(&self, settings: RuntimeSettings) -> Result<(), CndError> {
        self.db.save_settings(&settings)?;
        let old_settings = {
            let mut guard = self.settings.write().map_err(|e| CndError::Internal {
                message: format!("Settings lock poisoned: {e}"),
            })?;
            let prev = guard.clone();
            *guard = settings.clone();
            prev
        };
        self.http_pool
            .on_settings_updated(&old_settings, &settings)?;
        self.telemetry.info(
            "DB",
            format!(
                "RuntimeSettings updated (segments={}, max_active={})",
                settings.segments_per_file, settings.max_active_files
            ),
        );
        Ok(())
    }

    pub fn get_logs_since(&self, after_seq_id: u64, limit: u32) -> Vec<NetworkLogEntry> {
        self.telemetry.get_logs_since(after_seq_id, limit)
    }

    pub fn clear_logs(&self) {
        self.telemetry.clear_logs();
    }

    pub fn add_source(
        &self,
        name: String,
        source_type: String,
        url: Option<String>,
        server: Option<String>,
        username: Option<String>,
        password_ref: Option<String>,
    ) -> Result<i64, CndError> {
        let id = self.db.add_source(
            &name,
            &source_type,
            url.as_deref(),
            server.as_deref(),
            username.as_deref(),
            password_ref.as_deref(),
        )?;
        self.telemetry.record(
            LogLevel::Info,
            "DB",
            None,
            url.as_deref().or(server.as_deref()),
            None,
            None,
            format!("Added IPTV source '{name}' ({source_type}) -> id={id}"),
        );
        Ok(id)
    }

    pub fn update_source(
        &self,
        source_id: i64,
        name: String,
        url: Option<String>,
        epg_url: Option<String>,
    ) -> Result<(), CndError> {
        self.db
            .update_source(source_id, &name, url.as_deref(), epg_url.as_deref())?;
        self.telemetry.record(
            LogLevel::Info,
            "DB",
            None,
            url.as_deref(),
            None,
            None,
            format!("Updated IPTV source id={source_id} ('{name}')"),
        );
        Ok(())
    }

    pub fn update_xtream_source(
        &self,
        source_id: i64,
        name: String,
        server: String,
        username: String,
        password_ref: Option<String>,
        epg_url: Option<String>,
    ) -> Result<(), CndError> {
        let normalized_srv = iptv::xtream::normalize_server_base(&server);
        self.db.update_xtream_source(
            source_id,
            &name,
            &normalized_srv,
            &username,
            password_ref.as_deref(),
            epg_url.as_deref(),
        )?;
        self.telemetry.record(
            LogLevel::Info,
            "DB",
            None,
            Some(&normalized_srv),
            None,
            None,
            format!("Updated Xtream source id={source_id} ('{name}')"),
        );
        Ok(())
    }

    pub fn delete_source(&self, source_id: i64) -> Result<(), CndError> {
        self.db.delete_source(source_id)?;
        self.telemetry
            .info("DB", format!("Deleted IPTV source id={source_id}"));
        Ok(())
    }

    pub fn list_sources(&self) -> Result<Vec<db::IptvSourceRecord>, CndError> {
        Ok(self.db.list_sources()?)
    }

    pub async fn refresh_source(&self, source_id: i64) -> Result<SyncSummary, CndError> {
        let started = Instant::now();
        let src = self.db.get_source_by_id(source_id)?;
        let raw_url = src.url.ok_or_else(|| CndError::Parse {
            message: format!("Source id={source_id} ('{}') has no M3U URL", src.name),
        })?;

        if raw_url.starts_with("http://") || raw_url.starts_with("https://") {
            self.ingest_m3u_from_http(source_id, &src.uuid, &raw_url, started)
                .await
        } else {
            let local_path = raw_url.strip_prefix("file://").unwrap_or(&raw_url);
            self.ingest_m3u_from_file(source_id, &src.uuid, Path::new(local_path), started)
                .await
        }
    }

    pub async fn import_local_m3u(
        &self,
        name: String,
        file_path: String,
    ) -> Result<SyncSummary, CndError> {
        let started = Instant::now();
        let clean_path = file_path.strip_prefix("file://").unwrap_or(&file_path);
        let path_obj = Path::new(clean_path);
        if !tokio::fs::try_exists(path_obj).await.unwrap_or(false) {
            return Err(CndError::Io {
                message: format!("Local M3U file does not exist: {clean_path}"),
            });
        }

        let source_id = self.db.add_source(
            &name,
            "m3u",
            Some(&format!("file://{clean_path}")),
            None,
            None,
            None,
        )?;
        let src = self.db.get_source_by_id(source_id)?;
        self.ingest_m3u_from_file(source_id, &src.uuid, path_obj, started)
            .await
    }

    pub async fn verify_xtream_account(
        &self,
        server: String,
        username: String,
        password: String,
    ) -> Result<XtreamAccountInfo, CndError> {
        iptv::xtream::verify_xtream_account(&self.http_pool, &server, &username, &password).await
    }

    pub async fn refresh_xtream_source(
        &self,
        source_id: i64,
        password: String,
    ) -> Result<SyncSummary, CndError> {
        let started = Instant::now();
        let src = self.db.get_source_by_id(source_id)?;
        let server = src.server.ok_or_else(|| CndError::Parse {
            message: format!("Source id={source_id} ('{}') missing Xtream server URL", src.name),
        })?;
        let username = src.username.ok_or_else(|| CndError::Parse {
            message: format!("Source id={source_id} ('{}') missing Xtream username", src.name),
        })?;

        match iptv::xtream::probe_xtream_account(&self.http_pool, &server, &username, &password)
            .await?
        {
            iptv::xtream::XtreamAuthProbe::Authenticated(account_info) => {
                iptv::xtream::sync_xtream_native_catalog(
                    &self.db,
                    &self.http_pool,
                    &self.telemetry,
                    source_id,
                    &src.uuid,
                    &server,
                    &username,
                    &password,
                    &account_info,
                    started,
                )
                .await
            }
            iptv::xtream::XtreamAuthProbe::InvalidCredentials(msg) => {
                Err(CndError::Network { message: msg })
            }
            iptv::xtream::XtreamAuthProbe::FallbackM3uPlus(reason) => {
                self.telemetry.warn(
                    "XTREAM",
                    format!(
                        "Source id={source_id}: falling back to M3U+ streaming ({reason})"
                    ),
                );
                let fallback_url =
                    iptv::xtream::build_m3u_plus_fallback_url(&server, &username, &password);
                let default_epg = iptv::xtream::build_xtream_epg_template_url(&server);
                if src.epg_url.as_deref().unwrap_or("").trim().is_empty() {
                    let _ = self
                        .db
                        .update_source(source_id, &src.name, src.url.as_deref(), Some(&default_epg));
                }
                self.ingest_m3u_from_http(source_id, &src.uuid, &fallback_url, started)
                    .await
            }
        }
    }

    pub async fn ensure_xtream_series_episodes(
        &self,
        series_db_id: i64,
        password: String,
    ) -> Result<u32, CndError> {
        iptv::xtream::ensure_xtream_series_episodes(
            &self.db,
            &self.http_pool,
            &self.telemetry,
            series_db_id,
            &password,
        )
        .await
    }

    pub async fn refresh_source_epg(
        &self,
        source_id: i64,
        password: Option<String>,
    ) -> Result<u32, CndError> {
        let started = Instant::now();
        let src = self.db.get_source_by_id(source_id)?;

        let raw_epg_list = match src.epg_url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => s.to_string(),
            None if src.source_type == "xtream" => {
                if let Some(ref srv) = src.server {
                    iptv::xtream::build_xtream_epg_template_url(srv)
                } else {
                    return Ok(0);
                }
            }
            None => return Ok(0),
        };

        let now_ts = db::chrono_now_ts();
        let _ = self.db.purge_expired_epg(now_ts - (14 * 86_400))?;
        let known_channels = self.db.load_known_tvg_channels(None)?;

        let mut total_imported = 0u32;
        for part in raw_epg_list.split(',') {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                continue;
            }

            let resolved_url = if src.source_type == "xtream"
                && trimmed.ends_with("/xmltv.php")
                && !trimmed.contains('?')
            {
                if let (Some(ref u), Some(ref p)) = (&src.username, &password) {
                    let u_enc = iptv::xtream::encode_param(u);
                    let p_enc = iptv::xtream::encode_param(p);
                    format!("{trimmed}?username={u_enc}&password={p_enc}")
                } else {
                    trimmed.to_string()
                }
            } else {
                trimmed.to_string()
            };

            if resolved_url.starts_with("http://") || resolved_url.starts_with("https://") {
                let req = self.http_pool.get(&resolved_url)?;
                let resp = self.http_pool.send_logged("EPG", req).await?;
                if !resp.status().is_success() {
                    self.telemetry.warn(
                        "EPG",
                        format!(
                            "EPG endpoint returned HTTP {} for source id={source_id}",
                            resp.status().as_u16()
                        ),
                    );
                    continue;
                }
                let byte_stream = resp.bytes_stream().map_err(std::io::Error::other);
                let stream_reader = StreamReader::new(byte_stream);
                let buf_reader = BufReader::with_capacity(64 * 1024, stream_reader);
                let count = iptv::epg::parse_xmltv_auto_gzip(
                    buf_reader,
                    Some(&known_channels),
                    now_ts,
                    |batch| {
                        self.db.insert_epg_batch(batch)?;
                        Ok(())
                    },
                )
                .await?;
                total_imported += count;
            } else {
                let clean_path = resolved_url.strip_prefix("file://").unwrap_or(&resolved_url);
                let file = tokio::fs::File::open(clean_path).await?;
                let buf_reader = BufReader::with_capacity(64 * 1024, file);
                let count = iptv::epg::parse_xmltv_auto_gzip(
                    buf_reader,
                    Some(&known_channels),
                    now_ts,
                    |batch| {
                        self.db.insert_epg_batch(batch)?;
                        Ok(())
                    },
                )
                .await?;
                total_imported += count;
            }
        }

        // Channels without a theme take the one their guide agrees on (PRD-33 lot 33.5).
        if total_imported > 0 {
            match self.db.apply_epg_theme_hints() {
                Ok(0) => {}
                Ok(n) => {
                    self.telemetry.info("Categories", format!("{n} channel(s) classified from their guide"));
                }
                Err(e) => {
                    self.telemetry.warn("Categories", format!("EPG theme hints failed: {e}"));
                }
            }
        }

        let elapsed_ms = started.elapsed().as_millis() as u64;
        self.telemetry.info(
            "EPG",
            format!(
                "Synced EPG for source id={source_id}: {total_imported} programmes indexed in {elapsed_ms} ms"
            ),
        );

        Ok(total_imported)
    }

    pub fn get_epg_now_next(&self, tvg_ids: Vec<String>) -> Result<Vec<ChannelNowNext>, CndError> {
        Ok(self.db.get_epg_now_next(&tvg_ids)?)
    }

    pub fn get_channel_epg(
        &self,
        tvg_id: String,
        from_ts: i64,
        to_ts: i64,
    ) -> Result<Vec<EpgProgramRecord>, CndError> {
        Ok(self.db.get_channel_epg(&tvg_id, from_ts, to_ts)?)
    }

    pub fn build_catchup_url(
        &self,
        item_id: i64,
        start_ts: i64,
        end_ts: i64,
    ) -> Result<Option<String>, CndError> {
        let Some(item) = self.db.get_item_by_id(item_id)? else {
            return Ok(None);
        };
        let now_ts = db::chrono_now_ts();
        // Xtream writes replay start times on the server's clock; known from the last sync.
        let server_offset = self
            .db
            .with_conn(|conn| {
                let offset: Option<i64> = conn
                    .query_row(
                        "SELECT server_utc_offset_secs FROM iptv_sources WHERE id = ?1",
                        rusqlite::params![item.source_id],
                        |r| r.get(0),
                    )
                    .unwrap_or(None);
                Ok(offset)
            })
            .ok()
            .flatten()
            .unwrap_or(0);
        match iptv::catchup::build_catchup_url(
            &item.stream_url,
            item.catchup_type.as_deref(),
            item.catchup_source.as_deref(),
            item.catchup_days,
            start_ts,
            end_ts,
            now_ts,
            server_offset,
        ) {
            Ok(url) => Ok(Some(url)),
            Err(e) => {
                tracing::debug!("Catchup URL not generated for item {item_id}: {e}");
                Ok(None)
            }
        }
    }

    pub fn is_catchup_available(
        &self,
        item_id: i64,
        start_ts: i64,
    ) -> Result<bool, CndError> {
        let Some(item) = self.db.get_item_by_id(item_id)? else {
            return Ok(false);
        };
        let now_ts = db::chrono_now_ts();
        Ok(iptv::catchup::is_catchup_available(
            item.catchup_type.as_deref(),
            item.catchup_source.as_deref(),
            item.catchup_days,
            start_ts,
            now_ts,
        ))
    }

    pub fn resolve_catchup_url(
        &self,
        stream_url: String,
        catchup_type: Option<String>,
        catchup_source: Option<String>,
        catchup_days: Option<u32>,
        start_ts: i64,
        end_ts: i64,
    ) -> Result<Option<String>, CndError> {
        let now_ts = db::chrono_now_ts();
        match iptv::catchup::build_catchup_url(
            &stream_url,
            catchup_type.as_deref(),
            catchup_source.as_deref(),
            catchup_days,
            start_ts,
            end_ts,
            now_ts,
            // No item here, so no source clock: prefer `build_catchup_url`, which knows it.
            0,
        ) {
            Ok(url) => Ok(Some(url)),
            Err(e) => {
                tracing::debug!("Catchup URL not generated: {e}");
                Ok(None)
            }
        }
    }

    pub fn toggle_favorite(
        &self,
        stable_hash: String,
        item_kind: String,
    ) -> Result<bool, CndError> {
        Ok(self.db.toggle_favorite(&stable_hash, &item_kind)?)
    }

    pub fn toggle_broken(
        &self,
        stable_hash: String,
        item_kind: String,
    ) -> Result<bool, CndError> {
        Ok(self.db.toggle_broken(&stable_hash, &item_kind)?)
    }

    pub fn set_broken(
        &self,
        stable_hash: String,
        item_kind: String,
        broken: bool,
    ) -> Result<bool, CndError> {
        Ok(self.db.set_broken(&stable_hash, &item_kind, broken)?)
    }

    pub fn is_broken(&self, stable_hash: String) -> Result<bool, CndError> {
        Ok(self.db.is_broken(&stable_hash)?)
    }

    pub fn get_groups(
        &self,
        kind: String,
        source_id: Option<i64>,
    ) -> Result<Vec<FacetCount>, CndError> {
        Ok(self.db.get_groups(&kind, source_id, false)?)
    }

    pub fn get_groups_with_hidden(
        &self,
        kind: String,
        source_id: Option<i64>,
        include_hidden: bool,
    ) -> Result<Vec<FacetCount>, CndError> {
        Ok(self.db.get_groups(&kind, source_id, include_hidden)?)
    }

    /// Entries of the category column for one axis, narrowed by the other axis (PRD-33).
    /// `kind` is `live`, `movie` or `series`.
    pub fn get_category_facets(
        &self,
        kind: String,
        axis: CategoryAxis,
        source_id: Option<i64>,
        filter: TaxonomyFilter,
    ) -> Result<Vec<CategoryFacet>, CndError> {
        Ok(self.db.get_category_facets(&kind, axis, source_id, &filter)?)
    }

    /// `query_catalog` restricted to a country and / or a theme (PRD-33).
    pub fn query_catalog_by_taxonomy(
        &self,
        filter: CatalogFilter,
        taxonomy: TaxonomyFilter,
    ) -> Result<Vec<db::IptvItemRecord>, CndError> {
        Ok(self.db.query_catalog_with(&filter, &taxonomy)?)
    }

    /// `query_series` restricted to a country and / or a main theme (PRD-33).
    pub fn query_series_by_taxonomy(
        &self,
        filter: CatalogFilter,
        taxonomy: TaxonomyFilter,
    ) -> Result<Vec<SeriesRecord>, CndError> {
        Ok(self.db.query_series_with(&filter, &taxonomy)?)
    }

    /// Corrects the country and / or the themes of one item (PRD-33 § 7). `None` keeps the computed value.
    pub fn set_taxonomy_override(
        &self,
        stable_hash: String,
        country: Option<String>,
        themes: Option<Vec<String>>,
    ) -> Result<(), CndError> {
        Ok(self.db.set_taxonomy_override(&stable_hash, country.as_deref(), themes.as_deref())?)
    }

    pub fn clear_taxonomy_override(&self, stable_hash: String) -> Result<(), CndError> {
        Ok(self.db.clear_taxonomy_override(&stable_hash)?)
    }

    /// Classifies every item of a provider category the same way (PRD-33 § 7).
    pub fn set_taxonomy_group_override(
        &self,
        group_title: String,
        country: Option<String>,
        themes: Option<Vec<String>>,
    ) -> Result<(), CndError> {
        Ok(self.db.set_taxonomy_group_override(&group_title, country.as_deref(), themes.as_deref())?)
    }

    pub fn clear_taxonomy_group_override(&self, group_title: String) -> Result<(), CndError> {
        Ok(self.db.clear_taxonomy_group_override(&group_title)?)
    }

    /// Country and themes of an item as stored, and whether a correction or a category rule applies.
    pub fn get_item_taxonomy(&self, stable_hash: String) -> Result<Option<ItemTaxonomy>, CndError> {
        Ok(self.db.get_item_taxonomy(&stable_hash)?)
    }

    pub fn get_taxonomy_override(&self, stable_hash: String) -> Result<Option<TaxonomyCorrection>, CndError> {
        Ok(self.db.get_taxonomy_override(&stable_hash)?)
    }

    pub fn get_taxonomy_group_override(&self, group_title: String) -> Result<Option<TaxonomyCorrection>, CndError> {
        Ok(self.db.get_taxonomy_group_override(&group_title)?)
    }

    pub fn taxonomy_override_summary(&self) -> Result<TaxonomyOverrideSummary, CndError> {
        Ok(self.db.taxonomy_override_summary()?)
    }

    /// Forgets every correction; returns how many were removed.
    pub fn reset_taxonomy_overrides(&self) -> Result<u32, CndError> {
        Ok(self.db.reset_taxonomy_overrides()?)
    }

    pub fn taxonomy_status(&self) -> Result<TaxonomyStatus, CndError> {
        Ok(self.db.taxonomy_status()?)
    }

    /// True while the catalogue is being reclassified after an update of the rules.
    pub fn is_reclassifying_categories(&self) -> bool {
        self.taxonomy_busy.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn get_hosts(
        &self,
        kind: String,
        source_id: Option<i64>,
    ) -> Result<Vec<FacetCount>, CndError> {
        Ok(self.db.get_hosts(&kind, source_id)?)
    }

    pub fn query_catalog(&self, filter: CatalogFilter) -> Result<Vec<db::IptvItemRecord>, CndError> {
        Ok(self.db.query_catalog(&filter)?)
    }

    pub fn query_series(&self, filter: CatalogFilter) -> Result<Vec<SeriesRecord>, CndError> {
        Ok(self.db.query_series(&filter)?)
    }

    pub fn get_series_seasons(&self, series_id: i64) -> Result<Vec<SeasonSummary>, CndError> {
        Ok(self.db.get_series_seasons(series_id)?)
    }

    pub fn get_series_episodes(
        &self,
        series_id: i64,
        season_num: Option<u32>,
    ) -> Result<Vec<db::IptvItemRecord>, CndError> {
        Ok(self.db.get_series_episodes(series_id, season_num)?)
    }

    pub fn get_item_by_id(&self, item_id: i64) -> Result<Option<db::IptvItemRecord>, CndError> {
        Ok(self.db.get_item_by_id(item_id)?)
    }

    pub fn get_series_by_id(&self, series_id: i64) -> Result<Option<SeriesRecord>, CndError> {
        Ok(self.db.get_series_by_id(series_id)?)
    }

    pub fn list_favorite_items(&self, kind: String) -> Result<Vec<db::IptvItemRecord>, CndError> {
        Ok(self.db.list_favorite_items(&kind)?)
    }

    pub fn list_favorite_series(&self) -> Result<Vec<SeriesRecord>, CndError> {
        Ok(self.db.list_favorite_series()?)
    }

    pub fn list_broken_items(&self, kind: String) -> Result<Vec<db::IptvItemRecord>, CndError> {
        Ok(self.db.list_broken_items(&kind)?)
    }

    pub fn list_broken_series(&self) -> Result<Vec<SeriesRecord>, CndError> {
        Ok(self.db.list_broken_series()?)
    }

    pub fn find_mirrors(&self, item_id: i64) -> Result<Vec<db::IptvItemRecord>, CndError> {
        Ok(health::find_mirrors(&self.db, item_id)?)
    }

    pub async fn probe_stream(
        &self,
        url: String,
        headers_json: Option<String>,
        user_agent: Option<String>,
        referer: Option<String>,
    ) -> Result<ProbeResultRecord, CndError> {
        Ok(health::probe_stream(&url, headers_json.as_deref(), user_agent.as_deref(), referer.as_deref()).await)
    }

    pub fn get_stream_health(&self, stable_hash: String) -> Result<Option<StreamHealthRecord>, CndError> {
        Ok(self.db.get_stream_health(&stable_hash)?)
    }

    pub fn get_stream_health_batch(&self, hashes: Vec<String>) -> Result<Vec<StreamHealthRecord>, CndError> {
        Ok(self.db.get_stream_health_batch(&hashes)?)
    }

    pub fn start_health_checks(&self, interval_minutes: Option<u32>) {
        self.health_service.start(interval_minutes);
    }

    pub fn stop_health_checks(&self) {
        self.health_service.stop();
    }

    pub fn is_health_check_running(&self) -> bool {
        self.health_service.is_running()
    }

    pub async fn run_health_check_cycle(&self, throttle: bool) -> Result<u32, CndError> {
        Ok(self.health_service.run_single_cycle(throttle).await?)
    }

    pub fn record_watch_progress(
        &self,
        stable_hash: String,
        position_ms: i64,
        duration_ms: i64,
    ) -> Result<(), CndError> {
        Ok(self
            .db
            .record_watch_progress(&stable_hash, position_ms, duration_ms)?)
    }

    pub fn list_recent_items(
        &self,
        kind: String,
        limit: u32,
    ) -> Result<Vec<db::IptvItemRecord>, CndError> {
        Ok(self.db.list_recent_items(&kind, limit)?)
    }

    pub fn list_recent_series(&self, limit: u32) -> Result<Vec<SeriesRecord>, CndError> {
        Ok(self.db.list_recent_series(limit)?)
    }

    pub async fn fetch_item_metadata(
        &self,
        item_id: i64,
        api_key: String,
        force_refresh: bool,
    ) -> Result<Option<MediaMetadata>, CndError> {
        metadata::fetch_item_metadata(
            &self.db,
            &self.http_pool,
            &self.telemetry,
            item_id,
            &api_key,
            force_refresh,
        )
        .await
    }

    pub async fn fetch_series_metadata(
        &self,
        series_id: i64,
        api_key: String,
        force_refresh: bool,
    ) -> Result<Option<MediaMetadata>, CndError> {
        metadata::fetch_series_metadata(
            &self.db,
            &self.http_pool,
            &self.telemetry,
            series_id,
            &api_key,
            force_refresh,
        )
        .await
    }

    pub async fn enqueue_download(&self, req: NewDownloadRequest) -> Result<i64, CndError> {
        self.downloads.enqueue(req).await
    }

    pub async fn start_download(&self, task_id: i64) -> Result<(), CndError> {
        self.downloads.start(task_id)
    }

    pub async fn pause_download(&self, task_id: i64) -> Result<(), CndError> {
        self.downloads.pause(task_id).await
    }

    pub async fn retry_download(&self, task_id: i64) -> Result<(), CndError> {
        self.downloads.retry(task_id).await
    }

    pub async fn remove_download(&self, task_id: i64, delete_files: bool) -> Result<(), CndError> {
        self.downloads.remove(task_id, delete_files).await
    }

    pub async fn start_all_downloads(&self) -> Result<(), CndError> {
        self.downloads.start_all().await
    }

    pub async fn pause_all_downloads(&self) -> Result<(), CndError> {
        self.downloads.pause_all().await
    }

    pub fn list_downloads(&self, query: Option<String>) -> Result<Vec<DownloadTaskRecord>, CndError> {
        self.downloads.list(query.as_deref())
    }

    pub fn get_download_logs(
        &self,
        task_id: i64,
        limit: u32,
    ) -> Result<Vec<DownloadLogRecord>, CndError> {
        self.downloads.logs(task_id, limit)
    }

    /// Builds the plain share document for the chosen sources (the app encrypts it).
    /// Credentials are only included for sources whose option asks for it.
    pub fn export_share_document(
        &self,
        options: Vec<ShareSourceOption>,
        from: String,
        valid_for_seconds: u32,
    ) -> Result<ShareExport, CndError> {
        sharing::export(self, options, from, valid_for_seconds)
    }

    /// Reads a decrypted share document and tells what it contains and what already exists here.
    pub fn preview_share_document(&self, json: String) -> Result<SharePreview, CndError> {
        sharing::preview(self, &json)
    }

    /// Creates (or replaces) the sources of a share document according to the user's decisions.
    pub fn import_share_document(
        &self,
        json: String,
        decisions: Vec<ShareDecision>,
        imports_dir: String,
    ) -> Result<ShareImportResult, CndError> {
        sharing::import(self, &json, decisions, &imports_dir)
    }

    /// Turns a typed or spoken command into a channel action: local resolution first, then the
    /// user's own model provider, validated against a short candidate list.
    pub async fn ai_resolve_command(
        &self,
        command: String,
        context: AiCommandContext,
        config: AiProviderConfig,
    ) -> Result<AiCommandResult, CndError> {
        ai::resolve(self, command, context, config).await
    }

    /// Checks that the provider answers (key, model, address) and how fast.
    pub async fn ai_test_provider(&self, config: AiProviderConfig) -> Result<AiProviderStatus, CndError> {
        ai::test_provider(self, config).await
    }

    /// Changes made after `since_ts` (Unix seconds) to sources, favorites and watch progress.
    pub fn export_sync_delta(&self, since_ts: i64) -> Result<SyncDelta, CndError> {
        Ok(self.db.export_sync_delta(since_ts)?)
    }

    /// Merges a delta received from another device (Last-Write-Wins).
    pub fn import_sync_delta(&self, delta: SyncDelta) -> Result<SyncImportSummary, CndError> {
        let summary = self.db.import_sync_delta(&delta)?;
        self.telemetry.info(
            "SYNC",
            format!(
                "Imported delta: {} applied, {} skipped",
                summary.applied(),
                summary.skipped
            ),
        );
        Ok(summary)
    }

    pub fn query_items(
        &self,
        kind: String,
        query: String,
        group: Option<String>,
        host: Option<String>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<db::IptvItemRecord>, CndError> {
        Ok(self.db.query_items(
            &kind,
            &query,
            group.as_deref(),
            host.as_deref(),
            limit,
            offset,
        )?)
    }

    pub async fn search_subtitles(
        &self,
        query: subtitles::SubtitleSearchQuery,
        api_key: Option<String>,
    ) -> Result<Vec<subtitles::SubtitleSearchResult>, CndError> {
        self.subtitle_service
            .search(&query, api_key.as_deref())
            .await
    }

    pub async fn download_subtitle(
        &self,
        file_id: i64,
        target_dir: String,
        api_key: Option<String>,
    ) -> Result<String, CndError> {
        let path = self
            .subtitle_service
            .download(file_id, Path::new(&target_dir), api_key.as_deref())
            .await?;
        Ok(path.to_string_lossy().to_string())
    }

    /// Signs in to OpenSubtitles (lifts the anonymous download quota). The session lives in memory.
    pub async fn subtitles_sign_in(
        &self,
        username: String,
        password: String,
        api_key: Option<String>,
    ) -> Result<subtitles::SubtitleAccount, CndError> {
        self.subtitle_service.sign_in(&username, &password, api_key.as_deref()).await
    }

    pub fn subtitles_sign_out(&self) {
        self.subtitle_service.sign_out();
    }

    pub fn subtitles_signed_in(&self) -> bool {
        self.subtitle_service.is_signed_in()
    }

    pub fn parse_srt(&self, srt_content: String) -> Vec<subtitles::SubtitleCue> {
        subtitles::parse_srt(&srt_content)
    }

    /// `moviehash` of a catalog item's stream, for an exact subtitle match. It costs two short requests to
    /// the provider, which count as an extra connection next to the stream being watched: nothing is sent
    /// unless the subscription is known to allow at least two connections, so playback is never at risk.
    /// `None` when skipped or when the server cannot answer range requests.
    pub async fn compute_stream_moviehash(&self, item_id: i64) -> Result<Option<String>, CndError> {
        let Some(item) = self.db.get_item_by_id(item_id)? else {
            return Ok(None);
        };
        if !matches!(self.db.source_limit_for_item(item_id), Some((_, Some(max))) if max >= 2) {
            return Ok(None);
        }
        let headers = health::probe::build_headers(
            item.headers_json.as_deref(),
            item.user_agent.as_deref(),
            item.referer.as_deref(),
        );
        match subtitles::compute_remote_moviehash(&item.stream_url, headers).await {
            Ok(hash) => Ok(Some(hash)),
            Err(e) => {
                tracing::debug!("No moviehash for item {item_id}: {e}");
                Ok(None)
            }
        }
    }

    pub fn compute_file_moviehash(&self, file_path: String) -> Result<String, CndError> {
        subtitles::compute_file_moviehash(&file_path).map_err(|e| CndError::Parse {
            message: e.to_string(),
        })
    }

    // MARK: - PVR Scheduled Recording (PRD-19)

    pub fn schedule_recording(&self, req: NewRecordingRequest) -> Result<i64, CndError> {
        self.recorder.schedule_recording(req)
    }

    pub fn record_now(&self, item_id: i64, duration_minutes: u32) -> Result<i64, CndError> {
        self.recorder.record_now(item_id, duration_minutes)
    }

    pub fn cancel_recording(&self, id: i64) -> Result<bool, CndError> {
        self.recorder.cancel_recording(id)
    }

    pub fn delete_recording(&self, id: i64, delete_file: bool) -> Result<bool, CndError> {
        self.recorder.delete_recording(id, delete_file)
    }

    pub fn list_recordings(&self, state_filter: Option<String>) -> Result<Vec<RecordingRecord>, CndError> {
        self.recorder.list_recordings(state_filter)
    }

    pub fn set_recording_directory(&self, dir: String) {
        self.recorder.set_download_dir(std::path::PathBuf::from(dir));
    }

    pub fn add_series_rule(&self, channel_tvg_id: Option<String>, title: String) -> Result<i64, CndError> {
        self.recorder.add_series_rule(channel_tvg_id, title)
    }

    pub fn remove_series_rule(&self, id: i64) -> Result<bool, CndError> {
        self.recorder.remove_series_rule(id)
    }

    pub fn list_series_rules(&self) -> Result<Vec<SeriesRuleRecord>, CndError> {
        self.recorder.list_series_rules()
    }

    // MARK: - Profiles & Bouquets & Parental Control (PRD-20)

    pub fn active_profile_id(&self) -> i64 {
        self.active_profile_id.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_active_profile_id(&self, id: i64) {
        self.active_profile_id.store(id, std::sync::atomic::Ordering::Relaxed);
        // Hidden groups/items and the custom order of the catalog queries follow the active profile.
        let _ = self.db.set_active_profile(id);
    }

    pub fn list_profiles(&self) -> Result<Vec<ProfileRecord>, CndError> {
        Ok(self.db.list_profiles()?)
    }

    pub fn get_profile(&self, id: i64) -> Result<Option<ProfileRecord>, CndError> {
        Ok(self.db.get_profile(id)?)
    }

    pub fn create_profile(&self, name: String, avatar: String, is_kids: bool) -> Result<ProfileRecord, CndError> {
        Ok(self.db.create_profile(&name, &avatar, is_kids)?)
    }

    pub fn update_profile(&self, id: i64, name: String, avatar: String, is_kids: bool) -> Result<(), CndError> {
        Ok(self.db.update_profile(id, &name, &avatar, is_kids)?)
    }

    pub fn delete_profile(&self, id: i64) -> Result<bool, CndError> {
        Ok(self.db.delete_profile(id)?)
    }

    pub fn hide_group(&self, profile_id: i64, kind: String, group: String) -> Result<(), CndError> {
        Ok(self.db.hide_group(profile_id, &kind, &group)?)
    }

    pub fn unhide_group(&self, profile_id: i64, kind: String, group: String) -> Result<bool, CndError> {
        Ok(self.db.unhide_group(profile_id, &kind, &group)?)
    }

    pub fn list_hidden_groups(&self, profile_id: i64, kind: String) -> Result<Vec<String>, CndError> {
        Ok(self.db.list_hidden_groups(profile_id, &kind)?)
    }

    pub fn hide_item(&self, profile_id: i64, stable_hash: String) -> Result<(), CndError> {
        Ok(self.db.hide_item(profile_id, &stable_hash)?)
    }

    pub fn unhide_item(&self, profile_id: i64, stable_hash: String) -> Result<bool, CndError> {
        Ok(self.db.unhide_item(profile_id, &stable_hash)?)
    }

    pub fn list_hidden_items(&self, profile_id: i64) -> Result<Vec<String>, CndError> {
        Ok(self.db.list_hidden_items(profile_id)?)
    }

    pub fn set_custom_order(&self, profile_id: i64, scope: String, key: String, position: i32) -> Result<(), CndError> {
        Ok(self.db.set_custom_order(profile_id, &scope, &key, position)?)
    }

    pub fn create_favorite_folder(&self, profile_id: i64, name: String, position: i32) -> Result<i64, CndError> {
        Ok(self.db.create_favorite_folder(profile_id, &name, position)?)
    }

    pub fn delete_favorite_folder(&self, id: i64) -> Result<bool, CndError> {
        Ok(self.db.delete_favorite_folder(id)?)
    }

    pub fn list_favorite_folders(&self, profile_id: i64) -> Result<Vec<FavoriteFolderRecord>, CndError> {
        Ok(self.db.list_favorite_folders(profile_id)?)
    }

    pub fn add_to_favorite_folder(&self, folder_id: i64, stable_hash: String, position: i32) -> Result<(), CndError> {
        Ok(self.db.add_to_favorite_folder(folder_id, &stable_hash, position)?)
    }

    pub fn remove_from_favorite_folder(&self, folder_id: i64, stable_hash: String) -> Result<bool, CndError> {
        Ok(self.db.remove_from_favorite_folder(folder_id, &stable_hash)?)
    }

    pub fn list_favorite_folder_items(&self, folder_id: i64) -> Result<Vec<String>, CndError> {
        Ok(self.db.list_favorite_folder_items(folder_id)?)
    }

    /// Shows or hides the categories recognised as adult (hidden by default).
    pub fn set_adult_content_visible(&self, visible: bool) -> Result<(), CndError> {
        Ok(self.db.set_adult_content_visible(visible)?)
    }

    /// Categories a Kids profile may see (everything else is hidden from it).
    pub fn allow_group(&self, profile_id: i64, kind: String, group: String) -> Result<(), CndError> {
        Ok(self.db.allow_group(profile_id, &kind, &group)?)
    }

    pub fn disallow_group(&self, profile_id: i64, kind: String, group: String) -> Result<bool, CndError> {
        Ok(self.db.disallow_group(profile_id, &kind, &group)?)
    }

    pub fn list_allowed_groups(&self, profile_id: i64, kind: String) -> Result<Vec<String>, CndError> {
        Ok(self.db.list_allowed_groups(profile_id, &kind)?)
    }

    pub fn is_adult_category(&self, group: String) -> bool {
        db::profile_repo::is_adult_category(&group)
    }

    // MARK: - Recommendations & Up Next (PRD-27)

    pub fn recommendations_for(
        &self,
        item_id: i64,
        context: RecommendationContext,
        limit: u32,
    ) -> Result<Vec<Recommendation>, CndError> {
        self.recommendations_for_impl(item_id, context, limit)
    }

    pub fn next_up(&self, item_id: i64) -> Result<Option<Recommendation>, CndError> {
        self.next_up_impl(item_id)
    }

    pub fn dismiss_recommendation(&self, stable_hash: String) -> Result<(), CndError> {
        self.dismiss_recommendation_impl(stable_hash)
    }

    pub fn reset_dismissed_recommendations(&self) -> Result<(), CndError> {
        self.reset_dismissed_recommendations_impl()
    }

    pub fn saga_for(&self, item_id: i64) -> Result<Option<SagaInfo>, CndError> {
        self.saga_for_impl(item_id)
    }

    pub async fn resolve_saga_with_ai(&self, item_id: i64, config: AiProviderConfig) -> Result<Option<SagaInfo>, CndError> {
        self.resolve_saga_with_ai_impl(item_id, config).await
    }

    // ------------------------------------------------------------------------
    // Local Media Library (PRD-32)
    // ------------------------------------------------------------------------

    pub fn library_add_folder(
        &self,
        display_name: String,
        path: String,
        bookmark: Option<Vec<u8>>,
        include_subfolders: bool,
        kinds: String,
    ) -> Result<LibraryFolderRecord, CndError> {
        self.db
            .add_library_folder(&display_name, &path, bookmark, include_subfolders, &kinds)
            .map_err(Into::into)
    }

    pub fn library_remove_folder(&self, id: i64) -> Result<(), CndError> {
        self.db.remove_library_folder(id).map_err(Into::into)
    }

    pub fn library_update_folder_state(&self, id: i64, state: String) -> Result<(), CndError> {
        self.db.update_library_folder_state(id, &state).map_err(Into::into)
    }

    pub fn library_list_folders(&self) -> Result<Vec<LibraryFolderRecord>, CndError> {
        self.db.list_library_folders().map_err(Into::into)
    }

    pub fn library_get_folder(&self, id: i64) -> Result<Option<LibraryFolderRecord>, CndError> {
        self.db.get_library_folder(id).map_err(Into::into)
    }

    pub fn library_scan(&self, folder_id: i64, root_path: String) -> Result<LibraryScanReport, CndError> {
        self.library.scan_folder(&self.db, folder_id, &root_path)
    }

    pub fn library_cancel_scan(&self, folder_id: i64) {
        self.library.cancel_scan(folder_id);
    }

    pub fn library_list_items(
        &self,
        kind: Option<String>,
        folder_id: Option<i64>,
        sort_by: Option<String>,
        sort_asc: bool,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<LibraryItemRecord>, CndError> {
        self.db
            .list_library_items(kind, folder_id, sort_by, sort_asc, offset, limit)
            .map_err(Into::into)
    }

    pub fn library_get_item(&self, id: i64) -> Result<Option<LibraryItemRecord>, CndError> {
        self.db.get_library_item(id).map_err(Into::into)
    }

    pub fn library_search(
        &self,
        query: String,
        kind: Option<String>,
        limit: u32,
    ) -> Result<Vec<LibraryItemRecord>, CndError> {
        self.db.search_library(&query, kind, limit).map_err(Into::into)
    }

    pub fn library_albums(&self, limit: u32, offset: u32) -> Result<Vec<LibraryAlbumRecord>, CndError> {
        self.db.get_library_albums(limit, offset).map_err(Into::into)
    }

    pub fn library_artists(&self, limit: u32, offset: u32) -> Result<Vec<LibraryArtistRecord>, CndError> {
        self.db.get_library_artists(limit, offset).map_err(Into::into)
    }

    pub fn library_genres(&self) -> Result<Vec<LibraryGenreRecord>, CndError> {
        self.db.get_library_genres().map_err(Into::into)
    }

    pub fn library_browse(&self, folder_id: i64, rel_dir: String) -> Result<Vec<LibraryBrowseEntry>, CndError> {
        self.db.browse_library_dir(folder_id, &rel_dir).map_err(Into::into)
    }

    pub fn library_pending_metadata(&self, limit: u32) -> Result<Vec<LibraryItemRecord>, CndError> {
        self.db.get_pending_metadata(limit).map_err(Into::into)
    }

    pub fn library_store_metadata(
        &self,
        item_id: i64,
        metadata: LibraryItemMetadataInput,
    ) -> Result<(), CndError> {
        self.db.update_item_metadata(item_id, &metadata).map_err(Into::into)
    }

    pub fn library_record_progress(
        &self,
        fingerprint: String,
        position_ms: i64,
        duration_ms: i64,
    ) -> Result<(), CndError> {
        let profile_id = self.active_profile_id();
        self.db
            .record_library_progress(profile_id, &fingerprint, position_ms, duration_ms)
            .map_err(Into::into)
    }

    pub fn library_get_progress(&self, fingerprint: String) -> Result<Option<LibraryProgressRecord>, CndError> {
        let profile_id = self.active_profile_id();
        self.db.get_library_progress(profile_id, &fingerprint).map_err(Into::into)
    }

    pub fn library_recent(&self, kind: Option<String>, limit: u32) -> Result<Vec<LibraryItemRecord>, CndError> {
        let profile_id = self.active_profile_id();
        self.db.list_recent_library_items(kind, profile_id, limit).map_err(Into::into)
    }

    pub fn library_recently_added(&self, kind: Option<String>, limit: u32) -> Result<Vec<LibraryItemRecord>, CndError> {
        self.db.list_recently_added_library_items(kind, limit).map_err(Into::into)
    }
}

#[uniffi::export]
pub fn cnd_core_version() -> String {
    "1.0.0-native-rust".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, ReadBuf};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn stream_moviehash_is_only_computed_when_the_subscription_has_a_spare_connection() {
        use crate::subtitles::hash::tests::{sample, serve};
        let engine = CndEngine::in_memory().expect("engine");
        let data = sample(300_000);
        let (url, hits) = serve(data.clone(), true).await;
        let item_for = |max: Option<i64>, name: &str| {
            let src = engine
                .add_source(name.to_string(), "xtream".to_string(), None, Some("http://srv".to_string()), Some("u".to_string()), None)
                .expect("source");
            engine
                .db
                .with_conn(|conn| {
                    conn.execute("UPDATE iptv_sources SET max_connections = ?1 WHERE id = ?2", rusqlite::params![max, src])?;
                    conn.execute(
                        "INSERT INTO iptv_items (source_id, stable_hash, kind, name, name_clean, host, stream_url, created_at, updated_at)
                         VALUES (?1, ?2, 'movie', 'Film', 'film', 'h', ?3, 0, 0)",
                        rusqlite::params![src, format!("hash-{name}"), url],
                    )?;
                    Ok(conn.last_insert_rowid())
                })
                .expect("item")
        };

        for (max, name) in [(None, "unknown"), (Some(0), "zero"), (Some(1), "one")] {
            let item = item_for(max, name);
            assert_eq!(engine.compute_stream_moviehash(item).await.expect("call"), None, "{name}");
        }
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0, "the provider was never contacted");

        let item = item_for(Some(2), "two");
        let hash = engine.compute_stream_moviehash(item).await.expect("call").expect("a hash");
        assert_eq!(hash.len(), 16);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(engine.compute_stream_moviehash(99_999).await.expect("unknown item"), None);
    }

    #[test]
    fn test_engine_init() {
        let engine = CndEngine::in_memory().expect("engine init");
        assert_eq!(engine.version(), "1.0.0-native-rust");

        let stats = engine.get_stats().expect("get stats");
        assert_eq!(stats.total_sources, 0);

        let src_id = engine
            .add_source(
                "Freebox TV".to_string(),
                "m3u".to_string(),
                Some(
                    "http://mafreebox.freebox.fr/freeboxtv/playlist.m3u?password=secret"
                        .to_string(),
                ),
                None,
                None,
                None,
            )
            .expect("add source");

        assert!(src_id > 0);

        let sources = engine.list_sources().expect("list sources");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, "Freebox TV");

        let mut settings = engine.get_settings();
        assert_eq!(settings.segments_per_file, 4);
        settings.segments_per_file = 6;
        settings.default_user_agent = "CustomUA/1.0".to_string();
        engine.update_settings(settings).expect("update settings");
        assert_eq!(engine.get_settings().segments_per_file, 6);

        let logs = engine.get_logs_since(0, 50);
        assert!(logs.len() >= 2);
        for entry in &logs {
            if let Some(ref u) = entry.url_redacted {
                assert!(!u.contains("secret"));
            }
        }
        engine.clear_logs();
        assert!(engine.get_logs_since(0, 50).is_empty());
    }

    #[tokio::test]
    async fn test_m3u_refresh_preserves_favorites_and_groups_series() {
        let sample_m3u = r#"#EXTM3U x-tvg-url="http://epg.local/tv.xml.gz"
#EXTINF:-1 tvg-id="f24.fr" group-title="News",France 24 HD
http://iptv.local:8080/live/u/p/10.ts
#EXTINF:-1 group-title="Films",Inception (2010) 1080p
http://iptv.local:8080/movie/u/p/20.mkv
#EXTINF:-1 group-title="Séries",|FR| Severance S01E01 VOSTFR
http://iptv.local:8080/series/u/p/301.mkv
#EXTINF:-1 group-title="Séries",|FR| Severance S01E02 VOSTFR
http://iptv.local:8080/series/u/p/302.mkv
#EXTINF:-1 group-title="Séries",[VF] Severance S02E01 FHD
http://iptv.local:8080/series/u/p/401.mkv
"#;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let body = sample_m3u.to_string();

        let srv = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut sock, _) = listener.accept().await.expect("accept");
                let mut req_buf = vec![0u8; 1024];
                let _ = sock.read(&mut req_buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/x-mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                sock.write_all(resp.as_bytes()).await.expect("write");
            }
        });

        let engine = CndEngine::in_memory().expect("engine");
        let src_id = engine
            .add_source(
                "Local Test IPTV".to_string(),
                "m3u".to_string(),
                Some(format!("http://{addr}/playlist.m3u")),
                None,
                None,
                None,
            )
            .expect("add source");

        let summary1 = engine.refresh_source(src_id).await.expect("refresh 1");
        assert_eq!(summary1.total_items, 5);
        assert_eq!(summary1.live_count, 1);
        assert_eq!(summary1.movie_count, 1);
        assert_eq!(summary1.series_count, 1);
        assert_eq!(summary1.episode_count, 3);

        // Verify series_catalog has 1 series with season_count = 2 and episode_count = 3
        let (sc_seasons, sc_episodes): (u32, u32) = engine
            .db
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT season_count, episode_count FROM series_catalog WHERE source_id = ?1",
                    rusqlite::params![src_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .expect("series counts");
        assert_eq!(sc_seasons, 2);
        assert_eq!(sc_episodes, 3);

        // Verify EPG URL auto-populated from #EXTM3U header
        let sources = engine.list_sources().expect("sources");
        assert_eq!(
            sources[0].epg_url.as_deref(),
            Some("http://epg.local/tv.xml.gz")
        );

        // Mark France 24 as favorite, then re-refresh and verify is_favorite stays true!
        let live_items = engine
            .query_items("live".to_string(), "".to_string(), None, None, 10, 0)
            .expect("query live");
        assert_eq!(live_items.len(), 1);
        let fav_hash = live_items[0].stable_hash.clone();
        assert!(engine
            .toggle_favorite(fav_hash, "live".to_string())
            .expect("fav"));

        let summary2 = engine.refresh_source(src_id).await.expect("refresh 2");
        assert_eq!(summary2.total_items, 5);

        let live_after = engine
            .query_items("live".to_string(), "".to_string(), None, None, 10, 0)
            .expect("query live after");
        assert_eq!(live_after.len(), 1);
        assert!(live_after[0].is_favorite);

        srv.await.expect("server join");
    }

    /// Streaming synthetic generator yielding 50,000 #EXTINF entries on the fly without pre-allocating a giant String.
    struct SyntheticM3uStream {
        current_idx: usize,
        total_items: usize,
        pending: Vec<u8>,
        pending_pos: usize,
    }

    impl SyntheticM3uStream {
        fn new(total_items: usize) -> Self {
            Self {
                current_idx: 0,
                total_items,
                pending: b"#EXTM3U\n".to_vec(),
                pending_pos: 0,
            }
        }
    }

    impl AsyncRead for SyntheticM3uStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            while buf.remaining() > 0 {
                if self.pending_pos < self.pending.len() {
                    let avail = &self.pending[self.pending_pos..];
                    let to_copy = avail.len().min(buf.remaining());
                    buf.put_slice(&avail[..to_copy]);
                    self.pending_pos += to_copy;
                } else if self.current_idx < self.total_items {
                    self.pending.clear();
                    self.pending_pos = 0;
                    // Fill ~64 entries per chunk to simulate realistic TCP socket buffers
                    let chunk_end = (self.current_idx + 64).min(self.total_items);
                    for i in self.current_idx..chunk_end {
                        use std::io::Write;
                        let _ = writeln!(
                            self.pending,
                            "#EXTINF:-1 tvg-id=\"ch{i}.tv\" group-title=\"Group {}\",Channel {i} FHD\nhttp://cdn.iptv.test/live/u/p/{i}.ts",
                            i % 25
                        );
                    }
                    self.current_idx = chunk_end;
                } else {
                    break;
                }
            }
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn test_50k_streaming_m3u_ingestion_sla() {
        let engine = CndEngine::in_memory().expect("engine");
        let src_id = engine
            .add_source(
                "Bench 50k".to_string(),
                "m3u".to_string(),
                Some("http://cdn.iptv.test/50k.m3u".to_string()),
                None,
                None,
                None,
            )
            .expect("add bench source");
        let src = engine.db.get_source_by_id(src_id).expect("get src");

        let started = Instant::now();
        let stream = SyntheticM3uStream::new(50_000);
        let reader = BufReader::with_capacity(64 * 1024, stream);
        let summary = engine
            .ingest_m3u_reader(src_id, &src.uuid, reader, started)
            .await
            .expect("ingest 50k");

        assert_eq!(summary.total_items, 50_000);
        assert_eq!(summary.live_count, 50_000);

        // SLA: < 1.5s in release mode (allow generous headroom only in unoptimized debug builds)
        #[cfg(not(debug_assertions))]
        assert!(
            summary.duration_ms < 1_500,
            "50k ingestion took {} ms (SLA < 1500 ms)",
            summary.duration_ms
        );
        #[cfg(debug_assertions)]
        assert!(
            summary.duration_ms < 8_000,
            "50k debug ingestion took {} ms",
            summary.duration_ms
        );
    }

    #[tokio::test]
    async fn test_xtream_native_sync_lazy_episodes_and_m3u_fallback() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        let srv = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 2048];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let first_line = req.lines().next().unwrap_or("");

                    let (status, content_type, body) = if first_line.contains("username=fallback_user")
                        && first_line.contains("player_api.php")
                    {
                        ("404 Not Found", "text/plain", "Not Found".to_string())
                    } else if first_line.contains("get.php") {
                        (
                            "200 OK",
                            "audio/x-mpegurl",
                            "#EXTM3U\n#EXTINF:-1 group-title=\"News\",BFM TV\nhttp://srv/live/fallback_user/top_secret_pw/1.ts\n".to_string(),
                        )
                    } else if first_line.contains("action=get_live_categories") {
                        ("200 OK", "application/json", r#"[{"category_id":"1","category_name":"France HD"}]"#.to_string())
                    } else if first_line.contains("action=get_vod_categories") {
                        ("200 OK", "application/json", r#"[{"category_id":"10","category_name":"Sci-Fi"}]"#.to_string())
                    } else if first_line.contains("action=get_series_categories") {
                        ("200 OK", "application/json", r#"[{"category_id":"20","category_name":"Apple TV+"}]"#.to_string())
                    } else if first_line.contains("action=get_live_streams") {
                        ("200 OK", "application/json", r#"[{"stream_id":101,"num":1,"name":"TF1 HD","epg_channel_id":"tf1.fr","category_id":"1","tv_archive":1,"tv_archive_duration":7}]"#.to_string())
                    } else if first_line.contains("action=get_vod_streams") {
                        ("200 OK", "application/json", r#"[{"stream_id":"202","name":"Blade Runner 2049 (2017)","rating":"8.0","category_id":"10","container_extension":"mkv"}]"#.to_string())
                    } else if first_line.contains("action=get_series_info") {
                        ("200 OK", "application/json", r#"{"info":{"plot":"Dystopian silo"},"episodes":{"1":[{"id":"5001","episode_num":1,"season":1,"title":"Freedom Day","container_extension":"mkv"},{"id":"5002","episode_num":2,"season":1,"title":"Holston's Pick","container_extension":"mkv"}]}}"#.to_string())
                    } else if first_line.contains("action=get_series") {
                        ("200 OK", "application/json", r#"[{"series_id":303,"name":"Silo","cover":"http://img/silo.jpg","rating":"8.1","category_id":"20"}]"#.to_string())
                    } else {
                        (
                            "200 OK",
                            "application/json",
                            r#"{"user_info":{"auth":1,"status":"Active","exp_date":"1900000000","is_trial":"0","active_cons":1,"max_connections":3},"server_info":{"timezone":"UTC"}}"#.to_string(),
                        )
                    };

                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let engine = CndEngine::in_memory().expect("engine");
        let server_url = format!("http://{addr}");

        // 1. Verify account
        let acc = engine
            .verify_xtream_account(
                server_url.clone(),
                "alice".to_string(),
                "top_secret_pw".to_string(),
            )
            .await
            .expect("verify xtream");
        assert_eq!(acc.status, "Active");
        assert_eq!(acc.max_connections, 3);

        // 2. Add & sync native Xtream source (only password_ref stored in SQLite!)
        let src_id = engine
            .add_source(
                "My Xtream".to_string(),
                "xtream".to_string(),
                None,
                Some(server_url.clone()),
                Some("alice".to_string()),
                Some("cnd.xtream.keychain_1".to_string()),
            )
            .expect("add xtream source");

        let sync = engine
            .refresh_xtream_source(src_id, "top_secret_pw".to_string())
            .await
            .expect("refresh xtream");
        assert_eq!(sync.live_count, 1);
        assert_eq!(sync.movie_count, 1);
        assert_eq!(sync.series_count, 1);

        // Ensure iptv_sources NEVER contains plaintext password
        let stored_src = engine.db.get_source_by_id(src_id).expect("stored src");
        assert_eq!(
            stored_src.password_ref.as_deref(),
            Some("cnd.xtream.keychain_1")
        );
        assert!(!stored_src
            .epg_url
            .as_deref()
            .unwrap_or("")
            .contains("top_secret_pw"));

        // Lazy hydrate episodes for series
        let series_db_id: i64 = engine
            .db
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT id FROM series_catalog WHERE source_id = ?1",
                    rusqlite::params![src_id],
                    |r| r.get(0),
                )?)
            })
            .expect("series db id");

        let ep_count = engine
            .ensure_xtream_series_episodes(series_db_id, "top_secret_pw".to_string())
            .await
            .expect("hydrate episodes");
        assert_eq!(ep_count, 2);

        // 3. Test automatic M3U+ fallback when player_api.php returns 404
        let fb_id = engine
            .add_source(
                "Fallback Xtream".to_string(),
                "xtream".to_string(),
                None,
                Some(server_url),
                Some("fallback_user".to_string()),
                Some("cnd.xtream.keychain_2".to_string()),
            )
            .expect("add fb source");
        let fb_sync = engine
            .refresh_xtream_source(fb_id, "top_secret_pw".to_string())
            .await
            .expect("fallback sync");
        assert_eq!(fb_sync.live_count, 1);

        // Verify TelemetryHub NEVER leaked `top_secret_pw`
        for entry in engine.get_logs_since(0, 500) {
            assert!(!entry.message.contains("top_secret_pw"));
            if let Some(ref u) = entry.url_redacted {
                assert!(!u.contains("top_secret_pw"));
            }
        }

        srv.abort();
    }

    #[tokio::test]
    async fn test_cnd_engine_catchup_workflow() {
        let engine = CndEngine::in_memory().expect("engine");
        let src_id = engine
            .add_source(
                "Catchup Test".to_string(),
                "m3u".to_string(),
                Some("http://dummy/m3u".to_string()),
                None,
                None,
                None,
            )
            .expect("source");

        let now_ts = db::chrono_now_ts();
        let past_start = now_ts - 7200; // 2 hours ago
        let past_end = past_start + 3600; // 1 hour duration

        // Insert item with Xtream-style live url and default catchup
        let live_url = "http://iptv.server.io:8080/live/user1/pass1/999.ts";
        let item_id = engine
            .db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO iptv_items (
                        source_id, stable_hash, kind, name, name_clean, host,
                        container_ext, stream_url, catchup_type, catchup_days, created_at, updated_at
                    ) VALUES (?1, 'hash_catchup_1', 'live', 'France 2 HD', 'france 2 hd', 'iptv.server.io', 'ts', ?2, 'default', 7, ?3, ?3)",
                    rusqlite::params![src_id, live_url, now_ts],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .expect("insert item");

        // 1. Availability check
        assert!(engine.is_catchup_available(item_id, past_start).expect("avail"));
        assert!(!engine.is_catchup_available(item_id, now_ts + 1000).expect("future not avail"));
        assert!(!engine.is_catchup_available(item_id, now_ts - (10 * 86_400)).expect("expired not avail"));

        // 2. Build Catchup URL
        let replay_url = engine
            .build_catchup_url(item_id, past_start, past_end)
            .expect("build url")
            .expect("some replay url");

        assert!(replay_url.starts_with("http://iptv.server.io:8080/timeshift/user1/pass1/60/"));
        assert!(replay_url.ends_with("/999.ts"));

        // The start time is written on the server's clock: a server two hours ahead of UTC moves it.
        let hour_of = |url: &str| -> i64 {
            let stamp = url.split('/').nth(7).expect("start stamp");
            stamp[11..13].parse().expect("hour")
        };
        let utc_hour = hour_of(&replay_url);
        engine
            .db
            .with_conn(|conn| {
                conn.execute("UPDATE iptv_sources SET server_utc_offset_secs = 7200 WHERE id = ?1", rusqlite::params![src_id])?;
                Ok(())
            })
            .expect("set offset");
        let shifted = engine.build_catchup_url(item_id, past_start, past_end).expect("build").expect("url");
        assert_eq!(hour_of(&shifted), (utc_hour + 2) % 24, "{replay_url} -> {shifted}");

        // 3. Resolve Catchup URL standalone
        let resolved = engine
            .resolve_catchup_url(
                "http://stream.org/live/ch1.m3u8".to_string(),
                Some("flussonic".to_string()),
                None,
                Some(7),
                past_start,
                past_end,
            )
            .expect("resolve standalone")
            .expect("url");
        assert_eq!(resolved, format!("http://stream.org/live/ch1/timeshift_abs-{past_start}.m3u8"));
    }

    #[tokio::test]
    async fn test_cnd_engine_mirrors_and_failover() {
        let engine = CndEngine::in_memory().expect("in memory engine");
        let src_id = engine
            .add_source(
                "Source A".to_string(),
                "m3u".to_string(),
                Some("http://dummy/a.m3u".to_string()),
                None,
                None,
                None,
            )
            .expect("source a");
        let src_b = engine
            .add_source(
                "Source B".to_string(),
                "m3u".to_string(),
                Some("http://dummy/b.m3u".to_string()),
                None,
                None,
                None,
            )
            .expect("source b");

        let now_ts = db::chrono_now_ts();

        // Target: TF1 4K on Source A
        let target_id = engine
            .db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO iptv_items (source_id, stable_hash, kind, name, name_clean, tvg_id, host, stream_url, created_at, updated_at)
                     VALUES (?1, 'hash_tf1_4k', 'live', 'TF1 4K', 'tf1 4k', 'TF1.fr', 'srv1', 'http://srv1/tf1_4k.ts', ?2, ?2)",
                    rusqlite::params![src_id, now_ts],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .expect("insert target");

        // Mirror 1: TF1 FHD on Source A (tvg_id match, FHD quality)
        let m1_id = engine
            .db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO iptv_items (source_id, stable_hash, kind, name, name_clean, tvg_id, host, stream_url, created_at, updated_at)
                     VALUES (?1, 'hash_tf1_fhd', 'live', 'FR | TF1 FHD', 'fr tf1 fhd', 'TF1.fr', 'srv1', 'http://srv1/tf1_fhd.ts', ?2, ?2)",
                    rusqlite::params![src_id, now_ts],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .expect("insert m1");

        // Mirror 2: TF1 HD on Source B (no tvg_id, token match "tf 1")
        let m2_id = engine
            .db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO iptv_items (source_id, stable_hash, kind, name, name_clean, tvg_id, host, stream_url, created_at, updated_at)
                     VALUES (?1, 'hash_tf1_hd', 'live', 'TF1 HD', 'tf1 hd', '', 'srv2', 'http://srv2/tf1_hd.ts', ?2, ?2)",
                    rusqlite::params![src_b, now_ts],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .expect("insert m2");

        // Mirror 3: TF1 SD on Source A (flagged broken -> should be excluded!)
        engine
            .db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO iptv_items (source_id, stable_hash, kind, name, name_clean, tvg_id, host, stream_url, created_at, updated_at)
                     VALUES (?1, 'hash_tf1_sd', 'live', 'TF1 SD', 'tf1 sd', 'TF1.fr', 'srv1', 'http://srv1/tf1_sd.ts', ?2, ?2)",
                    rusqlite::params![src_id, now_ts],
                )?;
                Ok(())
            })
            .expect("insert m3");
        engine
            .set_broken("hash_tf1_sd".to_string(), "live".to_string(), true)
            .expect("flag broken");

        // Unrelated channel: France 2 HD
        engine
            .db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO iptv_items (source_id, stable_hash, kind, name, name_clean, tvg_id, host, stream_url, created_at, updated_at)
                     VALUES (?1, 'hash_fr2', 'live', 'France 2 HD', 'france 2 hd', 'France2.fr', 'srv1', 'http://srv1/fr2.ts', ?2, ?2)",
                    rusqlite::params![src_id, now_ts],
                )?;
                Ok(())
            })
            .expect("insert fr2");

        // Set health: m1 is OK with 30ms latency
        engine
            .db
            .save_stream_health(&db::StreamHealthRecord {
                stable_hash: "hash_tf1_fhd".to_string(),
                status: "ok".to_string(),
                latency_ms: Some(30),
                checked_at: now_ts,
                consecutive_failures: 0,
                last_ok_at: Some(now_ts),
            })
            .expect("save health");

        // Find mirrors for target
        let mirrors = engine.find_mirrors(target_id).expect("find mirrors");
        assert_eq!(
            mirrors.len(),
            2,
            "broken and target excluded, fr2 not a mirror"
        );
        assert_eq!(
            mirrors[0].id, m1_id,
            "first mirror is TF1 FHD (OK health + FHD quality)"
        );
        assert_eq!(mirrors[1].id, m2_id, "second mirror is TF1 HD");
    }

    #[tokio::test]
    async fn test_health_worker_auto_flag_and_user_origin() {
        let engine = CndEngine::in_memory().expect("in memory engine");
        let src_id = engine
            .add_source(
                "Source A".to_string(),
                "m3u".to_string(),
                Some("http://dummy/a.m3u".to_string()),
                None,
                None,
                None,
            )
            .expect("source a");

        let now_ts = db::chrono_now_ts();
        engine
            .db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO iptv_items (source_id, stable_hash, kind, name, name_clean, host, stream_url, created_at, updated_at)
                     VALUES (?1, 'hash_auto_broken', 'live', 'Canal+ Test', 'canal test', 'srv', 'http://127.0.0.1:9999/bad.ts', ?2, ?2)",
                    rusqlite::params![src_id, now_ts],
                )?;
                Ok(())
            })
            .expect("insert item");

        // Mark as favorite so worker will pick it up
        engine
            .toggle_favorite("hash_auto_broken".to_string(), "live".to_string())
            .expect("fav");

        // Initially not broken
        assert!(!engine
            .is_broken("hash_auto_broken".to_string())
            .expect("is broken"));

        // Simulate 3 failures via worker logic
        for _ in 0..3 {
            let _ = engine.run_health_check_cycle(false).await;
        }

        // Now should be auto-flagged broken
        assert!(engine
            .is_broken("hash_auto_broken".to_string())
            .expect("is broken"));

        // Succeeded probe clears origin='auto'
        let _ = engine
            .db
            .set_broken_with_origin("hash_auto_broken", "live", false, "auto");
        assert!(!engine
            .is_broken("hash_auto_broken".to_string())
            .expect("is broken"));

        // User sets broken -> origin='user'
        engine
            .set_broken(
                "hash_auto_broken".to_string(),
                "live".to_string(),
                true,
            )
            .expect("set broken");
        assert!(engine
            .is_broken("hash_auto_broken".to_string())
            .expect("is broken"));

        // Worker auto-success does NOT remove user-flagged broken!
        let _ = engine
            .db
            .set_broken_with_origin("hash_auto_broken", "live", false, "auto");
        assert!(engine
            .is_broken("hash_auto_broken".to_string())
            .expect("still broken because user flagged it"));
    }

    #[tokio::test]
    async fn test_cnd_engine_subtitles_parsing_and_hash() {
        let engine = CndEngine::in_memory().expect("engine");

        let srt = "1\n00:00:01,000 --> 00:00:04,500\n<i>Bienvenue</i> sur CND IPTV\n\n2\n00:00:05,000 --> 00:00:08,000\nDeuxième réplique";
        let cues = engine.parse_srt(srt.to_string());
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "Bienvenue sur CND IPTV");
        assert_eq!(cues[0].start_ms, 1000);
        assert_eq!(cues[0].end_ms, 4500);

        // Test hash calculation on temporary file >= 64KB
        use std::io::Write;
        let tmp_path = std::env::temp_dir().join(format!("cnd_test_lib_hash_{}.bin", std::process::id()));
        let mut tmp = std::fs::File::create(&tmp_path).unwrap();
        let payload = vec![0x33u8; 100 * 1024];
        tmp.write_all(&payload).unwrap();
        tmp.flush().unwrap();
        drop(tmp);

        let hash = engine
            .compute_file_moviehash(tmp_path.to_string_lossy().to_string())
            .expect("moviehash");
        assert_eq!(hash.len(), 16);
        let _ = std::fs::remove_file(tmp_path);
    }

    #[tokio::test]
    async fn test_cnd_engine_pvr_schedule_and_rules() {
        let engine = CndEngine::in_memory().expect("engine");
        // In the future: the background scheduler marks a past recording as missed, which made the
        // "scheduled" assertion below depend on timing.
        let soon = db::chrono_now_ts() + 3_600;

        let req = NewRecordingRequest {
            item_id: 101,
            channel_tvg_id: Some("canal.plus".to_string()),
            channel_name: "Canal+".to_string(),
            title: "Grand Film du Dimanche".to_string(),
            subtitle: None,
            stream_url: "http://stream.example/canal.ts".to_string(),
            start_ts: soon,
            end_ts: soon + 1000,
            pre_padding_s: Some(60),
            post_padding_s: Some(180),
            rule_id: None,
        };

        let id = engine.schedule_recording(req).expect("schedule");
        assert!(id > 0);

        let list = engine.list_recordings(None).expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);
        assert_eq!(list[0].title, "Grand Film du Dimanche");
        assert_eq!(list[0].state, "scheduled");

        // Cancel
        let cancelled = engine.cancel_recording(id).expect("cancel");
        assert!(cancelled);

        let list_after = engine.list_recordings(Some("cancelled".to_string())).expect("list");
        assert_eq!(list_after.len(), 1);

        // Delete
        let deleted = engine.delete_recording(id, false).expect("delete");
        assert!(deleted);
        assert_eq!(engine.list_recordings(None).expect("list").len(), 0);

        // Series rule
        let rule_id = engine
            .add_series_rule(Some("canal.plus".to_string()), "Top 14".to_string())
            .expect("add rule");
        let rules = engine.list_series_rules().expect("rules");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].title_key, "top 14");

        assert!(engine.remove_series_rule(rule_id).expect("remove rule"));
        assert_eq!(engine.list_series_rules().expect("rules").len(), 0);
    }

    #[tokio::test]
    async fn test_cnd_engine_profiles_bouquets_and_parental_control() {
        let engine = CndEngine::in_memory().expect("engine");

        // 1. Initial default profile
        assert_eq!(engine.active_profile_id(), 1);
        let profiles = engine.list_profiles().expect("profiles");
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, 1);
        assert_eq!(profiles[0].name, "Principal");
        assert!(!profiles[0].is_kids);

        // 2. Create Kids Profile
        let kids_prof = engine
            .create_profile("Kids Room".to_string(), "figure.child".to_string(), true)
            .expect("create kids");
        assert!(kids_prof.id > 1);
        assert!(kids_prof.is_kids);

        engine.set_active_profile_id(kids_prof.id);
        assert_eq!(engine.active_profile_id(), kids_prof.id);

        // 3. Hide adult group
        assert!(engine.is_adult_category("XXX Adultes".to_string()));
        assert!(!engine.is_adult_category("Dessins Animés".to_string()));

        engine
            .hide_group(kids_prof.id, "live".to_string(), "XXX Adultes".to_string())
            .expect("hide group");
        let hidden_groups = engine.list_hidden_groups(kids_prof.id, "live".to_string()).expect("hidden groups");
        assert_eq!(hidden_groups, vec!["xxx adultes"]);

        // 4. Hide individual item
        engine.hide_item(kids_prof.id, "hash_violent_channel".to_string()).expect("hide item");
        let hidden_items = engine.list_hidden_items(kids_prof.id).expect("hidden items");
        assert_eq!(hidden_items, vec!["hash_violent_channel"]);

        // 5. Favorite folders
        let f_id = engine.create_favorite_folder(kids_prof.id, "Cartoons".to_string(), 0).expect("fav folder");
        let folders = engine.list_favorite_folders(kids_prof.id).expect("list folders");
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name, "Cartoons");

        engine.add_to_favorite_folder(f_id, "hash_cartoon_1".to_string(), 0).expect("add to folder");
        let f_items = engine.list_favorite_folder_items(f_id).expect("folder items");
        assert_eq!(f_items, vec!["hash_cartoon_1"]);

        // 6. Delete profile (cannot delete default 1)
        assert!(engine.delete_profile(1).is_err());
        assert!(engine.delete_profile(kids_prof.id).expect("delete kids"));
        assert_eq!(engine.list_profiles().expect("list").len(), 1);
    }

    #[tokio::test]
    async fn test_cnd_engine_local_media_library_workflow() {
        use std::fs::{self, File};
        use std::io::Write;

        let engine = CndEngine::in_memory().expect("engine");
        let dir = std::env::temp_dir().join(format!("cnd_test_lib_wf_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Subdirectories: Movies/ and Music/
        let movies_dir = dir.join("Movies");
        let music_dir = dir.join("Music");
        fs::create_dir_all(&movies_dir).unwrap();
        fs::create_dir_all(&music_dir).unwrap();

        // 1. Files
        let mut f_vid = File::create(movies_dir.join("Interstellar.2014.mkv")).unwrap();
        f_vid.write_all(&[0x1Au8, 0x45, 0xDF, 0xA3]).unwrap();
        f_vid.write_all(&vec![0u8; 10000]).unwrap();
        drop(f_vid);

        let mut f_sub = File::create(movies_dir.join("Interstellar.2014.fr.srt")).unwrap();
        f_sub.write_all(b"1\n00:00:01,000 --> 00:00:04,000\nBonjour\n").unwrap();
        drop(f_sub);

        let mut f_aud = File::create(music_dir.join("song.flac")).unwrap();
        f_aud.write_all(b"fLaC\x00\x00\x00\x22").unwrap();
        drop(f_aud);

        let mut f_img = File::create(dir.join("photo.jpg")).unwrap();
        f_img.write_all(&[0xFF, 0xD8, 0xFF, 0xE0]).unwrap();
        drop(f_img);

        // 2. Add folder & scan
        let folder = engine
            .library_add_folder(
                "My Files".to_string(),
                dir.to_str().unwrap().to_string(),
                None,
                true,
                "video,audio,image".to_string(),
            )
            .expect("add folder");
        assert_eq!(folder.display_name, "My Files");

        let scan_report = engine
            .library_scan(folder.id, dir.to_str().unwrap().to_string())
            .expect("scan");
        assert_eq!(scan_report.added, 3); // 1 video, 1 audio, 1 image

        // 3. Verify video & companion subtitle
        let videos = engine
            .library_list_items(Some("video".to_string()), None, None, true, 0, 10)
            .expect("list videos");
        assert_eq!(videos.len(), 1);
        assert_eq!(videos[0].subtitles, vec!["Movies/Interstellar.2014.fr.srt"]);

        // 4. Update metadata for audio track
        let audios = engine
            .library_list_items(Some("audio".to_string()), None, None, true, 0, 10)
            .expect("list audio");
        assert_eq!(audios.len(), 1);
        let audio_id = audios[0].id;

        let meta = LibraryItemMetadataInput {
            title: Some("Starman".to_string()),
            duration_ms: Some(250000),
            width: None,
            height: None,
            codec: Some("flac".to_string()),
            artist: Some("David Bowie".to_string()),
            album: Some("Ziggy Stardust".to_string()),
            album_artist: Some("David Bowie".to_string()),
            track_no: Some(4),
            disc_no: Some(1),
            genre: Some("Glam Rock".to_string()),
            year: Some(1972),
            taken_at: None,
            thumbnail_path: None,
        };
        engine.library_store_metadata(audio_id, meta).expect("store metadata");

        // 5. Verify Albums, Artists, Genres
        let albums = engine.library_albums(10, 0).expect("albums");
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].name, "Ziggy Stardust");

        let artists = engine.library_artists(10, 0).expect("artists");
        assert_eq!(artists.len(), 1);
        assert_eq!(artists[0].name, "David Bowie");

        let genres = engine.library_genres().expect("genres");
        assert_eq!(genres.len(), 1);
        assert_eq!(genres[0].name, "Glam Rock");

        // 6. Directory browser
        let root_entries = engine.library_browse(folder.id, "".to_string()).expect("browse root");
        let dir_names: Vec<&str> = root_entries.iter().filter(|e| e.is_directory).map(|e| e.name.as_str()).collect();
        assert!(dir_names.contains(&"Movies"));
        assert!(dir_names.contains(&"Music"));

        // 7. Search FTS5
        let search_res = engine.library_search("Bowie".to_string(), None, 10).expect("search");
        assert_eq!(search_res.len(), 1);
        assert_eq!(search_res[0].title.as_deref(), Some("Starman"));

        // 8. Progress and Recently Played
        let fp = &videos[0].fingerprint;
        engine.library_record_progress(fp.clone(), 30000, 120000).expect("record progress");

        let prog = engine.library_get_progress(fp.clone()).expect("get progress");
        assert!(prog.is_some());
        assert_eq!(prog.unwrap().position_ms, 30000);

        let recent = engine.library_recent(Some("video".to_string()), 10).expect("recent");
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].id, videos[0].id);

        // 9. Remove folder
        engine.library_remove_folder(folder.id).expect("remove folder");
        let list_folders = engine.library_list_folders().expect("list");
        assert_eq!(list_folders.len(), 0);

        let _ = fs::remove_dir_all(dir);
    }
}
