//! Standalone test server for the on-demand HLS segmenter.
//!
//! usage: hls_serve <file> [--port 42095] [--aac] [--ts] [--cache DIR] [--cap-gib 8] [--audio N]
//! Routes: /master.m3u8 /index.m3u8 /init.mp4 /seg/:n (.m4s | .ts) /_info /_quit /_player.html
#![allow(dead_code)]

#[path = "../hls_vod.rs"]
mod hls_vod;
#[path = "../mkv_index.rs"]
mod mkv_index;
#[path = "../process.rs"]
mod process;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path as UrlPath, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use hls_vod::{AudioMode, HlsManager, SegFormat, VodOptions};

#[derive(Clone)]
struct App {
    mgr: HlsManager,
    sid: Arc<String>,
    ts: bool,
}

fn reply(code: StatusCode, ctype: &'static str, body: impl Into<axum::body::Body>) -> Response {
    (
        code,
        [(header::CONTENT_TYPE, ctype), (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"), (header::CACHE_CONTROL, "no-store")],
        body.into(),
    )
        .into_response()
}

fn log(what: &str, t0: Instant, status: u16, bytes: usize) {
    eprintln!("{:>8.1} ms  {status} {what} {bytes} B", t0.elapsed().as_secs_f64() * 1000.0);
}

async fn master(State(a): State<App>) -> Response {
    let t0 = Instant::now();
    let r = match a.mgr.master(&a.sid) {
        Some(m) => {
            let n = m.len();
            let r = reply(StatusCode::OK, "application/vnd.apple.mpegurl", m);
            log("/master.m3u8", t0, 200, n);
            return r;
        }
        None => reply(StatusCode::NOT_FOUND, "text/plain", "no session"),
    };
    r
}

async fn index(State(a): State<App>) -> Response {
    let t0 = Instant::now();
    match a.mgr.playlist(&a.sid) {
        Some(m) => {
            let n = m.len();
            let r = reply(StatusCode::OK, "application/vnd.apple.mpegurl", m);
            log("/index.m3u8", t0, 200, n);
            r
        }
        None => reply(StatusCode::NOT_FOUND, "text/plain", "no session"),
    }
}

async fn init(State(a): State<App>) -> Response {
    let t0 = Instant::now();
    match a.mgr.init(&a.sid).await {
        Ok(p) => match tokio::fs::read(&p).await {
            Ok(b) => {
                let n = b.len();
                let r = reply(StatusCode::OK, "video/mp4", b);
                log("/init.mp4", t0, 200, n);
                r
            }
            Err(e) => reply(StatusCode::INTERNAL_SERVER_ERROR, "text/plain", e.to_string()),
        },
        Err(e) => reply(StatusCode::NOT_FOUND, "text/plain", e),
    }
}

async fn seg(State(a): State<App>, UrlPath(name): UrlPath<String>) -> Response {
    let t0 = Instant::now();
    let stem = name.split('.').next().unwrap_or("");
    let Ok(n) = stem.strip_prefix("seg").unwrap_or(stem).parse::<usize>() else {
        return reply(StatusCode::NOT_FOUND, "text/plain", "bad segment");
    };
    match a.mgr.segment(&a.sid, n).await {
        Ok(p) => match tokio::fs::read(&p).await {
            Ok(b) => {
                let len = b.len();
                let r = reply(StatusCode::OK, if a.ts { "video/mp2t" } else { "video/iso.segment" }, b);
                log(&format!("/seg/{n}"), t0, 200, len);
                r
            }
            Err(e) => reply(StatusCode::INTERNAL_SERVER_ERROR, "text/plain", e.to_string()),
        },
        Err(e) => {
            log(&format!("/seg/{n} ERR {e}"), t0, 500, 0);
            reply(StatusCode::INTERNAL_SERVER_ERROR, "text/plain", e)
        }
    }
}

async fn info(State(a): State<App>) -> Response {
    match a.mgr.debug(&a.sid) {
        Some(j) => reply(StatusCode::OK, "application/json", j.to_string()),
        None => reply(StatusCode::NOT_FOUND, "text/plain", "no session"),
    }
}

async fn quit(State(a): State<App>) -> Response {
    a.mgr.stop_all();
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        std::process::exit(0);
    });
    reply(StatusCode::OK, "text/plain", "bye")
}

