"""Browser E2E (Playwright + Edge): the Play button against the mock receiver."""
import os, sys, time
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "fixtures"))
from app import App
from check import Checker, wait_until
from mock_receiver import MockReceiver
import make_fixtures

c = Checker("ui_play")
try:
    from playwright.sync_api import sync_playwright
except ImportError:
    c.check("playwright is installed (run via run_all.py / uv run --with playwright)", False)
    c.finish()

media = make_fixtures.make()
tv = MockReceiver(launch_delay=0.5, load_delay=0.5)
app = App("ui", mock_port=tv.port)
try:
    app.start()
    dev = app.device_id
    item = app.add(media["aac"])
    s = app.set_settings(quality="passthrough", last_device=dev, last_item=item)
    app.prepare(item, s)
    with sync_playwright() as pw:
        browser = pw.chromium.launch(channel="msedge", headless=True)
        page = browser.new_page()
        page.goto(app.base + "/")
        page.wait_for_selector(".row", timeout=15000)
        page.click(".row")  # select the item
        page.wait_for_function("document.querySelector('#devName').textContent.includes('MockTV')", timeout=20000)

        # 1. Play button plays
        page.click("#btnPlayPause")
        dt = wait_until(lambda: tv.loads >= 1, 20)
        c.check("UI Play button launches the player and sends LOAD", dt is not None and tv.launches == 1 and tv.loads == 1,
                f"launches={tv.launches} loads={tv.loads}")
        dt = wait_until(lambda: page.inner_text("#tState").strip().upper() in ("PLAYING", "BUFFERING"), 15, 0.25)
        c.check("UI shows the PLAYING state", dt is not None, page.inner_text("#tState"))

        # 2. silent close -> Play relaunches
        time.sleep(3)
        tv.close_player_silently()
        time.sleep(1)
        page.click("#btnPlayPause")
        dt = wait_until(lambda: tv.loads >= 2, 25)
        c.check("after silent player close the UI Play relaunches and loads", dt is not None and tv.launches == 2 and tv.loads == 2,
                f"launches={tv.launches} loads={tv.loads}")
        wait_until(lambda: page.inner_text("#tState").strip().upper() in ("PLAYING", "BUFFERING"), 15, 0.25)

        # 3. double-click spam -> exactly one LOAD
        time.sleep(3)
        tv.close_player_silently()
        wait_until(lambda: page.inner_text("#tState").strip().upper() not in ("PLAYING", "BUFFERING", "PAUSED"), 15, 0.25)
        before_l, before_launch = tv.loads, tv.launches
        for _ in range(6):
            page.click("#btnPlayPause", delay=0, no_wait_after=True)
        time.sleep(8)
        c.check("click spam on Play sends exactly one LOAD", tv.loads == before_l + 1 and tv.launches == before_launch + 1,
                f"loads +{tv.loads - before_l}, launches +{tv.launches - before_launch}")
        browser.close()
except Exception as e:
    import traceback; traceback.print_exc()
    c.check("test run completed without exception", False, repr(e))
finally:
    app.stop(); tv.close()
c.finish()
