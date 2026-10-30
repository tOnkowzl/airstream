"""Start/stop an isolated Airstream backend and talk to its HTTP API."""
import json, os, shutil, socket, subprocess, time, urllib.error, urllib.request

NOWIN = 0x08000000
E2E = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(E2E)
WORK = os.path.join(E2E, ".work")
EXE = os.environ.get("AIRSTREAM_EXE") or os.path.join(REPO, "target", "release", "airstream.exe")


def free_port(lo=42040, hi=42099):
    for p in range(lo, hi):
        s = socket.socket()
        try:
            s.bind(("127.0.0.1", p)); s.close(); return p
        except OSError:
            s.close()
    raise RuntimeError("no free port")


class HttpError(Exception):
    def __init__(self, status, body):
        super().__init__(f"HTTP {status}: {body}")
        self.status, self.body = status, body


class App:
    def __init__(self, name, mock_port=None, mock_name="MockTV"):
        self.name = name
        self.mock_name = mock_name
        self.mock_port = mock_port
        self.localappdata = os.path.join(WORK, "app_" + name)
        self.data = os.path.join(self.localappdata, "Airstream")
        self.port = free_port()
        self.base = f"http://127.0.0.1:{self.port}"
        self.proc = None
        self.device_id = None

    # ---- lifecycle
    def start(self):
        if not os.path.exists(EXE):
            raise RuntimeError(f"{EXE} missing — run `cargo build --release` first")
        shutil.rmtree(self.localappdata, ignore_errors=True)
        os.makedirs(self.localappdata, exist_ok=True)
        env = dict(os.environ, LOCALAPPDATA=self.localappdata, AIRSTREAM_HEADLESS="1", AIRSTREAM_PORT=str(self.port))
        if self.mock_port:
            env["AIRSTREAM_TEST_CAST"] = f"{self.mock_name}|127.0.0.1|{self.mock_port}"
        self.proc = subprocess.Popen([EXE], env=env, creationflags=NOWIN,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(200):
            try:
                if self.get("/api/ping").get("app") == "airstream":
                    break
            except Exception:
                if self.proc.poll() is not None:
                    raise RuntimeError("airstream exited during startup")
                time.sleep(0.1)
        else:
            raise RuntimeError("airstream did not answer /api/ping")
        if self.mock_port:
            self.device_id = self.wait_device()
        return self

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.kill()
            try: self.proc.wait(10)
            except Exception: pass

    def __enter__(self): return self.start()
    def __exit__(self, *a): self.stop()

    # ---- http
    def get(self, path, timeout=20):
        return self._req(urllib.request.Request(self.base + path), timeout)

    def post(self, path, body=None, timeout=60):
        r = urllib.request.Request(self.base + path, data=json.dumps(body or {}).encode(),
                                   headers={"Content-Type": "application/json"}, method="POST")
        return self._req(r, timeout)

    def post_status(self, path, body=None, timeout=60):
        """Like post() but returns (status, json) instead of raising on HTTP errors."""
        try:
            return 200, self.post(path, body, timeout)
        except HttpError as e:
            try: return e.status, json.loads(e.body)
            except Exception: return e.status, {"raw": e.body}

    @staticmethod
    def _req(req, timeout):
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                data = r.read()
        except urllib.error.HTTPError as e:
            raise HttpError(e.code, e.read().decode("utf-8", "replace"))
        return json.loads(data) if data else {}

    # ---- helpers
    def wait_device(self, timeout=60):
        t0 = time.time()
        while time.time() - t0 < timeout:
            d = [x for x in self.get("/api/devices")["devices"] if x["name"] == self.mock_name]
            if d:
                return d[0]["id"]
            time.sleep(0.25)
        raise RuntimeError("mock device never appeared in /api/devices")

    def add(self, path):
        """Add a media file to the library; returns its item id."""
        self.post("/api/add", {"paths": [path]})
        items = self.get("/api/state")["prefs"]["playlists"][0]["items"]
        norm = lambda p: os.path.normcase(os.path.normpath(p))
        for it in items:
            if norm(it.get("path", "")) == norm(path):
                return it["id"]
        return items[-1]["id"]

    def set_settings(self, **kw):
        s = dict(self.get("/api/settings"), **kw)
        self.post("/api/settings", s)
        return s

    def wait_prepared(self, item, timeout=300):
        t0 = time.time()
        while time.time() - t0 < timeout:
            st = self.get(f"/api/prepare/{item}")
            if st.get("state") in ("ready", "error"):
                return st
            time.sleep(0.5)
        raise RuntimeError("prepare timeout")

    def prepare(self, item, settings, speculative=True):
        """prepare_only cast (no TV involved); waits until ready; returns the final prepare_only reply."""
        r = self.post("/api/cast", {"device": self.device_id or "", "id": item, "prepare_only": True,
                                    "speculative": speculative, "settings": settings})
        if r.get("preparing"):
            st = self.wait_prepared(item)
            if st.get("state") != "ready":
                raise RuntimeError(f"prepare failed: {st}")
            r = self.post("/api/cast", {"device": self.device_id or "", "id": item, "prepare_only": True,
                                        "settings": settings})
        return r

    def prepared_file(self, key):
        return os.path.join(self.data, "prepared", key + ".mp4")

    def log(self):
        try:
            with open(os.path.join(self.data, "logs", "backend.log"), "r", encoding="utf-8", errors="replace") as f:
                return f.read()
        except OSError:
            return ""

    def wait_log(self, needle, timeout=30, since=0):
        t0 = time.time()
        while time.time() - t0 < timeout:
            if needle in self.log()[since:]:
                return True
            time.sleep(0.25)
        return False
