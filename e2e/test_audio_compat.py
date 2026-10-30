"""Receiver without Dolby (mock eureka_info on :8443): E-AC-3 -> AAC 6ch, video untouched; AAC source untouched."""
import os, subprocess, sys
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "fixtures"))
from app import App
from check import Checker
from mock_receiver import MockReceiver, MockEureka
import make_fixtures

NOWIN = 0x08000000
c = Checker("audio_compat")


def probe(path, sel):
    return subprocess.run(["ffprobe", "-v", "error", "-select_streams", sel, "-show_entries", "stream=codec_name,channels",
                           "-of", "csv=p=0", path], capture_output=True, text=True, creationflags=NOWIN).stdout.strip().rstrip(",")


def vhash(path):
    return subprocess.run(["ffmpeg", "-v", "error", "-i", path, "-map", "0:v:0", "-c", "copy", "-f", "hash", "-hash", "md5", "-"],
                          capture_output=True, text=True, creationflags=NOWIN).stdout.strip()


media = make_fixtures.make()
tv = MockReceiver()
eureka = MockEureka(dolby=False)
app = App("audio", mock_port=tv.port)
try:
    app.start()
    dev = app.device_id
    s = app.set_settings(quality="passthrough", last_device=dev, stream_mode="prepare")  # this test inspects the prepared MP4

    # --- E-AC-3 5.1 source
    src = media["eac3"]
    c.check("fixture really is E-AC-3 6ch", probe(src, "a") == "eac3,6", probe(src, "a"))
    item = app.add(src)
    r = app.post("/api/cast", {"device": dev, "id": item, "prepare_only": True, "speculative": True, "settings": s})
    c.check("UI is told about the conversion (audio_note)", "AAC 6ch" in r.get("audio_note", ""), r.get("audio_note", ""))
    r = app.prepare(item, s)
    out = app.prepared_file(r["key"])
    c.check("prepared output exists", os.path.exists(out), out)
    c.check("audio becomes AAC with all 6 channels", probe(out, "a") == "aac,6", probe(out, "a"))
    c.check("video is bit-identical to the source", vhash(out) != "" and vhash(out) == vhash(src), vhash(out)[:24])

    # --- AAC source: nothing converted
    src = media["aac"]
    item2 = app.add(src)
    r = app.post("/api/cast", {"device": dev, "id": item2, "prepare_only": True, "speculative": True, "settings": s})
    c.check("AAC source: no conversion note", not r.get("audio_note"), r.get("audio_note", ""))
    r = app.prepare(item2, s)
    out2 = app.prepared_file(r["key"])
    c.check("AAC source: audio stays AAC stereo", probe(out2, "a") == "aac,2", probe(out2, "a"))
    c.check("AAC source: video bit-identical", vhash(out2) != "" and vhash(out2) == vhash(src))
except Exception as e:
    import traceback; traceback.print_exc()
    c.check("test run completed without exception", False, repr(e))
finally:
    app.stop(); tv.close(); eureka.close()
c.finish()
