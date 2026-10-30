"""TS variant checks vs hls_serve --ts. usage: PORT=.. python verify_ts.py"""
import os, re, json, subprocess, http.client, sys, tempfile
PORT = int(os.environ.get('PORT', '42095'))
def get(u):
    c = http.client.HTTPConnection('127.0.0.1', PORT, timeout=300); c.request('GET', u); return c.getresponse().read()
pl = get('/index.m3u8').decode(); ext = [float(x) for x in re.findall(r'#EXTINF:([\d.]+)', pl)]
info = json.loads(get('/_info')); kf0 = info['kf'][0]
print('ts playlist:', len(ext), 'segs, no MAP:', '#EXT-X-MAP' not in pl, 'ENDLIST:', '#EXT-X-ENDLIST' in pl)
starts = [kf0]
for e in ext[:-1]: starts.append(starts[-1] + e)
tmp = os.environ.get('VTMP', tempfile.gettempdir())
ok = True; segs = list(range(100, 112)); data = {}
def pk(path, st):
    o = subprocess.run(['ffprobe', '-v', 'error', '-select_streams', st, '-show_entries', 'packet=pts_time,duration_time', '-of', 'csv=p=0', path], capture_output=True).stdout.decode()
    return [(float(l.split(',')[0]), float(l.split(',')[1]) if l.split(',')[1] not in ('N/A', '') else 0) for l in o.splitlines() if l.strip() and 'N/A' != l.split(',')[0]]
prev = None
for n in segs:
    b = get(f'/seg{n}.ts'); p = os.path.join(tmp, f'ts{n}.ts'); open(p, 'wb').write(b); data[n] = b
    v = pk(p, 'v:0'); a = pk(p, 'a:0')
    vs = min(x for x, _ in v); ve = max(x + d for x, d in v); as_ = min(x for x, _ in a)
    dev = abs((ve - vs) - ext[n]); st = abs(vs - starts[n]); sy = abs(as_ - vs)
    good = dev <= 0.05 and st < 0.01 and sy < 0.05
    ok &= good
    print(f'{n} vstart {vs:.3f} (playlist {starts[n]:.3f}) vdur {ve-vs:.3f} expect {ext[n]:.3f} a-v {sy*1000:.0f}ms', 'ok' if good else 'BAD')
    os.remove(p)
bad = []
for n in segs:
    p = os.path.join(tmp, f'd{n}.ts'); open(p, 'wb').write(data[n])
    r = subprocess.run(['ffmpeg', '-v', 'error', '-i', p, '-f', 'null', '-'], capture_output=True); os.remove(p)
    if r.returncode or r.stderr.strip(): bad.append((n, r.stderr.decode()[:100]))
print('per-segment decode errors:', bad or 'none')
# informational: concatenated decode (audio of different ffmpeg runs overlaps by <1 AAC frame at run restarts -> muxer dts warning)
cat = os.path.join(tmp, 'range.ts'); open(cat, 'wb').write(b''.join(data[n] for n in segs))
r = subprocess.run(['ffmpeg', '-v', 'error', '-i', cat, '-f', 'null', '-'], capture_output=True); os.remove(cat)
print('concatenated decode (info only): rc', r.returncode, 'stderr lines', len(r.stderr.decode().strip().splitlines()))
print('PASS' if ok and not bad else 'FAIL', 'TS variant durations/decode')