async fn player() -> Response {
    let p = std::env::var("HLS_PLAYER_HTML").unwrap_or_default();
    match tokio::fs::read(&p).await {
        Ok(b) => reply(StatusCode::OK, "text/html", b),
        Err(_) => reply(StatusCode::NOT_FOUND, "text/plain", "set HLS_PLAYER_HTML"),
    }
}

async fn options() -> Response {
    (
        StatusCode::NO_CONTENT,
        [(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"), (header::ACCESS_CONTROL_ALLOW_HEADERS, "*")],
    )
        .into_response()
}

#[tokio::main]
async fn main() {
    let mut file: Option<PathBuf> = None;
    let mut port: u16 = 42095;
    let (mut aac, mut ts) = (false, false);
    let mut cache = std::env::temp_dir().join("hls_serve_cache");
    let mut cap_gib: f64 = 8.0;
    let mut audio: Option<usize> = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => port = it.next().and_then(|v| v.parse().ok()).expect("--port N"),
            "--aac" => aac = true,
            "--ts" => ts = true,
            "--cache" => cache = PathBuf::from(it.next().expect("--cache DIR")),
            "--cap-gib" => cap_gib = it.next().and_then(|v| v.parse().ok()).expect("--cap-gib N"),
            "--audio" => audio = it.next().and_then(|v| v.parse().ok()),
            _ => file = Some(PathBuf::from(a)),
        }
    }
    let Some(file) = file else {
        eprintln!("usage: hls_serve <file> [--port 42095] [--aac] [--ts] [--cache DIR] [--cap-gib 8] [--audio N]");
        std::process::exit(2);
    };
    let t0 = Instant::now();
    let basics = mkv_index::probe_basic(&file).unwrap_or_else(|e| {
        eprintln!("probe failed: {e}");
        std::process::exit(1)
    });
    let a = match audio {
        Some(i) => basics.audio.iter().find(|a| a.index == i),
        None => basics.audio.first(),
    }
    .unwrap_or_else(|| {
        eprintln!("no such audio stream");
        std::process::exit(1)
    });
    let mode = if !aac && a.codec == "aac" { AudioMode::Copy } else { AudioMode::Aac { channels: a.channels.min(6) } };
    let opts = VodOptions { audio_index: a.index, audio: mode, format: if ts { SegFormat::Ts } else { SegFormat::Fmp4 } };
    let mgr = HlsManager::new(cache, (cap_gib * 1024.0 * 1024.0 * 1024.0) as u64);
    let sid = mgr.open(&file, opts).await.unwrap_or_else(|e| {
        eprintln!("open failed: {e}");
        std::process::exit(1)
    });
    let t_idx = Instant::now();
    let kf = mkv_index::keyframes(&file, &std::env::temp_dir().join("hls_serve_kfprobe_nocache")).map(|v| v.len());
    eprintln!(
        "session {sid} opened in {:.3}s ({:?} {:?}); uncached keyframe index ({:?} kf) in {:.3}s",
        t0.elapsed().as_secs_f64(),
        mode,
        opts.format,
        kf,
        t_idx.elapsed().as_secs_f64()
    );
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join("hls_serve_kfprobe_nocache"));
    let st = App { mgr, sid: Arc::new(sid), ts };
    let app = Router::new()
        .route("/master.m3u8", get(master).options(options))
        .route("/index.m3u8", get(index).options(options))
        .route("/init.mp4", get(init).options(options))
        .route("/seg/:n", get(seg).options(options))
        .route("/:file", get(seg).options(options))
        .route("/_info", get(info))
        .route("/_quit", get(quit))
        .route("/_player.html", get(player))
        .with_state(st);
    let l = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.expect("bind");
    eprintln!("listening on 127.0.0.1:{port}");
    axum::serve(l, app).await.unwrap();
}
