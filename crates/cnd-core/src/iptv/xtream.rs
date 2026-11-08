use crate::db::ingestion_writer::{make_series_stable_hash, SyncSummary};
use crate::db::{chrono_now_ts, DatabaseManager, DbError};
use crate::http_client::HttpClientPool;
use crate::iptv::m3u_parser::extract_url_host_and_ext;
use crate::iptv::normalizer::{clean_title_and_year, extract_imdb_id, normalize_imdb_rating};
use crate::telemetry::TelemetryHub;
use crate::CndError;
use rusqlite::{params, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Instant;

const XTREAM_BATCH_SIZE: usize = 2_500;
const SERIES_EPISODES_TTL_SECS: i64 = 86_400;

#[derive(Debug, Clone, uniffi::Record)]
pub struct XtreamAccountInfo {
    pub status: String,
    pub exp_date_ts: Option<i64>,
    pub is_trial: bool,
    pub active_connections: u32,
    pub max_connections: u32,
    pub server_timezone: Option<String>,
    /// Seconds the server's clock is ahead of UTC (from `time_now` / `timestamp_now`); Xtream replay URLs
    /// are written on that clock.
    pub server_utc_offset_secs: Option<i64>,
}

#[derive(Debug)]
pub enum XtreamAuthProbe {
    Authenticated(XtreamAccountInfo),
    InvalidCredentials(String),
    FallbackM3uPlus(String),
}

#[derive(Debug, Deserialize)]
struct RawAuthEnvelope {
    user_info: Option<RawUserInfo>,
    server_info: Option<RawServerInfo>,
}

#[derive(Debug, Deserialize)]
struct RawUserInfo {
    #[serde(default, deserialize_with = "de_opt_bool")]
    auth: Option<bool>,
    #[serde(default, deserialize_with = "de_opt_string")]
    status: Option<String>,
    #[serde(default, deserialize_with = "de_opt_i64")]
    exp_date: Option<i64>,
    #[serde(default, deserialize_with = "de_bool")]
    is_trial: bool,
    #[serde(default, deserialize_with = "de_u32")]
    active_cons: u32,
    #[serde(default, deserialize_with = "de_u32")]
    max_connections: u32,
}

#[derive(Debug, Deserialize)]
struct RawServerInfo {
    #[serde(default, deserialize_with = "de_opt_string")]
    timezone: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    time_now: Option<String>,
    #[serde(default, deserialize_with = "de_opt_i64")]
    timestamp_now: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct RawCategory {
    #[serde(default, deserialize_with = "de_string")]
    category_id: String,
    #[serde(default, deserialize_with = "de_string")]
    category_name: String,
}

#[derive(Debug, Deserialize)]
struct RawLiveStream {
    #[serde(default, deserialize_with = "de_string")]
    stream_id: String,
    #[serde(default, deserialize_with = "de_opt_u32")]
    num: Option<u32>,
    #[serde(default, deserialize_with = "de_string")]
    name: String,
    #[serde(default, deserialize_with = "de_opt_string")]
    stream_icon: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    epg_channel_id: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    category_id: Option<String>,
    #[serde(default, deserialize_with = "de_u32")]
    tv_archive: u32,
    #[serde(default, deserialize_with = "de_opt_u32")]
    tv_archive_duration: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct RawVodStream {
    #[serde(default, deserialize_with = "de_string")]
    stream_id: String,
    #[serde(default, deserialize_with = "de_string")]
    name: String,
    #[serde(default, deserialize_with = "de_opt_string")]
    stream_icon: Option<String>,
    #[serde(default, deserialize_with = "de_opt_f64")]
    rating: Option<f64>,
    #[serde(default, deserialize_with = "de_opt_f64")]
    rating_5based: Option<f64>,
    #[serde(default, deserialize_with = "de_opt_string")]
    category_id: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    container_extension: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    tmdb: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawSeriesEntry {
    #[serde(default, deserialize_with = "de_string")]
    series_id: String,
    #[serde(default, deserialize_with = "de_string")]
    name: String,
    #[serde(default, deserialize_with = "de_opt_string")]
    cover: Option<String>,
    #[serde(default, deserialize_with = "de_opt_backdrop")]
    backdrop_path: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    plot: Option<String>,
    #[serde(default, deserialize_with = "de_opt_f64")]
    rating: Option<f64>,
    #[serde(default, deserialize_with = "de_opt_f64")]
    rating_5based: Option<f64>,
    #[serde(default, deserialize_with = "de_opt_string")]
    category_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawSeriesInfoResponse {
    #[serde(default)]
    info: Option<RawSeriesDetailInfo>,
    #[serde(default)]
    episodes: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct RawSeriesDetailInfo {
    #[serde(default, deserialize_with = "de_opt_string")]
    plot: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    cover: Option<String>,
    #[serde(default, deserialize_with = "de_opt_backdrop")]
    backdrop_path: Option<String>,
    #[serde(default, deserialize_with = "de_opt_f64")]
    rating: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RawEpisodeEntry {
    #[serde(default, deserialize_with = "de_string")]
    id: String,
    #[serde(default, deserialize_with = "de_opt_u32")]
    episode_num: Option<u32>,
    #[serde(default, deserialize_with = "de_opt_u32")]
    season: Option<u32>,
    #[serde(default, deserialize_with = "de_string")]
    title: String,
    #[serde(default, deserialize_with = "de_opt_string")]
    container_extension: Option<String>,
    #[serde(default)]
    info: Option<RawEpisodeInfo>,
}

#[derive(Debug, Deserialize)]
struct RawEpisodeInfo {
    #[serde(default, deserialize_with = "de_opt_string")]
    movie_image: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    plot: Option<String>,
    #[serde(default, deserialize_with = "de_opt_f64")]
    rating: Option<f64>,
}

pub fn normalize_server_base(raw_server: &str) -> String {
    let trimmed = raw_server.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

pub fn encode_param(val: &str) -> String {
    url::form_urlencoded::byte_serialize(val.as_bytes()).collect()
}

pub fn build_m3u_plus_fallback_url(server: &str, username: &str, password: &str) -> String {
    let base = normalize_server_base(server);
    let u = encode_param(username);
    let p = encode_param(password);
    format!("{base}/get.php?username={u}&password={p}&type=m3u_plus&output=ts")
}

pub fn build_xtream_epg_template_url(server: &str) -> String {
    let base = normalize_server_base(server);
    format!("{base}/xmltv.php")
}

pub async fn probe_xtream_account(
    http_pool: &HttpClientPool,
    server: &str,
    username: &str,
    password: &str,
) -> Result<XtreamAuthProbe, CndError> {
    let base = normalize_server_base(server);
    let u = encode_param(username);
    let p = encode_param(password);
    let auth_url = format!("{base}/player_api.php?username={u}&password={p}");

    let req = http_pool.get(&auth_url)?;
    let resp = http_pool.send_logged("XTREAM", req).await?;
    let status = resp.status().as_u16();

    if status == 404 || status == 403 {
        return Ok(XtreamAuthProbe::FallbackM3uPlus(format!(
            "player_api.php returned HTTP {status}"
        )));
    }

    if !resp.status().is_success() {
        return Err(CndError::Network {
            message: format!("Xtream server HTTP error {status}"),
        });
    }

    let bytes = resp.bytes().await.map_err(|e| CndError::Network {
        message: crate::telemetry::redact_text(&e.to_string()),
    })?;

    Ok(parse_auth_probe_bytes(&bytes))
}

pub fn parse_auth_probe_bytes(bytes: &[u8]) -> XtreamAuthProbe {
    let Ok(envelope) = serde_json::from_slice::<RawAuthEnvelope>(bytes) else {
        return XtreamAuthProbe::FallbackM3uPlus(
            "player_api.php returned invalid JSON".to_string(),
        );
    };

    let Some(user_info) = envelope.user_info else {
        return XtreamAuthProbe::FallbackM3uPlus("player_api.php missing user_info".to_string());
    };

    if user_info.auth == Some(false) {
        return XtreamAuthProbe::InvalidCredentials(
            "Xtream authentication rejected (invalid username or password)".to_string(),
        );
    }

    let status = user_info
        .status
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Active".to_string());

    let (server_timezone, server_utc_offset_secs) = match envelope.server_info {
        Some(info) => {
            let offset = match (info.time_now.as_deref(), info.timestamp_now) {
                (Some(local), Some(utc)) => crate::iptv::catchup::server_utc_offset(local, utc),
                _ => None,
            };
            (info.timezone, offset)
        }
        None => (None, None),
    };

    XtreamAuthProbe::Authenticated(XtreamAccountInfo {
        status,
        exp_date_ts: user_info.exp_date.filter(|&ts| ts > 0),
        is_trial: user_info.is_trial,
        active_connections: user_info.active_cons,
        max_connections: user_info.max_connections,
        server_timezone,
        server_utc_offset_secs,
    })
}

pub async fn verify_xtream_account(
    http_pool: &HttpClientPool,
    server: &str,
    username: &str,
    password: &str,
) -> Result<XtreamAccountInfo, CndError> {
    match probe_xtream_account(http_pool, server, username, password).await? {
        XtreamAuthProbe::Authenticated(info) => Ok(info),
        XtreamAuthProbe::InvalidCredentials(msg) => Err(CndError::Network { message: msg }),
        XtreamAuthProbe::FallbackM3uPlus(reason) => Err(CndError::Parse {
            message: format!("Xtream player_api unavailable ({reason})"),
        }),
    }
}

async fn fetch_xtream_vec<T: DeserializeOwned>(
    http_pool: &HttpClientPool,
    url: &str,
) -> Result<Vec<T>, CndError> {
    let req = http_pool.get(url)?;
    let resp = http_pool.send_logged("XTREAM", req).await?;
    if !resp.status().is_success() {
        return Ok(Vec::new());
    }
    let bytes = resp.bytes().await.map_err(|e| CndError::Network {
        message: crate::telemetry::redact_text(&e.to_string()),
    })?;
    parse_flexible_json_array(&bytes)
}

pub fn parse_flexible_json_array<T: DeserializeOwned>(bytes: &[u8]) -> Result<Vec<T>, CndError> {
    let trimmed = trim_ascii_bytes(bytes);
    if trimmed.is_empty() || trimmed == b"null" || trimmed == b"{}" || trimmed == b"[]" {
        return Ok(Vec::new());
    }
    serde_json::from_slice::<Vec<T>>(trimmed).map_err(|e| CndError::Parse {
        message: format!("Xtream JSON parse error: {e}"),
    })
}

fn trim_ascii_bytes(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map(|i| i + 1)
        .unwrap_or(start);
    &bytes[start..end]
}

#[allow(clippy::too_many_arguments)]
pub async fn sync_xtream_native_catalog(
    db: &DatabaseManager,
    http_pool: &HttpClientPool,
    telemetry: &TelemetryHub,
    source_id: i64,
    source_uuid: &str,
    server: &str,
    username: &str,
    password: &str,
    account_info: &XtreamAccountInfo,
    started: Instant,
) -> Result<SyncSummary, CndError> {
    let base = normalize_server_base(server);
    let u_enc = encode_param(username);
    let p_enc = encode_param(password);

    let live_cats_url =
        format!("{base}/player_api.php?username={u_enc}&password={p_enc}&action=get_live_categories");
    let vod_cats_url =
        format!("{base}/player_api.php?username={u_enc}&password={p_enc}&action=get_vod_categories");
    let series_cats_url = format!(
        "{base}/player_api.php?username={u_enc}&password={p_enc}&action=get_series_categories"
    );
    let live_streams_url =
        format!("{base}/player_api.php?username={u_enc}&password={p_enc}&action=get_live_streams");
    let vod_streams_url =
        format!("{base}/player_api.php?username={u_enc}&password={p_enc}&action=get_vod_streams");
    let series_url =
        format!("{base}/player_api.php?username={u_enc}&password={p_enc}&action=get_series");

    let (live_cats, vod_cats, series_cats, live_streams, vod_streams, series_entries) = tokio::try_join!(
        fetch_xtream_vec::<RawCategory>(http_pool, &live_cats_url),
        fetch_xtream_vec::<RawCategory>(http_pool, &vod_cats_url),
        fetch_xtream_vec::<RawCategory>(http_pool, &series_cats_url),
        fetch_xtream_vec::<RawLiveStream>(http_pool, &live_streams_url),
        fetch_xtream_vec::<RawVodStream>(http_pool, &vod_streams_url),
        fetch_xtream_vec::<RawSeriesEntry>(http_pool, &series_url),
    )?;

    let live_cat_map: HashMap<String, String> = live_cats
        .into_iter()
        .filter(|c| !c.category_id.is_empty())
        .map(|c| (c.category_id, c.category_name))
        .collect();
    let vod_cat_map: HashMap<String, String> = vod_cats
        .into_iter()
        .filter(|c| !c.category_id.is_empty())
        .map(|c| (c.category_id, c.category_name))
        .collect();
    let series_cat_map: HashMap<String, String> = series_cats
        .into_iter()
        .filter(|c| !c.category_id.is_empty())
        .map(|c| (c.category_id, c.category_name))
        .collect();

    let (host, _) = extract_url_host_and_ext(&base);
    let default_epg_url = build_xtream_epg_template_url(&base);

    let summary = write_xtream_catalog_to_db(
        db,
        source_id,
        source_uuid,
        &base,
        &host,
        username,
        password,
        &default_epg_url,
        account_info,
        &live_cat_map,
        &vod_cat_map,
        &series_cat_map,
        &live_streams,
        &vod_streams,
        &series_entries,
        started,
    )?;

    // A failed classification must not fail the refresh: the catalogue stays usable and the next
    // launch reclassifies anything left without a theme.
    if let Err(e) = db.classify_source(source_id) {
        telemetry.warn("TAXONOMY", format!("Could not classify source id={source_id}: {e}"));
    }

    telemetry.info(
        "XTREAM",
        format!(
            "Synced Xtream source id={source_id}: live={}, movies={}, series={} in {} ms",
            summary.live_count, summary.movie_count, summary.series_count, summary.duration_ms
        ),
    );

    Ok(summary)
}

#[allow(clippy::too_many_arguments)]
fn write_xtream_catalog_to_db(
    db: &DatabaseManager,
    source_id: i64,
    source_uuid: &str,
    base: &str,
    host: &str,
    username: &str,
    password: &str,
    default_epg_url: &str,
    account_info: &XtreamAccountInfo,
    live_cat_map: &HashMap<String, String>,
    vod_cat_map: &HashMap<String, String>,
    series_cat_map: &HashMap<String, String>,
    live_streams: &[RawLiveStream],
    vod_streams: &[RawVodStream],
    series_entries: &[RawSeriesEntry],
    started: Instant,
) -> Result<SyncSummary, DbError> {
    let mut live_count = 0u32;
    let mut movie_count = 0u32;
    let mut series_count = 0u32;

    // 1. Purge old catalog for this source
    db.with_conn_mut(|conn| {
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM iptv_items WHERE source_id = ?1", params![source_id])?;
        tx.execute(
            "DELETE FROM series_catalog WHERE source_id = ?1",
            params![source_id],
        )?;
        tx.commit()?;
        Ok(())
    })?;

    // 2. Insert Live streams in batches
    for chunk in live_streams.chunks(XTREAM_BATCH_SIZE) {
        db.with_conn_mut(|conn| {
            let tx = conn.transaction()?;
            let now = chrono_now_ts();
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO iptv_items (
                        source_id, stable_hash, kind, name, name_clean, year, group_title,
                        tvg_id, tvg_name, logo_url, chno, catchup_type, catchup_days,
                        host, container_ext, stream_url, created_at, updated_at
                    ) VALUES (?1, ?2, 'live', ?3, ?4, ?5, ?6, ?7, ?3, ?8, ?9, ?10, ?11, ?12, 'ts', ?13, ?14, ?14)
                    ON CONFLICT(stable_hash) DO UPDATE SET
                        name = excluded.name,
                        name_clean = excluded.name_clean,
                        group_title = excluded.group_title,
                        tvg_id = excluded.tvg_id,
                        logo_url = excluded.logo_url,
                        chno = excluded.chno,
                        stream_url = excluded.stream_url,
                        updated_at = excluded.updated_at",
                )?;

                for s in chunk {
                    if s.stream_id.is_empty() {
                        continue;
                    }
                    let stream_url =
                        format!("{base}/live/{username}/{password}/{}.ts", s.stream_id);
                    let stable_hash =
                        blake3::hash(format!("{source_uuid}:xtream_live:{}", s.stream_id).as_bytes())
                            .to_hex()
                            .to_string();
                    let display_name = if s.name.is_empty() {
                        format!("Channel {}", s.stream_id)
                    } else {
                        s.name.clone()
                    };
                    let (name_clean, year) = clean_title_and_year(&display_name);
                    let group_title = s
                        .category_id
                        .as_ref()
                        .and_then(|cid| live_cat_map.get(cid))
                        .cloned();
                    let catchup_type = if s.tv_archive > 0 {
                        Some("default")
                    } else {
                        None
                    };

                    stmt.execute(params![
                        source_id,
                        stable_hash,
                        display_name,
                        name_clean,
                        year,
                        group_title,
                        s.epg_channel_id,
                        s.stream_icon,
                        s.num,
                        catchup_type,
                        s.tv_archive_duration,
                        host,
                        stream_url,
                        now
                    ])?;
                    live_count += 1;
                }
            }
            tx.commit()?;
            Ok(())
        })?;
    }

    // 3. Insert VOD Movie streams in batches
    for chunk in vod_streams.chunks(XTREAM_BATCH_SIZE) {
        db.with_conn_mut(|conn| {
            let tx = conn.transaction()?;
            let now = chrono_now_ts();
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO iptv_items (
                        source_id, stable_hash, kind, name, name_clean, year, group_title,
                        logo_url, host, container_ext, stream_url, imdb_id, imdb_rating,
                        created_at, updated_at
                    ) VALUES (?1, ?2, 'movie', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)
                    ON CONFLICT(stable_hash) DO UPDATE SET
                        name = excluded.name,
                        name_clean = excluded.name_clean,
                        year = excluded.year,
                        group_title = excluded.group_title,
                        logo_url = excluded.logo_url,
                        container_ext = excluded.container_ext,
                        stream_url = excluded.stream_url,
                        imdb_rating = COALESCE(excluded.imdb_rating, iptv_items.imdb_rating),
                        updated_at = excluded.updated_at",
                )?;

                for m in chunk {
                    if m.stream_id.is_empty() {
                        continue;
                    }
                    let ext = m
                        .container_extension
                        .as_deref()
                        .filter(|e| !e.is_empty())
                        .unwrap_or("mp4")
                        .to_ascii_lowercase();
                    let stream_url =
                        format!("{base}/movie/{username}/{password}/{}.{ext}", m.stream_id);
                    let stable_hash =
                        blake3::hash(format!("{source_uuid}:xtream_vod:{}", m.stream_id).as_bytes())
                            .to_hex()
                            .to_string();
                    let display_name = if m.name.is_empty() {
                        format!("Movie {}", m.stream_id)
                    } else {
                        m.name.clone()
                    };
                    let (name_clean, year) = clean_title_and_year(&display_name);
                    let group_title = m
                        .category_id
                        .as_ref()
                        .and_then(|cid| vod_cat_map.get(cid))
                        .cloned();
                    let imdb_id = m
                        .tmdb
                        .as_deref()
                        .and_then(extract_imdb_id)
                        .or_else(|| extract_imdb_id(&display_name));
                    let rating = resolve_xtream_rating(m.rating, m.rating_5based);

                    stmt.execute(params![
                        source_id,
                        stable_hash,
                        display_name,
                        name_clean,
                        year,
                        group_title,
                        m.stream_icon,
                        host,
                        ext,
                        stream_url,
                        imdb_id,
                        rating,
                        now
                    ])?;
                    movie_count += 1;
                }
            }
            tx.commit()?;
            Ok(())
        })?;
    }

    // 4. Insert Series catalog entries in batches (episodes are lazily hydrated via get_series_info)
    for chunk in series_entries.chunks(XTREAM_BATCH_SIZE) {
        db.with_conn_mut(|conn| {
            let tx = conn.transaction()?;
            let now = chrono_now_ts();
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO series_catalog (
                        source_id, canonical_key, stable_hash, title, group_title, poster_url,
                        backdrop_url, imdb_rating, plot, episode_count, season_count, updated_at
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, 0, ?10)
                    ON CONFLICT(source_id, canonical_key) DO UPDATE SET
                        stable_hash = excluded.stable_hash,
                        title = excluded.title,
                        group_title = excluded.group_title,
                        poster_url = COALESCE(excluded.poster_url, series_catalog.poster_url),
                        backdrop_url = COALESCE(excluded.backdrop_url, series_catalog.backdrop_url),
                        imdb_rating = COALESCE(excluded.imdb_rating, series_catalog.imdb_rating),
                        plot = COALESCE(excluded.plot, series_catalog.plot),
                        updated_at = excluded.updated_at",
                )?;

                for sr in chunk {
                    if sr.series_id.is_empty() {
                        continue;
                    }
                    let canonical_key = format!("xtream_series_{}", sr.series_id);
                    let s_hash = make_series_stable_hash(source_uuid, &canonical_key);
                    let raw_title = if sr.name.is_empty() {
                        format!("Series {}", sr.series_id)
                    } else {
                        sr.name.clone()
                    };
                    let (clean_title, _) = clean_title_and_year(&raw_title);
                    let group_title = sr
                        .category_id
                        .as_ref()
                        .and_then(|cid| series_cat_map.get(cid))
                        .cloned();
                    let rating = resolve_xtream_rating(sr.rating, sr.rating_5based);

                    stmt.execute(params![
                        source_id,
                        canonical_key,
                        s_hash,
                        clean_title,
                        group_title,
                        sr.cover,
                        sr.backdrop_path,
                        rating,
                        sr.plot,
                        now
                    ])?;
                    series_count += 1;
                }
            }
            tx.commit()?;
            Ok(())
        })?;
    }

    let total_items = live_count + movie_count;
    let duration_ms = started.elapsed().as_millis() as u64;

    db.with_conn(|conn| {
        let now = chrono_now_ts();
        conn.execute(
            "UPDATE iptv_sources
             SET item_count = ?1,
                 last_sync_at = ?2,
                 updated_at = ?2,
                 exp_date = ?3,
                 max_connections = ?4,
                 active_cons = ?5,
                 account_status = ?6,
                 server_utc_offset_secs = COALESCE(?9, server_utc_offset_secs),
                 epg_url = CASE
                     WHEN epg_url IS NULL OR TRIM(epg_url) = '' THEN ?7
                     ELSE epg_url
                 END
             WHERE id = ?8",
            params![
                total_items,
                now,
                account_info.exp_date_ts,
                account_info.max_connections,
                account_info.active_connections,
                account_info.status,
                default_epg_url,
                source_id,
                account_info.server_utc_offset_secs
            ],
        )?;
        Ok(())
    })?;

    Ok(SyncSummary {
        source_id,
        total_items,
        live_count,
        movie_count,
        series_count,
        episode_count: 0,
        duration_ms,
    })
}

pub async fn ensure_xtream_series_episodes(
    db: &DatabaseManager,
    http_pool: &HttpClientPool,
    telemetry: &TelemetryHub,
    series_db_id: i64,
    password: &str,
) -> Result<u32, CndError> {
    let (source_id, source_uuid, server, username, canonical_key, group_title, series_title, existing_ep_count, last_ep_sync) =
        db.with_conn(|conn| {
            let row = conn
                .query_row(
                    "SELECT sc.source_id, s.uuid, s.server, s.username, sc.canonical_key,
                            sc.group_title, sc.title,
                            (SELECT COUNT(*) FROM iptv_items i WHERE i.series_id = sc.id),
                            (SELECT MAX(i.updated_at) FROM iptv_items i WHERE i.series_id = sc.id)
                     FROM series_catalog sc
                     JOIN iptv_sources s ON s.id = sc.source_id
                     WHERE sc.id = ?1",
                    params![series_db_id],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, Option<String>>(5)?,
                            r.get::<_, String>(6)?,
                            r.get::<_, u32>(7)?,
                            r.get::<_, Option<i64>>(8)?,
                        ))
                    },
                )
                .optional()?
                .ok_or_else(|| DbError::NotFound(format!("Series id={series_db_id} not found")))?;
            Ok(row)
        })?;

    let now = chrono_now_ts();
    if existing_ep_count > 0 {
        if let Some(ts) = last_ep_sync {
            if now - ts < SERIES_EPISODES_TTL_SECS {
                return Ok(existing_ep_count);
            }
        }
    }

    let Some(xtream_series_id) = canonical_key.strip_prefix("xtream_series_") else {
        return Ok(existing_ep_count);
    };

    let server_str = server.ok_or_else(|| CndError::Parse {
        message: "Xtream source missing server URL".to_string(),
    })?;
    let username_str = username.ok_or_else(|| CndError::Parse {
        message: "Xtream source missing username".to_string(),
    })?;

    let base = normalize_server_base(&server_str);
    let (host, _) = extract_url_host_and_ext(&base);
    let u_enc = encode_param(&username_str);
    let p_enc = encode_param(password);
    let info_url = format!(
        "{base}/player_api.php?username={u_enc}&password={p_enc}&action=get_series_info&series_id={xtream_series_id}"
    );

    let req = http_pool.get(&info_url)?;
    let resp = http_pool.send_logged("XTREAM", req).await?;
    if !resp.status().is_success() {
        return Err(CndError::Network {
            message: format!("get_series_info failed with HTTP {}", resp.status().as_u16()),
        });
    }

    let bytes = resp.bytes().await.map_err(|e| CndError::Network {
        message: crate::telemetry::redact_text(&e.to_string()),
    })?;

    let parsed = parse_series_info_bytes(&bytes)?;
    let episodes = flatten_series_episodes(&parsed.episodes)?;

    let hydrated_count = db.with_conn_mut(|conn| {
        let tx = conn.transaction()?;
        let ts = chrono_now_ts();

        tx.execute(
            "DELETE FROM iptv_items WHERE series_id = ?1",
            params![series_db_id],
        )?;

        let mut inserted = 0u32;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO iptv_items (
                    source_id, series_id, stable_hash, kind, season_num, episode_num,
                    name, name_clean, group_title, logo_url, host, container_ext,
                    stream_url, imdb_rating, plot, created_at, updated_at
                ) VALUES (
                    ?1, ?2, ?3, 'episode', ?4, ?5,
                    ?6, ?7, ?8, ?9, ?10, ?11,
                    ?12, ?13, ?14, ?15, ?15
                )
                ON CONFLICT(stable_hash) DO UPDATE SET
                    series_id = excluded.series_id,
                    season_num = excluded.season_num,
                    episode_num = excluded.episode_num,
                    name = excluded.name,
                    name_clean = excluded.name_clean,
                    stream_url = excluded.stream_url,
                    updated_at = excluded.updated_at",
            )?;

            for (season_num, ep_idx, ep) in &episodes {
                if ep.id.is_empty() {
                    continue;
                }
                let ext = ep
                    .container_extension
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("mkv")
                    .to_ascii_lowercase();
                let stream_url =
                    format!("{base}/series/{username_str}/{password}/{}.{ext}", ep.id);
                let stable_hash =
                    blake3::hash(format!("{source_uuid}:xtream_ep:{}", ep.id).as_bytes())
                        .to_hex()
                        .to_string();

                let ep_number = ep.episode_num.unwrap_or(*ep_idx).max(1);
                let season_number = ep.season.unwrap_or(*season_num).max(1);
                let raw_title = if ep.title.is_empty() {
                    format!("{series_title} S{season_number:02}E{ep_number:02}")
                } else {
                    ep.title.clone()
                };
                let (name_clean, _) = clean_title_and_year(&raw_title);
                let ep_logo = ep.info.as_ref().and_then(|i| i.movie_image.clone());
                let ep_plot = ep.info.as_ref().and_then(|i| i.plot.clone());
                let ep_rating = ep
                    .info
                    .as_ref()
                    .and_then(|i| i.rating)
                    .filter(|&r| r > 0.0 && r <= 10.0);

                stmt.execute(params![
                    source_id,
                    series_db_id,
                    stable_hash,
                    season_number,
                    ep_number,
                    raw_title,
                    name_clean,
                    group_title,
                    ep_logo,
                    host,
                    ext,
                    stream_url,
                    ep_rating,
                    ep_plot,
                    ts
                ])?;
                inserted += 1;
            }
        }

        let detail_plot = parsed.info.as_ref().and_then(|i| i.plot.clone());
        let detail_cover = parsed.info.as_ref().and_then(|i| i.cover.clone());
        let detail_backdrop = parsed.info.as_ref().and_then(|i| i.backdrop_path.clone());
        let detail_rating = parsed
            .info
            .as_ref()
            .and_then(|i| i.rating)
            .filter(|&r| r > 0.0 && r <= 10.0);

        tx.execute(
            "UPDATE series_catalog SET
                episode_count = (SELECT COUNT(*) FROM iptv_items WHERE series_id = ?1),
                season_count = (SELECT COUNT(DISTINCT COALESCE(season_num, 1)) FROM iptv_items WHERE series_id = ?1),
                plot = COALESCE(?2, plot),
                poster_url = COALESCE(?3, poster_url),
                backdrop_url = COALESCE(?4, backdrop_url),
                imdb_rating = COALESCE(?5, imdb_rating),
                updated_at = ?6
             WHERE id = ?1",
            params![
                series_db_id,
                detail_plot,
                detail_cover,
                detail_backdrop,
                detail_rating,
                ts
            ],
        )?;

        tx.execute(
            "UPDATE iptv_sources
             SET item_count = (SELECT COUNT(*) FROM iptv_items WHERE source_id = ?1),
                 updated_at = ?2
             WHERE id = ?1",
            params![source_id, ts],
        )?;

        tx.commit()?;
        Ok(inserted)
    })?;

    if let Err(e) = db.classify_series_episodes(series_db_id) {
        telemetry.warn("TAXONOMY", format!("Could not classify episodes of series db_id={series_db_id}: {e}"));
    }

    telemetry.info(
        "XTREAM",
        format!("Hydrated {hydrated_count} episodes for Xtream series db_id={series_db_id}"),
    );

    Ok(hydrated_count)
}

