"""Verification suite against running hlsproto server. usage: python verify.py A|B"""
import sys, os, json, time, subprocess, hashlib, shutil, http.client, re
W = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, W)
import mp4util
NOWIN = 0x08000000
SRC = {'A': "C:/Users/wpans/Downloads/[Netflix] Haunted Universities 4 (2026)/เทอม 4_by MerkavaMII.mkv",
       'D': "C:/Users/wpans/Downloads/[Netflix] Where the Mask Ends (Season 1) Ep.15/S01E015_ตอน 15_by MerkavaMII.mkv",
       'B': "C:/Users/wpans/Downloads/หลอนจิตปริศนา The Taking of Deborah Logan (2014)/The.Taking.of.Deborah.Logan.2014.2160p.H265.HEVC.mkv"}
name = sys.argv[1]
SRCP = SRC[name]
TMP = os.path.join(W, 'vtmp_' + name)
shutil.rmtree(TMP, ignore_errors=True); os.makedirs(TMP)
CACHE = os.path.join(W, 'cache', name)


def get(u):
    c = http.client.HTTPConnection('127.0.0.1', int(os.environ.get('PORT','8777')), timeout=300)
    t0 = time.time()
    c.request('GET', '/' + name + '/' + u)
    r = c.getresponse()
    ttfb = time.time() - t0
    b = r.read()
    return b, ttfb, time.time() - t0


def run(cmd, **kw):
    return subprocess.run(cmd, capture_output=True, creationflags=NOWIN, **kw)


def pkts(path, stream):
    o = run(['ffprobe', '-v', 'error', '-select_streams', stream, '-show_entries', 'packet=pts_time,dts_time,duration_time', '-of', 'csv=p=0', path]).stdout.decode()
    r = []
    for l in o.splitlines():
        p = l.strip().split(',')
        if len(p) >= 3 and p[0] != 'N/A':
            r.append((float(p[0]), float(p[2]) if p[2] != 'N/A' else 0.0))
    return r


results = []
def check(label, ok, detail=''):
    results.append(ok)
    print(('PASS ' if ok else 'FAIL ') + label + (' :: ' + detail if detail else ''), flush=True)


pl = get('index.m3u8')[0].decode()
ext = [float(x) for x in re.findall(r'#EXTINF:([\d.]+)', pl)]
nseg = len(ext)
starts = [0.0]
for e in ext[:-1]: starts.append(starts[-1] + e)
idx = json.load(open(os.path.join(CACHE, 'kf.json')))
kf0 = idx['kf'][0]
init = get('init.mp4')[0]
print(f'[{name}] nseg={nseg} first keyframe={kf0} playlist total={sum(ext):.3f} container dur={idx["dur"]}')

# playlist sanity
check('playlist VOD+ENDLIST+MAP', '#EXT-X-PLAYLIST-TYPE:VOD' in pl and '#EXT-X-ENDLIST' in pl and '#EXT-X-MAP' in pl)
kfs = set(round(k, 3) for k in idx['kf'])
absstart = [kf0 + s for s in starts]
check('every segment start is a source keyframe (<2ms)', all(min(abs(a - k) for k in idx['kf']) < 0.002 for a in absstart[:-1] if True), 'all segment starts (cumulative EXTINF sum vs keyframe list)')
check('no segment < 2 s except last', all(e >= 2.0 for e in ext[:-1]))

# segment range: 120 s window mid-film, fetched sequentially -> run restarts at chunk boundaries
N0 = 100 if name in ('A', 'D') else 300
N1 = N0
while sum(ext[N0:N1 + 1]) < 120: N1 += 1
rng = list(range(N0, N1 + 1))
segs = {}
t = time.time()
for n in rng:
    b, ttfb, tot = get(f'seg{n}.m4s')
    segs[n] = b
print(f'fetched seg {N0}..{N1} ({sum(ext[N0:N1+1]):.1f}s) in {time.time()-t:.1f}s')
open(os.path.join(TMP, 'init.mp4'), 'wb').write(init)

# 1) durations per segment
info = []
bad = 0
for n in rng:
    f = os.path.join(TMP, f'one{n}.mp4')
    open(f, 'wb').write(init + segs[n])
    v = pkts(f, 'v:0'); a = pkts(f, 'a:0')
    vs = min(p for p, d in v); ve = max(p + d for p, d in v)
    as_ = min(p for p, d in a); ae = max(p + d for p, d in a)
    info.append(dict(n=n, vs=vs, ve=ve, as_=as_, ae=ae, dur=ve - vs, expect=ext[n]))
    if abs((ve - vs) - ext[n]) > 0.05: bad += 1
print('seg  vstart   vdur  expect  astart  adur')
for i in info:
    print(f"{i['n']:4d} {i['vs']:9.3f} {i['dur']:7.3f} {i['expect']:7.3f} {i['as_']:9.3f} {i['ae']-i['as_']:7.3f}")
check(f'segment video duration == EXTINF (<=0.05 s) for {len(rng)} segs', bad == 0, f'max dev {max(abs(i["dur"]-i["expect"]) for i in info):.4f}')
check('video start == playlist keyframe start (<0.01 s)', all(abs(i['vs'] - (kf0 + starts[i['n']])) < 0.01 for i in info), f'max dev {max(abs(i["vs"]-(kf0+starts[i["n"]])) for i in info):.4f}')

