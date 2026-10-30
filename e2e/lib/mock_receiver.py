"""Mock Google Cast receiver (CASTV2 over TLS) + mock eureka_info endpoint.

Only what Airstream needs: heartbeat, receiver GET_STATUS/LAUNCH, media
LOAD/GET_STATUS/PLAY/PAUSE/SEEK/STOP.  Test hooks simulate the real-world
failures: player closed silently by the TV, half-open sockets, TV powered off.
"""
import http.server, json, os, re, shutil, socket, ssl, struct, subprocess, threading, time, urllib.parse, urllib.request

NOWIN = 0x08000000
NS_CONN = "urn:x-cast:com.google.cast.tp.connection"
NS_HB = "urn:x-cast:com.google.cast.tp.heartbeat"
NS_RECV = "urn:x-cast:com.google.cast.receiver"
NS_MEDIA = "urn:x-cast:com.google.cast.media"
DMR = "CC1AD845"
TRANSPORT = "web-5"

HERE = os.path.dirname(os.path.abspath(__file__))
WORK = os.path.join(os.path.dirname(HERE), ".work")


# ------------------------------------------------------------------ certificate
def ensure_cert():
    os.makedirs(WORK, exist_ok=True)
    crt, key = os.path.join(WORK, "mock_cert.pem"), os.path.join(WORK, "mock_key.pem")
    if os.path.exists(crt) and os.path.exists(key):
        return crt, key
    exe = shutil.which("openssl")
    if not exe:
        for c in (r"C:\Program Files\Git\usr\bin\openssl.exe", r"C:\Program Files\Git\mingw64\bin\openssl.exe"):
            if os.path.exists(c):
                exe = c
                break
    if not exe:
        raise RuntimeError("openssl not found (needed to make the mock TLS certificate)")
    subprocess.run([exe, "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", key, "-out", crt,
                    "-days", "3650", "-subj", "/CN=airstream-mock"],
                   check=True, capture_output=True, creationflags=NOWIN)
    return crt, key


def _server_ctx():
    crt, key = ensure_cert()
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(crt, key)
    return ctx


# ------------------------------------------------------------------ protobuf
def _varint(v):
    out = bytearray()
    while True:
        b = v & 0x7F
        v >>= 7
        if v:
            out.append(b | 0x80)
        else:
            out.append(b)
            return bytes(out)


def _sfield(f, s):
    b = s.encode()
    return _varint((f << 3) | 2) + _varint(len(b)) + b


def encode(src, dst, ns, payload):
    body = _varint(1 << 3) + _varint(0) + _sfield(2, src) + _sfield(3, dst) + _sfield(4, ns) \
        + _varint(5 << 3) + _varint(0) + _sfield(6, payload)
    return struct.pack(">I", len(body)) + body


def _rv(buf, pos):
    v = shift = 0
    while pos < len(buf):
        b = buf[pos]; pos += 1
        v |= (b & 0x7F) << shift
        if not b & 0x80:
            break
        shift += 7
    return v, pos


def decode(buf):
    pos, f = 0, {}
    while pos < len(buf):
        key, pos = _rv(buf, pos)
        field, wire = key >> 3, key & 7
        if wire == 0:
            _, pos = _rv(buf, pos)
        elif wire == 2:
            n, pos = _rv(buf, pos)
            f[field] = buf[pos:pos + n].decode("utf-8", "replace")
            pos += n
        elif wire == 5:
            pos += 4
        elif wire == 1:
            pos += 8
        else:
            break
    return f.get(2, ""), f.get(3, ""), f.get(4, ""), f.get(6, "")


# ------------------------------------------------------------------ receiver
class _Conn:
    def __init__(self, sock):
        self.sock = sock
        self.wlock = threading.Lock()  # SSLSocket corrupts with concurrent writers
        self.muted = False             # half-open: swallow everything, answer nothing
        self.alive = True

    def send(self, src, dst, ns, obj):
        if self.muted or not self.alive:
            return
        data = encode(src, dst, ns, json.dumps(obj))
        try:
            with self.wlock:
                self.sock.sendall(data)
        except Exception:
            self.alive = False