fn parse_series_info_bytes(bytes: &[u8]) -> Result<RawSeriesInfoResponse, CndError> {
    serde_json::from_slice::<RawSeriesInfoResponse>(bytes).map_err(|e| CndError::Parse {
        message: format!("Invalid get_series_info JSON: {e}"),
    })
}

fn flatten_series_episodes(
    episodes_val: &serde_json::Value,
) -> Result<Vec<(u32, u32, RawEpisodeEntry)>, CndError> {
    let mut out = Vec::new();
    match episodes_val {
        serde_json::Value::Object(map) => {
            let mut season_keys: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            season_keys.sort_by_key(|(k, _)| k.parse::<u32>().unwrap_or(u32::MAX));
            for (season_key, arr_val) in season_keys {
                let season_num = season_key.parse::<u32>().unwrap_or(1).max(1);
                if let serde_json::Value::Array(items) = arr_val {
                    for (idx, item_val) in items.iter().enumerate() {
                        if let Ok(ep) = serde_json::from_value::<RawEpisodeEntry>(item_val.clone())
                        {
                            out.push((season_num, (idx as u32) + 1, ep));
                        }
                    }
                }
            }
        }
        serde_json::Value::Array(seasons_arr) => {
            for (s_idx, season_val) in seasons_arr.iter().enumerate() {
                if let serde_json::Value::Array(items) = season_val {
                    for (e_idx, item_val) in items.iter().enumerate() {
                        if let Ok(ep) = serde_json::from_value::<RawEpisodeEntry>(item_val.clone())
                        {
                            out.push(((s_idx as u32) + 1, (e_idx as u32) + 1, ep));
                        }
                    }
                }
            }
        }
        _ => {}
    }
    Ok(out)
}

