//! HTTP control plane + media plane (the airflow_server.dll equivalent).
//!
//! Routes observed on the original server: `/` (hello banner), `/verification-id`,
//! `/inspect`, `/master.m3u8`, `/playlist/`, `/segment/`, `/live-transcoding`.
//! Airstream keeps the same shape — a LAN HTTP server that devices pull HLS or
//! the raw file from — and adds a small JSON API for the cast-only UI.

use crate::devices::{self, Device};
use crate::library::{Library, Settings};
use crate::probe;
use crate::transcode::{Quality, Transcoder};
use axum::{
    body::Body,
    extract::{ConnectInfo, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use std::net::{SocketAddr, TcpListener as StdListener};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// What is currently being pushed to a TV.
#[derive(Clone, Default, serde::Serialize)]
pub struct NowPlaying {
    pub device_id: String,
    pub device_name: String,
    #[serde(rename = "item_id")]
    pub item_id: String,
    pub title: String,
    pub url: String,
    pub mode: String,
    pub quality: String,
    pub hls: bool,
    /// hls (on-demand fMP4 HLS) | mp4 (prepared) | direct
    pub stream: String,
}

pub struct AppState {
    pub library: Library,
    pub transcoder: Transcoder,
    pub pipes: crate::stream::PipeRegistry,
    pub port: u16,
    pub lan_ip: String,
    pub devices: Mutex<Vec<Device>>,
    pub now: Mutex<NowPlaying>,
    /// Live Google Cast session (TLS 8009). Reused while the same TV is targeted.
    pub cast: Mutex<Option<Arc<crate::cast::CastClient>>>,
    /// Prepared (faststart) MP4 files: cast targets that support exact seeking.
    pub prep: Mutex<std::collections::HashMap<String, PrepState>>,
    pub prep_keys: Mutex<std::collections::HashMap<String, String>>,
    /// The one speculative (pre-Play) preparation; replaced or promoted on demand.
    pub bg_prepare: Mutex<Option<(String, tokio::task::AbortHandle)>>,
    pub subtitle_fetches: std::sync::atomic::AtomicU64,
    /// Only one filesystem search for dropped files runs at a time.
    pub drop_scan: tokio::sync::Mutex<()>,
    /// What the user wants from the TV right now (drives automatic recovery).
    pub intent: Mutex<Option<PlayIntent>>,
    /// Bumped by every user-initiated cast; running recoveries abort on change.
    pub intent_gen: std::sync::atomic::AtomicU64,
    pub recovery: Mutex<Recovery>,
    /// Bumped by every successful LOAD (user or recovery).
    pub load_gen: std::sync::atomic::AtomicU64,
    /// When we last sent a LOAD (our own relaunch/LOAD transiently shows no player).
    pub last_load_at: Mutex<Option<std::time::Instant>>,
    /// Number of user-initiated casts currently running (whole api_cast LOAD path).
    pub user_cast_inflight: std::sync::atomic::AtomicU32,
    /// Serialises connect+LOAD (user Play vs. automatic recovery).
    pub cast_lock: tokio::sync::Mutex<()>,
    /// On-demand HLS VOD sessions (instant start for Original quality).
    pub hls: crate::hls_vod::HlsManager,
}

/// Marks a user cast as in flight for its whole duration (supervisor stays hands-off).
pub struct CastInflight(Arc<AppState>);
impl CastInflight {
    fn new(st: &Arc<AppState>) -> Self {
        st.user_cast_inflight.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        CastInflight(st.clone())
    }
}
impl Drop for CastInflight {
    fn drop(&mut self) { self.0.user_cast_inflight.fetch_sub(1, std::sync::atomic::Ordering::SeqCst); }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wanted { Playing, Paused, Stopped }

/// How to reload the current item after a lost TV session.
#[derive(Clone, Debug)]
pub struct CastPlan {
    pub url: String,
    /// The URL is an on-demand HLS VOD playlist (LOAD as application/x-mpegurl).
    pub hls: bool,
    pub subtitle: Option<serde_json::Value>,
}

#[derive(Clone, Debug)]
pub struct PlayIntent {
    pub item_id: String,
    pub device_id: String,
    pub wanted: Wanted,
    pub last_pos: f64,
    pub last_ok_at: std::time::Instant,
    pub plan: CastPlan,
    /// Status-derived updates are ignored until this moment (user just acted).
    pub locked_until: Option<std::time::Instant>,
    /// Last time the supervisor relaunched a crashed player (once per minute).
    pub crash_relaunch_at: Option<std::time::Instant>,
}

#[derive(Clone, Debug, Default)]
pub struct Recovery {
    pub recovering: bool,
    pub attempt: u32,
    pub error: String,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct PrepState {
    pub state: String,   // preparing | ready | error
    pub progress: f64,   // 0..100
    pub file: String,
    pub note: String,
}

impl AppState {
    pub fn new() -> AppState {
        let library = Library::load();
        let root = crate::library::data_dir().join("hls");
        let transcoder = Transcoder::new(root);
        // Prefer the port the original Airflow uses (TVs/firewalls already trust
        // it); fall back to the next free port when it is taken.
        let preferred = std::env::var("AIRSTREAM_PORT")
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(42015);
        let port = pick_port(preferred);
        let lan_ip = lan_ip();
        tracing::info!(
            "airstream {APP_VERSION} on http://{lan_ip}:{port} — {}",
            transcoder.hw.summary
        );
        AppState {
            library,
            transcoder,
            pipes: crate::stream::PipeRegistry::new(),
            port,
            lan_ip,
            devices: Mutex::new(std::fs::read(crate::library::data_dir().join("devices-cache.json")).ok().and_then(|v| serde_json::from_slice(&v).ok()).unwrap_or_default()),
            now: Mutex::new(NowPlaying::default()),
            cast: Mutex::new(None),
            prep: Mutex::new(std::collections::HashMap::new()),
            prep_keys: Mutex::new(std::collections::HashMap::new()),
            bg_prepare: Mutex::new(None),
            subtitle_fetches: std::sync::atomic::AtomicU64::new(0),
            drop_scan: tokio::sync::Mutex::new(()),
            intent: Mutex::new(None),
            intent_gen: std::sync::atomic::AtomicU64::new(0),
            recovery: Mutex::new(Recovery::default()),
            load_gen: std::sync::atomic::AtomicU64::new(0),
            last_load_at: Mutex::new(None),
            user_cast_inflight: std::sync::atomic::AtomicU32::new(0),
            cast_lock: tokio::sync::Mutex::new(()),
            hls: crate::hls_vod::HlsManager::new(crate::library::data_dir().join("hls"), 8 * 1024 * 1024 * 1024),
        }
    }

    pub fn settings(&self) -> Settings {
        self.library.prefs.lock().unwrap().settings.clone()
    }

    pub fn save_settings(&self, s: Settings) {
        self.library.prefs.lock().unwrap().settings = s;
        self.library.save();
    }

    fn find_item(&self, id: &str) -> Option<crate::library::Item> {
        let prefs = self.library.prefs.lock().unwrap();
        prefs
            .playlists
            .iter()
            .flat_map(|p| p.items.iter())
            .find(|i| i.id == id)
            .cloned()
    }

    fn device_by_id(&self, id: &str) -> Option<Device> {
        self.devices.lock().unwrap().iter().find(|d| d.id == id).cloned()
    }

    /// A user-initiated cast takes over: running recoveries abort, old error cleared.
    fn begin_user_cast(&self) {
        self.intent_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *self.recovery.lock().unwrap() = Recovery::default();
    }

    fn set_intent(&self, item_id: &str, device_id: &str, plan: CastPlan, pos: f64) {
        let now = std::time::Instant::now();
        *self.intent.lock().unwrap() = Some(PlayIntent {
            item_id: item_id.into(), device_id: device_id.into(), wanted: Wanted::Playing,
            last_pos: pos, last_ok_at: now, plan, locked_until: Some(now + std::time::Duration::from_secs(3)),
            crash_relaunch_at: None,
        });
    }

    fn clear_intent(&self) {
        *self.intent.lock().unwrap() = None;
    }

    /// The user paused/resumed/stopped: that wish beats whatever the TV reports for a moment.
    fn user_wants(&self, wanted: Wanted) {
        self.intent_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(i) = self.intent.lock().unwrap().as_mut() {
            tracing::info!(?wanted, item = %i.item_id, position = i.last_pos, "intent set by user");
            i.wanted = wanted;
            i.locked_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(4));
        }
        *self.recovery.lock().unwrap() = Recovery::default();
    }
}


fn valid_index(index: i32, len: usize) -> bool {
    index >= 0 && (index as usize) < len
}

fn language_hint(setting: &str) -> Option<String> {
    let first = setting.split(';').next()?.trim();
    if first.is_empty() || first.eq_ignore_ascii_case("none") || first.eq_ignore_ascii_case("auto") {
        None
    } else {
        Some(probe::canonical_lang(first))
    }
}

/// Resolves the audio/subtitle indexes for THIS file from the saved choices.
/// Remembered choices (by language/title/codec) follow the user from episode
/// to episode. A positional index is trusted only for the file it was saved
/// for, so one movie's track numbers never leak into another file.
fn resolve_tracks(mi: &probe::MediaInfo, s: &Settings, item_id: &str) -> (i32, i32) {
    let same_item = s.last_item == item_id;
    let audio = if let Some(pref) = &s.audio_pref {
        probe::pick_track(&mi.audio_tracks, pref, false)
    } else if same_item && valid_index(s.audio_track, mi.audio_tracks.len()) {
        s.audio_track
    } else if let Some(lang) = language_hint(&s.audio_language) {
        mi.audio_tracks.iter().position(|t| probe::canonical_lang(&t.lang) == lang).map(|i| i as i32).unwrap_or(-1)
    } else {
        -1
    };
    let subtitle = if s.subtitle_off {
        -1
    } else if let Some(pref) = &s.subtitle_pref {
        probe::pick_track(&mi.sub_tracks, pref, true)
    } else if same_item && valid_index(s.subtitle_track, mi.sub_tracks.len()) {
        s.subtitle_track
    } else if let Some(lang) = language_hint(&s.subtitle_language) {
        mi.sub_tracks.iter().position(|t| t.burnable && probe::canonical_lang(&t.lang) == lang).map(|i| i as i32).unwrap_or(-1)
    } else {
        -1
    };
    (audio, subtitle)
}

fn pick_port(preferred: u16) -> u16 {
    for p in preferred..preferred + 40 {
        if StdListener::bind(("0.0.0.0", p)).is_ok() {
            return p;
        }
    }
    preferred
}

fn lan_ip() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".into())
}

pub async fn serve(state: Arc<AppState>) -> Result<(), Box<dyn std::error::Error>> {
    {
        let st = state.clone();
        tokio::task::spawn_blocking(move || trim_prepared_cache(&st));
    }
    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(js))
        .route("/api/ping", get(api_ping))
        .route("/api/rate", post(api_rate))
        .route("/subtitles/:id/:track", get(subtitle_vtt))
        .route("/api/subtitle_debug", get(subtitle_debug))
        .route("/logo.png", get(logo_png))
        .route("/logo.svg", get(logo_svg))
        .route("/favicon.ico", get(logo_png))
        .route("/style.css", get(css))
        .route("/api/state", get(api_state))
        .route("/api/settings", get(api_get_settings).post(api_set_settings))
        .route("/api/add", post(api_add))
        .route("/api/add_dropped", post(api_add_dropped))
        .route("/api/playlist/move", post(api_playlist_move))
        .route("/api/playlist/remove", post(api_playlist_remove))
        .route("/api/pick_folder", post(api_pick_folder))
        .route("/api/pick_files", post(api_pick_files))
        .route("/api/open_prefs", post(api_open_prefs))
        .route("/api/item/:id", delete(api_remove_item))
        .route("/api/item/:id/info", post(api_item_info))
        .route("/api/item/:id/tracks", get(api_item_tracks))
        .route("/api/thumb/:id", get(api_thumb))
        .route("/api/devices", get(api_devices))
        .route("/api/cast", post(api_cast))
        .route("/api/transport", post(api_transport))
        .route("/api/seek", post(api_seek))
        .route("/api/position", get(api_position))
        .route("/api/now", get(api_now))
        .route("/api/cast_debug", get(api_cast_debug))
        .route("/api/prepare/:id", get(api_prepare_status))
        .route("/api/play", post(api_play))
        .route("/api/stats", get(api_stats))
        .route("/media/:id", get(media))
        .route("/prepared/:id", get(prepared_media))
        .route("/stream/:id", get(stream_media))
        .route("/hls/:sid/:file", get(hls_file))
        .route("/vod/:sid/:file", get(vod_file).options(vod_options))
        .layer(axum::middleware::from_fn(lan_guard))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", state.port)).await?;
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

/// The media server must be reachable by the TV, but the control API (add
/// files, settings, folder pickers, playlist edits) is for this PC only.
async fn lan_guard(
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<SocketAddr>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = req.uri().path();
    let media = ["/media/", "/prepared/", "/subtitles/", "/stream/", "/hls/", "/vod/"].iter().any(|p| path.starts_with(p));
    if !peer.ip().is_loopback() && !media {
        tracing::warn!(peer = %peer, path = %path, "blocked LAN request to control API");
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(req).await
}

// ---------------------------------------------------------------- static UI

const LOGO_PNG: &[u8] = include_bytes!("../ui/logo.png");
const LOGO_SVG: &str = include_str!("../ui/logo.svg");

/// UI health probe only; missing browser pings never stop the backend.
pub static LAST_PING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

async fn api_ping() -> impl IntoResponse {
    LAST_PING.store(now_secs(), std::sync::atomic::Ordering::Relaxed);
    Json(json!({"ok": true, "app": "airstream"}))
}

async fn logo_png() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "max-age=86400")], LOGO_PNG)
}

