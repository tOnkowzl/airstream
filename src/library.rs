//! Playlist / library persistence.
//!
//! The prefs schema intentionally mirrors Airflow's `PlaylistManager.prefs`
//! (observed on disk): playlists[] with name/items/autoPlay/looping, each item
//! carrying a base64 `bookmarkData` plus a plain `path`.

use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const VIDEO_EXTS: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "mov", "ts", "m2ts", "webm", "flv", "wmv", "mpg", "mpeg",
];

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Item {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "bookmarkData", default)]
    pub bookmark_data: String,
    pub path: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub added: u64,
    #[serde(default)]
    pub duration: f64,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub vcodec: String,
    #[serde(default)]
    pub acodec: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Playlist {
    pub name: String,
    #[serde(default)]
    pub items: Vec<Item>,
    #[serde(rename = "autoPlay", default)]
    pub auto_play: bool,
    #[serde(default)]
    pub looping: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Prefs {
    #[serde(default)]
    pub playlists: Vec<Playlist>,
    #[serde(rename = "selectedPlaylist", default)]
    pub selected_playlist: usize,
    #[serde(rename = "currentItemIndex", default)]
    pub current_item_index: usize,
    #[serde(default)]
    pub settings: Settings,
}

/// Playback/transcoding preferences. Key names follow Airflow's Playback.prefs
/// (`quality`, `audioLanguages`, `subtitleLanguages`, `subtitleRendering`,
/// `subtitleScaleAdjustment`) so an existing Airflow profile maps 1:1.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    #[serde(default = "normal_playback_rate")]
    pub playback_rate: f64,
    /// passthrough | variable-720p | variable-1080p | variable-1080p-hq
    pub quality: String,
    /// auto | nvenc | amf | qsv | x264
    pub encoder: String,
    #[serde(rename = "audioLanguages")]
    pub audio_language: String,
    pub audio_stream: String,
    #[serde(rename = "subtitleLanguages")]
    pub subtitle_language: String,
    #[serde(rename = "subtitleRendering")]
    pub subtitle_rendering: bool,
    #[serde(rename = "subtitleScaleAdjustment")]
    pub subtitle_scale: f64,
    pub auto_play: bool,
    pub looping: bool,
    pub resume: bool,
    pub last_device: String,
    pub last_item: String,
    pub hls_segment: u32,
    /// index within the audio streams, -1 = first/default
    pub audio_track: i32,
    /// index within the subtitle streams, -1 = off
    pub subtitle_track: i32,
}

fn normal_playback_rate() -> f64 { 1.0 }

impl Default for Settings {
    fn default() -> Self {
        Settings {
            playback_rate: 1.0,
            quality: "passthrough".into(),
            encoder: "auto".into(),
            audio_language: "TH".into(),
            audio_stream: String::new(),
            subtitle_language: "TH;NONE".into(),
            subtitle_rendering: true,
            subtitle_scale: 1.0,
            auto_play: false,
            looping: false,
            resume: true,
            last_device: String::new(),
            last_item: String::new(),
            hls_segment: 4,
            audio_track: -1,
            subtitle_track: -1,
        }
    }
}

pub struct Library {
    pub prefs: Mutex<Prefs>,
    pub path: PathBuf,
    pub thumbs: PathBuf,
}

pub fn data_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("Airstream")
}

pub fn item_id(path: &str) -> String {
    let mut h = DefaultHasher::new();
    path.hash(&mut h);
    format!("{:016x}", h.finish())
}

