//! Live HLS transcoding.
//!
//! Reproduces Airflow's topology decision (`transcoding_session:143
//! Creating topology for video: passthrough|variable-720p`) and adds the
//! quality profiles from its Playback.prefs (`passthrough`,
//! `variable-720p`, `variable-1080p`, `variable-1080p-hq`).
//!
//! Unlike the original — which can only accelerate through Intel QuickSync and
//! therefore re-encodes on the CPU on any non-QSV machine — this build prefers
//! NVENC, then AMF, then QSV, and only then libx264.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Passthrough,
    Nvenc,
    Amf,
    Qsv,
    X264,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Passthrough => "passthrough (remux)",
            Mode::Nvenc => "h264_nvenc (GPU)",
            Mode::Amf => "h264_amf (GPU)",
            Mode::Qsv => "h264_qsv (GPU)",
            Mode::X264 => "libx264 (CPU)",
        }
    }
}

/// Quality ladder, named exactly like Airflow's `quality` preference values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    Passthrough,
    V720,
    V1080,
    V1080Hq,
}

impl Quality {
    pub fn parse(s: &str) -> Quality {
        match s {
            "passthrough" => Quality::Passthrough,
            "variable-720p" => Quality::V720,
            "variable-1080p-hq" => Quality::V1080Hq,
            _ => Quality::V1080,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Quality::Passthrough => "passthrough",
            Quality::V720 => "variable-720p",
            Quality::V1080 => "variable-1080p",
            Quality::V1080Hq => "variable-1080p-hq",
        }
    }

    fn scale_filter(self) -> &'static str {
        match self {
            Quality::Passthrough | Quality::V1080 | Quality::V1080Hq => "scale='min(1920,iw)':-2",
            Quality::V720 => "scale='min(1280,iw)':-2",
        }
    }

    fn cq(self) -> &'static str {
        match self {
            Quality::V1080Hq => "19",
            Quality::V1080 => "22",
            Quality::V720 => "23",
            Quality::Passthrough => "23",
        }
    }

    fn maxrate(self) -> &'static str {
        match self {
            Quality::V1080Hq => "20M",
            Quality::V1080 => "12M",
            Quality::V720 => "6M",
            Quality::Passthrough => "12M",
        }
    }

    fn bufsize(self) -> &'static str {
        match self {
            Quality::V1080Hq => "40M",
            Quality::V1080 => "24M",
            Quality::V720 => "12M",
            Quality::Passthrough => "24M",
        }
    }
}

pub struct HwSupport {
    pub nvenc: bool,
    pub amf: bool,
    pub qsv: bool,
    pub summary: String,
}

pub fn detect_hw() -> HwSupport {
    // Probing three GPU encoders costs seconds, so the verdict is cached per
    // ffmpeg binary (path + size + mtime) and only redone when it changes.
    let cache = crate::library::data_dir().join("hw.cache");
    let key = ffmpeg_fingerprint();
    if let Ok(txt) = std::fs::read_to_string(&cache) {
        let mut it = txt.lines();
        if it.next() == Some(key.as_str()) {
            if let Some(flags) = it.next() {
                let f: Vec<bool> = flags.split(',').map(|x| x == "1").collect();
                if f.len() == 3 {
                    return build_hw(f[0], f[1], f[2]);
                }
            }
        }
    }

    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    // Listing an encoder is not proof the GPU can run it: do a 1-frame encode,
    // all three in parallel.
    let t = |name: &'static str, extra: &'static [&'static str], listed: bool| {
        std::thread::spawn(move || listed && smoke(name, extra))
    };
    let h_n = t("h264_nvenc", &["-preset", "p5"], out.contains("h264_nvenc"));
    let h_a = t("h264_amf", &[], out.contains("h264_amf"));
    let h_q = t("h264_qsv", &[], out.contains("h264_qsv"));
    let (nvenc, amf, qsv) = (
        h_n.join().unwrap_or(false),
        h_a.join().unwrap_or(false),
        h_q.join().unwrap_or(false),
    );
    let _ = std::fs::write(
        &cache,
        format!("{key}\n{},{},{}\n", nvenc as u8, amf as u8, qsv as u8),
    );
    build_hw(nvenc, amf, qsv)
}

