//! On-demand VOD HLS segmenter: stream-copied video, copied or AAC-transcoded audio.
//!
//! Segment boundaries are source keyframes (from `mkv_index`), segments are produced lazily by
//! short ffmpeg runs (~30 s of media each) and cached on disk under `<base>/hls/<session>/`.
//! A request for a segment outside the running window kills the run and restarts at that segment.
//! Self-contained apart from the sibling `mkv_index` and `process` helpers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime};

use tokio::process::Child;
use tokio::sync::Notify;

use crate::mkv_index;
use crate::process;

/// Target segment length (s): a boundary is the first keyframe >= start + TARGET.
const TARGET: f64 = 6.0;
/// A final tail shorter than this is merged into the previous segment.
const MIN_SEG: f64 = 2.0;
/// One ffmpeg run produces about this many seconds of segments.
const CHUNK_S: f64 = 30.0;
/// Never produce further than this ahead of the latest requested segment.
const AHEAD_S: f64 = 45.0;
/// How far past a keyframe the input `-ss` is aimed (see `inputs`).
const SEEK_SLOP: f64 = 0.25;
/// The ffmpeg run is stopped this long after the first keyframe beyond its last segment.
const TO_PAST: f64 = 0.35;
const SEG_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_FAILS: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AudioMode {
    /// Source audio is AAC: stream copy.
    Copy,
    /// Transcode to AAC-LC, `channels` = min(6, source channels).
    Aac { channels: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SegFormat {
    Fmp4,
    #[allow(dead_code)] // used by the standalone hls_serve test server
    Ts,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VodOptions {
    /// Absolute stream index (ffprobe `index`) of the audio stream.
    pub audio_index: usize,
    pub audio: AudioMode,
    pub format: SegFormat,
}

struct Run {
    child: Child,
    start: usize,
    end: usize,
    next: usize,
    id: u64,
    /// Some(success) once the process has exited.
    exit: Option<bool>,
}

impl Run {
    fn poll(&mut self) -> Option<bool> {
        if self.exit.is_none() {
            if let Ok(Some(st)) = self.child.try_wait() {
                self.exit = Some(st.success());
            }
        }
        self.exit
    }
}

struct State {
    run: Option<Run>,
    latest: usize,
    hi: Option<usize>,
    next_run_id: u64,
    segrun: HashMap<usize, u64>,
    fails: HashMap<usize, u32>,
    last_used: SystemTime,
}

struct Session {
    #[allow(dead_code)]
    id: String,
    src: PathBuf,
    opts: VodOptions,
    dir: PathBuf,
    vcodec: String,
    v_idx: usize,
    width: u32,
    height: u32,
    duration: f64,
    size_bytes: u64,
    kf: Vec<f64>,
    segs: Vec<(f64, f64)>,
    codecs: OnceLock<String>,
    /// (video reorder delay in s = first keyframe's pts - dts, video timescale). The segment muxer
    /// cuts at the keyframe's *dts* (d earlier than its pts), which would put the audio cut d early;
    /// the video input is therefore delayed by d (`-itsoffset`) and the shift is undone in tfdt.
    vdelay: Mutex<(f64, u32)>,
    state: Mutex<State>,
    notify: Notify,
    init_lock: tokio::sync::Mutex<()>,
    stopped: AtomicBool,
}

enum Poll {
    Ready(PathBuf),
    Wait,
    Failed(String),
}

struct Inner {
    base: PathBuf,
    cap_bytes: u64,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    evictor_started: AtomicBool,
}

#[derive(Clone)]
pub struct HlsManager {
    inner: Arc<Inner>,
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn ffmpeg_bin() -> String {
    std::env::var("AIRSTREAM_FFMPEG").unwrap_or_else(|_| "ffmpeg".into())
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Greedy: next boundary = first keyframe >= start+TARGET; short tail merged; last ends at `duration`.
fn build_segments(kf: &[f64], duration: f64) -> Vec<(f64, f64)> {
    let mut bounds = vec![kf[0]];
    for &t in &kf[1..] {
        if t - *bounds.last().unwrap() >= TARGET - 1e-6 {
            bounds.push(t);
        }
    }
    if duration - *bounds.last().unwrap() < MIN_SEG && bounds.len() > 1 {
        bounds.pop();
    }
    bounds.push(duration);
    bounds.windows(2).map(|w| (w[0], w[1])).collect()
}

// ---------------------------------------------------------------- mp4 box helpers

/// (type, pos, size, header_len) of the boxes in buf[start..end].
fn boxes(buf: &[u8], start: usize, end: usize) -> Vec<([u8; 4], usize, usize, usize)> {
    let mut out = Vec::new();
    let mut p = start;
    while p + 8 <= end {
        let mut size = u32::from_be_bytes(buf[p..p + 4].try_into().unwrap()) as usize;
        let typ: [u8; 4] = buf[p + 4..p + 8].try_into().unwrap();
        let mut hdr = 8;
        if size == 1 {
            if p + 16 > end {
                break;
            }
            size = u64::from_be_bytes(buf[p + 8..p + 16].try_into().unwrap()) as usize;
            hdr = 16;
        } else if size == 0 {
            size = end - p;
        }
        if size < hdr || p + size > end {
            break;
        }
        out.push((typ, p, size, hdr));
        p += size;
    }
    out
}

/// Split a raw fragmented mp4 into (ftyp+moov, moof+mdat...) dropping styp.
fn split_mp4(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let (mut init, mut media) = (Vec::new(), Vec::new());
    for (t, p, s, _) in boxes(data, 0, data.len()) {
        match &t {
            b"ftyp" | b"moov" => init.extend_from_slice(&data[p..p + s]),
            b"styp" => {}
            _ => media.extend_from_slice(&data[p..p + s]),
        }
    }
    (init, media)
}

/// movenc (without an edit list, which we strip) shifts the dts of B-frame streams by the first
/// sample's composition offset `d`, so every video pts ends up `d` late relative to the audio and
/// the playlist. Subtract `d` from the video `tfdt` of every fragment (each starts on a keyframe,
/// whose pts-dts is exactly that offset) so that pts == source pts again.
fn fix_video_tfdt(media: &mut [u8], extra: i64) {
    walk_video_trafs(media, Some(extra));
}

/// Composition offset (ticks) of the first video sample of the first fragment, if any.
fn first_video_cts(media: &mut [u8]) -> Option<i64> {
    walk_video_trafs(media, None)
}

/// Visit every `traf` that carries composition offsets; returns the first one's first-sample offset.
/// With `patch` the traf's `tfdt` is moved back by that offset.
fn walk_video_trafs(media: &mut [u8], patch: Option<i64>) -> Option<i64> {
    let mut found = None;
    let top = boxes(media, 0, media.len());
    for (t, p, s, h) in top {
        if &t != b"moof" {
            continue;
        }
        for (tt, tp, ts, th) in boxes(media, p + h, p + s) {
            if &tt != b"traf" {
                continue;
            }
            let (mut tfdt, mut first_cts, mut trun_info) = (None, None, None);
            for (bt, bp, bs, bh) in boxes(media, tp + th, tp + ts) {
                let b = bp + bh;
                if &bt == b"tfdt" {
                    tfdt = Some((b, media[b]));
                } else if &bt == b"trun" && bs >= bh + 8 {
                    let ver = media[b];
                    let flags = u32::from_be_bytes([0, media[b + 1], media[b + 2], media[b + 3]]);
                    if flags & 0x800 == 0 || u32::from_be_bytes(media[b + 4..b + 8].try_into().unwrap()) == 0 {
                        continue;
                    }
                    let mut o = b + 8;
                    if flags & 0x1 != 0 {
                        o += 4;
                    }
                    let first_flags = flags & 0x4 != 0;
                    if first_flags {
                        o += 4;
                    }
                    for bit in [0x100u32, 0x200, 0x400] {
                        if flags & bit != 0 {
                            o += 4;
                        }
                    }
                    let count = u32::from_be_bytes(media[b + 4..b + 8].try_into().unwrap()) as usize;
                    let stride = 4 * [0x100u32, 0x200, 0x400, 0x800].iter().filter(|f| flags & **f != 0).count();
                    if o + 4 <= bp + bs {
                        trun_info = Some((o, stride, count, ver));
                        let raw = u32::from_be_bytes(media[o..o + 4].try_into().unwrap());
                        first_cts = Some(if ver == 0 { raw as i64 } else { raw as i32 as i64 });
                    }
                }
            }
            if let (Some((b, ver)), Some(d)) = (tfdt, first_cts) {
                if found.is_none() {
                    found = Some(d);
                }
                if let (Some(extra), true) = (patch, d > 0) {
                    let d = d + extra;
                    let old = if ver == 1 {
                        u64::from_be_bytes(media[b + 4..b + 12].try_into().unwrap())
                    } else {
                        u32::from_be_bytes(media[b + 4..b + 8].try_into().unwrap()) as u64
                    };
                    // dts cannot go negative (start of the file): clamp tfdt at 0 and give the
                    // missing amount to the composition offsets instead, so pts stay exact
                    let new = old.saturating_sub(d as u64);
                    let deficit = (d as u64).saturating_sub(old) as i64;
                    if ver == 1 {
                        media[b + 4..b + 12].copy_from_slice(&new.to_be_bytes());
                    } else {
                        media[b + 4..b + 8].copy_from_slice(&(new as u32).to_be_bytes());
                    }
                    if let (true, Some((o, stride, count, tver))) = (deficit > 0, trun_info) {
                        for i in 0..count {
                            let q = o + i * stride;
                            if q + 4 > media.len() {
                                break;
                            }
                            let raw = u32::from_be_bytes(media[q..q + 4].try_into().unwrap());
                            let v = if tver == 0 { raw as i64 } else { raw as i32 as i64 } - deficit;
                            let v = if tver == 0 { v.max(0) } else { v };
                            media[q..q + 4].copy_from_slice(&(v as u32).to_be_bytes());
                        }
                    }
                }
            }
        }
    }
    found
}

/// Remove `edts` boxes inside trak/mdia so absolute tfdt are authoritative.
fn strip_edts(body: &[u8]) -> Vec<u8> {
    let mut res = Vec::new();
    for (t, p, s, h) in boxes(body, 0, body.len()) {
        match &t {
            b"edts" => {}
            b"trak" | b"mdia" => {
                let inner = strip_edts(&body[p + h..p + s]);
                res.extend_from_slice(&((8 + inner.len()) as u32).to_be_bytes());
                res.extend_from_slice(&t);
                res.extend_from_slice(&inner);
            }
            _ => res.extend_from_slice(&body[p..p + s]),
        }
    }
    res
}

fn make_init(raw_init: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for (t, p, s, h) in boxes(raw_init, 0, raw_init.len()) {
        if &t == b"moov" {
            let inner = strip_edts(&raw_init[p + h..p + s]);
            out.extend_from_slice(&((8 + inner.len()) as u32).to_be_bytes());
            out.extend_from_slice(b"moov");
            out.extend_from_slice(&inner);
        } else {
            out.extend_from_slice(&raw_init[p..p + s]);
        }
    }
    out
}

/// Timescale of the first track's `mdhd` in an init segment.
fn first_track_timescale(init: &[u8]) -> Option<u32> {
    let i = find_sub(init, b"mdhd")?;
    let v = *init.get(i + 4)?;
    let o = i + 4 + 4 + if v == 1 { 16 } else { 8 };
    Some(u32::from_be_bytes(init.get(o..o + 4)?.try_into().ok()?))
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn hex_upper(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// RFC 6381 video+audio codec string from the init segment (avcC / hvcC).
fn codec_string(init: &[u8], vcodec: &str) -> String {
    let mut v = if vcodec == "h264" { "avc1.640028".to_string() } else { "hvc1.1.6.L120.B0".to_string() };
    if let Some(i) = find_sub(init, b"avcC") {
        if init.len() >= i + 8 {
            v = format!("avc1.{}", hex_upper(&init[i + 5..i + 8]));
        }
    }
    if let Some(i) = find_sub(init, b"hvcC") {
        if init.len() >= i + 4 + 13 {
            let b = &init[i + 4..i + 4 + 13];
            let sp = b[1] >> 6;
            let tier = (b[1] >> 5) & 1;
            let idc = b[1] & 31;
            let compat = u32::from_be_bytes([b[2], b[3], b[4], b[5]]).reverse_bits();
            let space = match sp {
                1 => "A",
                2 => "B",
                3 => "C",
                _ => "",
            };
            v = format!("hvc1.{space}{idc}.{compat:X}.{}{}", if tier == 1 { "H" } else { "L" }, b[12]);
            let mut cs: Vec<String> = b[6..12].iter().map(|x| format!("{x:X}")).collect();
            while cs.last().map(|s| s == "0").unwrap_or(false) {
                cs.pop();
            }
            if !cs.is_empty() {
                v.push('.');
                v.push_str(&cs.join("."));
            }
        }
    }
    format!("{v},mp4a.40.2")
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", TMP_COUNTER.fetch_add(1, Ordering::Relaxed)));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

// ---------------------------------------------------------------- session

impl Session {
    fn ext(&self) -> &'static str {
        match self.opts.format {
            SegFormat::Fmp4 => "m4s",
            SegFormat::Ts => "ts",
        }
    }

    fn raw_ext(&self) -> &'static str {
        match self.opts.format {
            SegFormat::Fmp4 => "mp4",
            SegFormat::Ts => "ts",
        }
    }

    fn final_path(&self, n: usize) -> PathBuf {
        self.dir.join(format!("seg{n}.{}", self.ext()))
    }

    fn raw_path(&self, run_id: u64, n: usize) -> PathBuf {
        self.dir.join("raw").join(format!("r{run_id}_{n}.{}", self.raw_ext()))
    }

    fn have(&self, n: usize) -> bool {
        self.final_path(n).exists()
    }

    fn init_path(&self) -> PathBuf {
        self.dir.join("init.mp4")
    }

    fn playlist(&self) -> String {
        let fmp4 = self.opts.format == SegFormat::Fmp4;
        let td = self.segs.iter().map(|(s, e)| e - s).fold(0.0f64, f64::max).ceil() as u64;
        let mut l = vec![
            "#EXTM3U".to_string(),
            format!("#EXT-X-VERSION:{}", if fmp4 { 7 } else { 3 }),
            format!("#EXT-X-TARGETDURATION:{td}"),
            "#EXT-X-MEDIA-SEQUENCE:0".into(),
            "#EXT-X-PLAYLIST-TYPE:VOD".into(),
            "#EXT-X-INDEPENDENT-SEGMENTS".into(),
        ];
        if fmp4 {
            l.push("#EXT-X-MAP:URI=\"init.mp4\"".into());
        }
        for (i, (s, e)) in self.segs.iter().enumerate() {
            l.push(format!("#EXTINF:{:.3},", e - s));
            l.push(format!("seg{i}.{}", self.ext()));
        }
        l.push("#EXT-X-ENDLIST".into());
        l.join("\n") + "\n"
    }

    fn master(&self) -> Option<String> {
        let codecs = self.codecs.get()?;
        let bw = (self.size_bytes as f64 * 8.0 / self.duration) as u64;
        Some(format!(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-STREAM-INF:BANDWIDTH={bw},CODECS=\"{codecs}\",RESOLUTION={}x{}\nindex.m3u8\n",
            self.width, self.height
        ))
    }

    /// Input + map + codec args. Transcoded audio uses a 2nd input seeked 1 s earlier plus
    /// `atrim=start=T` so audio starts sample-accurately at T.
    fn inputs(&self, t: f64, loglevel: &str) -> Vec<String> {
        let src = fwd(&self.src);
        let (vd, _) = *self.vdelay.lock().unwrap();
        let mut c: Vec<String> = ["-hide_banner", "-nostdin", "-v", loglevel].iter().map(|s| s.to_string()).collect();
        if vd > 0.0 {
            c.extend(["-itsoffset".to_string(), format!("{vd:.6}")]);
        }
        c.push("-ss".into());
        // ffmpeg's matroska seek lands on the keyframe *before* the target when the target is
        // within ~0.1-0.2 s after a keyframe whose dts < pts (B-frames), i.e. seeking to exactly a
        // keyframe time returns the previous GOP. Aim a little past the keyframe, but never
        // closer than half way to the next one.
        let i = self.kf.partition_point(|&k| k <= t + 1e-4);
        let gap = self.kf.get(i).map(|k| k - t).unwrap_or(1.0);
        let seek = t + (gap / 2.0).min(SEEK_SLOP);
        c.push(format!("{seek:.3}"));
        c.push("-i".into());
        c.push(src.clone());
        let a = self.opts.audio_index;
        let mut vargs = vec!["-c:v".to_string(), "copy".into()];
        if self.vcodec == "hevc" && self.opts.format == SegFormat::Fmp4 {
            vargs.extend(["-tag:v".to_string(), "hvc1".into()]);
        }
        match self.opts.audio {
            AudioMode::Copy => {
                if vd > 0.0 {
                    // audio must not inherit the video delay: separate (undelayed) input
                    c.extend(["-ss".into(), format!("{seek:.3}"), "-i".into(), src]);
                    c.extend(["-map".into(), format!("0:{}", self.v_idx), "-map".into(), format!("1:{a}")]);
                } else {
                    c.extend(["-map".into(), format!("0:{}", self.v_idx), "-map".into(), format!("0:{a}")]);
                }
                c.extend(vargs);
                c.extend(["-c:a".into(), "copy".into()]);
            }
            AudioMode::Aac { channels } => {
                c.extend(["-ss".into(), format!("{:.3}", (t - 1.0).max(0.0)), "-i".into(), src]);
                c.extend(["-map".into(), format!("0:{}", self.v_idx), "-map".into(), format!("1:{a}")]);
                c.extend(vargs);
                c.extend(
                    ["-c:a", "aac", "-aac_coder", "fast", "-b:a", "512k", "-ac"].iter().map(|s| s.to_string()),
                );
                c.push(channels.clamp(1, 6).to_string());
                c.extend(["-af".into(), format!("atrim=start={t:.3}")]);
            }
        }
        c
    }

    /// ffmpeg arguments producing segments s..e-1 into raw files of run `run_id`.
    fn cmd(&self, s: usize, e: usize, run_id: u64) -> Vec<String> {
        let t = self.segs[s].0;
        // Also cut at segs[e] (when it exists): that partial extra segment is discarded, but its
        // presence is what closes segment e-1 - and `-to` must lie past the next keyframe's *dts*,
        // which is earlier than its pts for B-frame streams (a `-to` just before the boundary would
        // leak that keyframe into segment e-1).
        let mut bounds: Vec<String> = (s + 1..=e.min(self.segs.len() - 1)).map(|i| format!("{:.3}", self.segs[i].0 - t)).collect();
        if bounds.is_empty() {
            bounds.push("1000000".into());
        }
        let raw = fwd(&self.dir.join("raw").join(format!("r{run_id}_%d.{}", self.raw_ext())));
        let mut c = self.inputs(t, "warning");
        c.extend(["-sn", "-dn", "-copyts", "-avoid_negative_ts", "disabled"].iter().map(|s| s.to_string()));
        if e < self.segs.len() {
            c.extend(["-to".into(), format!("{:.3}", self.segs[e].0 + TO_PAST)]);
        }
        c.extend(
            ["-f", "segment", "-segment_times"].iter().map(|s| s.to_string()),
        );
        c.push(bounds.join(","));
        c.extend(
            ["-segment_time_delta", "0.02", "-reset_timestamps", "0", "-segment_start_number"].iter().map(|s| s.to_string()),
        );
        c.push(s.to_string());
        match self.opts.format {
            SegFormat::Fmp4 => c.extend(
                [
                    "-segment_format",
                    "mp4",
                    "-segment_format_options",
                    "movflags=+frag_keyframe+frag_discont+empty_moov+default_base_moof+delay_moov",
                ]
                .iter()
                .map(|s| s.to_string()),
            ),
            SegFormat::Ts => c.extend(
                ["-segment_format", "mpegts", "-segment_format_options", "mpegts_copyts=1"].iter().map(|s| s.to_string()),
            ),
        }
        c.push(raw);
        c
    }

    // ---- run management (all called with the state lock held, from blocking threads)

    fn start_run(&self, st: &mut State, n: usize) {
        self.stop_run(st);
        let nseg = self.segs.len();
        let mut e = n + 1;
        while e < nseg && self.segs[e].0 - self.segs[n].0 < CHUNK_S {
            e += 1;
        }
        // stop early where later segments already exist
        let mut e2 = n + 1;
        while e2 < e && !self.have(e2) {
            e2 += 1;
        }
        let e = e2;
        st.next_run_id += 1;
        let id = st.next_run_id;
        let args = self.cmd(n, e, id);
        let log = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("ffmpeg.log")).ok();
        if let Some(mut l) = log.as_ref() {
            use std::io::Write;
            let _ = writeln!(l, "ffmpeg {}", args.join(" "));
        }
        let mut cmd = process::async_command(ffmpeg_bin());
        cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::null()).kill_on_drop(true);
        cmd.stderr(log.map(Stdio::from).unwrap_or_else(Stdio::null));
        match cmd.spawn() {
            Ok(child) => st.run = Some(Run { child, start: n, end: e, next: n, id, exit: None }),
            Err(err) => {
                tracing::warn!("hls: ffmpeg spawn failed: {err}");
                *st.fails.entry(n).or_insert(0) += MAX_FAILS;
            }
        }
    }

    fn stop_run(&self, st: &mut State) {
        if let Some(mut r) = st.run.take() {
            self.end_run(st, &mut r, true);
        }
    }

    /// Kill (if still running), finalize closed segments, and remove leftover raw files.
    fn end_run(&self, st: &mut State, r: &mut Run, kill: bool) {
        if kill && r.poll().is_none() {
            let _ = r.child.start_kill();
            let t0 = Instant::now();
            while r.poll().is_none() && t0.elapsed() < Duration::from_secs(3) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        self.finalize(st, r);
        if r.next < r.end && !kill {
            // the run ended without producing everything it was asked for
            *st.fails.entry(r.next).or_insert(0) += 1;
        }
        let prefix = format!("r{}_", r.id);
        if let Ok(rd) = std::fs::read_dir(self.dir.join("raw")) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with(&prefix) {
                    for _ in 0..20 {
                        if std::fs::remove_file(e.path()).is_ok() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        }
    }

    /// Move closed raw segments to their final names. Segment k is closed when raw k+1 exists
    /// or the process exited successfully. Returns whether anything was finalized.
    fn finalize(&self, st: &mut State, r: &mut Run) -> bool {
        let mut any = false;
        while r.next < r.end {
            let k = r.next;
            let rp = self.raw_path(r.id, k);
            if !rp.exists() {
                break;
            }
            let nxt = self.raw_path(r.id, k + 1).exists();
            let done = r.poll() == Some(true);
            if !(nxt || done) {
                break;
            }
            let Ok(mut data) = std::fs::read(&rp) else { break };
            if self.opts.format == SegFormat::Fmp4 {
                let (init, media) = split_mp4(&data);
                if !self.init_path().exists() && !init.is_empty() {
                    let _ = write_atomic(&self.init_path(), &make_init(&init));
                }
                data = media;
                let (vd, vts) = *self.vdelay.lock().unwrap();
                fix_video_tfdt(&mut data, (vd * vts as f64).round() as i64);
            }
            if write_atomic(&self.final_path(k), &data).is_err() {
                break;
            }
            st.segrun.insert(k, r.id);
            let _ = std::fs::remove_file(&rp);
            r.next += 1;
            st.hi = Some(st.hi.map_or(k, |h| h.max(k)));
            any = true;
        }
        any
    }

    /// Finalize the current run; drop it if it ended. Returns true if anything was finalized.
    fn housekeep(&self, st: &mut State) -> bool {
        let Some(mut r) = st.run.take() else { return false };
        let any = self.finalize(st, &mut r);
        // once the process has exited, finalize() has done everything it ever can
        let over = r.poll().is_some();
        if over {
            self.end_run(st, &mut r, false);
        } else {
            st.run = Some(r);
        }
        any
    }

    /// Read-ahead scheduler: when idle, continue after the highest produced segment while it
    /// stays within AHEAD_S of the latest request.
    fn schedule(&self, st: &mut State) {
        if st.run.is_some() {
            return;
        }
        let Some(hi) = st.hi else { return };
        let nxt = hi + 1;
        let lat = st.latest.min(self.segs.len() - 1);
        if nxt < self.segs.len()
            && lat <= nxt
            && !self.have(nxt)
            && self.segs[nxt].0 - self.segs[lat].0 < AHEAD_S - CHUNK_S
            && st.fails.get(&nxt).copied().unwrap_or(0) < MAX_FAILS
        {
            self.start_run(st, nxt);
        }
    }

    /// Pump tick; returns whether a run is active.
    fn tick(&self) -> bool {
        let mut st = self.state.lock().unwrap();
        if self.housekeep(&mut st) {
            self.notify.notify_waiters();
        }
        self.schedule(&mut st);
        st.run.is_some()
    }

    fn request(&self, n: usize) -> Poll {
        let mut st = self.state.lock().unwrap();
        st.latest = n;
        st.last_used = SystemTime::now();
        if self.housekeep(&mut st) {
            self.notify.notify_waiters();
        }
        if self.have(n) {
            return Poll::Ready(self.final_path(n));
        }
        if st.fails.get(&n).copied().unwrap_or(0) >= MAX_FAILS {
            return Poll::Failed(format!("segment {n}: ffmpeg could not produce it (see ffmpeg.log)"));
        }
        let mut action = 0; // 0 start new run, 1 wait
        if let Some(r) = st.run.as_mut() {
            let alive = r.poll().is_none();
            if r.start <= n && n < r.end && r.next <= n {
                action = 1;
            } else if alive && n == r.end {
                action = 1; // next chunk: let the current run finish first
            }
        }
        if action == 0 {
            self.start_run(&mut st, n);
        }
        Poll::Wait
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        let mut st = self.state.lock().unwrap();
        self.stop_run(&mut st);
        self.notify.notify_waiters();
    }

    async fn ensure_init(&self) -> Result<PathBuf, String> {
        if self.opts.format != SegFormat::Fmp4 {
            return Err("no init segment in MPEG-TS mode".into());
        }
        let p = self.init_path();
        let vd_path = self.dir.join("vdelay.txt");
        let load = || -> bool {
            let txt = std::fs::read_to_string(&vd_path).unwrap_or_default();
            let mut it = txt.split_whitespace();
            match (it.next().and_then(|x| x.parse::<f64>().ok()), it.next().and_then(|x| x.parse::<u32>().ok())) {
                (Some(d), Some(ts)) if p.exists() => {
                    *self.vdelay.lock().unwrap() = (d, ts);
                    true
                }
                _ => false,
            }
        };
        if p.exists() && (load() || self.opts.audio == AudioMode::Copy) {
            return Ok(p);
        }
        let _g = self.init_lock.lock().await;
        if p.exists() && load() {
            return Ok(p);
        }
        // throwaway run on the first keyframe just to obtain the moov
        let t = self.segs[0].0;
        let tmp = self.dir.join(format!("init_probe{}.mp4", TMP_COUNTER.fetch_add(1, Ordering::Relaxed)));
        let mut args = self.inputs(t, "error");
        args.extend(["-y", "-copyts", "-avoid_negative_ts", "disabled", "-to"].iter().map(|s| s.to_string()));
        args.push(format!("{:.3}", t + 1.0));
        args.extend(
            ["-f", "mp4", "-movflags", "+frag_keyframe+frag_discont+empty_moov+default_base_moof+delay_moov"]
                .iter()
                .map(|s| s.to_string()),
        );
        args.push(fwd(&tmp));
        let mut cmd = process::async_command(ffmpeg_bin());
        cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| format!("ffmpeg: {e}"))?;
        match tokio::time::timeout(Duration::from_secs(60), child.wait()).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(format!("ffmpeg: {e}")),
            Err(_) => return Err("init probe timed out".into()),
        }
        let data = tokio::fs::read(&tmp).await.map_err(|e| format!("init probe produced nothing: {e}"));
        let _ = tokio::fs::remove_file(&tmp).await;
        let (init, mut media) = split_mp4(&data?);
        if init.is_empty() {
            return Err("init probe produced no moov".into());
        }
        let vd = match (first_video_cts(&mut media), first_track_timescale(&init)) {
            (Some(c), Some(ts)) if c > 0 && ts > 0 => (c as f64 / ts as f64, ts),
            _ => (0.0, 0),
        };
        *self.vdelay.lock().unwrap() = vd;
        let _ = std::fs::write(&vd_path, format!("{:.6} {}", vd.0, vd.1));
        write_atomic(&p, &make_init(&init)).map_err(|e| e.to_string())?;
        Ok(p)
    }
}

// ---------------------------------------------------------------- manager

impl HlsManager {
    pub fn new(base: PathBuf, cap_bytes: u64) -> Self {
        HlsManager {
            inner: Arc::new(Inner {
                base,
                cap_bytes,
                sessions: Mutex::new(HashMap::new()),
                evictor_started: AtomicBool::new(false),
            }),
        }
    }

    fn get(&self, sid: &str) -> Option<Arc<Session>> {
        self.inner.sessions.lock().unwrap().get(sid).cloned()
    }

    /// Index the source (cached), create the session and return its stable id.
    pub async fn open(&self, src: &Path, opts: VodOptions) -> Result<String, String> {
        let meta = std::fs::metadata(src).map_err(|e| format!("{}: {e}", src.display()))?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let key = format!("hlsvod1|{}|{}|{}|{:?}", src.to_string_lossy(), meta.len(), mtime, opts);
        let sid = format!("{:016x}", fnv1a(&key));
        if self.get(&sid).is_some() {
            return Ok(sid);
        }
        let kf_dir = self.inner.base.join("kfidx");
        let s2 = src.to_path_buf();
        let (kf, basics) = tokio::task::spawn_blocking(move || {
            let kf = mkv_index::keyframes(&s2, &kf_dir)?;
            let b = mkv_index::probe_basic(&s2)?;
            Ok::<_, String>((kf, b))
        })
        .await
        .map_err(|e| e.to_string())??;

        let audio = basics
            .audio
            .iter()
            .find(|a| a.index == opts.audio_index)
            .ok_or_else(|| format!("stream {} is not an audio stream", opts.audio_index))?;
        if opts.audio == AudioMode::Copy && audio.codec != "aac" {
            return Err(format!("audio copy requires AAC, stream is {}", audio.codec));
        }
        let segs = build_segments(&kf, basics.duration);
        let dir = self.inner.base.join("hls").join(&sid);
        std::fs::create_dir_all(dir.join("raw")).map_err(|e| e.to_string())?;
        if let Ok(rd) = std::fs::read_dir(dir.join("raw")) {
            for e in rd.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
        let _ = std::fs::write(dir.join(".used"), b"");
        let session = Arc::new(Session {
            id: sid.clone(),
            src: src.to_path_buf(),
            opts,
            dir,
            vcodec: basics.video.codec.clone(),
            v_idx: basics.video.index,
            width: basics.video.width,
            height: basics.video.height,
            duration: basics.duration,
            size_bytes: meta.len(),
            kf,
            segs,
            codecs: OnceLock::new(),
            vdelay: Mutex::new((0.0, 0)),
            state: Mutex::new(State {
                run: None,
                latest: 0,
                hi: None,
                next_run_id: 0,
                segrun: HashMap::new(),
                fails: HashMap::new(),
                last_used: SystemTime::now(),
            }),
            notify: Notify::new(),
            init_lock: tokio::sync::Mutex::new(()),
            stopped: AtomicBool::new(false),
        });
        match opts.format {
            SegFormat::Fmp4 => {
                let p = session.ensure_init().await?;
                let init = tokio::fs::read(&p).await.map_err(|e| e.to_string())?;
                let _ = session.codecs.set(codec_string(&init, &session.vcodec));
            }
            SegFormat::Ts => {
                let _ = session.codecs.set(codec_string(&[], &session.vcodec));
            }
        }
        {
            let mut map = self.inner.sessions.lock().unwrap();
            if map.contains_key(&sid) {
                return Ok(sid);
            }
            map.insert(sid.clone(), session.clone());
        }
        // pump: finalizes segments and schedules read-ahead
        let weak: Weak<Session> = Arc::downgrade(&session);
        tokio::spawn(async move {
            loop {
                let Some(s) = weak.upgrade() else { break };
                if s.stopped.load(Ordering::SeqCst) {
                    break;
                }
                let active = tokio::task::spawn_blocking(move || s.tick()).await.unwrap_or(false);
                tokio::time::sleep(Duration::from_millis(if active { 25 } else { 250 })).await;
            }
        });
        self.start_evictor();
        Ok(sid)
    }

    pub fn master(&self, sid: &str) -> Option<String> {
        self.get(sid)?.master()
    }

    pub fn playlist(&self, sid: &str) -> Option<String> {
        Some(self.get(sid)?.playlist())
    }

    pub async fn init(&self, sid: &str) -> Result<PathBuf, String> {
        self.get(sid).ok_or("unknown session")?.ensure_init().await
    }

    /// Produce (if needed) and return the path of segment `n`.
    pub async fn segment(&self, sid: &str, n: usize) -> Result<PathBuf, String> {
        let s = self.get(sid).ok_or("unknown session")?;
        if n >= s.segs.len() {
            return Err(format!("segment {n} out of range"));
        }
        let t0 = Instant::now();
        loop {
            if s.stopped.load(Ordering::SeqCst) {
                return Err("session stopped".into());
            }
            let s2 = s.clone();
            match tokio::task::spawn_blocking(move || s2.request(n)).await.map_err(|e| e.to_string())? {
                Poll::Ready(p) => return Ok(p),
                Poll::Failed(e) => return Err(e),
                Poll::Wait => {
                    let _ = tokio::time::timeout(Duration::from_millis(40), s.notify.notified()).await;
                }
            }
            if t0.elapsed() > SEG_TIMEOUT {
                return Err(format!("segment {n}: timeout"));
            }
        }
    }

    /// Index of the segment containing playback position `t` seconds.
    pub fn segment_at(&self, sid: &str, t: f64) -> Option<usize> {
        let s = self.get(sid)?;
        let t = t.max(0.0);
        let i = s.segs.iter().position(|(a, b)| t >= *a && t < *b).unwrap_or(s.segs.len().saturating_sub(1));
        Some(i)
    }

    /// Warm a session: init segment plus the segment at `t` and the next one.
    pub async fn prefetch(&self, sid: &str, t: f64) {
        let _ = self.init(sid).await;
        if let Some(n) = self.segment_at(sid, t) {
            let _ = self.segment(sid, n).await;
            let _ = self.segment(sid, n + 1).await;
        }
    }

    /// Kill all running ffmpeg processes and stop all sessions.
    pub fn stop_all(&self) {
        let all: Vec<Arc<Session>> = self.inner.sessions.lock().unwrap().drain().map(|(_, v)| v).collect();
        for s in all {
            s.stop();
        }
    }

    #[allow(dead_code)]
    /// Debug/test info: keyframes, segment table and which ffmpeg run produced each segment.
    pub fn debug(&self, sid: &str) -> Option<serde_json::Value> {
        let s = self.get(sid)?;
        let st = s.state.lock().unwrap();
        let runs: serde_json::Map<String, serde_json::Value> =
            st.segrun.iter().map(|(k, v)| (k.to_string(), serde_json::json!(v))).collect();
        Some(serde_json::json!({
            "id": s.id, "dir": fwd(&s.dir), "duration": s.duration, "kf": s.kf, "segs": s.segs,
            "nseg": s.segs.len(), "segrun": runs, "codecs": s.codecs.get(),
        }))
    }

    // ---- cache eviction

    fn start_evictor(&self) {
        if self.inner.evictor_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            loop {
                let Some(inner) = weak.upgrade() else { break };
                let _ = tokio::task::spawn_blocking(move || evict(&inner)).await;
                tokio::time::sleep(Duration::from_secs(15)).await;
            }
        });
    }
}

/// Delete least-recently-used sessions' segment files until the cache fits `cap_bytes`.
/// Segments of an active run are never removed.
fn evict(inner: &Inner) {
    struct Cand {
        path: PathBuf,
        size: u64,
        session_used: SystemTime,
        mtime: SystemTime,
    }
    let hls = inner.base.join("hls");
    let Ok(rd) = std::fs::read_dir(&hls) else { return };
    // snapshot of open sessions: id -> (last_used, protected segment range)
    let open: HashMap<String, (SystemTime, Option<(usize, usize)>)> = inner
        .sessions
        .lock()
        .unwrap()
        .iter()
        .map(|(k, s)| {
            let st = s.state.lock().unwrap();
            (k.clone(), (st.last_used, st.run.as_ref().map(|r| (r.start, r.end))))
        })
        .collect();
    let mut total = 0u64;
    let mut cands = Vec::new();
    for d in rd.flatten() {
        let sid = d.file_name().to_string_lossy().to_string();
        let used = match open.get(&sid) {
            Some((u, _)) => *u,
            None => std::fs::metadata(d.path().join(".used")).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH),
        };
        let prot = open.get(&sid).and_then(|(_, p)| *p);
        let Ok(files) = std::fs::read_dir(d.path()) else { continue };
        for f in files.flatten() {
            let name = f.file_name().to_string_lossy().to_string();
            let Some(num) = name
                .strip_prefix("seg")
                .and_then(|r| r.strip_suffix(".m4s").or_else(|| r.strip_suffix(".ts")))
                .and_then(|n| n.parse::<usize>().ok())
            else {
                continue;
            };
            let Ok(m) = f.metadata() else { continue };
            total += m.len();
            if prot.map(|(a, b)| num >= a && num < b).unwrap_or(false) {
                continue;
            }
            cands.push(Cand { path: f.path(), size: m.len(), session_used: used, mtime: m.modified().unwrap_or(SystemTime::UNIX_EPOCH) });
        }
    }
    if total <= inner.cap_bytes {
        return;
    }
    cands.sort_by(|a, b| a.session_used.cmp(&b.session_used).then(a.mtime.cmp(&b.mtime)));
    for c in cands {
        if total <= inner.cap_bytes {
            break;
        }
        if std::fs::remove_file(&c.path).is_ok() {
            total = total.saturating_sub(c.size);
        }
    }
}
