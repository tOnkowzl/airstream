"""On-demand VOD HLS segmenter prototype (python + ffmpeg, stream-copied video).

usage: python hlsproto.py --port 8777 --cache DIR name=path [name=path ...]
URLs: /<name>/master.m3u8  /<name>/index.m3u8 (fmp4)  /<name>/init.mp4  /<name>/seg<N>.m4s
      /<name>/ts/index.m3u8 (MPEG-TS variant)  /<name>/ts/seg<N>.ts
      /_stats  (json: per-request timing log)
"""
import argparse, json, os, shutil, struct, subprocess, sys, threading, time, math, re
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import keyidx, mp4util

NOWIN = 0x08000000
TARGET = 6.0      # target segment seconds
MIN_SEG = 2.0     # never shorter (except last)
CHUNK_S = 30.0    # one ffmpeg run produces ~this many seconds, then stops
AHEAD_S = 60.0    # never produce further than this ahead of latest request
REQ_LOG = []


def probe(path):
    out = subprocess.run(['ffprobe', '-v', 'error', '-show_streams', '-show_format', '-of', 'json', path],
                         capture_output=True, text=True, encoding='utf-8', errors='replace', creationflags=NOWIN).stdout
    return json.loads(out)


def build_segments(kf, duration):
    """Greedy: next boundary = first keyframe >= start+TARGET. Tail < MIN_SEG merged. Returns list of (start,end)."""
    bounds = [kf[0]]
    for t in kf[1:]:
        if t - bounds[-1] >= TARGET - 1e-6:
            bounds.append(t)
    if duration - bounds[-1] < MIN_SEG and len(bounds) > 1:
        bounds.pop()
    bounds.append(duration)
    return [(bounds[i], bounds[i + 1]) for i in range(len(bounds) - 1)]