fn build_hw(nvenc: bool, amf: bool, qsv: bool) -> HwSupport {
    let mut usable = Vec::new();
    if nvenc {
        usable.push("NVENC");
    }
    if amf {
        usable.push("AMF");
    }
    if qsv {
        usable.push("QSV");
    }
    let summary = if usable.is_empty() {
        "software only (libx264)".to_string()
    } else {
        format!("hardware encode: {}", usable.join(" + "))
    };
    HwSupport { nvenc, amf, qsv, summary }
}

fn ffmpeg_fingerprint() -> String {
    let path = std::env::var_os("PATH")
        .and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("ffmpeg.exe"))
                .find(|f| f.exists())
        })
        .unwrap_or_default();
    let meta = std::fs::metadata(&path).ok();
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let mtime = meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}|{size}|{mtime}", path.display())
}

fn smoke(encoder: &str, extra: &[&str]) -> bool {
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=320x240:rate=30:duration=0.2",
        "-c:v",
        encoder,
    ]);
    cmd.args(extra);
    cmd.args(["-f", "null", "-"]);
    matches!(cmd.output(), Ok(o) if o.status.success())
}

pub struct Session {
    pub id: String,
    pub item_id: String,
    pub dir: PathBuf,
    pub mode: Mode,
    pub quality: Quality,
    pub started: u64,
    pub child: Mutex<Option<Child>>,
}

pub struct Transcoder {
    pub root: PathBuf,
    pub sessions: Mutex<HashMap<String, Session>>,
    pub hw: HwSupport,
}

impl Transcoder {
    pub fn new(root: PathBuf) -> Transcoder {
        let _ = std::fs::create_dir_all(&root);
        Transcoder {
            root,
            sessions: Mutex::new(HashMap::new()),
            hw: detect_hw(),
        }
    }

    /// Resolves the encoder: an explicit preference wins when the hardware
    /// supports it, otherwise fall back to the auto order.
    pub fn pick_mode(&self, quality: Quality, passthrough_ok: bool, pref: &str) -> Mode {
        if quality == Quality::Passthrough && passthrough_ok {
            return Mode::Passthrough;
        }
        match pref {
            "nvenc" if self.hw.nvenc => Mode::Nvenc,
            "amf" if self.hw.amf => Mode::Amf,
            "qsv" if self.hw.qsv => Mode::Qsv,
            "x264" => Mode::X264,
            _ => {
                if self.hw.nvenc {
                    Mode::Nvenc
                } else if self.hw.amf {
                    Mode::Amf
                } else if self.hw.qsv {
                    Mode::Qsv
                } else {
                    Mode::X264
                }
            }
        }
    }

