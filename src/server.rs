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
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use std::net::TcpListener as StdListener;
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
    pub subtitle_fetches: std::sync::atomic::AtomicU64,
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
            subtitle_fetches: std::sync::atomic::AtomicU64::new(0),
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
}


/// A stored track index may not exist in the new file: clamp it.
fn clamp_tracks(mi: &probe::MediaInfo, audio: i32, sub: i32) -> (i32, i32) {
    let a = if audio >= 0 && (audio as usize) < mi.audio_tracks.len() { audio } else { -1 };
    let s = if sub >= 0 && (sub as usize) < mi.sub_tracks.len() { sub } else { -1 };
    (a, s)
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
    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(js))
        .route("/api/ping", get(api_ping))
        .route("/subtitles/:id/:track", get(subtitle_vtt))
        .route("/api/subtitle_debug", get(subtitle_debug))
        .route("/logo.png", get(logo_png))
        .route("/logo.svg", get(logo_svg))
        .route("/favicon.ico", get(logo_png))
        .route("/style.css", get(css))
        .route("/api/state", get(api_state))
        .route("/api/settings", get(api_get_settings).post(api_set_settings))
        .route("/api/add", post(api_add))
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
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", state.port)).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// ---------------------------------------------------------------- static UI

const LOGO_PNG: &[u8] = include_bytes!("../ui/logo.png");
const LOGO_SVG: &str = include_str!("../ui/logo.svg");

/// UI heartbeat: the process exits once the window stops pinging.
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
    let now = st.now.lock().unwrap().clone();
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
    st.save_settings(s.clone());
    Json(json!({ "ok": true, "settings": s }))
}

#[derive(Deserialize)]
struct AddReq {
    paths: Vec<String>,
}