async fn logo_svg() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/svg+xml")], LOGO_SVG)
}

const INDEX_HTML: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const APP_CSS: &str = include_str!("../ui/style.css");

async fn index() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], INDEX_HTML)
}
async fn js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/javascript")], APP_JS)
}
async fn css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css")], APP_CSS)
}

// ---------------------------------------------------------------- API

async fn api_state(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let prefs = st.library.prefs.lock().unwrap().clone();
    let now = now_json(&st);
    Json(json!({
        "version": APP_VERSION,
        "backend": "rust",
        "port": st.port,
        "lanIp": st.lan_ip,
        "hardware": st.transcoder.hw.summary,
        "codecs": {
            "nvenc": st.transcoder.hw.nvenc,
            "amf": st.transcoder.hw.amf,
            "qsv": st.transcoder.hw.qsv,
        },
        "prefsPath": st.library.path.to_string_lossy().to_string(),
        "now": now,
        "prefs": prefs,
    }))
}

async fn api_get_settings(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    Json(st.settings())
}

async fn api_set_settings(State(st): State<Arc<AppState>>, Json(s): Json<Settings>) -> impl IntoResponse {
    if !valid_playback_rate(s.playback_rate) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"เลือกความเร็ว ×1, ×1.25, ×1.5, ×1.75 หรือ ×2"}))).into_response();
    }
    let mut s = s;
    s.looping = false;
    st.save_settings(s.clone());
    Json(json!({ "ok": true, "settings": s })).into_response()
}

#[derive(Deserialize)]
struct AddReq {
    paths: Vec<String>,
}

async fn api_add(State(st): State<Arc<AppState>>, Json(req): Json<AddReq>) -> impl IntoResponse {
    let (added, skipped) = st.library.add_paths(&req.paths);
    Json(json!({ "added": added, "skipped": skipped }))
}

#[derive(Deserialize)]
struct DroppedReq {
    files: Vec<crate::dropfind::DroppedFile>,
    #[serde(default)]
    before: Option<String>,
}

/// Adds files dropped from Explorer. Browser JavaScript only sees name, size
/// and modified time, so the local path is recovered by matching all three.
/// This searches the user's disks, therefore it is limited to this computer.
async fn api_add_dropped(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<AppState>>,
    Json(req): Json<DroppedReq>,
) -> Response {
    if !peer.ip().is_loopback() {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "drag and drop import is local-only"}))).into_response();
    }
    if req.files.is_empty() || req.files.len() > 1000 {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "ส่งไฟล์ได้ 1-1000 รายการต่อครั้ง"}))).into_response();
    }
    let Ok(_scan_guard) = st.drop_scan.try_lock() else {
        return (StatusCode::CONFLICT, Json(json!({"error": "กำลังค้นหาไฟล์จากการวางก่อนหน้า กรุณารอสักครู่"}))).into_response();
    };
    let (video, unsupported): (Vec<_>, Vec<_>) = req.files.into_iter().partition(|f| {
        std::path::Path::new(&f.name)
            .extension()
            .map(|e| crate::library::VIDEO_EXTS.contains(&e.to_string_lossy().to_lowercase().as_str()))
            .unwrap_or(false)
    });
    let started = std::time::Instant::now();
    let to_search = video.clone();
    let (found, timed_out) = tokio::task::spawn_blocking(move || crate::dropfind::resolve(&to_search, std::time::Duration::from_secs(25)))
        .await
        .unwrap_or((Vec::new(), false));
    let mut paths = Vec::new();
    let mut missing = Vec::new();
    for (file, path) in video.iter().zip(found.iter()) {
        match path {
            Some(path) => paths.push(path.to_string_lossy().to_string()),
            None => missing.push(file.name.clone()),
        }
    }
    let (added, skipped) = st.library.add_paths_at(&paths, req.before.as_deref());
    Json(json!({
        "added": added,
        "skipped": skipped,
        "missing": missing,
        "unsupported": unsupported.into_iter().map(|f| f.name).collect::<Vec<_>>(),
        "timed_out": timed_out,
        "seconds": started.elapsed().as_secs_f64(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct MoveReq {
    #[serde(default)]
    id: String,
    #[serde(default)]
    ids: Vec<String>,
    #[serde(default)]
    before: Option<String>,
}

#[derive(Deserialize)]
struct RemoveManyReq {
    ids: Vec<String>,
}

async fn api_playlist_remove(State(st): State<Arc<AppState>>, Json(req): Json<RemoveManyReq>) -> Response {
    if req.ids.is_empty() || req.ids.len() > 5000 {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "ไม่มีรายการที่เลือก"}))).into_response();
    }
    let removed = st.library.remove_items(&req.ids);
    Json(json!({"ok": true, "removed": removed})).into_response()
}

async fn api_playlist_move(State(st): State<Arc<AppState>>, Json(req): Json<MoveReq>) -> Response {
    let ok = if req.ids.is_empty() {
        st.library.move_item(&req.id, req.before.as_deref())
    } else {
        st.library.move_items(&req.ids, req.before.as_deref())
    };
    if ok {
        Json(json!({"ok": true})).into_response()
    } else {
        (StatusCode::CONFLICT, Json(json!({"error": "รายการเปลี่ยนไปแล้ว กำลังรีเฟรช"}))).into_response()
    }
}

async fn api_open_prefs(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let dir = st.library.path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let _ = std::process::Command::new("explorer")
        .arg(dir.to_string_lossy().to_string())
        .spawn();
    Json(json!({ "ok": true, "path": dir.to_string_lossy() }))
}

async fn api_pick_folder() -> impl IntoResponse {
    let path = tokio::task::spawn_blocking(pick_folder_dialog).await.ok().flatten();
    Json(json!({ "path": path }))
}

async fn api_pick_files() -> impl IntoResponse {
    let path = tokio::task::spawn_blocking(pick_files_dialog).await.ok().flatten();
    Json(json!({ "path": path }))
}

fn pick_folder_dialog() -> Option<String> {
    let script = r#"Add-Type -AssemblyName System.Windows.Forms | Out-Null
$f = New-Object System.Windows.Forms.FolderBrowserDialog
$f.Description = 'Add a folder to Airstream'
if ($f.ShowDialog() -eq 'OK') { $f.SelectedPath }"#;
    run_ps(script)
}

fn pick_files_dialog() -> Option<String> {
    let script = r#"Add-Type -AssemblyName System.Windows.Forms | Out-Null
$f = New-Object System.Windows.Forms.OpenFileDialog
$f.Multiselect = $true
$f.Filter = 'Video|*.mkv;*.mp4;*.m4v;*.avi;*.mov;*.ts;*.m2ts;*.webm'
if ($f.ShowDialog() -eq 'OK') { $f.FileNames -join "`n" }"#;
    run_ps(script)
}

fn run_ps(script: &str) -> Option<String> {
    let out = crate::process::command("powershell")
        .args(["-NoProfile", "-STA", "-Command", script])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

async fn api_remove_item(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    {
        let mut prefs = st.library.prefs.lock().unwrap();
        for p in prefs.playlists.iter_mut() {
            p.items.retain(|i| i.id != id);
        }
    }
    st.library.save();
    Json(json!({ "ok": true }))
}

async fn api_item_info(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    let item = st.find_item(&id);
    let Some(item) = item else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    let mi = tokio::task::spawn_blocking(move || probe::probe(&item.path))
        .await
        .unwrap_or_default();

    {
        let mut prefs = st.library.prefs.lock().unwrap();
        for p in prefs.playlists.iter_mut() {
            for it in p.items.iter_mut() {
                if it.id == id {
                    it.duration = mi.duration;
                    it.width = mi.width;
                    it.height = mi.height;
                    it.vcodec = mi.vcodec.clone();
                    it.acodec = mi.acodec.clone();
                    if mi.size > 0 {
                        it.size = mi.size;
                    }
                }
            }
        }
    }
    st.library.save();
    Json(json!(mi)).into_response()
}

/// Audio/subtitle track list for the track pickers (no typing required).
async fn api_item_tracks(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    let Some(item) = st.find_item(&id) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response();
    };
    let item_id = item.id.clone();
    let mi = tokio::task::spawn_blocking(move || probe::probe(&item.path))
        .await
        .unwrap_or_default();
    let (audio, subtitle) = resolve_tracks(&mi, &st.settings(), &item_id);
    Json(json!({
        "audio": mi.audio_tracks,
        "subtitles": mi.sub_tracks,
        "duration": mi.duration,
        "resolved": { "audio": audio, "subtitle": subtitle },
    }))
    .into_response()
}

