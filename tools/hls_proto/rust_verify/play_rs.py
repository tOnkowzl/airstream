"""Headless Edge + hls.js playback test. run: uv run --with playwright python play.py A [seekto]"""
import sys, time, json
from playwright.sync_api import sync_playwright
import os
PORT=os.environ.get('PORT','42095')
name = sys.argv[1]
seekto = float(sys.argv[2]) if len(sys.argv) > 2 else 3600.0
with sync_playwright() as p:
    br = p.chromium.launch(channel='msedge', headless=True, args=['--autoplay-policy=no-user-gesture-required', '--mute-audio'])
    pg = br.new_page()
    logs = []
    pg.on('console', lambda m: logs.append(m.text))
    pg.goto('http://127.0.0.1:%s/_player.html'%PORT+'')
    pg.wait_for_function('typeof Hls !== "undefined"', timeout=30000)
    print('hls.js version', pg.evaluate('Hls.version'), 'supported', pg.evaluate('Hls.isSupported()'))
    t0 = time.time()
    pg.evaluate(f'start("/master.m3u8", 0)')
    pg.wait_for_function('R.playing !== undefined', timeout=60000)
    print('time-to-first-frame from start() (ms):', pg.evaluate('R.playing'), ' loadeddata:', pg.evaluate('R.loadeddata'), 'wall', round((time.time() - t0) * 1000))
    c0 = pg.evaluate('document.getElementById("v").currentTime')
    time.sleep(8)
    c1 = pg.evaluate('document.getElementById("v").currentTime')
    st = pg.evaluate('(()=>{const v=document.getElementById("v");return [v.videoWidth,v.videoHeight,v.readyState,v.paused,v.buffered.length?v.buffered.end(0):-1]})()')
    print(f'play from 0: currentTime {c0:.2f} -> {c1:.2f} after 8 s; videoWxH/ready/paused/bufEnd={st}')
    ok0 = c1 - c0 > 6
    # seek
    pg.evaluate('window.tSeek=performance.now(); R.seekFrames=[]; window.lastSN=R.frag.length')
    pg.evaluate(f'(()=>{{const v=document.getElementById("v"); v.addEventListener("seeked",()=>{{R.seeked=Math.round(performance.now()-tSeek)}}); v.addEventListener("playing",()=>{{R.playingAfterSeek=Math.round(performance.now()-tSeek)}}); R.seeked=undefined;R.playingAfterSeek=undefined; v.currentTime={seekto};}})()')
    pg.wait_for_function('R.seeked !== undefined', timeout=60000)
    # wait until currentTime advances past seek +1
    t1 = time.time()
    pg.wait_for_function(f'document.getElementById("v").currentTime > {seekto + 1.0}', timeout=60000)
    adv = round((time.time() - t1) * 1000)
    ct = pg.evaluate('document.getElementById("v").currentTime')
    print(f'seek to {seekto}: seeked event after {pg.evaluate("R.seeked")} ms; currentTime reached {ct:.2f} (>seek+1) {adv} ms after seeked; playing-after-seek {pg.evaluate("R.playingAfterSeek")} ms')
    time.sleep(5)
    c2 = pg.evaluate('document.getElementById("v").currentTime')
    ok1 = c2 - ct > 3
    print(f'after seek: currentTime {ct:.2f} -> {c2:.2f} in 5 s')
    # seek backwards to a not-yet-requested region 300 s
    pg.evaluate('R.seeked=undefined; document.getElementById("v").currentTime=300')
    pg.wait_for_function('document.getElementById("v").currentTime > 301.5', timeout=60000)
    print('seek back to 300 s ok, currentTime', pg.evaluate('document.getElementById("v").currentTime'))
    print('fatal errors:', pg.evaluate('R.fatal'), ' all errors/warnings:', pg.evaluate('R.errors'))
    print('fragments loaded (sn, ms):', pg.evaluate('R.frag.map(f=>f[0]).join(",")'))
    print('PASS' if ok0 and ok1 and not pg.evaluate('R.fatal.length') else 'FAIL', f'hls.js playback {name}: play-from-0 advance={c1-c0:.2f}s, post-seek advance={c2-ct:.2f}s, fatal={pg.evaluate("R.fatal.length")}')
    br.close()