async fn api_add(State(st): State<Arc<AppState>>, Json(req): Json<AddReq>) -> impl IntoResponse {
    let (added, skipped) = st.library.add_paths(&req.paths);
    Json(json!({ "added": added, "skipped": skipped }))
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
    let out = std::process::Command::new("powershell")
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
    let mi = tokio::task::spawn_blocking(move || probe::probe(&item.path))
        .await
        .unwrap_or_default();
    Json(json!({
        "audio": mi.audio_tracks,
        "subtitles": mi.sub_tracks,
        "duration": mi.duration,
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

pub fn start_device_discovery(st: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            let list = devices::discover().await;
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
    device: String,
    id: String,
    /// optional override: passthrough | variable-720p | variable-1080p | variable-1080p-hq
    #[serde(default)]
    quality: Option<String>,
}

async fn api_cast(State(st): State<Arc<AppState>>, Json(req): Json<CastReq>) -> impl IntoResponse {
    let Some(item) = st.find_item(&req.id) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"item not found"}))).into_response();
    };
    let Some(device) = st.device_by_id(&req.device) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"unknown device — refresh the device list"})),
        )
            .into_response();
    };

    let settings = st.settings();
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
    let (audio_track, subtitle_track) = clamp_tracks(&mi, settings.audio_track, settings.subtitle_track);
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
    if let Some(audio) = mi.audio_tracks.get(audio_index) {
        if !matches!(audio.codec.as_str(), "aac" | "ac3" | "eac3" | "alac" | "mp3") {
            return (StatusCode::UNSUPPORTED_MEDIA_TYPE, Json(json!({"error": format!("Original {} audio is not supported in this MP4 path. No AAC conversion or channel downmix was performed; choose a compatible original audio track.", audio.codec)}))).into_response();
        }
    }
    if native && subtitle_track >= 0 && device.kind != "cast" {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, Json(json!({"error":"Separate subtitles are currently supported for Google Cast only. Refusing to silently omit selected subtitles."}))).into_response();
    }
    let render_subtitle = if native { -1 } else { subtitle_track };
    let cache_key = preparation_key(&item.path, quality, if native { "original-copy" } else { &settings.encoder }, audio_track, render_subtitle, if native { 1.0 } else { settings.subtitle_scale });
    let direct_ok = native && native_video_ok && is_mp4 && audio_track < 0;
    let native_subtitle = if native && subtitle_track >= 0 {
        match ensure_vtt(&item, subtitle_track).await {
            Ok(_) => {
                let track = &mi.sub_tracks[subtitle_track as usize];
                Some(json!({"trackId":1,"type":"TEXT","subtype":"SUBTITLES", "trackContentId":format!("http://{}:{}/subtitles/{}/{}",st.lan_ip,st.port,item.id,subtitle_track), "trackContentType":"text/vtt", "name":track.title, "language":track.lang.to_lowercase()}))
            }
            Err(error) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":error}))).into_response(),
        }
    } else { None };

    let (url, mode, quality_label) = if direct_ok {
        (
            format!("http://{}:{}/media/{}", st.lan_ip, st.port, item.id),
            if native_subtitle.is_some() { "original video + original audio + separate subtitles (direct)".to_string() } else { "original video + original audio (direct)".to_string() },
            "original".to_string(),
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
            );
            return Json(json!({
                "preparing": true,
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
        )
    };

    let msg = if device.kind == "cast" {
        let existing = {
            let guard = st.cast.lock().unwrap();
            let same = st.now.lock().unwrap().device_id == device.id;
            match (guard.as_ref(), same) {
                (Some(c), true) if c.status().connected => Some(c.clone()),
                _ => None,
            }
        };
        let client = match existing {
            Some(c) => c,
            None => match crate::cast::connect(&device.host, device.port, &device.name).await {
                Ok(c) => {
                    let arc = Arc::new(c);
                    *st.cast.lock().unwrap() = Some(arc.clone());
                    arc
                }
                Err(e) => {
                    return (StatusCode::BAD_GATEWAY, Json(json!({"error": e, "url": url}))).into_response()
                }
            },
        };
        // The receiver quits its app after STOP; a stale session then has no
        // transport id. Open a fresh connection, which re-launches the player.
        let client = if client.status().transport_id.is_empty() {
            *st.cast.lock().unwrap() = None;
            match crate::cast::connect(&device.host, device.port, &device.name).await {
                Ok(c) => {
                    let arc = Arc::new(c);
                    *st.cast.lock().unwrap() = Some(arc.clone());
                    arc
                }
                Err(e) => {
                    return (StatusCode::BAD_GATEWAY, Json(json!({"error": e, "url": url}))).into_response()
                }
            }
        } else {
            client
        };
        if client.status().transport_id.is_empty() {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error":"Cast receiver did not expose a transport id", "url": url})),
            )
                .into_response();
        }
        if !crate::cast::load(&client, &url, &item.title, false, native_subtitle.clone(), settings.subtitle_scale) {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error":"cast channel closed", "url": url})),
            )
                .into_response();
        }
        let mut started = false;
        for _ in 0..28 {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let s = client.status();
            if s.player_state == "PLAYING" || s.current_time > 0.0 {
                started = true;
                break;
            }
            if !s.last_error.is_empty() {
                let err = s.last_error.clone();
                return (StatusCode::BAD_GATEWAY, Json(json!({"error": err, "url": url}))).into_response();
            }
        }
        if started {
            format!("casting to {}", device.name)
        } else {
            format!("sent to {} (buffering)", device.name)
        }
    } else {
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
            hls: false,
        };
    }
    let mut s = settings.clone();
    s.last_device = device.id.clone();
    s.last_item = item.id.clone();
    st.save_settings(s);

    Json(json!({
        "ok": true, "message": msg, "url": url, "mode": mode, "quality": quality_label,
        "device": device.name, "native_subtitles": native_subtitle.is_some(),
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
    let result = tokio::process::Command::new("ffmpeg")
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
) {
    {
        let prep = st.prep.lock().unwrap();
        if let Some(p) = prep.get(item_id) {
            if p.state == "preparing" {
                return;
            }
        }
    }
    {
        let mut prep = st.prep.lock().unwrap();
        prep.insert(
            item_id.to_string(),
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
    tokio::spawn(async move {
        let out = prepared_path(&cache_key);
        let tmp = out.with_extension("part.mp4");
        let mut cmd = tokio::process::Command::new("ffmpeg");
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
            let extraction = tokio::process::Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
                .arg(&path).args(["-map", &format!("0:s:{sub_track}"), "-c:s", "ass"])
                .arg(prepared_dir().join(&subtitle_file)).output().await;
            let error = match extraction {
                Ok(o) if o.status.success() => None,
                Ok(o) => Some(String::from_utf8_lossy(&o.stderr).chars().take(1200).collect::<String>()),
                Err(e) => Some(e.to_string()),
            };
            if let Some(error) = error {
                if let Some(p) = st2.prep.lock().unwrap().get_mut(&item_id) { p.state = "error".into(); p.note = error; }
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
        cmd.args(["-c:a", "copy", "-map_metadata", "0", "-map_chapters", "0"]);
        cmd.args(["-sn"])
            .args(["-movflags", "+faststart", "-f", "mp4"])
            .arg(&tmp)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                st2.prep.lock().unwrap().insert(
                    item_id.clone(),
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
                                if let Some(p) = st2.prep.lock().unwrap().get_mut(&item_id) {
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
                if let Some(p) = st2.prep.lock().unwrap().get_mut(&item_id) { p.state = "error".into(); p.note = e.to_string(); }
                return;
            }
            if let Some(p) = st2.prep.lock().unwrap().get_mut(&item_id) {
                p.state = "ready".into();
                p.progress = 100.0;
            }
        } else {
            let _ = std::fs::remove_file(&tmp);
            if let Some(p) = st2.prep.lock().unwrap().get_mut(&item_id) {
                p.state = "error".into();
                p.note = format!("ffmpeg failed: {}", diagnostics.chars().rev().take(2000).collect::<String>().chars().rev().collect::<String>());
            }
        }
    });
}

async fn api_prepare_status(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    let prep = st.prep.lock().unwrap();
    match prep.get(&id) {
        Some(p) => Json(json!(p)).into_response(),
        None => Json(json!({
            "state": if prepared_path(&id).exists() { "ready" } else { "idle" },
            "progress": if prepared_path(&id).exists() { 100.0 } else { 0.0 },
        }))
        .into_response(),
    }
}

#[derive(Deserialize)]
struct TransportReq {
    action: String,
}

async fn api_transport(State(st): State<Arc<AppState>>, Json(req): Json<TransportReq>) -> impl IntoResponse {
    let cast_opt = { st.cast.lock().unwrap().clone() };
    if let Some(client) = cast_opt {
        let ok = match req.action.as_str() {
            "play" => client.play(),
            "pause" => client.pause(),
            "stop" => client.stop(),
            other => return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("unknown action {other}")}))).into_response(),
        };
        if ok {
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

#[derive(Deserialize)]
struct SeekReq {
    seconds: f64,
}

async fn api_seek(State(st): State<Arc<AppState>>, Json(req): Json<SeekReq>) -> impl IntoResponse {
    let cast_opt = { st.cast.lock().unwrap().clone() };
    if let Some(client) = cast_opt {
        if client.seek(req.seconds.max(0.0)) {
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
            "title": s.title,
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
    let now = st.now.lock().unwrap().clone();
    Json(json!(now))
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
            clamp_tracks(&mi, s.audio_track, if s.subtitle_rendering { s.subtitle_track } else { -1 }).0,
            clamp_tracks(&mi, s.audio_track, if s.subtitle_rendering { s.subtitle_track } else { -1 }).1,
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
    let f = prepared_path(&id);
    if !f.exists() {
        return StatusCode::NOT_FOUND.into_response();
    }
    serve_file(f, headers).await
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
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(limited));
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