pub fn b64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn pretty_title(path: &str) -> String {
    let stem = Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string());
    let cleaned = stem.replace('.', " ").replace('_', " ");
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Library {
    pub fn load() -> Library {
        let dir = data_dir();
        let _ = std::fs::create_dir_all(&dir);
        let thumbs = dir.join("thumbs");
        let _ = std::fs::create_dir_all(&thumbs);
        let path = dir.join("PlaylistManager.prefs");

        let mut prefs = read_prefs(&path).unwrap_or_default();
        if ![1.0, 1.25, 1.5, 1.75, 2.0].contains(&prefs.settings.playback_rate) {
            prefs.settings.playback_rate = 1.0;
        }
        if prefs.playlists.is_empty() {
            prefs.playlists.push(Playlist {
                name: "Playlist".into(),
                items: Vec::new(),
                auto_play: false,
                looping: false,
            });
        }
        // First-run import from the real Airflow install, when present.
        if prefs.playlists[0].items.is_empty() {
            if let Some(imported) = import_airflow() {
                prefs.playlists[0].items = imported;
            }
        }
        for p in prefs.playlists.iter_mut() {
            for it in p.items.iter_mut() {
                if it.id.is_empty() {
                    it.id = item_id(&it.path);
                }
                if it.title.is_empty() {
                    it.title = pretty_title(&it.path);
                }
            }
        }
        Library {
            prefs: Mutex::new(prefs),
            path,
            thumbs,
        }
    }

    pub fn save(&self) {
        let prefs = self.prefs.lock().unwrap().clone();
        if let Ok(txt) = serde_json::to_string_pretty(&prefs) {
            let _ = std::fs::write(&self.path, txt);
        }
    }

    pub fn add_paths(&self, paths: &[String]) -> (usize, usize) {
        let mut added = 0usize;
        let mut skipped = 0usize;
        let mut files: Vec<PathBuf> = Vec::new();
        for p in paths {
            let path = PathBuf::from(p);
            if path.is_dir() {
                collect_videos(&path, &mut files, 0);
            } else {
                files.push(path);
            }
        }
        let mut prefs = self.prefs.lock().unwrap();
        for f in files {
            let s = f.to_string_lossy().to_string();
            let dup = prefs.playlists.iter().any(|p| p.items.iter().any(|i| i.path == s));
            if dup {
                skipped += 1;
                continue;
            }
            let size = std::fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
            let item = Item {
                id: item_id(&s),
                bookmark_data: b64(s.as_bytes()),
                title: pretty_title(&s),
                added: now(),
                path: s,
                duration: 0.0,
                width: 0,
                height: 0,
                vcodec: String::new(),
                acodec: String::new(),
                size,
            };
            prefs.playlists[0].items.push(item);
            added += 1;
        }
        drop(prefs);
        self.save();
        (added, skipped)
    }
}

fn collect_videos(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 4 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_videos(&p, out, depth + 1);
        } else if let Some(ext) = p.extension() {
            let ext = ext.to_string_lossy().to_lowercase();
            if VIDEO_EXTS.contains(&ext.as_str()) {
                out.push(p);
            }
        }
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Airflow stores its prefs as a brace-less JSON fragment ("key" : value, ...).
fn read_prefs(path: &Path) -> Option<Prefs> {
    let raw = std::fs::read_to_string(path).ok()?;
    let raw = raw.trim_start_matches('\u{feff}').trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(p) = serde_json::from_str::<Prefs>(raw) {
        return Some(p);
    }
    let wrapped = format!("{{{raw}}}");
    serde_json::from_str::<Prefs>(&wrapped).ok()
}

fn import_airflow() -> Option<Vec<Item>> {
    let base = std::env::var("LOCALAPPDATA").ok()?;
    let p = PathBuf::from(base).join("Airflow").join("PlaylistManager.prefs");
    let prefs = read_prefs(&p)?;
    let mut out = Vec::new();
    for pl in prefs.playlists {
        for it in pl.items {
            if !Path::new(&it.path).exists() {
                continue;
            }
            out.push(Item {
                id: item_id(&it.path),
                bookmark_data: b64(it.path.as_bytes()),
                title: pretty_title(&it.path),
                added: now(),
                path: it.path,
                ..Default::default()
            });
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

impl Default for Item {
    fn default() -> Self {
        Item {
            id: String::new(),
            bookmark_data: String::new(),
            path: String::new(),
            title: String::new(),
            added: 0,
            duration: 0.0,
            width: 0,
            height: 0,
            vcodec: String::new(),
            acodec: String::new(),
            size: 0,
        }
    }
}
