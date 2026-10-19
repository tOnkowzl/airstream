//! Media probing via ffprobe — mirrors Airflow's media_analyzer object
//! (video_0 / audio_0 / subtitle_0 stream inventory + bitrate estimation).

use serde::Deserialize;
use std::process::Command;

#[derive(Deserialize, Debug, Default)]
pub struct FfStream {
    #[serde(default)]
    pub index: u32,
    #[serde(default)]
    pub codec_name: String,
    #[serde(default)]
    pub codec_type: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub pix_fmt: String,
    #[serde(default)]
    pub bit_rate: Option<String>,
    #[serde(default)]
    pub channels: u32,
    #[serde(default)]
    pub tags: Option<serde_json::Value>,
    #[serde(default)]
    pub disposition: Option<serde_json::Value>,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub color_transfer: String,
    #[serde(default)]
    pub side_data_list: Vec<serde_json::Value>,
}

impl FfStream {
    fn lang(&self) -> String {
        self.tags
            .as_ref()
            .and_then(|t| t.get("language"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_uppercase()
    }
    fn title(&self) -> String {
        self.tags
            .as_ref()
            .and_then(|t| t.get("title"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }
    fn is_default(&self) -> bool {
        self.disposition
            .as_ref()
            .and_then(|d| d.get("default"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            == 1
    }
}

#[derive(Deserialize, Debug, Default)]
pub struct FfFormat {
    #[serde(default)]
    pub duration: Option<String>,
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub format_name: String,
}

#[derive(Deserialize, Debug, Default)]
pub struct FfProbe {
    #[serde(default)]
    pub streams: Vec<FfStream>,
    #[serde(default)]
    pub format: FfFormat,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Track {
    /// index within its own stream type (0 = first audio/subtitle track)
    pub n: u32,
    pub codec: String,
    pub lang: String,
    pub title: String,
    pub channels: u32,
    pub default: bool,
    /// text subtitles can be burned in (ASS/SRT), bitmap ones (PGS/DVD) cannot
    pub burnable: bool,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MediaInfo {
    pub duration: f64,
    pub size: u64,
    pub container: String,
    pub width: u32,
    pub height: u32,
    pub vcodec: String,
    pub profile: String,
    pub pix_fmt: String,
    pub bit_depth: u32,
    pub hdr: bool,
    pub color_transfer: String,
    pub acodec: String,
    pub channels: u32,
    pub audio_streams: usize,
    pub subtitle_streams: usize,
    pub audio_tracks: Vec<Track>,
    pub sub_tracks: Vec<Track>,
    /// true when the video can be remuxed untouched for a TV (H.264 8-bit 4:2:0)
    pub passthrough_ok: bool,
}

pub fn probe(path: &str) -> MediaInfo {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_streams",
            "-show_format",
            path,
        ])
        .output();

    let Ok(out) = out else { return MediaInfo::default() };
    let txt = String::from_utf8_lossy(&out.stdout).to_string();
    let Ok(p) = serde_json::from_str::<FfProbe>(&txt) else {
        return MediaInfo::default();
    };

    let mut mi = MediaInfo {
        duration: p.format.duration.as_deref().unwrap_or("0").parse().unwrap_or(0.0),
        size: p.format.size.as_deref().unwrap_or("0").parse().unwrap_or(0),
        container: p.format.format_name.clone(),
        ..Default::default()
    };

    let (mut na, mut ns) = (0u32, 0u32);
    for s in &p.streams {
        match s.codec_type.as_str() {
            "video" if mi.vcodec.is_empty() => {
                mi.vcodec = s.codec_name.clone();
                mi.width = s.width;
                mi.height = s.height;
                mi.pix_fmt = s.pix_fmt.clone();
                mi.profile = s.profile.clone();
                mi.color_transfer = s.color_transfer.clone();
                mi.hdr = matches!(s.color_transfer.as_str(), "smpte2084" | "arib-std-b67") || s.side_data_list.iter().any(|v| v.to_string().contains("DOVI"));
            }
            "audio" => {
                mi.audio_streams += 1;
                if mi.acodec.is_empty() {
                    mi.acodec = s.codec_name.clone();
                    mi.channels = s.channels;
                }
                mi.audio_tracks.push(Track {
                    n: na,
                    codec: s.codec_name.clone(),
                    lang: s.lang(),
                    title: s.title(),
                    channels: s.channels,
                    default: s.is_default(),
                    burnable: false,
                });
                na += 1;
            }
            "subtitle" => {
                mi.subtitle_streams += 1;
                let codec = s.codec_name.clone();
                mi.sub_tracks.push(Track {
                    n: ns,
                    lang: s.lang(),
                    title: s.title(),
                    burnable: matches!(
                        codec.as_str(),
                        "subrip" | "ass" | "ssa" | "mov_text" | "webvtt" | "text"
                    ),
                    codec,
                    channels: 0,
                    default: s.is_default(),
                });
                ns += 1;
            }
            _ => {}
        }
    }

    mi.bit_depth = if mi.pix_fmt.contains("10") {
        10
    } else if mi.pix_fmt.contains("12") {
        12
    } else {
        8
    };
    mi.passthrough_ok = mi.vcodec == "h264"
        && mi.bit_depth == 8
        && (mi.pix_fmt.starts_with("yuv420p") || mi.pix_fmt.is_empty());
    mi
}

/// Extracts one thumbnail frame as JPEG (used by the playlist view).
pub fn thumbnail(path: &str, out: &std::path::Path, seek: f64) -> bool {
    let ts = format!("{:.2}", seek.max(1.0));
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-ss",
            &ts,
            "-i",
            path,
            "-frames:v",
            "1",
            "-vf",
            "scale=480:-2",
            "-q:v",
            "4",
            "-y",
        ])
        .arg(out)
        .status();
    matches!(status, Ok(s) if s.success())
}

/// Normalises ISO-639 variants (tha/th, chi/zho/zh, jp/jpn ...) for matching.
pub fn canonical_lang(raw: &str) -> String {
    let l = raw.trim().to_lowercase();
    match l.as_str() {
        "" | "und" | "unk" | "zxx" => String::new(),
        "th" | "tha" => "th".into(),
        "en" | "eng" => "en".into(),
        "ja" | "jp" | "jpn" => "ja".into(),
        "ko" | "kor" => "ko".into(),
        "zh" | "zho" | "chi" | "cmn" | "yue" | "chs" | "cht" => "zh".into(),
        "fr" | "fra" | "fre" => "fr".into(),
        "de" | "deu" | "ger" => "de".into(),
        "es" | "spa" => "es".into(),
        _ => l,
    }
}

// forced=1, hearing-impaired/captions=2, commentary/description=4
fn title_flags(title: &str) -> u8 {
    let t = title.to_lowercase();
    let mut flags = 0;
    if t.contains("forced") { flags |= 1 }
    if t.contains("sdh") || t.contains("hearing") || t.contains("caption") { flags |= 2 }
    if t.contains("commentary") || t.contains("descriptive") || t.contains("audio description") { flags |= 4 }
    flags
}

fn title_key(title: &str) -> String {
    title.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase()
}

/// Finds the track that best matches a remembered choice, or -1.
/// The language must match (when the choice had one). Within a language the
/// score prefers the same forced/SDH/commentary kind, title, codec, channel
/// count and finally the same position, so two identically named tracks stay
/// distinguishable. `usable_only` restricts subtitles to text tracks.
pub fn pick_track(tracks: &[Track], wanted: &crate::library::TrackPref, usable_only: bool) -> i32 {
    let want_lang = canonical_lang(&wanted.lang);
    let want_flags = title_flags(&wanted.title);
    let want_title = title_key(&wanted.title);
    let mut same_language = 0u32;
    let mut best: Option<(u32, usize)> = None;
    for (index, track) in tracks.iter().enumerate() {
        if !want_lang.is_empty() && canonical_lang(&track.lang) != want_lang {
            continue;
        }
        let nth = same_language;
        same_language += 1;
        if usable_only && !track.burnable {
            continue;
        }
        let mut score = 0u32;
        if title_flags(&track.title) == want_flags { score += 32 }
        if title_key(&track.title) == want_title { score += 16 }
        if track.codec == wanted.codec { score += 8 }
        if track.channels == wanted.channels { score += 4 }
        if nth == wanted.nth { score += 2 }
        if best.map(|(top, _)| score > top).unwrap_or(true) {
            best = Some((score, index));
        }
    }
    best.map(|(_, index)| index as i32).unwrap_or(-1)
}
