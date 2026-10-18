//! Device discovery and casting.
//!
//! Airflow ships four senders: Google Cast (cast_channel.pb), AirPlay 2
//! (/pair-setup, /pair-verify, /play), AirConnect and mDNS/Bonjour browsing.
//! This implementation uses mDNS for discovery of Cast/AirPlay targets and
//! SSDP + DLNA AVTransport for the actual push, which every modern TV accepts.

use serde::{Serialize, Deserialize};
use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub host: String,
    pub port: u16,
    pub model: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub control_url: String,
}

pub async fn discover() -> Vec<Device> {
    let mdns = tokio::task::spawn_blocking(discover_mdns);
    let ssdp = tokio::task::spawn_blocking(discover_ssdp);
    let direct = tokio::spawn(discover_cast_direct());
    let mut out = Vec::new();
    if let Ok(mut a) = mdns.await {
        out.append(&mut a);
    }
    if let Ok(mut b) = ssdp.await {
        out.append(&mut b);
    }
    if let Ok(mut found) = direct.await { out.append(&mut found); }
    // de-duplicate by name
    let mut seen = std::collections::HashSet::new();
    out.retain(|d| seen.insert(d.name.clone()));
    out
}

/// Bounded unicast fallback on the active private LAN /24. Multicast may
/// be filtered even when the TV's Cast service is reachable. Only accept a
/// host after both port 8009 and the Cast identity endpoint are verified.
async fn discover_cast_direct() -> Vec<Device> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Ok(route) = UdpSocket::bind("0.0.0.0:0") else { return Vec::new() };
    if route.connect("8.8.8.8:80").is_err() { return Vec::new() }
    let Ok(addr) = route.local_addr() else { return Vec::new() };
    let std::net::IpAddr::V4(local) = addr.ip() else { return Vec::new() };
    if !local.is_private() { return Vec::new() }
    let oct = local.octets();
    let mut jobs = tokio::task::JoinSet::new();
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(32));
    for last in 1..=254u8 {
        if last == oct[3] { continue }
        let permits = permits.clone();
        let host = format!("{}.{}.{}.{}", oct[0], oct[1], oct[2], last);
        jobs.spawn(async move {
            let _permit = permits.acquire().await.ok()?;
            let identify = async {
                let cast = tokio::net::TcpStream::connect((host.as_str(), 8009)).await.ok()?;
                drop(cast);
                let mut stream = tokio::net::TcpStream::connect((host.as_str(), 8008)).await.ok()?;
                let req = format!("GET /setup/eureka_info?params=name,device_info HTTP/1.1\r\nHost: {host}:8008\r\nConnection: close\r\n\r\n");
                stream.write_all(req.as_bytes()).await.ok()?;
                let mut response = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = stream.read(&mut chunk).await.ok()?;
                    if n == 0 { break }
                    response.extend_from_slice(&chunk[..n]);
                    if response.len() > 65536 { return None }
                    if let Some(end) = response.windows(4).position(|w| w == [13, 10, 13, 10]) {
                        let head = std::str::from_utf8(&response[..end]).ok()?;
                        let length = head.lines().filter_map(|line| line.split_once(':'))
                            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, value)| value.trim().parse::<usize>().ok());
                        if let Some(length) = length {
                            if response.len() >= end + 4 + length { break }
                        }
                    }
                }
                let text = String::from_utf8(response).ok()?;
                let (head, body) = text.split_once("\r\n\r\n")?;
                if !head.lines().next()?.contains(" 200 ") { return None }
                let info: serde_json::Value = serde_json::from_str(body).ok()?;
                let name = info.get("name")?.as_str()?.to_string();
                let uuid = info.get("ssdp_udn")?.as_str()?.replace('-', "");
                Some(Device {
                    id: format!("cast:{name}-{uuid}._googlecast._tcp.local."),
                    name, kind: "cast".into(), host: host.clone(), port: 8009,
                    model: info.pointer("/device_info/model_name").and_then(|x| x.as_str()).unwrap_or("Google Cast").into(),
                    location: String::new(), control_url: String::new(),
                })
            };
            tokio::time::timeout(Duration::from_millis(700), identify).await.ok().flatten()
        });
    }
    let mut out = Vec::new();
    while let Some(result) = jobs.join_next().await {
        if let Ok(Some(device)) = result { out.push(device); }
    }
    out
}

