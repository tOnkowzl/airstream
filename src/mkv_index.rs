//! Keyframe index for a media file.
//!
//! Primary method: parse the Matroska Cues (EBML) directly - ~50 ms on a 10 GB file.
//! Fallback: `ffprobe -skip_frame nokey -show_entries frame=pts_time`.
//! Results are cached as JSON in `cache_dir`, keyed by path + size + mtime.
//!
//! Self-contained apart from the sibling `process` helper (CREATE_NO_WINDOW).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::process;

const ID_EBML: u32 = 0x1A45_DFA3;
const ID_SEGMENT: u32 = 0x1853_8067;
const ID_SEEKHEAD: u32 = 0x114D_9B74;
const ID_SEEK: u32 = 0x4DBB;
const ID_SEEKID: u32 = 0x53AB;
const ID_SEEKPOS: u32 = 0x53AC;
const ID_INFO: u32 = 0x1549_A966;
const ID_TIMESCALE: u32 = 0x2A_D7B1;
const ID_TRACKS: u32 = 0x1654_AE6B;
const ID_TRACKENTRY: u32 = 0xAE;
const ID_TRACKNUM: u32 = 0xD7;
const ID_TRACKTYPE: u32 = 0x83;
const ID_CUES: u32 = 0x1C53_BB6B;
const ID_CUEPOINT: u32 = 0xBB;
const ID_CUETIME: u32 = 0xB3;
const ID_CUETRACKPOS: u32 = 0xB7;
const ID_CUETRACK: u32 = 0xF7;
const ID_CLUSTER: u32 = 0x1F43_B675;

/// Largest metadata element we are willing to load into memory.
const MAX_META: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct VideoInfo {
    pub index: usize,
    pub codec: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug)]
pub struct AudioInfo {
    /// Absolute stream index (as used by ffmpeg `-map 0:N`).
    pub index: usize,
    pub codec: String,
    pub channels: u32,
}

#[derive(Clone, Debug)]
pub struct MediaBasics {
    pub duration: f64,
    pub video: VideoInfo,
    pub audio: Vec<AudioInfo>,
}

// ---------------------------------------------------------------- EBML primitives

/// Element ID (marker bits kept) -> (id, length in bytes).
fn read_id(b: &[u8]) -> Option<(u32, usize)> {
    let first = *b.first()?;
    let len = first.leading_zeros() as usize + 1;
    if len > 4 || b.len() < len {
        return None;
    }
    let mut v = 0u32;
    for &c in &b[..len] {
        v = (v << 8) | c as u32;
    }
    Some((v, len))
}

/// Data size -> (size or None when "unknown", length in bytes).
fn read_size(b: &[u8]) -> Option<(Option<u64>, usize)> {
    let first = *b.first()?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || b.len() < len {
        return None;
    }
    let mut v = (first as u64) & ((1u64 << (8 - len)) - 1);
    for &c in &b[1..len] {
        v = (v << 8) | c as u64;
    }
    let unknown = v == (1u64 << (7 * len)) - 1;
    Some((if unknown { None } else { Some(v) }, len))
}

/// Children of an in-memory master element body: (id, data). Unknown size => rest of buffer.
fn children(mut b: &[u8]) -> Vec<(u32, &[u8])> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let Some((id, il)) = read_id(b) else { break };
        let Some((size, sl)) = read_size(&b[il..]) else { break };
        let start = il + sl;
        let end = match size {
            Some(s) => start.saturating_add(s as usize).min(b.len()),
            None => b.len(),
        };
        out.push((id, &b[start..end]));
        b = &b[end..];
    }
    out
}

fn uint(b: &[u8]) -> u64 {
    b.iter().take(8).fold(0u64, |a, &c| (a << 8) | c as u64)
}

fn read_at(f: &mut File, pos: u64, len: usize) -> Result<Vec<u8>, String> {
    f.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; len];
    let mut got = 0;
    while got < len {
        let n = f.read(&mut buf[got..]).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        got += n;
    }
    buf.truncate(got);
    Ok(buf)
}

