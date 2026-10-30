"""On-demand HLS (default for Original quality) vs. prepared-MP4 fallback, against the mock receiver."""
import json, os, socket, subprocess, sys, time, urllib.error, urllib.request
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "fixtures"))
from app import App
from check import Checker, wait_until
from mock_receiver import MockReceiver, MockEureka
import make_fixtures

NOWIN = 0x08000000
c = Checker("hls")


def probe(path, sel):
    return subprocess.run(["ffprobe", "-v", "error", "-select_streams", sel, "-show_entries", "stream=codec_name,channels",
                           "-of", "csv=p=0", path], capture_output=True, text=True, creationflags=NOWIN).stdout.strip().rstrip(",")


def first_pts(path):
    out = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts_time",
                          "-read_intervals", "%+#1", "-of", "csv=p=0", path], capture_output=True, text=True, creationflags=NOWIN).stdout
    try: return float(out.split()[0].strip(","))
    except Exception: return -1.0


def lan_ip():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect(("192.168.1.1", 9)); return s.getsockname()[0]
    except Exception:
        return ""
    finally:
        s.close()


def http_status(url):
    try:
        with urllib.request.urlopen(url, timeout=20) as r:
            return r.status, r.headers
    except urllib.error.HTTPError as e:
        return e.code, e.headers
    except Exception as e:
        return 0, {"err": repr(e)}


def prepared_mp4s(app):
    d = os.path.join(app.data, "prepared")
    return [f for f in os.listdir(d) if f.endswith(".mp4")] if os.path.isdir(d) else []