# 2) continuity across all adjacent pairs
vg = [info[i + 1]['vs'] - info[i]['ve'] for i in range(len(info) - 1)]
ag = [info[i + 1]['as_'] - info[i]['ae'] for i in range(len(info) - 1)]
fr = 1 / 23.976
check('video pts continuous across segment boundaries (|gap|<1 frame)', all(abs(g) < fr for g in vg), f'max |gap| {max(abs(g) for g in vg):.4f}')
check('audio pts continuous across segment boundaries (|gap|<1 frame)', all(abs(g) < fr for g in ag), f'max |gap| {max(abs(g) for g in ag):.4f}')
# tfdt consistency
tf = [dict(reversed(mp4util.tfdts(segs[n]))) for n in rng]  # first moof per track
ts_ = {t: s for t, (s, h) in mp4util.timescales(init).items()}
check('tfdt monotonic & matches playlist start (video, <0.15s: ctts offset)', all(abs(tf[i][1] / ts_[1] - (kf0 + starts[n])) < 0.15 for i, n in enumerate(rng)),
      f'tfdt0={tf[0][1]/ts_[1]:.3f} playlist={kf0+starts[rng[0]]:.3f}')

# 3) sync
sy = [abs(i['as_'] - i['vs']) for i in info]
check('audio/video start within 50 ms per segment', max(sy) < 0.05, f'max {max(sy)*1000:.1f} ms')

# 4) restart boundary: seg N from run X, seg N+1 from a DIFFERENT run (verified via server segrun map)
def runs():
    c = http.client.HTTPConnection('127.0.0.1', int(os.environ.get('PORT','8777'))); c.request('GET', '/_runs/' + name); return json.loads(c.getresponse().read())
R0 = N1 + 60
a = get(f'seg{R0}.m4s')[0]                 # run X starts here (chunk R0..R0+2)
get(f'seg{R0+80}.m4s')                     # far seek -> kills & restarts elsewhere
b = get(f'seg{R0+1}.m4s')[0]
rm = runs()
for off in (1, 2, 3, 4):                   # find first adjacent pair produced by different runs
    if rm.get(f'fmp4:{R0+off-1}') != rm.get(f'fmp4:{R0+off}'):
        pn = R0 + off - 1; break
print('restart pair', pn, pn + 1, 'runs', rm.get(f'fmp4:{pn}'), rm.get(f'fmp4:{pn+1}'))
a = get(f'seg{pn}.m4s')[0]; b = get(f'seg{pn+1}.m4s')[0]
check('restart pair really from two different ffmpeg runs', rm.get(f'fmp4:{pn}') != rm.get(f'fmp4:{pn+1}'))
fa = os.path.join(TMP, 'ra.mp4'); fb = os.path.join(TMP, 'rb.mp4')
open(fa, 'wb').write(init + a); open(fb, 'wb').write(init + b)
va, aa = pkts(fa, 'v:0'), pkts(fa, 'a:0'); vb, ab = pkts(fb, 'v:0'), pkts(fb, 'a:0')
gv = min(p for p, d in vb) - max(p + d for p, d in va)
ga = min(p for p, d in ab) - max(p + d for p, d in aa)
check('restart boundary video gap < 1 frame', abs(gv) < fr, f'gap {gv*1000:.2f} ms')
check('restart boundary audio gap < 1 frame', abs(ga) < fr, f'gap {ga*1000:.2f} ms')
tfa, tfb = dict(reversed(mp4util.tfdts(a))), dict(reversed(mp4util.tfdts(b)))
print('tfdt video N, N+1 ->', tfa[1], tfb[1], 'audio', tfa[2], tfb[2])
# also whole-range run composition
print('run ids across verified range:', [rm.get(f'fmp4:{n}') for n in rng])

# 5) decode + md5 over the 120 s range
cat = os.path.join(TMP, 'range.mp4')
with open(cat, 'wb') as f:
    f.write(init)
    for n in rng: f.write(segs[n])
t = time.time()
r = run(['ffmpeg', '-v', 'error', '-i', cat, '-f', 'null', '-'])
check('decode of init+range has no errors', r.returncode == 0 and not r.stderr.strip(), f'{time.time()-t:.1f}s stderr={r.stderr.decode()[:200]!r}')
m1 = run(['ffmpeg', '-v', 'error', '-i', cat, '-map', '0:v', '-c', 'copy', '-f', 'md5', '-']).stdout.decode().strip()
T0 = kf0 + starts[N0]; dur = sum(ext[N0:N1 + 1])
m2 = run(['ffmpeg', '-v', 'error', '-ss', f'{T0:.3f}', '-i', SRCP, '-map', '0:v:0', '-c', 'copy', '-t', f'{dur-0.010:.3f}', '-f', 'md5', '-']).stdout.decode().strip()
check('video stream md5 == direct stream-copy of same range', m1 == m2 and m1 != '', f'{m1} vs {m2}')

print(f'SUMMARY {name}: {sum(results)}/{len(results)} passed')