class Source:
    def __init__(self, name, path, cache_root):
        self.name, self.path = name, path
        self.dir = os.path.join(cache_root, name)
        os.makedirs(self.dir, exist_ok=True)
        t0 = time.time()
        idxf = os.path.join(self.dir, 'kf.json')
        st = os.stat(path)
        key = [st.st_size, int(st.st_mtime)]
        data = None
        if os.path.exists(idxf):
            d = json.load(open(idxf))
            if d.get('key') == key:
                data = d
        if data is None:
            kf, how = keyidx.keyframes(path)
            info = probe(path)
            dur = float(info['format']['duration'])
            data = dict(key=key, kf=kf, how=how, dur=dur, info=info)
            json.dump(data, open(idxf, 'w'))
        self.index_time = time.time() - t0
        self.kf, self.dur, self.info = data['kf'], data['dur'], data['info']
        v = [s for s in self.info['streams'] if s['codec_type'] == 'video' and s.get('disposition', {}).get('attached_pic', 0) == 0][0]
        a = [s for s in self.info['streams'] if s['codec_type'] == 'audio'][0]
        self.vcodec, self.acodec = v['codec_name'], a['codec_name']
        self.v_idx, self.a_idx = v['index'], a['index']
        self.width, self.height = v['width'], v['height']
        self.channels = int(a.get('channels', 2))
        self.segs = build_segments(self.kf, self.dur)
        self.nseg = len(self.segs)
        self.fmt = {}
        for kind in ('fmp4', 'ts'):
            os.makedirs(os.path.join(self.dir, kind, 'raw'), exist_ok=True)
        self.lock = threading.Condition()
        self.run = {'fmp4': None, 'ts': None}
        self.latest = {'fmp4': 0, 'ts': 0}
        self.hi = {'fmp4': -1, 'ts': -1}          # highest finalized contiguous-from-run segment
        self.runid = 0
        self.segrun = {}
        self.init_bytes = None
        self.log = open(os.path.join(self.dir, 'ffmpeg.log'), 'ab')

    # ---------- playlists
    def playlist(self, kind):
        ext = 'm4s' if kind == 'fmp4' else 'ts'
        td = math.ceil(max(e - s for s, e in self.segs))
        L = ['#EXTM3U', '#EXT-X-VERSION:7' if kind == 'fmp4' else '#EXT-X-VERSION:3', f'#EXT-X-TARGETDURATION:{td}',
             '#EXT-X-MEDIA-SEQUENCE:0', '#EXT-X-PLAYLIST-TYPE:VOD', '#EXT-X-INDEPENDENT-SEGMENTS']
        if kind == 'fmp4':
            L.append('#EXT-X-MAP:URI="init.mp4"')
        for i, (s, e) in enumerate(self.segs):
            L.append(f'#EXTINF:{e - s:.3f},')
            L.append(f'seg{i}.{ext}')
        L.append('#EXT-X-ENDLIST')
        return '\n'.join(L) + '\n'

    def master(self):
        init = self.get_init()
        codecs = self.codec_string(init)
        bw = int(os.stat(self.path).st_size * 8 / self.dur)
        return ('#EXTM3U\n#EXT-X-VERSION:7\n'
                f'#EXT-X-STREAM-INF:BANDWIDTH={bw},CODECS="{codecs}",RESOLUTION={self.width}x{self.height}\nindex.m3u8\n')

    def codec_string(self, init):
        v = 'avc1.640028' if self.vcodec == 'h264' else 'hvc1.1.6.L120.B0'
        i = init.find(b'avcC')
        if i > 0:
            v = 'avc1.' + init[i + 5:i + 8].hex().upper()
        i = init.find(b'hvcC')
        if i > 0:
            b = init[i + 4:i + 4 + 13]
            sp, tier, idc = b[1] >> 6, (b[1] >> 5) & 1, b[1] & 31
            compat = int.from_bytes(b[2:6], 'big')
            rev = int(f'{compat:032b}'[::-1], 2)
            cons = b[6:12].hex().upper().rstrip('0') or '0'
            cons = '.'.join(f'{x:X}' for x in b[6:12]).rstrip('.0') if False else b[6:12].hex().upper().rstrip('0')
            v = f'hvc1.{"" if sp == 0 else "ABC"[sp-1]}{idc}.{rev:X}.{"H" if tier else "L"}{b[12]}' + ('.' + '.'.join(f'{x:X}' for x in b[6:12]).rstrip('.0') if False else '')
            cs = [f'{x:X}' for x in b[6:12]]
            while cs and cs[-1] == '0':
                cs.pop()
            if cs:
                v += '.' + '.'.join(cs)
        a = 'mp4a.40.2'
        return f'{v},{a}'

    # ---------- ffmpeg command
    def audio_args(self):
        if self.acodec == 'aac':
            return ['-c:a', 'copy']
        return ['-c:a', 'aac', '-aac_coder', 'fast', '-b:a', '512k', '-ac', str(min(6, self.channels))]

    def video_args(self, kind):
        a = ['-c:v', 'copy']
        if self.vcodec == 'hevc' and kind == 'fmp4':
            a += ['-tag:v', 'hvc1']
        return a

    kind_hint = 'fmp4'

    def inputs(self, T, loglevel, kind='fmp4'):
        """input+map+codec args. Transcoded audio uses a 2nd input seeked 1 s earlier + atrim=start=T so audio begins
        sample-accurately at T (a single -ss input drops the whole audio packet containing T -> up to one packet late)."""
        c = ['ffmpeg', '-hide_banner', '-nostdin', '-v', loglevel, '-ss', f'{T:.3f}', '-i', self.path]
        if self.acodec == 'aac':
            return c + ['-map', f'0:{self.v_idx}', '-map', f'0:{self.a_idx}'] + self.video_args(kind) + self.audio_args()
        return c + ['-ss', f'{max(T - 1.0, 0):.3f}', '-i', self.path, '-map', f'0:{self.v_idx}', '-map', f'1:{self.a_idx}'] + \
            self.video_args(kind) + self.audio_args() + ['-af', f'atrim=start={T:.3f}']

    def cmd(self, kind, s, e, runid):
        """ffmpeg command producing segments s..e-1 (e exclusive)."""
        T = self.segs[s][0]
        bounds = [self.segs[i][0] - T for i in range(s + 1, e)] or [1e6]
        raw = os.path.join(self.dir, kind, 'raw', f'r{runid}_%d.' + ('mp4' if kind == 'fmp4' else 'ts'))
        c = self.inputs(T, 'warning', kind) + ['-sn', '-dn', '-copyts', '-avoid_negative_ts', 'disabled']
        if e < self.nseg:
            c += ['-to', f'{self.segs[e][0] - 0.010:.3f}']
        c += ['-f', 'segment', '-segment_times', ','.join(f'{b:.3f}' for b in bounds), '-segment_time_delta', '0.02',
              '-reset_timestamps', '0', '-segment_start_number', str(s)]
        if kind == 'fmp4':
            c += ['-segment_format', 'mp4', '-segment_format_options',
                  'movflags=+frag_keyframe+frag_discont+empty_moov+default_base_moof+delay_moov']
        else:
            c += ['-segment_format', 'mpegts', '-segment_format_options', 'mpegts_copyts=1']
        c += [raw]
        return c

    # ---------- on-demand production
    def final_path(self, kind, n):
        return os.path.join(self.dir, kind, f'seg{n}.' + ('m4s' if kind == 'fmp4' else 'ts'))

    def have(self, kind, n):
        return os.path.exists(self.final_path(kind, n))

    def start_run(self, kind, n):
        """lock held. kill current run, start new one at segment n."""
        self.stop_run(kind)
        # chunk end
        e = n + 1
        while e < self.nseg and self.segs[e][0] - self.segs[n][0] < CHUNK_S:
            e += 1
        # skip already-final prefix not needed: we start at n (which is missing)
        # stop early if next segments already exist
        e2 = n + 1
        while e2 < e and not self.have(kind, e2):
            e2 += 1
        e = e2
        self.runid += 1
        cmd = self.cmd(kind, n, e, self.runid)
        self.log.write((' '.join(cmd) + '\n').encode('utf8', 'replace')); self.log.flush()
        p = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=self.log, creationflags=NOWIN)
        self.run[kind] = dict(proc=p, start=n, end=e, next=n, id=self.runid, t0=time.time(), cmd=cmd)

    def stop_run(self, kind):
        r = self.run[kind]
        if r and r['proc'].poll() is None:
            r['proc'].kill(); r['proc'].wait()
        if r:
            self.finalize(kind, r, killed=True)
        self.run[kind] = None

    def raw_path(self, kind, r, n):
        return os.path.join(self.dir, kind, 'raw', f'r{r["id"]}_{n}.' + ('mp4' if kind == 'fmp4' else 'ts'))

    def finalize(self, kind, r, killed=False):
        """move closed raw segments to final. segment k closed if raw k+1 exists or proc finished OK (and k last)."""
        while r['next'] < r['end']:
            k = r['next']
            rp = self.raw_path(kind, r, k)
            if not os.path.exists(rp):
                break
            nxt = os.path.exists(self.raw_path(kind, r, k + 1))
            done = r['proc'].poll() == 0
            if not (nxt or (done and k == r['end'] - 1) or (done and not nxt)):
                break
            data = open(rp, 'rb').read()
            if kind == 'fmp4':
                init, media = mp4util.split(data)
                if self.init_bytes is None:
                    self.write_init(init)
                data = media
            fp = self.final_path(kind, k)
            with open(fp + '.tmp', 'wb') as f:
                f.write(data)
            os.replace(fp + '.tmp', fp)
            self.segrun[f'{kind}:{k}'] = r['id']
            os.remove(rp)
            r['next'] += 1
            self.hi[kind] = max(self.hi[kind], k)
        if killed:
            for fn in os.listdir(os.path.join(self.dir, kind, 'raw')):
                if fn.startswith(f'r{r["id"]}_'):
                    try: os.remove(os.path.join(self.dir, kind, 'raw', fn))
                    except OSError: pass

    def write_init(self, init_with_ftyp_moov):
        """strip edts (edit lists) so absolute tfdt are authoritative; keep ftyp+moov."""
        d = init_with_ftyp_moov
        out = b''
        for t, p, s, h in mp4util.boxes(d):
            if t == 'moov':
                out += self._strip_moov(d[p:p + s])
            else:
                out += d[p:p + s]
        self.init_bytes = out
        open(os.path.join(self.dir, 'init.mp4'), 'wb').write(out)

    def _strip_moov(self, moov):
        def rec(buf, depth):
            res = b''
            for t, p, s, h in mp4util.boxes(buf):
                if t == 'edts':
                    continue
                if t in ('trak', 'mdia'):
                    res += struct.pack('>I4s', 8 + len(rec(buf[p + h:p + s], depth + 1)), t.encode()) + rec(buf[p + h:p + s], depth + 1)
                else:
                    res += buf[p:p + s]
            return res
        body = rec(moov[8:], 0)
        return struct.pack('>I4s', 8 + len(body), b'moov') + body

    def get_init(self):
        if self.init_bytes is None:
            p = os.path.join(self.dir, 'init.mp4')
            if os.path.exists(p):
                self.init_bytes = open(p, 'rb').read()
            else:
                # quick throwaway run on first keyframe to get moov
                tmp = os.path.join(self.dir, 'init_probe.mp4')
                T = self.segs[0][0]
                c = self.inputs(T, 'error') + ['-y', '-copyts', '-avoid_negative_ts', 'disabled', '-to', f'{T + 1:.3f}', '-f', 'mp4', '-movflags',
                     '+frag_keyframe+frag_discont+empty_moov+default_base_moof+delay_moov', tmp]
                subprocess.run(c, creationflags=NOWIN, capture_output=True)
                with self.lock:
                    if self.init_bytes is None:
                        init, _ = mp4util.split(open(tmp, 'rb').read())
                        self.write_init(init)
                os.remove(tmp)
        return self.init_bytes

    def get_segment(self, kind, n, timeout=120):
        t0 = time.time()
        with self.lock:
            self.latest[kind] = n
        while True:
            if self.have(kind, n):
                return open(self.final_path(kind, n), 'rb').read()
            with self.lock:
                r = self.run[kind]
                if r:
                    self.finalize(kind, r)
                if self.have(kind, n):
                    continue
                inrun = r and r['start'] <= n < r['end'] and r['next'] <= n and \
                    (r['proc'].poll() is None or r['proc'].poll() == 0)
                if not inrun:
                    # seek or next chunk: (re)start at n
                    self.start_run(kind, n)
            if time.time() - t0 > timeout:
                raise TimeoutError(f'segment {n}')
            time.sleep(0.02)

    def scheduler_tick(self):
        """keep producing ahead (<= AHEAD_S) of latest request; kill nothing."""
        with self.lock:
            for kind in ('fmp4', 'ts'):
                r = self.run[kind]
                if r:
                    self.finalize(kind, r)
                    if r['proc'].poll() is not None and r['next'] >= r['end']:
                        self.run[kind] = None
                        r = None
                if r is None and self.hi[kind] >= 0:
                    nxt = self.hi[kind] + 1
                    lat = self.latest[kind]
                    if nxt < self.nseg and lat <= nxt and not self.have(kind, nxt) and \
                            self.segs[nxt][0] - self.segs[lat][0] < AHEAD_S - CHUNK_S:
                        self.start_run(kind, nxt)