media = make_fixtures.make()
fetch_a = os.path.join(os.path.dirname(os.path.abspath(__file__)), ".work", "hls_fetch")
tv = MockReceiver(hls_mode="play", fetch_dir=fetch_a)
eureka = MockEureka(dolby=False)
app = App("hls", mock_port=tv.port)
try:
    app.start()
    dev = app.device_id
    s = app.set_settings(quality="passthrough", last_device=dev)

    # ---- (a) H.264 + AAC mkv: instant HLS, audio copied, resume position honoured
    item = app.add(media["aac"])
    s = app.set_settings(quality="passthrough", last_device=dev, last_item=item)
    t0 = time.time()
    st, r = app.post_status("/api/cast", {"device": dev, "id": item})
    dt = time.time() - t0
    c.check("Play returns ok in < 3 s on a never-prepared mkv", st == 200 and r.get("ok") and dt < 3, f"status={st} {dt:.2f}s {r}")
    c.check("response says stream=hls, mode mentions instant streaming", r.get("stream") == "hls" and "สตรีมทันที" in r.get("mode", ""), f"{r.get('stream')} / {r.get('mode')}")
    c.check("no file preparation was started", "prepare started" not in app.log(), "")
    c.check("no prepared mp4 on disk", prepared_mp4s(app) == [], str(prepared_mp4s(app)))
    msg = tv.load_msgs[-1] if tv.load_msgs else {}
    media_obj = msg.get("media", {})
    c.check("LOAD contentType is application/x-mpegurl with fmp4 segment formats",
            media_obj.get("contentType") == "application/x-mpegurl" and media_obj.get("hlsSegmentFormat") == "fmp4"
            and media_obj.get("hlsVideoSegmentFormat") == "fmp4" and media_obj.get("streamType") == "BUFFERED", json.dumps(media_obj)[:300])
    c.check("LOAD contentId is the /vod/<sid>/master.m3u8 URL", "/vod/" in media_obj.get("contentId", "") and media_obj.get("contentId", "").endswith("/master.m3u8"), media_obj.get("contentId", ""))
    paths = [f[0] for f in tv.fetched]
    c.check("mock fetched master, index, init and a segment (all 200)",
            any(p.endswith("master.m3u8") for p in paths) and any(p.endswith("index.m3u8") for p in paths)
            and any(p.endswith("init.mp4") for p in paths) and any(p.endswith(".m4s") for p in paths) and all(f[1] == 200 for f in tv.fetched), str(tv.fetched))
    ctypes = {f[0].rsplit("/", 1)[-1].split(".")[-1]: f[3] for f in tv.fetched}
    c.check("content types: m3u8 mpegurl, init video/mp4, segment video/iso.segment",
            "mpegurl" in ctypes.get("m3u8", "") and ctypes.get("mp4") == "video/mp4" and ctypes.get("m4s") == "video/iso.segment", str(ctypes))
    c.check("first play starts at 0", tv.load_times[-1] == 0, str(tv.load_times))

    # resume position: play to ~25 s, let the tracker persist it, Play again
    tv.set_position(25)
    persisted = wait_until(lambda: any(i.get("id") == item and i.get("resume_pos", 0) > 20
                                        for i in app.get("/api/state")["prefs"]["playlists"][0]["items"]), 15, 0.5)
    c.check("resume position persisted by the tracker", persisted is not None, "")
    n_before = len(tv.fetched)
    tv.fetched_files = {}
    st, r = app.post_status("/api/cast", {"device": dev, "id": item})
    c.check("second Play resumes at ~25 s via HLS", st == 200 and 24 <= r.get("resumed_from", 0) <= 30 and 24 <= tv.load_times[-1] <= 30,
            f"status={st} resumed_from={r.get('resumed_from')} load_times={tv.load_times}")
    new = [f[0] for f in tv.fetched[n_before:]]
    c.check("receiver fetched init + the segment at the resume position", any(p.endswith("init.mp4") for p in new) and any(p.endswith(".m4s") for p in new), str(new))
    seg = tv.fetched_files.get("seg")
    if seg and os.path.exists(seg):
        pts = first_pts(seg)
        pos = tv.load_times[-1]
        c.check("fetched segment starts shortly before the resume position", pos - 8 < pts <= pos + 0.01, f"first pts={pts} resume={pos} seg_index={tv.fetched_files.get('seg_index')}")
        c.check("segment audio is AAC (copied), video H.264", probe(seg, "a") == "aac,2" and probe(seg, "v").startswith("h264"), f"a={probe(seg, 'a')} v={probe(seg, 'v')}")
    else:
        c.check("fetched segment saved for inspection", False, str(tv.fetched_files))
    c.check("AAC source: no audio_note", not r.get("audio_note"), r.get("audio_note", ""))
    now = app.get("/api/now")
    c.check("/api/now reports stream=hls", now.get("stream") == "hls" and now.get("hls") is True, str({k: now.get(k) for k in ("stream", "hls", "mode")}))
    caps = json.load(open(os.path.join(app.data, "receiver-caps.json"))) if os.path.exists(os.path.join(app.data, "receiver-caps.json")) else {}
    c.check("receiver recorded as hls=true after PLAYING", caps.get("MockTV", {}).get("hls") is True, json.dumps(caps)[:200])

    # Seek is handled by the receiver (Cast SEEK), no new LOAD
    loads = tv.loads
    st, r = app.post_status("/api/seek", {"seconds": 10})
    c.check("seek keeps working (no extra LOAD)", st == 200 and tv.loads == loads, f"status={st} {r}")

    # ---- (b) E-AC-3 5.1, TV without Dolby -> AAC 6ch + note
    item2 = app.add(media["eac3"])
    s = app.set_settings(quality="passthrough", last_device=dev, last_item=item2)
    tv.fetched_files = {}
    t0 = time.time()
    st, r = app.post_status("/api/cast", {"device": dev, "id": item2, "from_start": True})
    dt = time.time() - t0
    c.check("E-AC-3 Play over HLS returns quickly", st == 200 and r.get("stream") == "hls" and dt < 4, f"status={st} {dt:.2f}s {r.get('error', '')}")
    c.check("audio_note announces the conversion to AAC 6ch", "AAC 6ch" in r.get("audio_note", ""), r.get("audio_note", ""))
    seg = tv.fetched_files.get("seg")
    ok = bool(seg) and os.path.exists(seg)
    c.check("segment audio is AAC 6 channels, video H.264", ok and probe(seg, "a") == "aac,6" and probe(seg, "v").startswith("h264"),
            f"a={probe(seg, 'a') if ok else None} v={probe(seg, 'v') if ok else None}")

    # ---- (d) prepare_only on an HLS item: ready at once, nothing written to prepared/
    t0 = time.time()
    r = app.post("/api/cast", {"device": dev, "id": item2, "prepare_only": True, "speculative": True, "settings": s})
    dt = time.time() - t0
    c.check("prepare_only returns ready immediately with stream=hls", r.get("ready") is True and r.get("stream") == "hls" and not r.get("preparing") and dt < 2, f"{dt:.2f}s {r}")
    time.sleep(1.0)
    c.check("prepare_only created no mp4 in prepared/", prepared_mp4s(app) == [], str(prepared_mp4s(app)))

    # stream_mode=prepare restores the prepared-MP4 path
    app.set_settings(stream_mode="prepare")
    r = app.post("/api/cast", {"device": dev, "id": item, "prepare_only": True, "speculative": True, "settings": app.get("/api/settings")})
    c.check("stream_mode=prepare: prepare_only starts a real preparation", bool(r.get("preparing")), str(r))
    st_ = app.wait_prepared(item)
    hls_loads = tv.hls_loads
    st, r = app.post_status("/api/cast", {"device": dev, "id": item})
    c.check("stream_mode=prepare: LOAD uses /prepared/ (no HLS)", st == 200 and "/prepared/" in tv.load_urls[-1] and r.get("stream") == "mp4" and tv.hls_loads == hls_loads,
            f"status={st} {tv.load_urls[-1:]}")
    app.set_settings(stream_mode="auto")

    # ---- (e) LAN guard
    ip = lan_ip()
    sid_url = media_obj.get("contentId", "")
    if ip.startswith("192.168."):
        lan_vod = sid_url.replace("//" + sid_url.split("/")[2], f"//{ip}:{app.port}")
        code, hdr = http_status(lan_vod)
        c.check("/vod/ is reachable from the LAN address", code == 200 and hdr.get("Access-Control-Allow-Origin") == "*", f"{lan_vod} -> {code}")
        code, _ = http_status(f"http://{ip}:{app.port}/api/settings")
        c.check("/api/settings from the LAN address is still blocked (403)", code == 403, str(code))
    else:
        c.check("machine has a 192.168.x LAN address for the guard test", False, ip)
    code, _ = http_status(f"{app.base}/vod/0000000000000000/master.m3u8")
    c.check("unknown HLS session -> 404", code == 404, str(code))