class MockReceiver:
    def __init__(self, launch_delay=0.0, load_delay=0.0, duration=60.0, stop_closes_player=True, hls_mode=None, fetch_dir=None):
        self.launch_delay = launch_delay
        self.load_delay = load_delay
        self.duration = duration
        self.stop_closes_player = stop_closes_player
        # hls_mode: None = HLS LOAD treated like any URL (old behaviour);
        # 'play'  = fetch master/index/init/segment-at-currentTime, then PLAYING;
        # 'reject' = LOAD_FAILED for m3u8 content, MP4 keeps working.
        self.hls_mode = hls_mode
        self.fetch_dir = fetch_dir      # fetched init/segment bytes are saved here (for ffprobe)
        self.load_msgs = []             # full media object of every accepted/rejected LOAD
        self.hls_loads = 0              # LOADs with an m3u8 content type (accepted or not)
        self.fetched = []               # (url path, status, bytes) of every HTTP GET done for HLS
        self.fetched_files = {}         # 'init' / 'seg<N>' -> saved file path
        self.launches = 0
        self.loads = 0
        self.connections = 0
        self.load_urls = []
        self.load_times = []         # currentTime of each LOAD
        self.player_open = False
        self.session = 0
        self.state = "IDLE"          # media player state
        self.idle_reason = ""
        self._t0 = 0.0               # position bookkeeping
        self._pos = 0.0
        self.powered = True
        self._conns = []
        self._lock = threading.Lock()
        self._stop = False
        self._lsock = socket.socket()
        self._lsock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self._lsock.bind(("127.0.0.1", 0))
        self._lsock.listen(16)
        self.port = self._lsock.getsockname()[1]
        self._ctx = _server_ctx()
        threading.Thread(target=self._accept_loop, daemon=True).start()

    # ---- test hooks
    def close_player_silently(self):
        """TV screensaver/input switch: the player app disappears, nothing is announced."""
        with self._lock:
            self.player_open = False
            self.session = 0
            self.state = "IDLE"

    def drop_connections(self):
        """Hard-close every client socket (link lost); reconnecting works afterwards."""
        with self._lock:
            conns = list(self._conns)
        for c in conns:
            c.alive = False
            try: c.sock.close()
            except Exception: pass

    def set_position(self, pos):
        with self._lock:
            self._pos, self._t0 = float(pos), time.time()

    def half_open_existing(self):
        """Existing sockets stay open but never answer again (TV unplugged without FIN)."""
        with self._lock:
            for c in self._conns:
                c.muted = True

    def power_off(self):
        self.powered = False
        self.close_player_silently()
        self.half_open_existing()

    def power_on(self):
        self.powered = True

    def close(self):
        self._stop = True
        try: self._lsock.close()
        except Exception: pass
        with self._lock:
            for c in self._conns:
                c.alive = False
                try: c.sock.close()
                except Exception: pass

    def position(self):
        return self._pos + ((time.time() - self._t0) if self.state == "PLAYING" else 0.0)

    # ---- internals
    def _accept_loop(self):
        while not self._stop:
            try:
                raw, _ = self._lsock.accept()
            except Exception:
                return
            if not self.powered:
                try: raw.close()
                except Exception: pass
                continue
            threading.Thread(target=self._serve, args=(raw,), daemon=True).start()

    def _broadcast(self, ns, obj):
        with self._lock:
            conns = list(self._conns)
        for c in conns:
            c.send(TRANSPORT if ns == NS_MEDIA else "receiver-0", "*", ns, obj)

    def _recv_status(self, rid=0):
        apps = []
        if self.player_open:
            apps.append({"appId": DMR, "displayName": "Default Media Receiver", "transportId": TRANSPORT,
                         "sessionId": "sess-1", "namespaces": [{"name": NS_MEDIA}]})
        return {"type": "RECEIVER_STATUS", "requestId": rid,
                "status": {"applications": apps, "volume": {"level": 1, "muted": False}}}

    def _media_status(self, rid=0):
        st = []
        if self.player_open and self.session:
            st.append({"mediaSessionId": self.session, "playerState": self.state, "currentTime": self.position(),
                       "playbackRate": 1, "supportedMediaCommands": 15, "activeTrackIds": [],
                       "media": {"duration": self.duration, "contentId": self.load_urls[-1] if self.load_urls else "",
                                 "streamType": "BUFFERED", "metadata": {"title": "mock"}}})
            if self.state == "IDLE":
                st[0]["idleReason"] = self.idle_reason or "CANCELLED"
        return {"type": "MEDIA_STATUS", "requestId": rid, "status": st}

    def _serve(self, raw):
        try:
            sock = self._ctx.wrap_socket(raw, server_side=True)
        except Exception:
            return
        c = _Conn(sock)
        with self._lock:
            self._conns.append(c)
            self.connections += 1
        buf = b""
        try:
            while not self._stop and c.alive:
                chunk = sock.recv(8192)
                if not chunk:
                    break
                if c.muted:
                    continue
                buf += chunk
                while len(buf) >= 4:
                    n = struct.unpack(">I", buf[:4])[0]
                    if len(buf) < 4 + n:
                        break
                    body, buf = buf[4:4 + n], buf[4 + n:]
                    src, dst, ns, payload = decode(body)
                    try: msg = json.loads(payload)
                    except Exception: continue
                    self._handle(c, src, dst, ns, msg)
        except Exception:
            pass
        finally:
            c.alive = False
            with self._lock:
                if c in self._conns:
                    self._conns.remove(c)
            try: sock.close()
            except Exception: pass

    def _handle(self, c, src, dst, ns, m):
        t, rid = m.get("type"), m.get("requestId", 0)
        if ns == NS_HB and t == "PING":
            c.send(dst or "receiver-0", src, NS_HB, {"type": "PONG"})
        elif ns == NS_RECV:
            if t == "GET_STATUS":
                c.send("receiver-0", src, NS_RECV, self._recv_status(rid))
            elif t == "LAUNCH" and m.get("appId") == DMR:
                # run off the reader thread so a slow launch does not block PING/GET_STATUS
                threading.Thread(target=self._launch, args=(rid,), daemon=True).start()
        elif ns == NS_MEDIA:
            if t == "GET_STATUS":
                c.send(TRANSPORT, src, NS_MEDIA, self._media_status(rid))
            elif t == "LOAD":
                threading.Thread(target=self._load, args=(m, rid), daemon=True).start()
            elif t in ("PLAY", "PAUSE", "SEEK", "STOP") and self.player_open and self.session:
                with self._lock:
                    self._pos = m.get("currentTime", self.position()) if t == "SEEK" else self.position()
                    self._t0 = time.time()
                    if t == "PLAY": self.state = "PLAYING"
                    elif t == "PAUSE": self.state = "PAUSED"
                    elif t == "STOP":
                        self.state, self.idle_reason = "IDLE", "CANCELLED"
                msg = self._media_status(rid)
                if t == "STOP" and self.stop_closes_player:
                    self.player_open = False
                    self.session = 0
                    self._broadcast(NS_MEDIA, msg)
                    self._broadcast(NS_RECV, self._recv_status())
                else:
                    self._broadcast(NS_MEDIA, msg)

    def _get(self, url):
        try:
            with urllib.request.urlopen(url, timeout=60) as r:
                data = r.read()
                self.fetched.append((urllib.parse.urlparse(url).path, r.status, len(data), r.headers.get("Content-Type", "")))
                return data
        except Exception as e:
            self.fetched.append((urllib.parse.urlparse(url).path, 0, 0, repr(e)))
            return None

    def _fetch_hls(self, master_url, t):
        """Behave like a player: master -> index -> init -> the segment containing t."""
        master = self._get(master_url)
        if master is None:
            return False
        idx_rel = [l for l in master.decode().splitlines() if l and not l.startswith("#")]
        if not idx_rel:
            return False
        idx_url = urllib.parse.urljoin(master_url, idx_rel[0])
        index = self._get(idx_url)
        if index is None:
            return False
        lines = index.decode().splitlines()
        mp = [re.search(r'URI="([^"]+)"', l) for l in lines if l.startswith("#EXT-X-MAP")]
        init = self._get(urllib.parse.urljoin(idx_url, mp[0].group(1))) if mp and mp[0] else None
        acc, target, n = 0.0, None, 0
        durs = [float(l.split(":")[1].rstrip(",")) for l in lines if l.startswith("#EXTINF")]
        names = [l for l in lines if l and not l.startswith("#")]
        for i, d in enumerate(durs):
            if acc <= t < acc + d:
                target = i
                break
            acc += d
        if target is None:
            target = len(names) - 1
        seg = self._get(urllib.parse.urljoin(idx_url, names[target]))
        if init is None or seg is None:
            return False
        self.hls_target = target
        if self.fetch_dir:
            os.makedirs(self.fetch_dir, exist_ok=True)
            pi, ps = os.path.join(self.fetch_dir, "init.mp4"), os.path.join(self.fetch_dir, "seg.m4s")
            open(pi, "wb").write(init)
            open(ps, "wb").write(init + seg)  # init + media segment = playable fMP4
            self.fetched_files = {"init": pi, "seg": ps, "seg_index": target}
        return True

    def _launch(self, rid):
        time.sleep(self.launch_delay)
        with self._lock:
            if not self.powered:
                return
            if not self.player_open:
                self.player_open = True
                self.launches += 1
        self._broadcast(NS_RECV, self._recv_status(rid))

    def _load(self, m, rid):
        time.sleep(self.load_delay)
        media = m.get("media", {})
        is_hls = "mpegurl" in str(media.get("contentType", "")).lower() or ".m3u8" in str(media.get("contentId", ""))
        if is_hls:
            with self._lock:
                self.hls_loads += 1
                self.load_msgs.append({"currentTime": m.get("currentTime"), "media": media})
        if not (self.player_open and self.powered):
            self._broadcast(NS_MEDIA, {"type": "LOAD_FAILED", "requestId": rid})
            return
        if is_hls and self.hls_mode == "reject":
            self._broadcast(NS_MEDIA, {"type": "LOAD_FAILED", "requestId": rid, "detail": {"reason": "mock rejects HLS"}})
            return
        if is_hls and self.hls_mode == "play" and not self._fetch_hls(media.get("contentId", ""), float(m.get("currentTime") or 0)):
            self._broadcast(NS_MEDIA, {"type": "LOAD_FAILED", "requestId": rid, "detail": {"reason": "mock could not fetch HLS"}})
            return
        with self._lock:
            self.loads += 1
            self.session += 1
            self.load_urls.append(m.get("media", {}).get("contentId", ""))
            self._pos = float(m.get("currentTime") or 0)
            self.load_times.append(self._pos)
            self._t0 = time.time()
            self.state = "PLAYING"
            self.idle_reason = ""
        self._broadcast(NS_MEDIA, self._media_status(rid))


# ------------------------------------------------------------------ eureka_info
class MockEureka:
    """HTTPS GET /setup/eureka_info on 127.0.0.1:8443 (fixed by the app)."""

    def __init__(self, dolby=False, port=8443):
        outer = self

        class H(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                body = json.dumps({"audio": {"digital": bool(outer.dolby)}}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *a):
                pass

        self.dolby = dolby
        self.srv = http.server.ThreadingHTTPServer(("127.0.0.1", port), H)
        self.srv.socket = _server_ctx().wrap_socket(self.srv.socket, server_side=True)
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()

    def close(self):
        self.srv.shutdown()
        self.srv.server_close()
