"""Cast channel lifecycle against a mock receiver: warm channel, relaunch rules, half-open, TV off."""
import os, sys, time
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "fixtures"))
from app import App
from check import Checker, wait_until
from mock_receiver import MockReceiver
import make_fixtures

c = Checker("cast_resilience")
media = make_fixtures.make()
tv = MockReceiver(launch_delay=0.5, load_delay=0.2)
app = App("cast", mock_port=tv.port)
try:
    app.start()
    dev = app.device_id
    item = app.add(media["aac"])
    s = app.set_settings(quality="passthrough", last_device=dev, last_item=item, stream_mode="prepare")  # prepared-MP4 path; HLS default is covered by test_hls.py
    app.prepare(item, s)
    play = lambda: app.post_status("/api/cast", {"device": dev, "id": item})

    # 1. warm channel never launches the player
    ok = app.wait_log("warm cast channel ready", 30)
    time.sleep(3)
    c.check("warm channel opens without launching the player", ok and tv.connections >= 1 and tv.launches == 0,
            f"warm={ok} connections={tv.connections} launches={tv.launches}")

    # 2. Play launches once + loads
    st, r = play()
    c.check("Play launches the player and loads once", st == 200 and tv.launches == 1 and tv.loads == 1,
            f"status={st} launches={tv.launches} loads={tv.loads} {r.get('error', '')}")
    c.check("LOAD points at the prepared media URL", bool(tv.load_urls) and "/prepared/" in tv.load_urls[-1], str(tv.load_urls[-1:]))

    # 3. Play again reuses the player
    st, r = play()
    c.check("Play again re-LOADs without a second LAUNCH", st == 200 and tv.launches == 1 and tv.loads == 2,
            f"status={st} launches={tv.launches} loads={tv.loads}")

    # 4. Stop must not relaunch on its own
    st, _ = app.post_status("/api/transport", {"action": "stop"})
    for _ in range(8):  # keep the app polling the receiver for a while
        time.sleep(1)
        try: app.get("/api/position")
        except Exception: pass
    c.check("Stop is accepted and the player is NOT relaunched", st == 200 and tv.launches == 1 and not tv.player_open,
            f"status={st} launches={tv.launches} player_open={tv.player_open}")
    st, r = play()
    c.check("Play after Stop relaunches the player", st == 200 and tv.launches == 2 and tv.loads == 3,
            f"status={st} launches={tv.launches} loads={tv.loads}")

    # 5. Player closed silently by the TV
    time.sleep(3)  # the app ignores 'no player' answers within 2 s of attaching
    tv.close_player_silently()
    st, r = app.post_status("/api/transport", {"action": "play"})
    c.check("transport play after silent player close returns 409 no_session", st == 409 and r.get("error") == "no_session",
            f"status={st} body={r}")
    st, r = play()
    c.check("Play after silent close relaunches", st == 200 and tv.launches == 3 and tv.loads == 4,
            f"status={st} launches={tv.launches} loads={tv.loads} {r.get('error', '')}")

    # 6. Half-open socket (TV unplugged without FIN)
    time.sleep(1)
    tv.half_open_existing()
    loads_before, conns_before = tv.loads, tv.connections
    t0 = time.time()
    st, r = play()
    dt = time.time() - t0
    c.check("half-open socket: Play reconnects and loads in < 8 s",
            st == 200 and dt < 8 and tv.connections > conns_before and tv.loads == loads_before + 1,
            f"status={st} {dt:.1f}s connections {conns_before}->{tv.connections} loads={tv.loads} {r.get('error', '')}")

    # 7. TV off
    tv.power_off()
    t0 = time.time()
    st, r = play()
    dt = time.time() - t0
    c.check("TV off: /api/cast answers 502 within 15 s", st == 502 and dt < 15, f"status={st} {dt:.1f}s {r.get('error', '')}")
    tv.power_on()
    st, r = play()
    c.check("TV back on: Play works again", st == 200 and tv.player_open, f"status={st} {r.get('error', '')}")
except Exception as e:
    import traceback; traceback.print_exc()
    c.check("test run completed without exception", False, repr(e))
finally:
    app.stop(); tv.close()
c.finish()
