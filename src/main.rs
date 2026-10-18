//! airstream — Airflow-style local media caster, rewritten in Rust.
//!
//! Behaviour modelled on a reverse-engineering pass of Airflow 3.3.7
//! (see ../airflow-re/notes/architecture.md for the evidence trail):
//!   * playlist prefs file with base64 bookmarkData + path per item
//!   * media probe -> stream inventory (video_0 / audio_0 / subtitle_0 ...)
//!   * topology decision: video passthrough when the source is already
//!     H.264 8-bit 4:2:0, otherwise an HLS re-encode
//!   * HTTP server on the LAN + mDNS/Bonjour registration, devices pull HLS
//!
//! Differences from the original: NVENC/AMF are preferred over software
//! x264, and casting uses DLNA (plus mDNS discovery of Cast/AirPlay targets).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod devices;
mod cast;
mod library;
mod probe;
mod server;
mod stream;
mod transcode;

use std::sync::Arc;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "airstream=info".into()),
        )
        .init();

    // Single instance: if Airstream is already running, just show its window.
    if let Some(port) = running_instance().await {
        open_window(&format!("http://127.0.0.1:{port}/"));
        return;
    }

    let state = Arc::new(server::AppState::new());
    server::start_device_discovery(state.clone());
    let port = state.port;
    let url = format!("http://127.0.0.1:{}/", port);

    let st = state.clone();
    tokio::spawn(async move {
        if let Err(e) = server::serve(st).await {
            tracing::error!("server stopped: {e}");
        }
    });

    // Open the window as soon as the listener answers (no fixed delay).
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    open_window(&url);

    // One app, one lifetime: the UI pings every few seconds; when the window
    // is closed the pings stop and the whole process (server included) exits.
    let started = server::now_secs();
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let last = server::LAST_PING.load(std::sync::atomic::Ordering::Relaxed);
        let now = server::now_secs();
        let idle = if last == 0 { now.saturating_sub(started) } else { now.saturating_sub(last) };
        let limit = if last == 0 { 40 } else { 12 };
        if idle > limit {
            tracing::info!("window closed — exiting");
            std::process::exit(0);
        }
    }
}

/// Port of an already-running Airstream, if any (probes /api/ping).
async fn running_instance() -> Option<u16> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for port in 42015u16..42025 {
        // A port we can bind is free: skip it instantly (a refused loopback
        // connect on Windows can stall for hundreds of ms).
        if std::net::TcpListener::bind(("0.0.0.0", port)).is_ok() {
            continue;
        }
        let Ok(Ok(mut s)) = tokio::time::timeout(
            std::time::Duration::from_millis(1500),
            tokio::net::TcpStream::connect(("127.0.0.1", port)),
        )
        .await
        else {
            continue;
        };
        let req = format!("GET /api/ping HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n\r\n");
        if s.write_all(req.as_bytes()).await.is_err() {
            continue;
        }
        let mut buf = String::new();
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), s.read_to_string(&mut buf)).await;
        if buf.contains("\"app\":\"airstream\"") {
            return Some(port);
        }
    }
    None
}

/// Opens the UI in a WebView2 app window (Edge `--app`), which gives a clean
/// browser-free window with its own taskbar entry and no npm/Tauri toolchain.
fn open_window(url: &str) {
    let candidates = [
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    ];
    let mut edge = None;
    for c in candidates {
        if std::path::Path::new(c).exists() {
            edge = Some(c.to_string());
            break;
        }
    }
    if let Some(edge) = edge {
        let _ = std::process::Command::new(edge)
            .args([
                format!("--app={url}"),
                "--window-size=1280,820".into(),
                "--user-data-dir".into(),
                dirs_profile().to_string_lossy().to_string(),
                "--no-first-run".into(),
                "--force-dark-mode".into(),
                "--enable-features=WebUIDarkMode".into(),
                "--disable-features=msEdgeSidebarV2".into(),
            ])
            .spawn();
    } else {
        // Fallback: default browser.
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn();
    }
}

fn dirs_profile() -> std::path::PathBuf {
    let p = library::data_dir().join("webview");
    let _ = std::fs::create_dir_all(&p);
    p
}