    /// Starts (or returns the existing) HLS session for an item.
    pub fn start(
        &self,
        item_id: &str,
        path: &str,
        passthrough_ok: bool,
        quality: Quality,
        encoder: &str,
        segment: u32,
        seg_type: &str,
        audio_n: i32,
        sub_n: i32,
        sub_scale: f64,
    ) -> Result<(String, Mode), String> {
        {
            let sessions = self.sessions.lock().unwrap();
            if let Some(s) = sessions.values().find(|s| s.item_id == item_id && s.quality == quality) {
                return Ok((s.id.clone(), s.mode));
            }
        }

        let mut mode = self.pick_mode(quality, passthrough_ok, encoder);
        // burning subtitles means the video cannot be copied
        if sub_n >= 0 && mode == Mode::Passthrough {
            mode = self.pick_mode(Quality::V1080, false, encoder);
        }
        let id = format!("s{}{}", now(), short(item_id));
        let dir = self.root.join(&id);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

        let mut cmd = Command::new("ffmpeg");
        cmd.arg("-hide_banner")
            .args(["-loglevel", "warning", "-nostdin"])
            .arg("-i")
            .arg(path)
            .args(["-map", "0:v:0"]);

        if audio_n >= 0 {
            cmd.args(["-map", &format!("0:a:{audio_n}")]);
        } else {
            cmd.args(["-map", "0:a:0?"]);
        }

        // subtitle burn-in (text subtitles only; bitmap PGS/DVD cannot be rendered)
        let vf_sub = if sub_n >= 0 {
            Some(format!(
                "subtitles=filename='{}':si={}:force_style='Fontsize={}'",
                escape_filter_path(path),
                sub_n,
                (18.0 * sub_scale.clamp(0.6, 2.0)).round() as u32
            ))
        } else {
            None
        };
        let vf = match &vf_sub {
            Some(s) => format!("{},{}", quality.scale_filter(), s),
            None => quality.scale_filter().to_string(),
        };

        match mode {
            Mode::Passthrough => {
                cmd.args(["-c:v", "copy"]);
            }
            Mode::Nvenc => {
                cmd.args([
                    "-c:v", "h264_nvenc", "-preset", "p5", "-tune", "hq", "-rc", "vbr",
                    "-cq", quality.cq(), "-b:v", "0", "-maxrate", quality.maxrate(),
                    "-bufsize", quality.bufsize(), "-profile:v", "high", "-pix_fmt", "yuv420p",
                    "-g", "48", "-vf", &vf,
                ]);
            }
            Mode::Amf => {
                cmd.args([
                    "-c:v", "h264_amf", "-quality", "balanced", "-rc", "cqp",
                    "-qp_i", quality.cq(), "-qp_p", quality.cq(), "-pix_fmt", "yuv420p",
                    "-g", "48", "-vf", &vf,
                ]);
            }
            Mode::Qsv => {
                cmd.args([
                    "-c:v", "h264_qsv", "-global_quality", quality.cq(), "-preset", "medium",
                    "-pix_fmt", "nv12", "-g", "48", "-vf", &vf,
                ]);
            }
            Mode::X264 => {
                cmd.args([
                    "-c:v", "libx264", "-preset", "veryfast", "-crf", quality.cq(),
                    "-maxrate", quality.maxrate(), "-bufsize", quality.bufsize(),
                    "-pix_fmt", "yuv420p", "-g", "48", "-vf", &vf,
                ]);
            }
        }

        // Audio: always AAC stereo — TV-safe, and the AC3/DTS/Atmos sources in
        // this library cannot be passed to most renderers.
        let ext = if seg_type == "mpegts" { "ts" } else { "m4s" };
        cmd.args(["-c:a", "aac", "-b:a", "192k", "-ac", "2"])
            .args(["-f", "hls", "-hls_time", &segment.clamp(2, 10).to_string()])
            .args(["-hls_playlist_type", "event", "-hls_segment_type", seg_type])
            .args(["-hls_flags", "independent_segments+temp_file"])
            .arg("-hls_segment_filename")
            .arg(dir.join(format!("seg%05d.{ext}")))
            .arg(dir.join("index.m3u8"))
            .stdout(Stdio::null())
            .stderr(
                std::fs::File::create(dir.join("ffmpeg.log"))
                    .map(Stdio::from)
                    .unwrap_or_else(|_| Stdio::null()),
            );

        let child = cmd.spawn().map_err(|e| format!("ffmpeg spawn failed: {e}"))?;

        let session = Session {
            id: id.clone(),
            item_id: item_id.to_string(),
            dir,
            mode,
            quality,
            started: now(),
            child: Mutex::new(Some(child)),
        };
        self.sessions.lock().unwrap().insert(id.clone(), session);
        Ok((id, mode))
    }

    pub fn session_dir(&self, id: &str) -> Option<PathBuf> {
        self.sessions.lock().unwrap().get(id).map(|s| s.dir.clone())
    }

    pub fn stats(&self) -> serde_json::Value {
        let sessions = self.sessions.lock().unwrap();
        let list: Vec<serde_json::Value> = sessions
            .values()
            .map(|s| {
                let segs = std::fs::read_dir(&s.dir)
                    .map(|rd| {
                        rd.flatten()
                            .filter(|e| {
                                e.path()
                                    .extension()
                                    .map(|x| x == "m4s" || x == "ts")
                                    .unwrap_or(false)
                            })
                            .count()
                    })
                    .unwrap_or(0);
                serde_json::json!({
                    "id": s.id,
                    "item": s.item_id,
                    "mode": s.mode.label(),
                    "quality": s.quality.label(),
                    "segments": segs,
                    "uptime": now().saturating_sub(s.started),
                })
            })
            .collect();
        serde_json::json!({ "hardware": self.hw.summary, "sessions": list })
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn short(s: &str) -> String {
    s.chars().rev().take(6).collect::<String>().chars().rev().collect()
}

/// Escapes a Windows path for use inside the ffmpeg `subtitles` filter.
fn escape_filter_path(path: &str) -> String {
    path.replace('\\', "/")
        .replace(':', "\\:")
        .replace('\'', "\\'")
}