/// Element header at file position: (id, size, data_pos).
fn header_at(f: &mut File, pos: u64) -> Result<Option<(u32, Option<u64>, u64)>, String> {
    let b = read_at(f, pos, 16)?;
    let Some((id, il)) = read_id(&b) else { return Ok(None) };
    let Some((size, sl)) = read_size(&b[il..]) else { return Ok(None) };
    Ok(Some((id, size, pos + (il + sl) as u64)))
}

fn read_body(f: &mut File, data_pos: u64, size: u64) -> Result<Vec<u8>, String> {
    if size > MAX_META {
        return Err("metadata element too large".into());
    }
    let b = read_at(f, data_pos, size as usize)?;
    if (b.len() as u64) < size {
        return Err("truncated element".into());
    }
    Ok(b)
}

/// Matroska Cues -> video-track cue times in seconds (sorted, deduped).
pub fn cues_index(path: &Path) -> Result<Vec<f64>, String> {
    let mut f = File::open(path).map_err(|e| e.to_string())?;
    let (id, size, data) = header_at(&mut f, 0)?.ok_or("not EBML")?;
    if id != ID_EBML {
        return Err("not EBML".into());
    }
    let seg_pos = data + size.ok_or("bad EBML header")?;
    let (sid, ssize, seg_data) = header_at(&mut f, seg_pos)?.ok_or("no segment")?;
    if sid != ID_SEGMENT {
        return Err("no segment".into());
    }
    let seg_end = ssize.map(|s| seg_data + s).unwrap_or(u64::MAX);

    let mut timescale: u64 = 1_000_000;
    let mut vtrack: Option<u64> = None;
    let mut seekhead_cues: Option<u64> = None;
    let mut cues_pos: Option<u64> = None;

    let mut pos = seg_data;
    while pos < seg_end {
        let Some((eid, esize, edata)) = header_at(&mut f, pos)? else { break };
        match eid {
            ID_SEEKHEAD | ID_INFO | ID_TRACKS => {
                let Some(sz) = esize else { break };
                let body = read_body(&mut f, edata, sz)?;
                for (cid, cb) in children(&body) {
                    match (eid, cid) {
                        (ID_SEEKHEAD, ID_SEEK) => {
                            let (mut sid, mut sp) = (None, None);
                            for (xid, xb) in children(cb) {
                                if xid == ID_SEEKID {
                                    sid = Some(uint(xb) as u32);
                                } else if xid == ID_SEEKPOS {
                                    sp = Some(uint(xb));
                                }
                            }
                            if sid == Some(ID_CUES) {
                                seekhead_cues = sp.map(|p| seg_data + p);
                            }
                        }
                        (ID_INFO, ID_TIMESCALE) => timescale = uint(cb),
                        (ID_TRACKS, ID_TRACKENTRY) => {
                            let (mut num, mut typ) = (None, None);
                            for (xid, xb) in children(cb) {
                                if xid == ID_TRACKNUM {
                                    num = Some(uint(xb));
                                } else if xid == ID_TRACKTYPE {
                                    typ = Some(uint(xb));
                                }
                            }
                            if typ == Some(1) && vtrack.is_none() {
                                vtrack = num;
                            }
                        }
                        _ => {}
                    }
                }
            }
            ID_CUES => {
                cues_pos = Some(pos);
                break;
            }
            ID_CLUSTER if seekhead_cues.is_some() && vtrack.is_some() => break,
            _ => {}
        }
        let Some(sz) = esize else { break };
        pos = edata + sz;
    }
    let cues_pos = cues_pos.or(seekhead_cues).ok_or("no cues")?;
    let vtrack = vtrack.ok_or("no video track")?;
    let (cid, csize, cdata) = header_at(&mut f, cues_pos)?.ok_or("cues pos wrong")?;
    if cid != ID_CUES {
        return Err("cues pos wrong".into());
    }
    let body = read_body(&mut f, cdata, csize.ok_or("unknown-size cues")?)?;
    let mut times = Vec::new();
    for (id, cp) in children(&body) {
        if id != ID_CUEPOINT {
            continue;
        }
        let mut t = None;
        let mut hit = false;
        for (xid, xb) in children(cp) {
            if xid == ID_CUETIME {
                t = Some(uint(xb));
            } else if xid == ID_CUETRACKPOS {
                for (yid, yb) in children(xb) {
                    if yid == ID_CUETRACK && uint(yb) == vtrack {
                        hit = true;
                    }
                }
            }
        }
        if let (Some(t), true) = (t, hit) {
            times.push(t as f64 * timescale as f64 / 1e9);
        }
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    times.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    Ok(times)
}

// ---------------------------------------------------------------- ffprobe

fn ffprobe_bin() -> String {
    std::env::var("AIRSTREAM_FFPROBE").unwrap_or_else(|_| "ffprobe".into())
}

fn ffprobe_skip(path: &Path) -> Result<Vec<f64>, String> {
    let out = process::command(ffprobe_bin())
        .args(["-v", "error", "-select_streams", "v:0", "-skip_frame", "nokey", "-show_entries", "frame=pts_time", "-of", "csv=p=0"])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("ffprobe: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut v: Vec<f64> = text
        .split_whitespace()
        .filter_map(|x| x.trim_matches(',').parse::<f64>().ok())
        .collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Ok(v)
}

pub fn probe_basic(path: &Path) -> Result<MediaBasics, String> {
    let out = process::command(ffprobe_bin())
        .args(["-v", "error", "-show_streams", "-show_format", "-of", "json"])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("ffprobe: {e}"))?;
    let j: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).map_err(|e| format!("ffprobe json: {e}"))?;
    let duration = j["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .ok_or("no duration")?;
    let mut video = None;
    let mut audio = Vec::new();
    for s in j["streams"].as_array().ok_or("no streams")? {
        let index = s["index"].as_u64().unwrap_or(0) as usize;
        let codec = s["codec_name"].as_str().unwrap_or("").to_string();
        match s["codec_type"].as_str() {
            Some("video") if video.is_none() && s["disposition"]["attached_pic"].as_u64().unwrap_or(0) == 0 => {
                video = Some(VideoInfo {
                    index,
                    codec,
                    width: s["width"].as_u64().unwrap_or(0) as u32,
                    height: s["height"].as_u64().unwrap_or(0) as u32,
                });
            }
            Some("audio") => audio.push(AudioInfo { index, codec, channels: s["channels"].as_u64().unwrap_or(2) as u32 }),
            _ => {}
        }
    }
    Ok(MediaBasics { duration, video: video.ok_or("no video stream")?, audio })
}

// ---------------------------------------------------------------- cache + entry point

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn file_key(path: &Path) -> Result<(u64, u64), String> {
    let m = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mt = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok((m.len(), mt))
}

/// Keyframe presentation times (seconds, ascending) of the first video stream.
pub fn keyframes(path: &Path, cache_dir: &Path) -> Result<Vec<f64>, String> {
    let (size, mtime) = file_key(path)?;
    let canon = path.to_string_lossy().to_string();
    let cache_file: PathBuf = cache_dir.join(format!("kf_{:016x}.json", fnv1a(&canon)));
    if let Ok(txt) = std::fs::read_to_string(&cache_file) {
        if let Ok(j) = serde_json::from_str::<serde_json::Value>(&txt) {
            if j["size"].as_u64() == Some(size) && j["mtime"].as_u64() == Some(mtime) && j["path"].as_str() == Some(canon.as_str()) {
                if let Some(a) = j["kf"].as_array() {
                    let v: Vec<f64> = a.iter().filter_map(|x| x.as_f64()).collect();
                    if !v.is_empty() {
                        return Ok(v);
                    }
                }
            }
        }
    }
    let (kf, how) = match cues_index(path) {
        Ok(v) if !v.is_empty() => (v, "cues"),
        other => {
            if let Err(e) = other {
                tracing::debug!("cues parse failed ({e}); using ffprobe");
            }
            (ffprobe_skip(path)?, "ffprobe")
        }
    };
    if kf.is_empty() {
        return Err("no keyframes found".into());
    }
    let _ = std::fs::create_dir_all(cache_dir);
    let j = serde_json::json!({"path": canon, "size": size, "mtime": mtime, "how": how, "kf": kf});
    let tmp = cache_file.with_extension("tmp");
    if std::fs::write(&tmp, j.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &cache_file);
    }
    Ok(kf)
}
