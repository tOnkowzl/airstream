//! Progressive streaming for the in-app player.
//!
//! Chromium (WebView2/Edge) cannot play MKV/AC3/DTS and has no native HLS, so
//! the player consumes a fragmented-MP4 byte stream produced by ffmpeg: video
//! is copied when it is already H.264 8-bit, otherwise re-encoded on the GPU
//! (NVENC/AMF/QSV) or on the CPU as a last resort — the same topology rule
//! Airflow uses, without its QuickSync-only limitation.

use std::collections::HashMap;
use std::sync::Mutex;
use tokio::process::{Child, ChildStdout, Command};
use tokio_util::io::ReaderStream;

use crate::transcode::{Mode, Transcoder};

pub struct PipeRegistry {
    inner: Mutex<HashMap<String, Child>>,
}

impl PipeRegistry {
    pub fn new() -> PipeRegistry {
        PipeRegistry { inner: Mutex::new(HashMap::new()) }
    }

    /// Stops any previous stream for this item (seeking restarts the pipeline).
    pub fn stop(&self, item_id: &str) {
        if let Some(mut c) = self.inner.lock().unwrap().remove(item_id) {
            let _ = c.start_kill();
        }
    }

    pub fn stop_all(&self) {
        let mut map = self.inner.lock().unwrap();
        for (_, mut c) in map.drain() {
            let _ = c.start_kill();
        }
    }

    /// Spawns ffmpeg and returns a stream of fragmented MP4 bytes.
    pub fn start(
        &self,
        tc: &Transcoder,
        item_id: &str,
        path: &str,
        passthrough_ok: bool,
        seek: f64,
    ) -> Result<(Mode, ReaderStream<ChildStdout>), String> {
        self.stop(item_id);
        let mode = tc.pick_mode(crate::transcode::Quality::V1080, passthrough_ok, "auto");

        let mut cmd = Command::new("ffmpeg");
        cmd.arg("-hide_banner").args(["-loglevel", "error", "-nostdin"]);
        if seek > 0.5 {
            cmd.args(["-ss", &format!("{seek:.3}")]);
        }
        cmd.arg("-i").arg(path).args(["-map", "0:v:0", "-map", "0:a:0?"]);

        match mode {
            Mode::Passthrough => {
                cmd.args(["-c:v", "copy"]);
            }
            Mode::Nvenc => {
                cmd.args([
                    "-c:v", "h264_nvenc", "-preset", "p4", "-tune", "hq", "-rc", "vbr", "-cq",
                    "23", "-b:v", "0", "-maxrate", "12M", "-bufsize", "24M", "-profile:v", "high",
                    "-pix_fmt", "yuv420p", "-g", "48", "-vf", "scale='min(1920,iw)':-2",
                ]);
            }
            Mode::Amf => {
                cmd.args([
                    "-c:v", "h264_amf", "-quality", "balanced", "-rc", "cqp", "-qp_i", "22",
                    "-qp_p", "24", "-pix_fmt", "yuv420p", "-g", "48",
                ]);
            }
            Mode::Qsv => {
                cmd.args([
                    "-c:v", "h264_qsv", "-global_quality", "22", "-preset", "medium",
                    "-pix_fmt", "nv12", "-g", "48",
                ]);
            }
            Mode::X264 => {
                cmd.args([
                    "-c:v", "libx264", "-preset", "veryfast", "-crf", "22", "-maxrate", "12M",
                    "-bufsize", "24M", "-pix_fmt", "yuv420p", "-g", "48",
                ]);
            }
        }

        cmd.args(["-c:a", "aac", "-b:a", "192k", "-ac", "2"])
            .args(["-movflags", "frag_keyframe+empty_moov+default_base_moof"])
            .args(["-f", "mp4", "pipe:1"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| format!("ffmpeg spawn failed: {e}"))?;
        let stdout = child.stdout.take().ok_or("ffmpeg produced no stdout")?;
        self.inner.lock().unwrap().insert(item_id.to_string(), child);
        Ok((mode, ReaderStream::new(stdout)))
    }
}
