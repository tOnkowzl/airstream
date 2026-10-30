"""Keyframe index for a media file. Methods: cues (Matroska Cues parse), ffprobe_skip (a), ffprobe_pkt (b)."""
import struct, subprocess, sys, time, json

NOWIN = 0x08000000


def _vint(f, keep_marker=False):
    b = f.read(1)
    if not b:
        return None, 0
    first = b[0]
    ln = 1
    mask = 0x80
    while ln <= 8 and not (first & mask):
        ln += 1
        mask >>= 1
    if ln > 8:
        raise ValueError('bad vint')
    val = first if keep_marker else (first & (mask - 1))
    if ln > 1:
        rest = f.read(ln - 1)
        for c in rest:
            val = (val << 8) | c
    return val, ln


def _elem(f):
    pos = f.tell()
    eid, l1 = _vint(f, True)
    if eid is None:
        return None
    size, l2 = _vint(f)
    unknown = size == (1 << (7 * l2)) - 1
    return eid, (None if unknown else size), pos, pos + l1 + l2


def _children(f, start, end):
    f.seek(start)
    while f.tell() < end:
        e = _elem(f)
        if e is None:
            return
        eid, size, pos, data = e
        yield eid, size, pos, data
        if size is None:
            return
        f.seek(data + size)


def _uint(f, data, size):
    f.seek(data)
    return int.from_bytes(f.read(size), 'big')


def cues_index(path):
    """Return (timescale_s_per_tick, video_track, [cue times seconds], duration). Raises if no Cues."""
    with open(path, 'rb') as f:
        e = _elem(f)
        f.seek(e[3] + e[1])  # skip EBML header
        seg = _elem(f)
        seg_data = seg[3]
        seg_end = seg[3] + seg[1] if seg[1] is not None else float('inf')
        timescale = 1000000
        duration = None
        vtrack = None
        cues_pos = None
        seekhead_cues = None
        # scan top-level children until Cues found (skip clusters by size)
        for eid, size, pos, data in _children(f, seg_data, seg_end):
            if eid == 0x114D9B74:  # SeekHead
                for cid, csize, cpos, cdata in _children(f, data, data + size):
                    if cid == 0x4DBB:  # Seek
                        sid = None; sp = None
                        for xid, xsize, xpos, xdata in _children(f, cdata, cdata + csize):
                            if xid == 0x53AB:
                                f.seek(xdata); sid = int.from_bytes(f.read(xsize), 'big')
                            elif xid == 0x53AC:
                                sp = _uint(f, xdata, xsize)
                        if sid == 0x1C53BB6B:
                            seekhead_cues = seg_data + sp
            elif eid == 0x1549A966:  # Info
                for cid, csize, cpos, cdata in _children(f, data, data + size):
                    if cid == 0x2AD7B1:
                        timescale = _uint(f, cdata, csize)
                    elif cid == 0x4489:
                        f.seek(cdata)
                        duration = struct.unpack('>d' if csize == 8 else '>f', f.read(csize))[0]
            elif eid == 0x1654AE6B:  # Tracks
                for cid, csize, cpos, cdata in _children(f, data, data + size):
                    if cid == 0xAE:
                        num = None; typ = None
                        for xid, xsize, xpos, xdata in _children(f, cdata, cdata + csize):
                            if xid == 0xD7: num = _uint(f, xdata, xsize)
                            elif xid == 0x83: typ = _uint(f, xdata, xsize)
                        if typ == 1 and vtrack is None:
                            vtrack = num
            elif eid == 0x1C53BB6B:
                cues_pos = pos
                break
            if size is None:
                break
            if seekhead_cues and vtrack is not None and cues_pos is None and pos > 0 and eid == 0x1F43B675:
                break  # reached first cluster, jump to Cues via SeekHead
        if cues_pos is None:
            cues_pos = seekhead_cues
        if cues_pos is None:
            raise RuntimeError('no cues')
        f.seek(cues_pos)
        ce = _elem(f)
        if ce[0] != 0x1C53BB6B:
            raise RuntimeError('cues pos wrong')
        times = []
        for cid, csize, cpos, cdata in _children(f, ce[3], ce[3] + ce[1]):
            if cid != 0xBB:
                continue
            t = None; tracks = set()
            for xid, xsize, xpos, xdata in _children(f, cdata, cdata + csize):
                if xid == 0xB3:
                    t = _uint(f, xdata, xsize)
                elif xid == 0xB7:
                    for yid, ysize, ypos, ydata in _children(f, xdata, xdata + xsize):
                        if yid == 0xF7:
                            tracks.add(_uint(f, ydata, ysize))
            if t is not None and vtrack in tracks:
                times.append(t * timescale / 1e9)
        dur = duration * timescale / 1e9 if duration else None
        return times, dur


def _run(args):
    return subprocess.run(args, capture_output=True, text=True, encoding='utf-8', errors='replace', creationflags=NOWIN).stdout


def ffprobe_skip(path):
    out = _run(['ffprobe', '-v', 'error', '-select_streams', 'v:0', '-skip_frame', 'nokey',
                '-show_entries', 'frame=pts_time', '-of', 'csv=p=0', path])
    return sorted(float(x.strip(',')) for x in out.split() if x.strip(',') and x.strip(',') != 'N/A')


def ffprobe_pkt(path):
    out = _run(['ffprobe', '-v', 'error', '-select_streams', 'v:0',
                '-show_entries', 'packet=pts_time,flags', '-of', 'csv=p=0', path])
    ts = []
    for line in out.splitlines():
        p = line.strip().split(',')
        if len(p) >= 2 and 'K' in p[1] and p[0] != 'N/A':
            ts.append(float(p[0]))
    return sorted(ts)


def keyframes(path, fallback=True):
    try:
        t, dur = cues_index(path)
        if t:
            return t, 'cues'
    except Exception as e:
        print('cues failed:', e, file=sys.stderr)
    return ffprobe_skip(path), 'ffprobe'


if __name__ == '__main__':
    path = sys.argv[1]
    res = {}
    for name, fn in (('cues', lambda p: cues_index(p)[0]), ('skip', ffprobe_skip), ('pkt', ffprobe_pkt)):
        if name != 'cues' and len(sys.argv) > 2 and sys.argv[2] == 'cuesonly':
            continue
        t0 = time.time()
        r = fn(path)
        dt = time.time() - t0
        res[name] = r
        gaps = [b - a for a, b in zip(r, r[1:])]
        print(f'{name}: n={len(r)} wall={dt:.2f}s first={r[0]:.3f} last={r[-1]:.3f} maxgap={max(gaps):.3f}', flush=True)
    if 'skip' in res:
        for a, b in (('cues', 'skip'), ('cues', 'pkt'), ('skip', 'pkt')):
            A, B = res[a], res[b]
            same = len(A) == len(B) and all(abs(x - y) < 0.002 for x, y in zip(A, B))
            print(f'agree {a}~{b}: {same} (n {len(A)} vs {len(B)}) subset={set(round(x,3) for x in A) <= set(round(x,3) for x in B)}')