async fn api_thumb(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let item = st.find_item(&id);
    let Some(item) = item else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let file = st.library.thumbs.join(format!("{id}.jpg"));
    if !file.exists() {
        let src = item.path.clone();
        let out = file.clone();
        let seek = if item.duration > 120.0 { item.duration * 0.25 } else { 30.0 };
        let out2 = out.clone();
        let ok = tokio::task::spawn_blocking(move || probe::thumbnail(&src, &out2, seek))
            .await
            .unwrap_or(false);
        if !ok && !out.exists() {
            return StatusCode::NOT_FOUND.into_response();
        }
    }
    match std::fs::read(&file) {
        Ok(bytes) => {
            let mut hm = HeaderMap::new();
            hm.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
            hm.insert(header::CACHE_CONTROL, HeaderValue::from_static("max-age=86400"));
            (StatusCode::OK, hm, bytes).into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

// Snapshot only: network discovery never blocks the UI request.
async fn api_devices(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let list = st.devices.lock().unwrap().clone();
    Json(json!({ "devices": list }))
}

/// Keeps a warm Cast channel to the last used TV (TCP+TLS+receiver status,
/// attaching to a running player but never launching one), so Play only has
/// to send LOAD. Retries quietly while the TV is off.
pub fn start_cast_preconnect(st: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            let alive = st.cast.lock().unwrap().as_ref().map(|c| c.status().connected).unwrap_or(false);
            // While the user is watching, the recovery supervisor owns reconnection
            // (it reloads the item); a warm channel here would race with it.
            let watching = st.intent.lock().unwrap().as_ref().is_some_and(|i| i.wanted == Wanted::Playing);
            if !alive && !watching {
                let last = st.settings().last_device;
                if let Some(dev) = st.device_by_id(&last).filter(|d| d.kind == "cast") {
                    match crate::cast::connect_opts(&dev.host, dev.port, &dev.name, false).await {
                        Ok(c) => {
                            let mut slot = st.cast.lock().unwrap();
                            // Never replace a channel a Play created meanwhile.
                            if slot.as_ref().map(|c| !c.status().connected).unwrap_or(true) {
                                tracing::info!(device = %dev.name, "warm cast channel ready");
                                *slot = Some(Arc::new(c));
                            }
                        }
                        Err(e) => tracing::debug!(error = %e, "cast pre-connect failed"),
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    });
}

pub fn start_device_discovery(st: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            let mut list = devices::discover().await;
            // E2E hook (headless test backends): a mock Cast receiver
            // "name|host|port" that mDNS cannot announce.
            if let Ok(spec) = std::env::var("AIRSTREAM_TEST_CAST") {
                let f: Vec<&str> = spec.split('|').collect();
                if f.len() == 3 {
                    list.push(Device { id: format!("cast:test-{}", f[0]), name: f[0].into(), kind: "cast".into(), host: f[1].into(), port: f[2].parse().unwrap_or(8009), model: "mock".into(), location: String::new(), control_url: String::new() });
                }
            }
            // An empty scan is authoritative for this session (no ghost TVs).
            *st.devices.lock().unwrap() = list.clone();
            if let Ok(data) = serde_json::to_vec(&list) {
                let path = crate::library::data_dir().join("devices-cache.json");
                let _ = tokio::fs::write(path, data).await;
            }
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        }
    });
}

#[derive(Deserialize)]
struct CastReq {
    #[serde(default)]
    device: String,
    id: String,
    /// optional override: passthrough | variable-720p | variable-1080p | variable-1080p-hq
    #[serde(default)]
    quality: Option<String>,
    /// Prepare the file (remux/subtitles) without touching the TV: used to
    /// pre-warm the next episode so there is no wait at the end of this one.
    #[serde(default)]
    prepare_only: bool,
    /// Immutable UI snapshot: background preparation never changes preferences.
    #[serde(default)]
    settings: Option<Settings>,
    /// Selection warm-up: cancellable when the user picks something else.
    #[serde(default)]
    speculative: bool,
    /// Ignore the saved stop position and play from 0:00.
    #[serde(default)]
    from_start: bool,
}

pub struct CastOutcome {
    pub started: bool,
}

pub enum CastFail {
    /// The receiver answered with an error (LOAD_FAILED ...).
    Rejected(String),
    /// No usable answer from the TV.
    Unanswered(String),
    /// An HLS LOAD was refused or never started playing although the TV answered:
    /// the receiver is now remembered as not supporting our HLS.
    HlsFailed(String),
}

/// Connect (reusing a live channel when it answers), make sure the player runs,
/// LOAD the item and wait for a real media session. Shared by api_cast and the
/// automatic recovery.
pub async fn start_cast(
    st: &Arc<AppState>,
    device: &Device,
    item: &crate::library::Item,
    settings: &Settings,
    plan: &CastPlan,
    start_at: f64,
) -> Result<CastOutcome, CastFail> {
    let _serial = st.cast_lock.lock().await;
    let url = &plan.url;
    let t_cast = std::time::Instant::now();
    // Two attempts: the first reuses the live/warm channel; if the TV does
    // not answer (screensaver closed the player, TV slept and woke), the
    // second starts from a brand-new connection. Success = a real media
    // session in PLAYING/BUFFERING, never just "LOAD was written".
    let mut t_ready = 0u64;
    let mut started = false;
    let mut accepted = false;
    let mut last_err = String::new();
    for attempt in 0..2 {
        let existing = if attempt == 0 {
            let guard = st.cast.lock().unwrap();
            match guard.as_ref() {
                Some(c) if c.status().connected && c.host == device.host => Some(c.clone()),
                _ => None,
            }
        } else { None };
        // Prove the channel answers before trusting its cached state.
        let mut client = None;
        if let Some(c) = existing {
            if c.refresh(1500).await {
                if c.ensure_player().await { client = Some(c) }
            } else {
                tracing::warn!("cast channel did not answer; reconnecting");
                c.close();
            }
        }
        if client.is_none() {
            if let Some(old) = st.cast.lock().unwrap().take() { old.close(); }
            match crate::cast::connect(&device.host, device.port, &device.name).await {
                Ok(c) => {
                    let arc = Arc::new(c);
                    *st.cast.lock().unwrap() = Some(arc.clone());
                    if !arc.ensure_player().await {
                        last_err = "TV did not open the media player".into();
                        continue;
                    }
                    client = Some(arc);
                }
                Err(e) => { last_err = e; continue; }
            }
        }
        let client = client.unwrap();
        t_ready = t_cast.elapsed().as_millis() as u64;
        *st.last_load_at.lock().unwrap() = Some(std::time::Instant::now());
        let kind = if plan.hls { crate::cast::LoadKind::HlsVod } else { crate::cast::LoadKind::Mp4 };
        if !crate::cast::load(&client, url, &item.title, kind, plan.subtitle.clone(), settings.subtitle_scale, settings.playback_rate, start_at) {
            last_err = "cast channel closed".into();
            continue;
        }
        let mut failed = false;
        // HLS gets 12 s to reach PLAYING (first segment may need producing); MP4 8 s as before.
        for i in 0..(if plan.hls { 120 } else { 80 }) {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let s = client.status();
            if s.media_session_id != 0 {
                accepted = true;
                if s.player_state == "PLAYING" { started = true; break; }
            }
            if !s.last_error.is_empty() {
                if plan.hls {
                    tracing::warn!(error = %s.last_error, item = %item.id, device = %device.name, "HLS LOAD rejected by receiver; remembering hls=false");
                    crate::devices::set_cast_hls_support(&device.host, &device.name, false);
                    return Err(CastFail::HlsFailed(s.last_error.clone()));
                }
                return Err(CastFail::Rejected(s.last_error.clone()));
            }
            // No reply at all to LOAD in 4 s: the player is gone. Retry fresh.
            if i == 40 && !accepted { failed = true; break; }
            if !s.connected { failed = true; break; }
        }
        if failed {
            tracing::warn!(attempt, "TV did not answer LOAD; retrying on a fresh connection");
            client.drop_player();
            last_err = "TV did not answer".into();
            continue;
        }
        break;
    }
    *st.last_load_at.lock().unwrap() = Some(std::time::Instant::now());
    if !accepted { return Err(CastFail::Unanswered(last_err)) }
    if plan.hls {
        if started {
            if crate::devices::cast_hls_support(&device.host, &device.name) != Some(true) {
                crate::devices::set_cast_hls_support(&device.host, &device.name, true);
            }
        } else {
            tracing::warn!(item = %item.id, device = %device.name, "HLS LOAD accepted but no PLAYING within 12 s; remembering hls=false");
            crate::devices::set_cast_hls_support(&device.host, &device.name, false);
            return Err(CastFail::HlsFailed("no PLAYING within 12 s".into()));
        }
    }
    st.load_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    tracing::info!(player_ready_ms = t_ready, playing_ms = t_cast.elapsed().as_millis() as u64, started, item = %item.id, "cast timing");
    Ok(CastOutcome { started })
}

async fn api_cast(State(st): State<Arc<AppState>>, Json(req): Json<CastReq>) -> impl IntoResponse {
    tracing::info!(item = %req.id, device = %req.device, prepare_only = req.prepare_only, "cast request");
    // A user Play supersedes any pending automatic recovery from the very first moment.
    let _inflight = if req.prepare_only { None } else { st.begin_user_cast(); Some(CastInflight::new(&st)) };
    let Some(item) = st.find_item(&req.id) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"item not found"}))).into_response();
    };
    let device = st.device_by_id(&req.device);
    if device.is_none() && !req.prepare_only {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"unknown device — refresh the device list"})),
        )
            .into_response();
    };

    let settings = if req.prepare_only { req.settings.unwrap_or_else(|| st.settings()) } else { st.settings() };
    let quality = Quality::parse(req.quality.as_deref().unwrap_or(&settings.quality));

    let path = item.path.clone();
    let mi = tokio::task::spawn_blocking(move || probe::probe(&path))
        .await
        .unwrap_or_default();
    let is_mp4 = {
        let p = item.path.to_lowercase();
        p.ends_with(".mp4") || p.ends_with(".m4v")
    };

    // A plain MP4 the receiver can decode is streamed straight from disk.
    let (audio_track, subtitle_track) = resolve_tracks(&mi, &settings, &item.id);
    if subtitle_track >= 0 && !mi.sub_tracks[subtitle_track as usize].burnable {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"Selected bitmap subtitles cannot be burned in by this renderer; choose a text subtitle track."}))).into_response();
    }
    let native = quality == Quality::Passthrough;
    let native_video_ok = matches!(mi.vcodec.as_str(), "h264" | "hevc");
    // Original quality is strict: no silent 10-bit->8-bit, codec conversion or
    // audio downmix when a receiver does not support the original stream.
    if native && !native_video_ok {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, Json(json!({"error": "Original-quality playback supports H.264/HEVC here. Refusing silent video conversion."}))).into_response();
    }
    if !native && (mi.bit_depth > 8 || mi.hdr) {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, Json(json!({"error": "This source is 10/12-bit or HDR. Choose Original quality + separate subtitles; refusing precision loss or unverified HDR conversion."}))).into_response();
    }
    let audio_index = if audio_track >= 0 { audio_track as usize } else { 0 };
    // The receiver decides what it can play: Dolby on a box that reports no
    // Dolby support (or DTS/TrueHD/FLAC anywhere) would be silent, so convert
    // that one audio track to AAC with the same channel count. Video stays
    // untouched, and the conversion is shown in the UI, never hidden.
    let cast_dev = device.as_ref().filter(|d| d.kind == "cast").map(|d| (d.host.clone(), d.name.clone()))
        .or_else(|| st.device_by_id(&settings.last_device).filter(|d| d.kind == "cast").map(|d| (d.host, d.name)));
    let dolby_ok = match &cast_dev { Some((h, n)) => crate::devices::cast_dolby_support(h, n).await, None => true };
    let mut audio_note = String::new();
    let mut audio_aac_channels = 0u32;
    if let Some(audio) = mi.audio_tracks.get(audio_index) {
        let playable = match audio.codec.as_str() {
            "aac" | "mp3" => true,
            "ac3" | "eac3" => dolby_ok,
            _ => false,
        };
        if !playable {
            audio_aac_channels = audio.channels.clamp(1, 6);
            audio_note = format!("เสียง {} → AAC {}ch (ทีวีเล่น {} ไม่ได้)", audio.codec.to_uppercase(), audio_aac_channels, audio.codec.to_uppercase());
        }
    }
    if native && subtitle_track >= 0 && !req.prepare_only && device.as_ref().is_some_and(|d| d.kind != "cast") {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, Json(json!({"error":"Separate subtitles are currently supported for Google Cast only. Refusing to silently omit selected subtitles."}))).into_response();
    }
    let render_subtitle = if native { -1 } else { subtitle_track };
    let enc_tag = format!("{}{}", if native { "original-copy" } else { settings.encoder.as_str() }, if audio_aac_channels > 0 { format!("+aac{audio_aac_channels}") } else { String::new() });
    let cache_key = preparation_key(&item.path, quality, &enc_tag, audio_track, render_subtitle, if native { 1.0 } else { settings.subtitle_scale });
    let direct_ok = native && native_video_ok && is_mp4 && audio_track < 0 && audio_aac_channels == 0;
    let native_subtitle = if native && subtitle_track >= 0 {
        match ensure_vtt(&item, subtitle_track).await {
            Ok(_) => {
                let track = &mi.sub_tracks[subtitle_track as usize];
                Some(json!({"trackId":1,"type":"TEXT","subtype":"SUBTITLES", "trackContentId":format!("http://{}:{}/subtitles/{}/{}",st.lan_ip,st.port,item.id,subtitle_track), "trackContentType":"text/vtt", "name":track.title, "language":track.lang.to_lowercase()}))
            }
            Err(error) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":error}))).into_response(),
        }
    } else { None };

    // Receivers that play the Cast HLS flavour get an on-demand fMP4 playlist built from the
    // untouched video: no file to prepare, playback starts at once. A plain MP4 stays direct,
    // non-Original qualities / burned subtitles keep the prepared path, and a receiver that
    // refused HLS before (receiver-caps.json) goes straight to the prepared MP4.
    let is_cast_target = match &device { Some(d) => d.kind == "cast", None => cast_dev.is_some() };
    let hls_known = cast_dev.as_ref().and_then(|(h, n)| devices::cast_hls_support(h, n));
    let want_hls = native && native_video_ok && is_cast_target && !direct_ok && hls_known != Some(false) && settings.stream_mode != "prepare";
    let mut hls_note = String::new();
    let hls_sid: Option<String> = if want_hls {
        match open_hls_session(&st, &item.path, audio_index).await {
            Ok((sid, codec, channels, copy)) => {
                if !copy {
                    hls_note = if audio_note.is_empty() {
                        format!("เสียง {} → AAC {}ch (สตรีมทันที)", codec.to_uppercase(), channels)
                    } else { audio_note.clone() };
                }
                tracing::info!(item = %item.id, session = %sid, audio_copy = copy, audio_codec = %codec, receiver_hls = ?hls_known, "stream path: HLS on demand");
                Some(sid)
            }
            Err(e) => {
                tracing::warn!(item = %item.id, error = %e, "HLS session could not be opened; using prepared MP4");
                None
            }
        }
    } else {
        tracing::info!(item = %item.id, native, direct_ok, cast = is_cast_target, receiver_hls = ?hls_known, stream_mode = %settings.stream_mode,
            "stream path: {}", if direct_ok { "direct" } else { "prepared MP4" });
        None
    };

    // Continue where the user stopped (not for a nearly-finished item).
    let start_at = if settings.resume && !req.from_start && is_cast_target
        && item.resume_pos > 10.0 && (mi.duration <= 0.0 || item.resume_pos < mi.duration - 30.0) {
        item.resume_pos
    } else { 0.0 };

    if let Some(sid) = &hls_sid {
        // Warm the first segments in the background (resume position included).
        let mgr = st.hls.clone();
        let (sid2, pos) = (sid.clone(), start_at);
        tokio::spawn(async move { mgr.prefetch(&sid2, pos).await });
        if req.prepare_only {
            let url = format!("http://{}:{}/vod/{}/master.m3u8", st.lan_ip, st.port, sid);
            return Json(json!({ "ready": true, "prepared": true, "stream": "hls", "key": sid, "url": url, "audio_note": hls_note })).into_response();
        }
    }

    let mut try_hls = hls_sid.is_some();
    let (msg, url, mode, quality_label, stream, audio_note) = loop {
        let (url, mode, quality_label, stream, note) = if let (true, Some(sid)) = (try_hls, &hls_sid) {
            let audio = if hls_note.is_empty() { "original audio" } else { "AAC audio" };
            let subs = if native_subtitle.is_some() { " + separate subtitles" } else { "" };
            (
                format!("http://{}:{}/vod/{}/master.m3u8", st.lan_ip, st.port, sid),
                format!("original video + {audio}{subs} · สตรีมทันที"),
                "original".to_string(),
                "hls",
                hls_note.clone(),
            )
        } else if direct_ok {
            (
                format!("http://{}:{}/media/{}", st.lan_ip, st.port, item.id),
                if native_subtitle.is_some() { "original video + original audio + separate subtitles (direct)".to_string() } else { "original video + original audio (direct)".to_string() },
                "original".to_string(),
                "direct",
                audio_note.clone(),
            )
        } else {
            // Everything else is prepared once into a faststart MP4: the Cast
            // receiver on this TV rejects live HLS, but plays MP4 with Range
            // requests — which is also what makes exact seeking work.
            let file = prepared_path(&cache_key);
            if !file.exists() {
                start_prepare(
                    &st,
                    &item.id,
                    &cache_key,
                    &item.path,
                    &mi.vcodec,
                    native_video_ok,
                    quality,
                    &settings.encoder,
                    mi.duration,
                    audio_track,
                    render_subtitle,
                    settings.subtitle_scale,
                    req.prepare_only && req.speculative,
                    audio_aac_channels,
                );
                return Json(json!({
                    "preparing": true,
                    "key": cache_key,
                    "audio_note": audio_note,
                    "progress": 0.0,
                    "message": "กำลังเตรียมไฟล์ให้ทีวี (ครั้งแรกเท่านั้น)",
                    "quality": quality.label(),
                }))
                .into_response();
            }
            let mode = if native {
                if subtitle_track >= 0 { "original video + original audio + separate subtitles".to_string() } else { "original video + original audio (remux)".to_string() }
            } else if subtitle_track >= 0 {
                "prepared with burned-in subtitles".to_string()
            } else { match quality {
                Quality::Passthrough => "remux (copy)".to_string(),
                _ => format!("prepared {}", quality.label()),
            }};
            (
                format!("http://{}:{}/prepared/{}", st.lan_ip, st.port, cache_key),
                mode,
                quality.label().to_string(),
                "mp4",
                audio_note.clone(),
            )
        };

        let mode = if note.is_empty() { mode } else { format!("{mode} · {note}") };
        if req.prepare_only {
            return Json(json!({ "ready": true, "prepared": true, "stream": stream, "key": cache_key, "url": url, "audio_note": note })).into_response();
        }
        let device = device.as_ref().expect("playback device validated before preparation");

        let msg = if device.kind == "cast" {
            st.begin_user_cast();
            let plan = CastPlan { url: url.clone(), hls: stream == "hls", subtitle: native_subtitle.clone() };
            match start_cast(&st, device, &item, &settings, &plan, start_at).await {
                Ok(o) => {
                    st.set_intent(&item.id, &device.id, plan, start_at);
                    if o.started { format!("casting to {}", device.name) } else { format!("sent to {} (buffering)", device.name) }
                }
                Err(CastFail::HlsFailed(why)) => {
                    tracing::warn!(item = %item.id, device = %device.name, reason = %why, "HLS fallback: using the prepared MP4 path for this and future plays");
                    try_hls = false;
                    continue;
                }
                Err(CastFail::Rejected(err)) => {
                    return (StatusCode::BAD_GATEWAY, Json(json!({"error": err, "url": url}))).into_response();
                }
                Err(CastFail::Unanswered(last_err)) => {
                    tracing::error!(error = %last_err, item = %item.id, "cast failed");
                    return (StatusCode::BAD_GATEWAY, Json(json!({"error": format!("ทีวีไม่ตอบ ({last_err}) — ลองเปิดทีวีแล้วกดเล่นอีกครั้ง"), "url": url}))).into_response();
                }
            }
        } else {
            st.begin_user_cast();
            st.clear_intent();
            let dev = device.clone();
            let url_cast = url.clone();
            let title = item.title.clone();
            match tokio::task::spawn_blocking(move || devices::cast_dlna(&dev, &url_cast, &title))
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
            {
                Ok(m) => m,
                Err(e) => {
                    return (StatusCode::BAD_GATEWAY, Json(json!({ "error": e, "url": url }))).into_response()
                }
            }
        };
        break (msg, url, mode, quality_label, stream, note);
    };
    let device = device.expect("playback device validated before preparation");

    {
        let mut now = st.now.lock().unwrap();
        *now = NowPlaying {
            device_id: device.id.clone(),
            device_name: device.name.clone(),
            item_id: item.id.clone(),
            title: item.title.clone(),
            url: url.clone(),
            mode: mode.clone(),
            quality: quality_label.clone(),
            hls: stream == "hls",
            stream: stream.to_string(),
        };
    }
    let mut s = settings.clone();
    s.last_device = device.id.clone();
    s.last_item = item.id.clone();
    st.save_settings(s);

    Json(json!({
        "ok": true, "message": msg, "url": url, "mode": mode, "quality": quality_label,
        "device": device.name, "native_subtitles": native_subtitle.is_some(), "resumed_from": start_at, "audio_note": audio_note, "stream": stream,
    }))
    .into_response()
}

