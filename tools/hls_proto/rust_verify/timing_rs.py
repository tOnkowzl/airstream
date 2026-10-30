"""Cold-start / seek latency vs hls_serve. usage: PORT=.. python timing_rs.py [ext]  (deletes cached segs between trials)"""
import sys, os, re, time, http.client, glob, json
PORT = int(os.environ.get('PORT', '42095')); ext = sys.argv[1] if len(sys.argv) > 1 else 'm4s'
def get(u):
    c = http.client.HTTPConnection('127.0.0.1', PORT, timeout=300)
    t0 = time.time(); c.request('GET', u); r = c.getresponse(); tt = time.time() - t0; b = r.read(); return b, tt, time.time() - t0
info = json.loads(get('/_info')[0]); CD = info['dir']
d = [e - s for s, e in info['segs']]
acc = info['kf'][0]; seek = 0
for i, x in enumerate(d):
    if acc + x > 3600: seek = i; break
    acc += x
def clear():
    time.sleep(1.5)
    for f in glob.glob(CD + '/seg*.' + ext): os.remove(f)
    time.sleep(0.5)
for label, n in (('cold seg0', 0), ('seek to 3600 s (seg %d)' % seek, seek)):
    ts = []
    for trial in range(3):
        clear(); b, ttfb, tot = get(f'/seg{n}.{ext}'); ts.append((ttfb, tot, len(b)))
    print(f'[{ext}] {label}: ' + ' | '.join(f'ttfb {a:.3f}s total {b_:.3f}s {sz/1e6:.1f}MB' for a, b_, sz in ts), flush=True)
