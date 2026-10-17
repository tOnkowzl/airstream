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

    let state = Arc::new(server::AppState::new());
    let port = state.port;
    let url = format!("http://127.0.0.1:{}/", port);

    // Launch the HTTP server first, then the app window.
    let st = state.clone();
    tokio::spawn(async move {
        if let Err(e) = server::serve(st).await {
            tracing::error!("server stopped: {e}");
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    open_window(&url);

    // Keep the process alive: the axum task runs forever, but make the main
    // task wait too so the window owner does not exit early.
    std::future::pending::<()>().await;
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