fn resolve_xtream_rating(rating_10: Option<f64>, rating_5: Option<f64>) -> Option<f64> {
    if let Some(r) = rating_10.filter(|&v| v > 0.0) {
        return Some(r.clamp(0.0, 10.0));
    }
    if let Some(r5) = rating_5.filter(|&v| v > 0.0) {
        return Some((r5 * 2.0).clamp(0.0, 10.0));
    }
    None
}

// ============================================================================
// Ultra-tolerant Serde Deserializers for inconsistent PHP Xtream panels
// ============================================================================

fn value_to_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(if *b { "1" } else { "0" }.to_string()),
        _ => None,
    }
}

fn de_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    Ok(value_to_string(&v).unwrap_or_default())
}

fn de_opt_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    Ok(value_to_string(&v))
}

fn de_u32<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    Ok(match v {
        serde_json::Value::Number(n) => n.as_u64().unwrap_or(0) as u32,
        serde_json::Value::String(s) => s.trim().parse::<u32>().unwrap_or(0),
        serde_json::Value::Bool(b) => u32::from(b),
        _ => 0,
    })
}

fn de_opt_u32<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    Ok(match v {
        serde_json::Value::Number(n) => n.as_u64().map(|x| x as u32),
        serde_json::Value::String(s) => s.trim().parse::<u32>().ok(),
        _ => None,
    })
}