SOURCES = {}


class H(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *a):
        pass

    def send(self, code, body, ctype):
        self.send_response(code)
        self.send_header('Content-Type', ctype)
        self.send_header('Content-Length', str(len(body)))
        self.send_header('Access-Control-Allow-Origin', '*')
        self.end_headers()
        self.wfile.write(body)

    def do_OPTIONS(self):
        self.send(204, b'', 'text/plain')

    def do_GET(self):
        t0 = time.time()
        path = self.path.split('?')[0]
        try:
            if path == '/_player.html':
                return self.send(200, open(os.path.join(os.path.dirname(os.path.abspath(__file__)), 'player.html'), 'rb').read(), 'text/html')
            if path.startswith('/_runs/'):
                return self.send(200, json.dumps(SOURCES[path[7:]].segrun).encode(), 'application/json')
            if path == '/_stats':
                return self.send(200, json.dumps(REQ_LOG).encode(), 'application/json')
            m = re.match(r'^/([^/]+)/(.*)$', path)
            src = SOURCES.get(m.group(1)) if m else None
            if not src:
                return self.send(404, b'nosrc', 'text/plain')
            rest = m.group(2)
            kind = 'fmp4'
            if rest.startswith('ts/'):
                kind, rest = 'ts', rest[3:]
            if rest == 'master.m3u8':
                body, ct = src.master().encode(), 'application/vnd.apple.mpegurl'
            elif rest == 'index.m3u8':
                body, ct = src.playlist(kind).encode(), 'application/vnd.apple.mpegurl'
            elif rest == 'init.mp4':
                body, ct = src.get_init(), 'video/mp4'
            else:
                mm = re.match(r'^seg(\d+)\.(m4s|ts)$', rest)
                if not mm or int(mm.group(1)) >= src.nseg:
                    return self.send(404, b'noseg', 'text/plain')
                body = src.get_segment(kind, int(mm.group(1)))
                ct = 'video/iso.segment' if kind == 'fmp4' else 'video/mp2t'
            self.send(200, body, ct)
            REQ_LOG.append(dict(path=path, t=t0, dur=round(time.time() - t0, 3), bytes=len(body)))
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as ex:
            try: self.send(500, repr(ex).encode(), 'text/plain')
            except Exception: pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--port', type=int, default=8777)
    ap.add_argument('--cache', default=os.path.join(os.path.dirname(os.path.abspath(__file__)), 'cache'))
    ap.add_argument('src', nargs='+')
    a = ap.parse_args()
    for s in a.src:
        n, p = s.split('=', 1)
        t0 = time.time()
        SOURCES[n] = Source(n, p, a.cache)
        print(f'{n}: {SOURCES[n].nseg} segs, index {SOURCES[n].index_time:.2f}s, {SOURCES[n].vcodec}/{SOURCES[n].acodec}', flush=True)

    def loop():
        while True:
            for s in SOURCES.values():
                try: s.scheduler_tick()
                except Exception as e: print('sched', e, flush=True)
            time.sleep(0.1)
    threading.Thread(target=loop, daemon=True).start()
    srv = ThreadingHTTPServer(('127.0.0.1', a.port), H)
    srv.daemon_threads = True
    print('listening', a.port, flush=True)
    srv.serve_forever()


if __name__ == '__main__':
    main()