fn discover_mdns() -> Vec<Device> {
    use mdns_sd::{ServiceDaemon, ServiceEvent};
    let mut out = Vec::new();
    let Ok(daemon) = ServiceDaemon::new() else { return out };

    for (svc, kind) in [("_googlecast._tcp.local.", "cast"), ("_airplay._tcp.local.", "airplay")] {
        let Ok(rx) = daemon.browse(svc) else { continue };
        let deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(300)) {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    let host = info
                        .get_addresses()
                        .iter()
                        .find(|a| a.is_ipv4())
                        .map(|a| a.to_string())
                        .unwrap_or_default();
                    if host.is_empty() {
                        continue;
                    }
                    let name = info
                        .get_property_val_str("fn")
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| info.get_fullname().split('.').next().unwrap_or("device").to_string());
                    out.push(Device {
                        id: format!("{kind}:{}", info.get_fullname()),
                        name,
                        kind: kind.to_string(),
                        host,
                        port: info.get_port(),
                        model: info
                            .get_property_val_str("md")
                            .unwrap_or("")
                            .to_string(),
                        location: String::new(),
                        control_url: String::new(),
                    });
                }
                _ => {}
            }
        }
        let _ = daemon.stop_browse(svc);
    }
    let _ = daemon.shutdown();
    out
}

fn discover_ssdp() -> Vec<Device> {
    let mut out = Vec::new();
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return out };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(400)));
    let msg = "M-SEARCH * HTTP/1.1\r\n\
HOST: 239.255.255.250:1900\r\n\
MAN: \"ssdp:discover\"\r\n\
MX: 2\r\n\
ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
    for _ in 0..3 {
        let _ = sock.send_to(msg.as_bytes(), "239.255.255.250:1900");
    }
    let deadline = Instant::now() + Duration::from_millis(2000);
    let mut locations: Vec<String> = Vec::new();
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        match sock.recv_from(&mut buf) {
            Ok((n, _)) => {
                let txt = String::from_utf8_lossy(&buf[..n]).to_string();
                if let Some(loc) = header_value(&txt, "location") {
                    if !locations.contains(&loc) {
                        locations.push(loc);
                    }
                }
            }
            Err(_) => break,
        }
    }
    for loc in locations {
        if let Some(dev) = describe_dlna(&loc) {
            out.push(dev);
        }
    }
    out
}

fn header_value(txt: &str, key: &str) -> Option<String> {
    for line in txt.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case(key) {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn describe_dlna(location: &str) -> Option<Device> {
    let (host, port, path) = split_http(location)?;
    let body = http_get(&host, port, &path)?;

    let name = tag_text(&body, "friendlyName").unwrap_or_else(|| host.clone());
    let model = tag_text(&body, "modelName").unwrap_or_default();
    // Find the AVTransport control URL.
    let mut control = String::new();
    let mut rest = body.as_str();
    while let Some(pos) = rest.find("<service>") {
        let seg = &rest[pos..];
        let end = seg.find("</service>").unwrap_or(seg.len());
        let block = &seg[..end];
        if block.contains("AVTransport") {
            if let Some(c) = tag_text(block, "controlURL") {
                control = c;
                break;
            }
        }
        rest = &seg[end..];
    }
    let control_url = absolutize(location, &control);

    Some(Device {
        id: format!("dlna:{location}"),
        name,
        kind: "dlna".into(),
        host,
        port,
        model,
        location: location.to_string(),
        control_url,
    })
}

fn split_http(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (authority.to_string(), 80),
    };
    Some((host, port, path.to_string()))
}

fn absolutize(base: &str, path: &str) -> String {
    if path.starts_with("http://") {
        return path.to_string();
    }
    let (host, port, _) = match split_http(base) {
        Some(v) => v,
        None => return path.to_string(),
    };
    if path.starts_with('/') {
        format!("http://{host}:{port}{path}")
    } else {
        format!("http://{host}:{port}/{path}")
    }
}

fn tag_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    let v = xml[start..end].trim();
    if v.is_empty() {
        None
    } else {
        Some(unescape(&strip_ns(v)))
    }
}

fn strip_ns(tag: &str) -> String {
    tag.split(':').last().unwrap_or(tag).to_string()
}

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"")
}

fn http_get(host: &str, port: u16, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect((host, port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(2500))).ok()?;
    let req = format!("GET {path} HTTP/1.0\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    let txt = String::from_utf8_lossy(&buf).to_string();
    let body = match txt.find("\r\n\r\n") {
        Some(i) => txt[i + 4..].to_string(),
        None => txt,
    };
    Some(body)
}

fn http_post(host: &str, port: u16, path: &str, soap_action: &str, body: &str) -> Option<String> {
    let mut stream = TcpStream::connect((host, port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(4000))).ok()?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nSOAPACTION: \"{soap_action}\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).to_string())
}

fn didl(media_url: &str, title: &str) -> String {
    let title = title.replace('&', "&amp;").replace('<', "&lt;");
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
<item id=\"0\" parentID=\"-1\" restricted=\"1\"><dc:title>{title}</dc:title>\
<upnp:class>object.item.videoItem</upnp:class>\
<res protocolInfo=\"http-get:*:video/mp4:*\">{media_url}</res></item></DIDL-Lite>"
    )
}