async fn ensure_vtt(item: &crate::library::Item, track: i32) -> Result<PathBuf, String> {
    let key = preparation_key(&item.path, Quality::Passthrough, "webvtt-v1", -1, track, 1.0);
    let target = prepared_dir().join(format!("{key}.vtt"));
    if target.exists() { return Ok(target) }
    static VTT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = VTT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = prepared_dir().join(format!("{key}-{}-{nonce}.part.vtt", std::process::id()));
    let result = crate::process::async_command("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
        .arg(&item.path).args(["-map", &format!("0:s:{track}"), "-c:s", "webvtt", "-f", "webvtt"])
        .arg(&temporary).output().await.map_err(|e| e.to_string())?;
    if !result.status.success() {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(String::from_utf8_lossy(&result.stderr).chars().take(1500).collect());
    }
    if let Err(e) = tokio::fs::rename(&temporary, &target).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        if !target.exists() { return Err(e.to_string()) }
    }
    Ok(target)
}

async fn subtitle_vtt(State(st): State<Arc<AppState>>, Path((id, track)): Path<(String, i32)>) -> Response {
    let Some(item) = st.find_item(&id) else { return StatusCode::NOT_FOUND.into_response() };
    let path = item.path.clone();
    let info = tokio::task::spawn_blocking(move || probe::probe(&path)).await.unwrap_or_default();
    if track < 0 || info.sub_tracks.get(track as usize).map(|t| !t.burnable).unwrap_or(true) { return StatusCode::NOT_FOUND.into_response() }
    match ensure_vtt(&item, track).await {
        Ok(path) => match tokio::fs::read(path).await {
            Ok(bytes) => {
                st.subtitle_fetches.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                ([(header::CONTENT_TYPE, "text/vtt; charset=utf-8"), (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"), (header::CACHE_CONTROL, "no-cache")], bytes).into_response()
            }
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}

async fn subtitle_debug(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({"fetches":st.subtitle_fetches.load(std::sync::atomic::Ordering::Relaxed)}))
}

fn prepared_dir() -> PathBuf {
    let d = crate::library::data_dir().join("prepared");
    let _ = std::fs::create_dir_all(&d);
    d
}

// Versioned cache identity includes every setting that changes output. Never
// reuse a no-subtitle render after the user selects a subtitle or audio track.
fn preparation_key(path: &str, quality: Quality, encoder: &str, audio: i32, sub: i32, scale: f64) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    "prepared-v4-original-native-subs".hash(&mut h);
    path.hash(&mut h);
    if let Ok(m) = std::fs::metadata(path) {
        m.len().hash(&mut h);
        m.modified().ok().hash(&mut h);
    }
    quality.label().hash(&mut h);
    encoder.hash(&mut h);
    audio.hash(&mut h);
    sub.hash(&mut h);
    scale.to_bits().hash(&mut h);
    format!("{:016x}", h.finish())
}

fn prepared_path(id: &str) -> PathBuf {
    prepared_dir().join(format!("{id}.mp4"))
}

/// Kicks off a background ffmpeg run that produces a faststart MP4 the TV can
/// stream with Range requests (exact seeking included).
fn start_prepare(
    st: &Arc<AppState>,
    item_id: &str,
    cache_key: &str,
    path: &str,
    vcodec: &str,
    passthrough_ok: bool,
    quality: Quality,
    encoder: &str,
    duration: f64,
    audio_track: i32,
    sub_track: i32,
    sub_scale: f64,
    background: bool,
    audio_aac_channels: u32,
) {
    {
        st.prep_keys.lock().unwrap().insert(item_id.to_string(), cache_key.to_string());
        // Only one speculative job: a new selection or a real Cast of another
        // file cancels it; a real Cast of the same file promotes it.
        let mut bg = st.bg_prepare.lock().unwrap();
        if let Some((key, handle)) = bg.as_ref() {
            if key == cache_key {
                if !background { *bg = None; }
            } else {
                handle.abort();
                tracing::info!(key = %key, "speculative prepare cancelled");
                st.prep.lock().unwrap().remove(key);
                let _ = std::fs::remove_file(prepared_path(key).with_extension("part.mp4"));
                *bg = None;
            }
        }
        let mut prep = st.prep.lock().unwrap();
        if let Some(p) = prep.get(cache_key) {
            if p.state == "preparing" {
                return;
            }
        }
        prep.insert(
            cache_key.to_string(),
            PrepState {
                state: "preparing".into(),
                progress: 0.0,
                file: prepared_path(cache_key).to_string_lossy().to_string(),
                note: quality.label().to_string(),
            },
        );
    }

    let st2 = st.clone();
    let item_id = item_id.to_string();
    let cache_key = cache_key.to_string();
    let path = path.to_string();
    let vcodec = vcodec.to_string();
    let encoder = encoder.to_string();
    let bg_key = cache_key.clone();
    let task = tokio::spawn(async move {
        let started = std::time::Instant::now();
        tracing::info!(item = %item_id, key = %cache_key, background, "prepare started");
        let out = prepared_path(&cache_key);
        let tmp = out.with_extension("part.mp4");
        let mut cmd = crate::process::async_command("ffmpeg");
        cmd.kill_on_drop(true);
        cmd.arg("-hide_banner").args(["-loglevel", "info", "-nostdin", "-y"]);
        cmd.arg("-i").arg(&path).args(["-map", "0:v:0"]);
        let audio_n = audio_track;
        if audio_n >= 0 {
            cmd.args(["-map", &format!("0:a:{audio_n}")]);
        } else {
            cmd.args(["-map", "0:a:0?"]);
        }
        // Clearest picture = no generation loss: when the source video is already
        // TV-decodable and nothing must be burned in, copy it bit-for-bit.
        let mode = if passthrough_ok && sub_track < 0 && quality == Quality::Passthrough {
            crate::transcode::Mode::Passthrough
        } else {
            st2.transcoder.pick_mode(if sub_track >= 0 && quality == Quality::Passthrough { Quality::V1080Hq } else { quality }, passthrough_ok && sub_track < 0, &encoder)
        };
        // Extract subtitles beside the output and use a relative, ASCII-only
        // filter filename. This avoids Windows drive-colon/quote escaping bugs.
        let subtitle_file = format!("{cache_key}.ass");
        if sub_track >= 0 {
            let extraction = crate::process::async_command("ffmpeg")
                .kill_on_drop(true)
                .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
                .arg(&path).args(["-map", &format!("0:s:{sub_track}"), "-c:s", "ass"])
                .arg(prepared_dir().join(&subtitle_file)).output().await;
            let error = match extraction {
                Ok(o) if o.status.success() => None,
                Ok(o) => Some(String::from_utf8_lossy(&o.stderr).chars().take(1200).collect::<String>()),
                Err(e) => Some(e.to_string()),
            };
            if let Some(error) = error {
                tracing::error!(item = %item_id, key = %cache_key, error = %error, "subtitle preparation failed");
                if let Some(p) = st2.prep.lock().unwrap().get_mut(&cache_key) { p.state = "error".into(); p.note = error; }
                return;
            }
        }
        cmd.current_dir(prepared_dir());
        let vf_sub = if sub_track >= 0 {
            Some(format!("subtitles=filename={}:force_style='FontName=Tahoma,Fontsize={}'", subtitle_file, (18.0 * sub_scale.clamp(0.6, 2.0)).round() as u32))
        } else { None };
        let scale = if quality == Quality::V720 {
            "scale='min(1280,iw)':-2"
        } else {
            "scale='min(3840,iw)':-2"
        };
        let vf = match &vf_sub {
            Some(s) => format!("{scale},{s}"),
            None => scale.to_string(),
        };
        match mode {
            crate::transcode::Mode::Passthrough => {
                cmd.args(["-c:v", "copy"]);
                if vcodec == "hevc" { cmd.args(["-tag:v", "hvc1"]); }
            }
            crate::transcode::Mode::Nvenc => {
                cmd.args(["-c:v", "h264_nvenc", "-preset", "p7", "-tune", "hq", "-rc", "vbr", "-multipass", "fullres",
                          "-cq", "10", "-b:v", "0", "-maxrate", "40M", "-bufsize", "80M",
                          "-spatial-aq", "1", "-temporal-aq", "1", "-rc-lookahead", "32",
                          "-profile:v", "high", "-pix_fmt", "yuv420p", "-g", "48", "-vf", &vf]);
            }
            crate::transcode::Mode::X264 => {
                cmd.args(["-c:v", "libx264", "-preset", "slow", "-crf", "12",
                          "-maxrate", "40M", "-bufsize", "80M", "-pix_fmt", "yuv420p",
                          "-g", "48", "-vf", &vf]);
            }
            _ => {
                cmd.args(["-c:v", "h264_nvenc", "-preset", "p7", "-cq", "10", "-pix_fmt", "yuv420p", "-vf", &vf]);
            }
        }
        // No audio re-encoding/downmix. Compatibility was checked before
        // preparation; avoid probing the whole source again in the hot path.
        if audio_aac_channels > 0 {
            // Receiver cannot decode this codec: high-bitrate AAC, same channels.
            let rate = if audio_aac_channels >= 6 { "512k" } else if audio_aac_channels > 2 { "384k" } else { "256k" };
            // "fast" coder: ~55x realtime vs ~9x for twoloop; transparent at these bitrates.
            cmd.args(["-c:a", "aac", "-aac_coder", "fast", "-b:a", rate, "-ac", &audio_aac_channels.to_string()]);
        } else {
            cmd.args(["-c:a", "copy"]);
        }
        cmd.args(["-map_metadata", "0", "-map_chapters", "0"]);
        cmd.args(["-sn"])
            .args(["-movflags", "+faststart", "-f", "mp4"])
            .arg(&tmp)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                st2.prep.lock().unwrap().insert(
                    cache_key.clone(),
                    PrepState { state: "error".into(), progress: 0.0, file: String::new(), note: e.to_string() },
                );
                return;
            }
        };

        let mut diagnostics = String::new();
        // ffmpeg rewrites its progress line with '\r', so split on both \r and \n.
        if let Some(mut err) = child.stderr.take() {
            use tokio::io::AsyncReadExt;
            let mut chunk = [0u8; 4096];
            let mut line = String::new();
            loop {
                let n = match err.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                for &b in &chunk[..n] {
                    if b == b'\r' || b == b'\n' {
                        if let Some(idx) = line.find("time=") {
                            let hms: Vec<f64> = line[idx + 5..]
                                .split_whitespace()
                                .next()
                                .unwrap_or("")
                                .split(':')
                                .filter_map(|x| x.parse::<f64>().ok())
                                .collect();
                            if hms.len() == 3 && duration > 0.0 {
                                let secs = hms[0] * 3600.0 + hms[1] * 60.0 + hms[2];
                                let pct = (secs / duration * 100.0).clamp(0.0, 100.0);
                                if let Some(p) = st2.prep.lock().unwrap().get_mut(&cache_key) {
                                    p.progress = pct;
                                }
                            }
                        }
                        diagnostics.push_str(&line);
                        diagnostics.push('\n');
                        if diagnostics.len() > 8000 { diagnostics = diagnostics.chars().rev().take(4000).collect::<String>().chars().rev().collect(); }
                        line.clear();
                    } else if line.len() < 2000 {
                        line.push(b as char);
                    }
                }
            }
        }
        let status = child.wait().await;
        let ok = matches!(status, Ok(s) if s.success()) && tmp.exists();
        if ok {
            if let Err(e) = std::fs::rename(&tmp, &out) {
                tracing::error!(item = %item_id, key = %cache_key, error = %e, "prepare publish failed");
                if let Some(p) = st2.prep.lock().unwrap().get_mut(&cache_key) { p.state = "error".into(); p.note = e.to_string(); }
                return;
            }
            tracing::info!(item = %item_id, key = %cache_key, elapsed_ms = started.elapsed().as_millis() as u64, "prepare ready");
            if let Some(p) = st2.prep.lock().unwrap().get_mut(&cache_key) {
                p.state = "ready".into();
                p.progress = 100.0;
            }
            trim_prepared_cache(&st2);
        } else {
            let _ = std::fs::remove_file(&tmp);
            tracing::error!(item = %item_id, key = %cache_key, "prepare ffmpeg failed");
            if let Some(p) = st2.prep.lock().unwrap().get_mut(&cache_key) {
                p.state = "error".into();
                p.note = format!("ffmpeg failed: {}", diagnostics.chars().rev().take(2000).collect::<String>().chars().rev().collect::<String>());
            }
        }
    });
    if background {
        *st.bg_prepare.lock().unwrap() = Some((bg_key, task.abort_handle()));
    }
}

async fn api_prepare_status(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    if id.len() > 128 || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let key = st.prep_keys.lock().unwrap().get(&id).cloned().unwrap_or(id);
    let prep = st.prep.lock().unwrap();
    match prep.get(&key) {
        Some(p) => Json(json!(p)).into_response(),
        None => Json(json!({
            "state": if prepared_path(&key).exists() { "ready" } else { "idle" },
            "progress": if prepared_path(&key).exists() { 100.0 } else { 0.0 },
        }))
        .into_response(),
    }
}

#[derive(Deserialize)]
struct TransportReq {
    action: String,
}

async fn api_transport(State(st): State<Arc<AppState>>, Json(req): Json<TransportReq>) -> impl IntoResponse {
    tracing::info!(action = %req.action, "transport request");
    if req.action == "stop" { st.user_wants(Wanted::Stopped); }
    let cast_opt = { st.cast.lock().unwrap().clone() };
    if let Some(client) = cast_opt {
        let cs = client.status();
        // No live media session (TV slept / screensaver closed the player):
        // tell the UI so Play starts a fresh Cast with the resume point.
        if req.action != "stop" && (!cs.connected || cs.transport_id.is_empty() || cs.media_session_id == 0 || !client.refresh(1200).await || client.status().transport_id.is_empty()) {
            return (StatusCode::CONFLICT, Json(json!({"error":"no_session"}))).into_response();
        }
        let ok = match req.action.as_str() {
            "play" => client.play(),
            "pause" => client.pause(),
            "stop" => client.stop(),
            other => return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("unknown action {other}")}))).into_response(),
        };
        if ok {
            match req.action.as_str() {
                "pause" => st.user_wants(Wanted::Paused),
                "play" => st.user_wants(Wanted::Playing),
                _ => {}
            }
            return Json(json!({ "ok": true, "via": "cast" })).into_response();
        }
    }
    let device_id = st.now.lock().unwrap().device_id.clone();
    let Some(device) = st.device_by_id(&device_id) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"no active device"}))).into_response();
    };
    let action = req.action.clone();
    let res = tokio::task::spawn_blocking(move || devices::transport(&device, &action))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    match res {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response(),
    }
}