fn de_opt_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    Ok(match v {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    })
}

fn de_opt_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    Ok(match v {
        serde_json::Value::Number(n) => n.as_f64().filter(|&x| x.is_finite() && x > 0.0),
        serde_json::Value::String(s) => normalize_imdb_rating(&s),
        _ => None,
    })
}

fn de_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    Ok(match v {
        serde_json::Value::Bool(b) => b,
        serde_json::Value::Number(n) => n.as_i64().unwrap_or(0) != 0,
        serde_json::Value::String(s) => {
            matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes")
        }
        _ => false,
    })
}

fn de_opt_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    Ok(match v {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(b) => Some(b),
        serde_json::Value::Number(n) => Some(n.as_i64().unwrap_or(0) != 0),
        serde_json::Value::String(s) => {
            let lower = s.trim().to_ascii_lowercase();
            if lower.is_empty() {
                None
            } else {
                Some(matches!(lower.as_str(), "1" | "true" | "yes"))
            }
        }
        _ => None,
    })
}

fn de_opt_backdrop<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    Ok(match v {
        serde_json::Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        serde_json::Value::Array(arr) => arr
            .iter()
            .find_map(value_to_string)
            .filter(|s| !s.is_empty()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tolerant_xtream_deserialization_with_quirky_types() {
        let auth_json = br#"{
            "user_info": {
                "auth": "1",
                "status": "Active",
                "exp_date": "1893456000",
                "is_trial": "0",
                "active_cons": "1",
                "max_connections": 4
            },
            "server_info": {
                "timezone": "Europe/Paris",
                "time_now": "2026-11-02 17:30:05",
                "timestamp_now": "1793637000"
            }
        }"#;
        match parse_auth_probe_bytes(auth_json) {
            XtreamAuthProbe::Authenticated(info) => {
                assert_eq!(info.status, "Active");
                assert_eq!(info.exp_date_ts, Some(1_893_456_000));
                assert!(!info.is_trial);
                assert_eq!(info.active_connections, 1);
                assert_eq!(info.max_connections, 4);
                assert_eq!(info.server_timezone.as_deref(), Some("Europe/Paris"));
                // 17:30:05 on the server's clock against 16:30:00 UTC: one hour ahead.
                assert_eq!(info.server_utc_offset_secs, Some(3_600));
            }
            other => panic!("Expected Authenticated, got {other:?}"),
        }

        // VOD streams with mixed int/string IDs, empty ratings, comma ratings, null fields
        let vod_json = br#"[
            {
                "stream_id": 1001,
                "name": "|FR| Interstellar (2014) 4K",
                "rating": "8,7",
                "rating_5based": 4.3,
                "category_id": 12,
                "container_extension": "mkv"
            },
            {
                "stream_id": "1002",
                "name": "Arrival (2016)",
                "rating": "",
                "rating_5based": "4.0",
                "category_id": null,
                "container_extension": ""
            }
        ]"#;
        let vods: Vec<RawVodStream> = parse_flexible_json_array(vod_json).expect("parse vods");
        assert_eq!(vods.len(), 2);
        assert_eq!(vods[0].stream_id, "1001");
        assert_eq!(vods[0].rating, Some(8.7));
        assert_eq!(vods[1].stream_id, "1002");
        assert_eq!(
            resolve_xtream_rating(vods[1].rating, vods[1].rating_5based),
            Some(8.0)
        );

        // Series entries with backdrop_path as array, empty array, and string
        let series_json = br#"[
            {
                "series_id": 55,
                "name": "Dark",
                "cover": "http://img/dark.jpg",
                "backdrop_path": ["http://img/dark_bg1.jpg", "http://img/dark_bg2.jpg"],
                "rating": 8.8
            },
            {
                "series_id": "56",
                "name": "1899",
                "cover": null,
                "backdrop_path": [],
                "rating": ""
            }
        ]"#;
        let series: Vec<RawSeriesEntry> =
            parse_flexible_json_array(series_json).expect("parse series");
        assert_eq!(series.len(), 2);
        assert_eq!(
            series[0].backdrop_path.as_deref(),
            Some("http://img/dark_bg1.jpg")
        );
        assert_eq!(series[1].backdrop_path, None);

        // get_series_info with map episodes AND empty array episodes
        let series_info_json = br#"{
            "info": {
                "plot": "Time travel mystery",
                "backdrop_path": "http://img/bg.jpg",
                "rating": "8.8"
            },
            "episodes": {
                "1": [
                    {"id": 9001, "episode_num": "1", "season": 1, "title": "Secrets", "container_extension": "mkv"},
                    {"id": "9002", "episode_num": 2, "season": "1", "title": "Lies", "container_extension": "mp4"}
                ],
                "2": [
                    {"id": 9101, "episode_num": 1, "season": 2, "title": "Beginnings", "container_extension": "mkv"}
                ]
            }
        }"#;
        let parsed = parse_series_info_bytes(series_info_json).expect("parse series info");
        let eps = flatten_series_episodes(&parsed.episodes).expect("flatten");
        assert_eq!(eps.len(), 3);
        assert_eq!(eps[0].2.id, "9001");
        assert_eq!(eps[2].0, 2);
    }
}
