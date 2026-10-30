import struct


def boxes(buf, start=0, end=None):
    end = len(buf) if end is None else end
    p = start
    while p + 8 <= end:
        size, typ = struct.unpack('>I4s', buf[p:p + 8])
        hdr = 8
        if size == 1:
            size = struct.unpack('>Q', buf[p + 8:p + 16])[0]; hdr = 16
        elif size == 0:
            size = end - p
        yield typ.decode('latin1'), p, size, hdr
        p += size


def find(buf, path, start=0, end=None):
    """yield (pos,size,hdr) of boxes matching path list"""
    for t, p, s, h in boxes(buf, start, end):
        if t == path[0]:
            if len(path) == 1:
                yield p, s, h
            else:
                yield from find(buf, path[1:], p + h, p + s)


def split(data):
    """return (init_bytes(ftyp+moov), media_bytes(moof+mdat...))"""
    init = b''; media = b''
    for t, p, s, h in boxes(data):
        if t in ('ftyp', 'moov'):
            init += data[p:p + s]
        elif t in ('styp',):
            pass
        else:
            media += data[p:p + s]
    return init, media


def timescales(data):
    """track_id -> (timescale, handler)"""
    out = {}
    for mp, ms, mh in find(data, ['moov']):
        for tp, ts, th in find(data, ['trak'], mp + mh, mp + ms):
            tid = None; scale = None; hdlr = None
            for xp, xs, xh in find(data, ['tkhd'], tp + th, tp + ts):
                v = data[xp + xh]
                tid = struct.unpack('>I', data[xp + xh + (20 if v == 1 else 12):][:4])[0]
            for xp, xs, xh in find(data, ['mdia', 'mdhd'], tp + th, tp + ts):
                v = data[xp + xh]
                scale = struct.unpack('>I', data[xp + xh + (20 if v == 1 else 12):][:4])[0]
            for xp, xs, xh in find(data, ['mdia', 'hdlr'], tp + th, tp + ts):
                hdlr = data[xp + xh + 8:xp + xh + 12].decode('latin1')
            out[tid] = (scale, hdlr)
    return out


def tfdts(data):
    """list of (track_id, baseMediaDecodeTime) for each traf in data (media part ok)"""
    res = []
    for mp, ms, mh in find(data, ['moof']):
        for tp, ts, th in find(data, ['traf'], mp + mh, mp + ms):
            tid = None; bmdt = None
            for xp, xs, xh in find(data, ['tfhd'], tp + th, tp + ts):
                tid = struct.unpack('>I', data[xp + xh + 4:xp + xh + 8])[0]
            for xp, xs, xh in find(data, ['tfdt'], tp + th, tp + ts):
                v = data[xp + xh]
                bmdt = struct.unpack('>Q', data[xp + xh + 4:xp + xh + 12])[0] if v == 1 else struct.unpack('>I', data[xp + xh + 4:xp + xh + 8])[0]
            res.append((tid, bmdt))
    return res
