//! Google Cast v2 client (the protocol Airflow uses via `cast_channel.pb`).
//!
//! Wire format: 4-byte big-endian length prefix + `CastMessage` protobuf:
//!   1 protocol_version (varint, 0 = CASTV2_1_0)
//!   2 source_id        (string)
//!   3 destination_id   (string)
//!   4 namespace        (string)
//!   5 payload_type     (varint, 0 = STRING)
//!   6 payload_utf8     (string)
//!
//! Flow: CONNECT to receiver-0 -> GET_STATUS -> LAUNCH Default Media Receiver
//! (CC1AD845) -> CONNECT to its transportId -> LOAD media -> then PLAY / PAUSE /
//! STOP / SEEK and periodic GET_STATUS for the position readout.

use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_native_tls::TlsConnector;

pub const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
pub const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
pub const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
pub const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";
pub const DMR_APP_ID: &str = "CC1AD845"; // Default Media Receiver

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct CastStatus {
    pub connected: bool,
    pub player_state: String,
    pub current_time: f64,
    pub duration: f64,
    pub title: String,
    pub transport_id: String,
    pub media_session_id: i64,
    pub last_error: String,
    /// last few raw channel messages, for diagnostics
    pub log: Vec<String>,
}

enum Cmd {
    Send(String, String, Value),
    Close,
}

pub struct CastClient {
    tx: mpsc::UnboundedSender<Cmd>,
    pub status: Arc<Mutex<CastStatus>>,
    pub name: String,
}

impl CastClient {
    pub fn status(&self) -> CastStatus {
        self.status.lock().unwrap().clone()
    }

    pub fn send(&self, namespace: &str, dest: &str, payload: Value) -> bool {
        self.tx.send(Cmd::Send(namespace.into(), dest.into(), payload)).is_ok()
    }

    pub fn play(&self) -> bool {
        let st = self.status();
        self.send(NS_MEDIA, &st.transport_id, json!({"type":"PLAY","mediaSessionId":st.media_session_id}))
    }
    pub fn pause(&self) -> bool {
        let st = self.status();
        self.send(NS_MEDIA, &st.transport_id, json!({"type":"PAUSE","mediaSessionId":st.media_session_id}))
    }
    pub fn stop(&self) -> bool {
        let st = self.status();
        self.send(NS_MEDIA, &st.transport_id, json!({"type":"STOP","mediaSessionId":st.media_session_id}))
    }
    pub fn seek(&self, seconds: f64) -> bool {
        let st = self.status();
        self.send(
            NS_MEDIA,
            &st.transport_id,
            json!({"type":"SEEK","mediaSessionId":st.media_session_id,"currentTime":seconds}),
        )
    }
    pub fn request_status(&self) {
        let st = self.status();
        self.send(NS_MEDIA, &st.transport_id, json!({"type":"GET_STATUS"}));
    }
}

// ------------------------------------------------------------------ protobuf

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