/// Pushes a URL to a DLNA renderer: SetAVTransportURI then Play.
pub fn cast_dlna(device: &Device, media_url: &str, title: &str) -> Result<String, String> {
    if device.control_url.is_empty() {
        return Err("device has no AVTransport control URL".into());
    }
    let (host, port, path) = split_http(&device.control_url).ok_or("bad control url")?;
    let meta = didl(media_url, title).replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
<u:SetAVTransportURI xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">\
<InstanceID>0</InstanceID><CurrentURI>{media_url}</CurrentURI>\
<CurrentURIMetaData>{meta}</CurrentURIMetaData></u:SetAVTransportURI></s:Body></s:Envelope>"
    );
    let r1 = http_post(&host, port, &path, "urn:schemas-upnp-org:service:AVTransport:1#SetAVTransportURI", &body)
        .ok_or("SetAVTransportURI failed (no response)")?;

    let play_body = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
<u:Play xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">\
<InstanceID>0</InstanceID><Speed>1</Speed></u:Play></s:Body></s:Envelope>";
    let r2 = http_post(&host, port, &path, "urn:schemas-upnp-org:service:AVTransport:1#Play", play_body)
        .ok_or("Play failed (no response)")?;

    if r1.contains(" 200 ") && r2.contains(" 200 ") {
        Ok(format!("casting to {}", device.name))
    } else {
        Err(format!(
            "renderer rejected: {} / {}",
            first_line(&r1),
            first_line(&r2)
        ))
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

// ------------------------------------------------------------------ control

const AVT: &str = "urn:schemas-upnp-org:service:AVTransport:1";

fn envelope(action: &str, inner: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
<u:{action} xmlns:u=\"{AVT}\">{inner}</u:{action}></s:Body></s:Envelope>"
    )
}

fn avt_call(device: &Device, action: &str, inner: &str) -> Option<String> {
    let (host, port, path) = split_http(&device.control_url)?;
    let body = envelope(action, inner);
    http_post(&host, port, &path, &format!("{AVT}#{action}"), &body)
}

/// action: play | pause | stop
pub fn transport(device: &Device, action: &str) -> Result<(), String> {
    let (act, inner) = match action {
        "play" => ("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>".to_string()),
        "pause" => ("Pause", "<InstanceID>0</InstanceID>".to_string()),
        "stop" => ("Stop", "<InstanceID>0</InstanceID>".to_string()),
        other => return Err(format!("unknown action {other}")),
    };
    let r = avt_call(device, act, &inner).ok_or("no response from renderer")?;
    if r.contains(" 200 ") {
        Ok(())
    } else {
        Err(first_line(&r))
    }
}

/// Seeks relative-time position (seconds) on the renderer.
pub fn seek(device: &Device, seconds: f64) -> Result<(), String> {
    let s = seconds.max(0.0);
    let target = format!(
        "{:02}:{:02}:{:02}",
        (s / 3600.0) as u32,
        ((s % 3600.0) / 60.0) as u32,
        (s % 60.0) as u32
    );
    let inner = format!(
        "<InstanceID>0</InstanceID><Unit>REL_TIME</Unit><Target>{target}</Target>"
    );
    let r = avt_call(device, "Seek", &inner).ok_or("no response from renderer")?;
    if r.contains(" 200 ") {
        Ok(())
    } else {
        Err(first_line(&r))
    }
}

pub struct Position {
    pub elapsed: f64,
    pub duration: f64,
    pub state: String,
}

pub fn position(device: &Device) -> Position {
    let pos = avt_call(device, "GetPositionInfo", "<InstanceID>0</InstanceID>").unwrap_or_default();
    let elapsed = parse_hms(&tag_text(&pos, "RelTime").unwrap_or_default());
    let duration = parse_hms(&tag_text(&pos, "TrackDuration").unwrap_or_default());
    let info = avt_call(device, "GetTransportInfo", "<InstanceID>0</InstanceID>").unwrap_or_default();
    let state = tag_text(&info, "CurrentTransportState").unwrap_or_else(|| "UNKNOWN".into());
    Position { elapsed, duration, state }
}

fn parse_hms(s: &str) -> f64 {
    let parts: Vec<f64> = s.split(':').filter_map(|p| p.trim().parse().ok()).collect();
    match parts.len() {
        3 => parts[0] * 3600.0 + parts[1] * 60.0 + parts[2],
        2 => parts[0] * 60.0 + parts[1],
        1 => parts[0],
        _ => 0.0,
    }
}