fn valid_playback_rate(rate: f64) -> bool {
    rate.is_finite() && [1.0, 1.25, 1.5, 1.75, 2.0].contains(&rate)
}

#[derive(Deserialize)]
struct RateReq { rate: f64 }

async fn api_rate(State(st): State<Arc<AppState>>, Json(req): Json<RateReq>) -> Response {
    if !valid_playback_rate(req.rate) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"เลือกความเร็ว ×1, ×1.25, ×1.5, ×1.75 หรือ ×2"}))).into_response();
    }
    let Some(client) = st.cast.lock().unwrap().clone() else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"เริ่ม Cast ไปทีวีก่อนเปลี่ยนความเร็ว"}))).into_response();
    };
    let _lock = client.rate_lock.lock().await;
    let initial = client.status();
    if !initial.connected || !matches!(initial.player_state.as_str(), "PLAYING" | "PAUSED" | "BUFFERING") {
        return (StatusCode::CONFLICT, Json(json!({"error":"ไม่มีหนังที่กำลังเล่นบน Google Cast"}))).into_response();
    }
    let Some(request_id) = client.set_playback_rate(req.rate) else {
        return (StatusCode::BAD_GATEWAY, Json(json!({"error":"Cast channel is closed"}))).into_response();
    };
    for attempt in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let status = client.status();
        if status.last_error_request_id == request_id && !status.last_error.is_empty() {
            return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"error":format!("ทีวีไม่ยอมรับความเร็ว ×{} — ยังคง ×{}",req.rate,status.playback_rate),"rate":status.playback_rate,"detail":status.last_error}))).into_response();
        }
        if status.status_revision > initial.status_revision && (status.playback_rate - req.rate).abs() < 0.01 {
            return Json(json!({"ok":true,"rate":status.playback_rate,"confirmed_by_receiver":true})).into_response();
        }
        if attempt % 4 == 3 { client.request_status(); }
    }
    let actual = client.status().playback_rate;
    (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"error":format!("ทีวีไม่ได้ยืนยัน ×{} (รายงานปัจจุบัน ×{}) — ไม่ได้แปลงไฟล์เพื่อลดคุณภาพ",req.rate,actual),"rate":actual}))).into_response()
}