fn put_string(out: &mut Vec<u8>, field: u64, s: &str) {
    put_varint(out, (field << 3) | 2);
    put_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

fn encode_message(source: &str, dest: &str, namespace: &str, payload: &str) -> Vec<u8> {
    let mut body = Vec::with_capacity(payload.len() + 96);
    put_varint(&mut body, 1 << 3); // field 1, varint
    put_varint(&mut body, 0); // CASTV2_1_0
    put_string(&mut body, 2, source);
    put_string(&mut body, 3, dest);
    put_string(&mut body, 4, namespace);
    put_varint(&mut body, 5 << 3);
    put_varint(&mut body, 0); // STRING
    put_string(&mut body, 6, payload);
    let mut framed = Vec::with_capacity(body.len() + 4);
    framed.extend_from_slice(&(body.len() as u32).to_be_bytes());
    framed.extend_from_slice(&body);
    framed
}

fn read_varint(buf: &[u8], pos: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut shift = 0;
    while *pos < buf.len() {
        let b = buf[*pos];
        *pos += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 63 {
            break;
        }
    }
    v
}

fn decode_message(buf: &[u8]) -> (String, String, String) {
    let mut pos = 0usize;
    let (mut source, mut dest, mut ns, mut payload) = (String::new(), String::new(), String::new(), String::new());
    while pos < buf.len() {
        let key = read_varint(buf, &mut pos);
        let field = key >> 3;
        let wire = key & 7;
        match wire {
            0 => {
                let _ = read_varint(buf, &mut pos);
            }
            2 => {
                let len = read_varint(buf, &mut pos) as usize;
                if pos + len > buf.len() {
                    break;
                }
                let val = String::from_utf8_lossy(&buf[pos..pos + len]).to_string();
                pos += len;
                match field {
                    2 => source = val,
                    3 => dest = val,
                    4 => ns = val,
                    6 => payload = val,
                    _ => {}
                }
            }
            5 => pos += 4,
            1 => pos += 8,
            _ => break,
        }
    }
    (source, dest, format!("{}|{}", ns, payload))
}

// ------------------------------------------------------------------ session

pub async fn connect(host: &str, port: u16, device_name: &str) -> Result<CastClient, String> {
    let tcp = tokio::time::timeout(Duration::from_secs(6), TcpStream::connect((host, port)))
        .await
        .map_err(|_| "connect timeout".to_string())?
        .map_err(|e| format!("connect failed: {e}"))?;
    let _ = tcp.set_nodelay(true);

    let builder = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()
        .map_err(|e| e.to_string())?;
    let connector = TlsConnector::from(builder);
    let tls = tokio::time::timeout(Duration::from_secs(8), connector.connect(host, tcp))
        .await
        .map_err(|_| "tls timeout".to_string())?
        .map_err(|e| format!("tls failed: {e}"))?;

    let (mut rd, mut wr) = tokio::io::split(tls);
    let (tx, mut rx) = mpsc::unbounded_channel::<Cmd>();
    let status = Arc::new(Mutex::new(CastStatus { connected: true, ..Default::default() }));
    let req_id = Arc::new(AtomicU64::new(1));

    // writer task
    let status_w = status.clone();
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                Cmd::Send(ns, dest, mut payload) => {
                    if payload.get("requestId").is_none() {
                        if let Some(o) = payload.as_object_mut() {
                            o.insert("requestId".into(), json!(0));
                        }
                    }
                    let txt = payload.to_string();
                    let frame = encode_message("sender-0", &dest, &ns, &txt);
                    if wr.write_all(&frame).await.is_err() {
                        status_w.lock().unwrap().connected = false;
                        break;
                    }
                    let _ = wr.flush().await;
                }
                Cmd::Close => break,
            }
        }
    });

    // handshake
    let _ = tx.send(Cmd::Send(
        NS_CONNECTION.into(),
        "receiver-0".into(),
        json!({"type":"CONNECT"}),
    ));
    let _ = tx.send(Cmd::Send(
        NS_RECEIVER.into(),
        "receiver-0".into(),
        json!({"type":"GET_STATUS","requestId":req_id.fetch_add(1, Ordering::Relaxed)}),
    ));

    let client = CastClient { tx: tx.clone(), status: status.clone(), name: device_name.to_string() };

    // reader task: keeps status fresh and drives LAUNCH -> CONNECT -> LOAD
    let status_r = status.clone();
    let tx_reader = tx.clone();
    let req = req_id.clone();
    tokio::spawn(async move {
        let mut buf: Vec<u8> = Vec::new();
        let mut pending_launch = false;
        let mut launched = false;
        loop {
            let mut chunk = [0u8; 8192];
            let n = match tokio::time::timeout(Duration::from_secs(60), rd.read(&mut chunk)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => n,
                Ok(Err(_)) => break,
                Err(_) => {
                    // idle: ping to keep the channel alive
                    let _ = tx_reader.send(Cmd::Send(
                        NS_HEARTBEAT.into(),
                        "receiver-0".into(),
                        json!({"type":"PING"}),
                    ));
                    continue;
                }
            };
            buf.extend_from_slice(&chunk[..n]);
            loop {
                if buf.len() < 4 {
                    break;
                }
                let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                if buf.len() < 4 + len {
                    break;
                }
                let body = buf[4..4 + len].to_vec();
                buf.drain(..4 + len);
                let (_src, _dst, joined) = decode_message(&body);
                let (ns, payload) = match joined.split_once('|') {
                    Some(v) => v,
                    None => continue,
                };
                let Ok(v) = serde_json::from_str::<Value>(payload) else { continue };

                match ns {
                    NS_RECEIVER => {
                        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        if ty == "RECEIVER_STATUS" {
                            // already running an app?
                            if let Some(apps) = v.pointer("/status/applications").and_then(|a| a.as_array()) {
                                if let Some(app) = apps.iter().find(|a| {
                                    a.get("appId").and_then(|x| x.as_str()) == Some(DMR_APP_ID)
                                }) {
                                    let tid = app.get("transportId").and_then(|x| x.as_str()).unwrap_or("");
                                    if !tid.is_empty() {
                                        let mut s = status_r.lock().unwrap();
                                        s.transport_id = tid.to_string();
                                        drop(s);
                                        let _ = tx_reader.send(Cmd::Send(
                                            NS_CONNECTION.into(),
                                            tid.to_string(),
                                            json!({"type":"CONNECT"}),
                                        ));
                                        let _ = tx_reader.send(Cmd::Send(
                                            NS_MEDIA.into(),
                                            tid.to_string(),
                                            json!({"type":"GET_STATUS"}),
                                        ));
                                        launched = true;
                                    }
                                }
                            }
                            if !launched && !pending_launch {
                                pending_launch = true;
                                let _ = tx_reader.send(Cmd::Send(
                                    NS_RECEIVER.into(),
                                    "receiver-0".into(),
                                    json!({"type":"LAUNCH","appId":DMR_APP_ID,
                                           "requestId":req.fetch_add(1, Ordering::Relaxed)}),
                                ));
                            }
                        }
                        if ty == "LAUNCH_ERROR" {
                            status_r.lock().unwrap().last_error =
                                v.get("reason").and_then(|x| x.as_str()).unwrap_or("launch error").to_string();
                        }
                    }
                    NS_MEDIA => {
                        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        if ty == "LOAD_FAILED" || ty == "INVALID_REQUEST" || ty == "LOAD_CANCELLED" {
                            let mut s = status_r.lock().unwrap();
                            let detail = payload.chars().take(700).collect::<String>();
                            s.last_error = format!("{ty}: {detail}");
                        }
                        if let Some(arr) = v.get("status").and_then(|x| x.as_array()) {
                            if let Some(st) = arr.first() {
                                let mut s = status_r.lock().unwrap();
                                if let Some(t) = st.get("currentTime").and_then(|x| x.as_f64()) {
                                    s.current_time = t;
                                }
                                if let Some(d) = st.pointer("/media/duration").and_then(|x| x.as_f64()) {
                                    s.duration = d;
                                }
                                if let Some(p) = st.get("playerState").and_then(|x| x.as_str()) {
                                    s.player_state = p.to_string();
                                }
                                if let Some(mid) = st.get("mediaSessionId").and_then(|x| x.as_i64()) {
                                    s.media_session_id = mid;
                                }
                                if let Some(tid) = st.get("transportId").and_then(|x| x.as_str()) {
                                    s.transport_id = tid.to_string();
                                }
                                if let Some(t) = st.pointer("/media/metadata/title").and_then(|x| x.as_str()) {
                                    s.title = t.to_string();
                                }
                            }
                        }
                    }
                    _ => {}
                }

                // keep a small tail of raw messages for /api/cast_debug
                {
                    let mut s = status_r.lock().unwrap();
                    let line = format!("{} {}", ns, payload.chars().take(220).collect::<String>());
                    s.log.push(line);
                    if s.log.len() > 40 {
                        s.log.remove(0);
                    }
                }
            }
        }
        status_r.lock().unwrap().connected = false;
    });

    // give the handshake a moment to resolve the transport id
    for _ in 0..32 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if !client.status().transport_id.is_empty() {
            break;
        }
    }
    Ok(client)
}

/// Loads a media URL into the (already launched) Default Media Receiver.
pub fn load(client: &CastClient, url: &str, title: &str, hls: bool) -> bool {
    let st = client.status();
    if st.transport_id.is_empty() {
        return false;
    }
    let content_type = if hls { "application/x-mpegurl" } else { "video/mp4" };
    client.send(
        NS_MEDIA,
        &st.transport_id,
        json!({
            "type": "LOAD",
            "autoplay": true,
            "currentTime": 0,
            "media": {
                "contentId": url,
                "streamType": if hls { "LIVE" } else { "BUFFERED" },
                "contentType": content_type,
                "metadata": { "type": 0, "title": title }
            }
        }),
    )
}
