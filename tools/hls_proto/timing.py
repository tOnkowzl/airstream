"""Cold-start / seek latency. usage: python timing.py A|B [fmp4|ts]"""
import sys, os, re, time, http.client, glob, json
name = sys.argv[1]; kind = sys.argv[2] if len(sys.argv) > 2 else 'fmp4'
W = os.path.dirname(os.path.abspath(__file__))
CD = os.path.join(W, 'cache', name, kind)
pre = '/' + name + '/' + ('ts/' if kind == 'ts' else '')
ext = 'm4s' if kind == 'fmp4' else 'ts'

def get(u):
    c = http.client.HTTPConnection('127.0.0.1', 8777, timeout=300)
    t0 = time.time(); c.request('GET', u); r = c.getresponse(); tt = time.time() - t0; b = r.read(); return b, tt, time.time() - t0
pl = get(pre + 'index.m3u8')[0].decode()
d = [float(x) for x in re.findall(r'#EXTINF:([\d.]+)', pl)]
kf0 = json.load(open(os.path.join(W, 'cache', name, 'kf.json')))['kf'][0]
acc = kf0; seek = 0
for i, x in enumerate(d):
    if acc + x > 3600: seek = i; break
    acc += x
def clear():
    for f in glob.glob(os.path.join(CD, 'seg*.' + ext)): os.remove(f)
    time.sleep(1.5)
for label, n in (('cold seg0', 0), ('seek to 3600 s (seg %d)' % seek, seek)):
    ts = []
    for trial in range(3):
        clear()
        time.sleep(0.5)
        b, ttfb, tot = get(f'{pre}seg{n}.{ext}')
        ts.append((ttfb, tot, len(b)))
    print(f'[{name} {kind}] {label}: ' + ' | '.join(f'ttfb {a:.3f}s total {b_:.3f}s {sz/1e6:.1f}MB' for a, b_, sz in ts), flush=True)