except Exception as e:
    import traceback; traceback.print_exc()
    c.check("part 1 completed without exception", False, repr(e))
finally:
    app.stop(); tv.close()

# ---- (c) receiver rejects HLS -> same Play falls back to the prepared MP4; next Play skips HLS
tv2 = MockReceiver(hls_mode="reject")
app2 = App("hls_reject", mock_port=tv2.port)
try:
    app2.start()
    dev2 = app2.device_id
    item = app2.add(media["aac"])
    app2.set_settings(quality="passthrough", last_device=dev2, last_item=item)
    st, r = app2.post_status("/api/cast", {"device": dev2, "id": item})
    c.check("rejected HLS LOAD was attempted once", tv2.hls_loads == 1, f"hls_loads={tv2.hls_loads}")
    c.check("same Play falls back to the MP4 path (preparing or prepared url)", st == 200 and (r.get("preparing") or "/prepared/" in r.get("url", "")), f"status={st} {r}")
    caps = {}
    p = os.path.join(app2.data, "receiver-caps.json")
    if os.path.exists(p): caps = json.load(open(p))
    c.check("receiver-caps.json now has hls:false for MockTV", caps.get("MockTV", {}).get("hls") is False, json.dumps(caps)[:200])
    c.check("fallback is logged", "HLS fallback" in app2.log(), "")
    if r.get("preparing"):
        app2.wait_prepared(item)
    st, r = app2.post_status("/api/cast", {"device": dev2, "id": item})
    c.check("next Play goes straight to MP4, HLS not tried again",
            st == 200 and r.get("ok") and "/prepared/" in tv2.load_urls[-1] and tv2.hls_loads == 1 and tv2.loads == 1 and r.get("stream") == "mp4",
            f"status={st} hls_loads={tv2.hls_loads} loads={tv2.loads} urls={tv2.load_urls} {r.get('error', '')}")
    r = app2.post("/api/cast", {"device": dev2, "id": item, "prepare_only": True, "speculative": True, "settings": app2.get("/api/settings")})
    c.check("prepare_only on a known no-HLS receiver reports the MP4 flow", r.get("stream") == "mp4" and r.get("ready"), str(r))
except Exception as e:
    import traceback; traceback.print_exc()
    c.check("reject scenario completed without exception", False, repr(e))
finally:
    app2.stop(); tv2.close(); eureka.close()
c.finish()