#[derive(Deserialize)]
struct SeekReq {
    seconds: f64,
}

async fn api_seek(State(st): State<Arc<AppState>>, Json(req): Json<SeekReq>) -> impl IntoResponse {
    let cast_opt = { st.cast.lock().unwrap().clone() };
    if let Some(client) = cast_opt {
        if client.seek(req.seconds.max(0.0)) {
            if let Some(i) = st.intent.lock().unwrap().as_mut() { i.last_pos = req.seconds.max(0.0); }
            return Json(json!({ "ok": true, "seconds": req.seconds, "via": "cast" })).into_response();
        }
    }
    let device_id = st.now.lock().unwrap().device_id.clone();
    let Some(device) = st.device_by_id(&device_id) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"no active device"}))).into_response();
    };
    let secs = req.seconds;
    let res = tokio::task::spawn_blocking(move || devices::seek(&device, secs))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    match res {
        Ok(_) => Json(json!({ "ok": true, "seconds": secs })).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct PosQuery {
    #[serde(default)]
    device: Option<String>,
}

async fn api_position(State(st): State<Arc<AppState>>, Query(q): Query<PosQuery>) -> impl IntoResponse {
    let cast_opt = { st.cast.lock().unwrap().clone() };
    if let Some(client) = cast_opt {
        client.request_status();
        tokio::time::sleep(std::time::Duration::from_millis(180)).await;
        let s = client.status();
        let state = if !s.connected {
            "NO_DEVICE".to_string()
        } else if s.player_state.is_empty() {
            "IDLE".to_string()
        } else {
            s.player_state.clone()
        };
        return Json(json!({
            "elapsed": s.current_time,
            "duration": s.duration,
            "state": state,
            "via": "cast",
            "playback_rate": s.playback_rate,
            "title": s.title,
            "idle_reason": s.idle_reason,
            "media_session_id": s.media_session_id,
        }));
    }
    let id = q
        .device
        .unwrap_or_else(|| st.now.lock().unwrap().device_id.clone());
    let Some(device) = st.device_by_id(&id) else {
        return Json(json!({ "elapsed": 0.0, "duration": 0.0, "state": "NO_DEVICE" }));
    };
    let pos = tokio::task::spawn_blocking(move || devices::position(&device))
        .await
        .unwrap_or(devices::Position { elapsed: 0.0, duration: 0.0, state: "UNKNOWN".into() });
    Json(json!({ "elapsed": pos.elapsed, "duration": pos.duration, "state": pos.state }))
}

async fn api_now(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    Json(now_json(&st))
}

/// NowPlaying plus the automatic-recovery fields the UI shows.
fn now_json(st: &AppState) -> serde_json::Value {
    let now = st.now.lock().unwrap().clone();
    let rec = st.recovery.lock().unwrap().clone();
    let mut v = json!(now);
    if let Some(o) = v.as_object_mut() {
        o.insert("recovering".into(), json!(rec.recovering));
        o.insert("recover_attempt".into(), json!(rec.attempt));
        o.insert("recover_error".into(), json!(rec.error));
    }
    v
}

/// Cast channel diagnostics: raw messages + resolved status.
async fn api_cast_debug(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let client = { st.cast.lock().unwrap().clone() };
    match client {
        Some(c) => {
            c.request_status();
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            Json(json!(c.status()))
        }
        None => Json(json!({ "connected": false, "last_error": "no cast session" })),
    }
}

#[derive(Deserialize)]
struct PlayReq {
    id: String,
}

/// Starts an HLS session without casting (used by external players).
async fn api_play(State(st): State<Arc<AppState>>, Json(req): Json<PlayReq>) -> impl IntoResponse {
    let Some(item) = st.find_item(&req.id) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"item not found"}))).into_response();
    };
    let path = item.path.clone();
    let mi = tokio::task::spawn_blocking(move || probe::probe(&path))
        .await
        .unwrap_or_default();
    let s = st.settings();
    let quality = Quality::parse(&s.quality);
    match st
        .transcoder
        .start(
            &item.id,
            &item.path,
            mi.passthrough_ok,
            quality,
            &s.encoder,
            s.hls_segment,
            "fmp4",
            resolve_tracks(&mi, &s, &item.id).0,
            if s.subtitle_rendering { resolve_tracks(&mi, &s, &item.id).1 } else { -1 },
            s.subtitle_scale,
        )
    {
        Ok((sid, mode)) => Json(json!({
            "url": format!("http://{}:{}/hls/{}/index.m3u8", st.lan_ip, st.port, sid),
            "mode": mode.label(),
            "quality": quality.label(),
            "passthrough": mi.passthrough_ok,
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

async fn api_stats(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    Json(st.transcoder.stats())
}

// ---------------------------------------------------------------- media plane

async fn media(State(st): State<Arc<AppState>>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    tracing::info!(
        "GET /media/{} range={:?} ua={:?}",
        id,
        headers.get(header::RANGE).and_then(|v| v.to_str().ok()),
        headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok())
    );
    let Some(item) = st.find_item(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    serve_file(PathBuf::from(item.path), headers).await
}

#[derive(Deserialize)]
struct StreamQuery {
    #[serde(default)]
    t: f64,
}

/// Fragmented-MP4 stream (kept for preview/debug clients; the cast-only UI does
/// not play video itself).
async fn stream_media(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<StreamQuery>,
) -> Response {
    let Some(item) = st.find_item(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = item.path.clone();
    let mi = tokio::task::spawn_blocking(move || probe::probe(&path))
        .await
        .unwrap_or_default();

    match st.pipes.start(&st.transcoder, &id, &item.path, mi.passthrough_ok, q.t) {
        Ok((mode, stream)) => {
            let mut hm = HeaderMap::new();
            hm.insert(header::CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
            hm.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            if let Ok(v) = HeaderValue::from_str(mode.label()) {
                hm.insert("x-airstream-mode", v);
            }
            (StatusCode::OK, hm, Body::from_stream(stream)).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    }
}

async fn prepared_media(State(st): State<Arc<AppState>>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    // The server listens on the LAN: only accept our own 16-hex cache keys.
    if id.len() != 16 || !id.bytes().all(|c| c.is_ascii_hexdigit()) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let f = prepared_path(&id);
    if !f.exists() {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Recently served files are kept by the cache trimmer (LRU by mtime).
    if let Ok(file) = std::fs::File::options().write(true).open(&f) {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
    serve_file(f, headers).await
}

/// Keeps the app-owned prepared cache bounded. Deletes only files inside
/// %LOCALAPPDATA%\Airstream\prepared, never source media, never the file
/// currently playing or still being prepared.
pub fn trim_prepared_cache(st: &AppState) {
    const LIMIT: u64 = 30 * 1024 * 1024 * 1024;
    let dir = prepared_dir();
    let now_url = st.now.lock().unwrap().url.clone();
    let busy: std::collections::HashSet<String> = st.prep.lock().unwrap().iter()
        .filter(|(_, p)| p.state == "preparing").map(|(k, _)| k.clone()).collect();
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    let mut files = Vec::new();
    let mut total = 0u64;
    for e in rd.flatten() {
        let p = e.path();
        let Ok(m) = e.metadata() else { continue };
        if !m.is_file() { continue }
        let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
        let key = name.split(['.', '-']).next().unwrap_or("").to_string();
        let age = m.modified().ok().and_then(|t| t.elapsed().ok()).unwrap_or_default();
        // Partial output left by a crash or cancelled speculative job.
        if name.contains(".part.") && !busy.contains(&key) && age > std::time::Duration::from_secs(600) {
            let _ = std::fs::remove_file(&p);
            continue;
        }
        total += m.len();
        if busy.contains(&key) || (!key.is_empty() && now_url.contains(&key)) { continue }
        files.push((m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len(), p));
    }
    files.sort_by_key(|f| f.0);
    let mut freed = 0u64;
    for (_, len, p) in files {
        if total <= LIMIT { break }
        if std::fs::remove_file(&p).is_ok() { total -= len; freed += len; }
    }
    if freed > 0 { tracing::info!(freed_mb = freed / 1048576, remaining_mb = total / 1048576, "prepared cache trimmed"); }
}

async fn hls_file(
    State(st): State<Arc<AppState>>,
    Path((sid, file)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if file.contains('/') || file.contains('\\') || file.contains("..") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(dir) = st.transcoder.session_dir(&sid) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = dir.join(&file);
    tracing::info!(
        "GET /hls/{}/{} range={:?} ua={:?}",
        sid,
        file,
        headers.get(header::RANGE).and_then(|v| v.to_str().ok()),
        headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok())
    );
    for _ in 0..60 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    serve_file(path, headers).await
}

// ---------------------------------------------------------------- on-demand HLS VOD

/// Opens (or reuses) the HLS session for `path` with the audio track `audio_ordinal`
/// (index among audio streams). Returns (session id, source audio codec, AAC channels, copied).
/// AAC sources are copied; everything else becomes AAC-LC with min(6, channels).
async fn open_hls_session(st: &AppState, path: &str, audio_ordinal: usize) -> Result<(String, String, u32, bool), String> {
    let p = PathBuf::from(path);
    let p2 = p.clone();
    let basics = tokio::task::spawn_blocking(move || crate::mkv_index::probe_basic(&p2))
        .await
        .map_err(|e| e.to_string())??;
    let a = basics.audio.get(audio_ordinal).ok_or_else(|| format!("audio track {audio_ordinal} not found"))?;
    let copy = a.codec == "aac";
    let channels = a.channels.clamp(1, 6);
    let opts = crate::hls_vod::VodOptions {
        audio_index: a.index,
        audio: if copy { crate::hls_vod::AudioMode::Copy } else { crate::hls_vod::AudioMode::Aac { channels } },
        format: crate::hls_vod::SegFormat::Fmp4,
    };
    let sid = st.hls.open(&p, opts).await?;
    Ok((sid, a.codec.clone(), channels, copy))
}

fn vod_reply(code: StatusCode, ctype: &'static str, cache: &'static str, body: impl Into<Body>) -> Response {
    (
        code,
        [(header::CONTENT_TYPE, ctype), (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"), (header::CACHE_CONTROL, cache)],
        body.into(),
    )
        .into_response()
}

async fn vod_options() -> Response {
    (
        StatusCode::NO_CONTENT,
        [(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"), (header::ACCESS_CONTROL_ALLOW_HEADERS, "*")],
    )
        .into_response()
}

async fn vod_file(State(st): State<Arc<AppState>>, Path((sid, file)): Path<(String, String)>, headers: HeaderMap) -> Response {
    let t0 = std::time::Instant::now();
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let r = match file.as_str() {
        "master.m3u8" => match st.hls.master(&sid) {
            Some(m) => vod_reply(StatusCode::OK, "application/vnd.apple.mpegurl", "no-cache", m),
            None => vod_reply(StatusCode::NOT_FOUND, "text/plain", "no-cache", "no session"),
        },
        "index.m3u8" => match st.hls.playlist(&sid) {
            Some(m) => vod_reply(StatusCode::OK, "application/vnd.apple.mpegurl", "no-cache", m),
            None => vod_reply(StatusCode::NOT_FOUND, "text/plain", "no-cache", "no session"),
        },
        "init.mp4" => match st.hls.init(&sid).await {
            Ok(p) => match tokio::fs::read(&p).await {
                Ok(b) => vod_reply(StatusCode::OK, "video/mp4", "max-age=3600", b),
                Err(e) => vod_reply(StatusCode::INTERNAL_SERVER_ERROR, "text/plain", "no-cache", e.to_string()),
            },
            Err(e) => vod_reply(StatusCode::NOT_FOUND, "text/plain", "no-cache", e),
        },
        name => {
            let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
            let n = stem.strip_prefix("seg").and_then(|v| v.parse::<usize>().ok());
            let ctype = match ext { "m4s" => "video/iso.segment", "ts" => "video/mp2t", _ => "" };
            match (n, ctype) {
                (Some(n), c) if !c.is_empty() => match st.hls.segment(&sid, n).await {
                    Ok(p) => match tokio::fs::read(&p).await {
                        Ok(b) => vod_reply(StatusCode::OK, c, "max-age=3600", b),
                        Err(e) => vod_reply(StatusCode::INTERNAL_SERVER_ERROR, "text/plain", "no-cache", e.to_string()),
                    },
                    Err(e) => {
                        tracing::warn!(session = %sid, segment = n, error = %e, "vod segment failed");
                        vod_reply(StatusCode::INTERNAL_SERVER_ERROR, "text/plain", "no-cache", e)
                    }
                },
                _ => vod_reply(StatusCode::NOT_FOUND, "text/plain", "no-cache", "not found"),
            }
        }
    };
    tracing::info!(session = %sid, file = %file, status = r.status().as_u16(), ms = t0.elapsed().as_millis() as u64, ua = %ua, "GET /vod");
    r
}

async fn serve_file(path: PathBuf, headers: HeaderMap) -> Response {
    let Ok(meta) = std::fs::metadata(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let len = meta.len();
    let mime = mime_for(&path);

    let mut base = HeaderMap::new();
    base.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    base.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));

    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes=").map(|s| s.to_string()));

    let (start, want, status) = match range {
        Some(spec) => {
            let (a, b) = parse_range(&spec, len);
            if a > b || a >= len {
                let mut hm = HeaderMap::new();
                hm.insert(
                    header::CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes */{len}")).unwrap(),
                );
                return (StatusCode::RANGE_NOT_SATISFIABLE, hm).into_response();
            }
            let mut hm = base.clone();
            hm.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {a}-{b}/{len}")).unwrap(),
            );
            hm.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&(b - a + 1).to_string()).unwrap(),
            );
            (a, b - a + 1, (StatusCode::PARTIAL_CONTENT, hm))
        }
        None => {
            let mut hm = base.clone();
            hm.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&len.to_string()).unwrap(),
            );
            (0, len, (StatusCode::OK, hm))
        }
    };

    // Stream the byte window straight from disk: a 2 GB range must never be
    // buffered in memory (hyper rejects a body shorter than the declared length).
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut file = tokio::fs::File::from_std(file);
    if start > 0 {
        use tokio::io::AsyncSeekExt;
        if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    use tokio::io::AsyncReadExt;
    let limited = file.take(want);
    // 256 KiB chunks: the 4 KiB default capped 4K remux reads at ~60-80 MB/s.
    let body = Body::from_stream(tokio_util::io::ReaderStream::with_capacity(limited, 256 * 1024));
    (status.0, status.1, body).into_response()
}

fn parse_range(spec: &str, len: u64) -> (u64, u64) {
    let (a, b) = spec.split_once('-').unwrap_or((spec, ""));
    let start = a.trim().parse::<u64>().unwrap_or(0);
    let end = if b.trim().is_empty() {
        len.saturating_sub(1)
    } else {
        b.trim().parse::<u64>().unwrap_or(len.saturating_sub(1))
    };
    (start, end.min(len.saturating_sub(1)))
}

fn mime_for(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "m3u8" => "application/x-mpegurl",
        "m4s" | "mp4" | "m4v" => "video/mp4",
        "ts" => "video/mp2t",
        "jpg" | "jpeg" => "image/jpeg",
        "mkv" => "video/x-matroska",
        _ => "application/octet-stream",
    }
}


/// Saves the TV position of the playing item every few seconds so closing or
/// crashing the app loses at most a moment of progress. Finished items reset.
pub fn start_resume_tracker(st: Arc<AppState>) {
    tokio::spawn(async move {
        let mut finished_for = String::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            // Keep the Cast channel and saved position fresh even with no UI.
            let client = st.cast.lock().unwrap().clone();
            if let Some(client) = client {
                if client.status().connected {
                    client.request_status();
                    tokio::time::sleep(std::time::Duration::from_millis(180)).await;
                }
            }
            update_intent(&st);
            save_resume_now(&st, &mut finished_for);
        }
    });
}

/// Derives the user's intent from what the TV reports (MEDIA_STATUS).
pub fn update_intent(st: &Arc<AppState>) {
    if st.recovery.lock().unwrap().recovering { return }
    let Some(client) = st.cast.lock().unwrap().clone() else { return };
    let s = client.status();
    let mut guard = st.intent.lock().unwrap();
    let Some(i) = guard.as_mut() else { return };
    if i.locked_until.is_some_and(|t| std::time::Instant::now() < t) { return }
    if !s.connected || s.media_session_id == 0 { return }
    let now = std::time::Instant::now();
    let new = match s.player_state.as_str() {
        "PLAYING" | "BUFFERING" => { i.last_pos = s.current_time; i.last_ok_at = now; Wanted::Playing }
        "PAUSED" => { i.last_pos = s.current_time; Wanted::Paused }
        "IDLE" => Wanted::Stopped,
        _ => return,
    };
    if new != i.wanted {
        tracing::info!(from = ?i.wanted, to = ?new, state = %s.player_state, idle_reason = %s.idle_reason, position = s.current_time, "intent follows TV state");
        i.wanted = new;
    }
}

fn set_recovery(st: &AppState, recovering: bool, attempt: u32, error: &str) {
    *st.recovery.lock().unwrap() = Recovery { recovering, attempt, error: error.into() };
}

/// Automatic Cast session recovery. Reconnects (and relaunches the player) when the
/// TV link drops or the player app crashes while the user was watching. Never
/// wakes the TV for a paused/stopped session or a screensaver-closed player.
pub fn start_cast_supervisor(st: Arc<AppState>) {
    tokio::spawn(async move {
        let mut down_since: Option<std::time::Instant> = None;
        let mut handled_close = 0u64;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            update_intent(&st);
            let Some(intent) = st.intent.lock().unwrap().clone() else { down_since = None; continue };
            if intent.wanted != Wanted::Playing { down_since = None; continue }
            let client = st.cast.lock().unwrap().clone();
            let cs = client.as_ref().map(|c| c.status());
            let link_down = cs.as_ref().map(|s| !s.connected).unwrap_or(true);
            if link_down {
                let sighted = *down_since.get_or_insert_with(std::time::Instant::now);
                // Prefer the real moment the channel dropped over when we first noticed.
                let down_for = cs.as_ref().and_then(|s| s.closed_at).map(|t| t.elapsed()).unwrap_or_else(|| sighted.elapsed());
                if down_for >= std::time::Duration::from_secs(5) {
                    tracing::warn!(item = %intent.item_id, position = intent.last_pos, down_for_s = down_for.as_secs(), "cast link lost while PLAYING; recovering");
                    recover_session(&st, "connection_lost", 3, intent.last_pos).await;
                    down_since = None;
                    handled_close = st.cast.lock().unwrap().as_ref().map(|c| c.status().player_closed_seq).unwrap_or(0);
                }
                continue;
            }
            down_since = None;
            let s = cs.unwrap();
            if s.player_closed_seq != handled_close {
                handled_close = s.player_closed_seq;
                if !s.transport_id.is_empty() { continue }
                // Our own relaunch/LOAD (or a user Play in progress) transiently shows no player.
                let recent_load = st.last_load_at.lock().unwrap().is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(3));
                if st.user_cast_inflight.load(std::sync::atomic::Ordering::SeqCst) > 0 || recent_load {
                    tracing::info!(inflight = st.user_cast_inflight.load(std::sync::atomic::Ordering::SeqCst), recent_load, "player close ignored: cast/LOAD in progress");
                    continue;
                }
                let recent = intent.crash_relaunch_at.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(60));
                if s.player_closed_after_playing && !recent {
                    tracing::warn!(item = %intent.item_id, position = intent.last_pos, "player closed within 10 s of PLAYING; treating as a crash and relaunching once");
                    if let Some(i) = st.intent.lock().unwrap().as_mut() { i.crash_relaunch_at = Some(std::time::Instant::now()); }
                    recover_session(&st, "player_closed", 1, intent.last_pos).await;
                    handled_close = st.cast.lock().unwrap().as_ref().map(|c| c.status().player_closed_seq).unwrap_or(0);
                } else {
                    tracing::info!(after_playing = s.player_closed_after_playing, recent_relaunch = recent, "player closed without recent PLAYING; staying hands-off (no relaunch)");
                    if let Some(i) = st.intent.lock().unwrap().as_mut() { i.wanted = Wanted::Stopped; }
                }
            }
        }
    });
}

async fn recover_session(st: &Arc<AppState>, reason: &str, max_attempts: u32, lost_pos: f64) {
    use std::sync::atomic::Ordering;
    let gen = st.intent_gen.load(Ordering::SeqCst);
    // Position at the moment of loss; a new LOAD resets the live value to 0, so never re-read it.
    let start_at = (lost_pos - 3.0).max(0.0);
    let still_wanted = |st: &AppState| {
        st.intent_gen.load(Ordering::SeqCst) == gen
            && st.intent.lock().unwrap().as_ref().is_some_and(|i| i.wanted == Wanted::Playing)
    };
    const BACKOFF: [u64; 3] = [3, 10, 30];
    for attempt in 1..=max_attempts {
        set_recovery(st, true, attempt, "");
        let load_gen = st.load_gen.load(Ordering::SeqCst);
        // Back off before each attempt (the TV may be rebooting / switching input). A closed
        // player gets a 4 s grace: a user pressing Play meanwhile cancels the automatic relaunch.
        let wait = if reason == "player_closed" { 4 } else { BACKOFF[(attempt as usize - 1).min(2)] };
        tracing::info!(reason, attempt, wait_s = wait, "recovery attempt scheduled");
        for _ in 0..(wait * 4) {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            if !still_wanted(st) { tracing::info!(reason, attempt, "recovery cancelled by user action"); return }
        }
        let Some(intent) = st.intent.lock().unwrap().clone() else { return };
        // Last look before touching the TV: a newer cast (user Play in flight/finished, any other
        // LOAD) or a session that is alive again beats the automatic recovery.
        let live = st.cast.lock().unwrap().clone().map(|c| c.status()).is_some_and(|s|
            s.connected && s.media_session_id != 0 && matches!(s.player_state.as_str(), "PLAYING" | "BUFFERING" | "PAUSED"));
        if !still_wanted(st) || st.user_cast_inflight.load(Ordering::SeqCst) > 0
            || st.load_gen.load(Ordering::SeqCst) != load_gen || live {
            tracing::info!(reason, attempt, live, "recovery cancelled: newer cast");
            if st.recovery.lock().unwrap().recovering { set_recovery(st, false, 0, ""); }
            return;
        }
        let (Some(item), Some(device)) = (st.find_item(&intent.item_id), st.device_by_id(&intent.device_id)) else {
            tracing::warn!(reason, attempt, "recovery impossible: item or device no longer known");
            break;
        };
        tracing::info!(reason, attempt, position = start_at, item = %item.id, device = %device.name, "recovery: reconnecting and reloading");
        let settings = st.settings();
        match start_cast(st, &device, &item, &settings, &intent.plan, start_at).await {
            Ok(o) => {
                if !still_wanted(st) { tracing::info!(reason, "recovered but user changed intent meanwhile"); return }
                tracing::info!(reason, attempt, started = o.started, position = start_at, "recovery succeeded");
                if let Some(i) = st.intent.lock().unwrap().as_mut() {
                    i.last_ok_at = std::time::Instant::now();
                    i.locked_until = Some(i.last_ok_at + std::time::Duration::from_secs(3));
                }
                set_recovery(st, false, 0, "");
                return;
            }
            Err(e) => {
                let msg = match e { CastFail::Rejected(m) | CastFail::Unanswered(m) | CastFail::HlsFailed(m) => m };
                tracing::warn!(reason, attempt, error = %msg, "recovery attempt failed");
                if !still_wanted(st) { return }
            }
        }
    }
    if !still_wanted(st) { return }
    tracing::warn!(reason, "recovery gave up: reconnect_failed");
    if let Some(i) = st.intent.lock().unwrap().as_mut() { i.wanted = Wanted::Stopped; }
    set_recovery(st, false, max_attempts, "reconnect_failed");
}

pub fn save_resume_now(st: &Arc<AppState>, finished_for: &mut String) {
    let Some(client) = st.cast.lock().unwrap().clone() else { return };
    let item_id = st.now.lock().unwrap().item_id.clone();
    if item_id.is_empty() { return }
    let s = client.status();
    // media_session_id is 0 right after a LOAD, so a stale position is never saved.
    if !s.connected || s.media_session_id == 0 { return }
    let key = format!("{item_id}:{}", s.media_session_id);
    if s.player_state == "IDLE" {
        if s.idle_reason == "FINISHED" && *finished_for != key {
            *finished_for = key;
            st.library.set_resume(&item_id, 0.0);
        }
        return;
    }
    if !matches!(s.player_state.as_str(), "PLAYING" | "PAUSED" | "BUFFERING") { return }
    if s.duration > 0.0 && s.current_time > s.duration - 30.0 {
        st.library.set_resume(&item_id, 0.0);
    } else if s.current_time > 5.0 {
        st.library.set_resume(&item_id, s.current_time);
    }
}
