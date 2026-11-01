#!/usr/bin/env python3
"""Check SPICE LZ4 images in a ryll display.pcap against liblz4 and QMP screendumps.

Independent of ryll's decoder: parses the raw server->client display stream from the
pcap, decodes each SPICE_IMAGE_TYPE_LZ4 image with liblz4 (python-lz4), and compares
the result with the screendump taken after that draw.

Usage: lz4_check.py <display.pcap> <capture-start-epoch> <qmp-log.txt> [--dump-dir DIR]
"""
import argparse
import collections
import json
import struct

import lz4.block

IMAGE_TYPE_LZ4 = 109
DRAW_COPY = 304
BPP = {6: 2, 7: 3, 8: 4, 9: 4}


def reassemble(path):
    """Return {(sport, dport): (bytearray, [(offset, rel_ts)])} from a ryll pcap."""
    with open(path, 'rb') as f:
        data = f.read()
    assert data[:4] == b'\xa1\xb2\xc3\xd4', 'not a big-endian pcap'
    streams = collections.defaultdict(lambda: (bytearray(), []))
    o = 24
    while o + 16 <= len(data):
        ts_sec, ts_usec, incl, _orig = struct.unpack('>IIII', data[o:o + 16])
        o += 16
        pkt = data[o:o + incl]
        o += incl
        sport, dport = struct.unpack('>HH', pkt[34:38])
        tcp_hdr_len = ((pkt[46] >> 4) & 0xf) * 4
        payload = pkt[14 + 20 + tcp_hdr_len:]
        if not payload:
            continue
        buf, idx = streams[(sport, dport)]
        idx.append((len(buf), ts_sec + ts_usec / 1e6))
        buf.extend(payload)
    return streams


def ts_at(idx, off):
    last = 0.0
    for seg_off, ts in idx:
        if seg_off > off:
            break
        last = ts
    return last


def iter_messages(buf):
    i = 0
    while i + 6 <= len(buf):
        mt, ms = struct.unpack('<HI', buf[i:i + 6])
        if i + 6 + ms > len(buf):
            return i, False
        yield i, mt, i + 6, i + 6 + ms
        i += 6 + ms
    return i, True


def parse_draw_copy(body):
    sid, top, left, bottom, right = struct.unpack('<IIIII', body[0:20])
    clip_type = body[20]
    o = 21
    clip_rects = []
    if clip_type == 1:
        (n,) = struct.unpack('<I', body[o:o + 4])
        o += 4
        for _ in range(n):
            clip_rects.append(struct.unpack('<IIII', body[o:o + 16]))
            o += 16
    src_off = struct.unpack('<I', body[o:o + 4])[0]
    s_top, s_left, s_bottom, s_right = struct.unpack('<IIII', body[o + 4:o + 20])
    rop = struct.unpack('<H', body[o + 20:o + 22])[0]
    scale = body[o + 22]
    mask_flags = body[o + 23]
    mask_x, mask_y, mask_off = struct.unpack('<iiI', body[o + 24:o + 36])
    return {
        'surface': sid, 'box': (left, top, right, bottom), 'clip_type': clip_type,
        'clip_rects': clip_rects, 'src_off': src_off, 'src': (s_left, s_top, s_right, s_bottom),
        'rop': rop, 'scale': scale, 'mask': (mask_flags, mask_x, mask_y, mask_off),
        'fixed_end': o + 36,
    }


def lz4_decode(body, width, height):
    """Decode a BinaryData LZ4 body (top-down byte, format byte, BE-length blocks).

    Returns (top_down, fmt, [block lengths], decoded bytes, problems)."""
    problems = []
    top_down, fmt = body[0], body[1]
    bpp = BPP.get(fmt)
    if bpp is None:
        return top_down, fmt, [], b'', [f'unknown format {fmt}']
    want = width * height * bpp
    out = bytearray()
    lengths = []
    o = 2
    while o < len(body):
        if o + 4 > len(body):
            problems.append(f'{len(body) - o} stray bytes at {o}')
            break
        (n,) = struct.unpack('>I', body[o:o + 4])
        o += 4
        if o + n > len(body):
            problems.append(f'block claims {n} bytes, {len(body) - o} remain')
            break
        lengths.append(n)
        block = bytes(body[o:o + n])
        o += n
        remaining = want - len(out)
        if remaining <= 0:
            problems.append('block after buffer already full')
            break
        dec = lz4.block.decompress(block, uncompressed_size=remaining, dict=bytes(out[-65536:]))
        out.extend(dec)
    return top_down, fmt, lengths, bytes(out), problems


def to_rgb_rows(dec, width, height, fmt, top_down):
    """Convert decoded pixels to a list of RGB rows, top row first."""
    bpp = BPP[fmt]
    stride = width * bpp
    rows = []
    for r in range(height):
        row = dec[r * stride:(r + 1) * stride]
        rgb = bytearray(width * 3)
        for x in range(width):
            if fmt in (8, 9):
                b, g, rr = row[x * 4], row[x * 4 + 1], row[x * 4 + 2]
            elif fmt == 7:
                b, g, rr = row[x * 3], row[x * 3 + 1], row[x * 3 + 2]
            else:
                (v,) = struct.unpack('<H', row[x * 2:x * 2 + 2])
                r5, g5, b5 = (v >> 10) & 31, (v >> 5) & 31, v & 31
                rr, g, b = (r5 << 3) | (r5 >> 2), (g5 << 3) | (g5 >> 2), (b5 << 3) | (b5 >> 2)
            rgb[x * 3:x * 3 + 3] = bytes((rr, g, b))
        rows.append(bytes(rgb))
    if not top_down:
        rows.reverse()
    return rows


def read_ppm(path):
    with open(path, 'rb') as f:
        data = f.read()
    parts = data.split(b'\n', 3)
    assert parts[0] == b'P6' and parts[2] == b'255'
    w, h = map(int, parts[1].split())
    pix = parts[3]
    assert len(pix) == w * h * 3
    return w, h, pix


def compare(rows, shot, box):
    sw, _sh, pix = shot
    left, top, right, bottom = box
    diff = 0
    total = 0
    for y in range(bottom - top):
        a = rows[y]
        o = ((top + y) * sw + left) * 3
        b = pix[o:o + (right - left) * 3]
        for x in range(right - left):
            total += 1
            if a[x * 3:x * 3 + 3] != b[x * 3:x * 3 + 3]:
                diff += 1
    return diff, total


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('pcap')
    ap.add_argument('start_epoch', type=float)
    ap.add_argument('qmp_log')
    ap.add_argument('--dump-dir')
    args = ap.parse_args()

    streams = reassemble(args.pcap)
    buf, idx = max(streams.values(), key=lambda p: len(p[0]))
    shots = []
    with open(args.qmp_log) as f:
        for line in f:
            t, kind, what = line.split(None, 2)
            if kind == 'screendump':
                shots.append((float(t), what.strip()))
    shots.sort()
    shot_cache = {}

    msgs = list(iter_messages(buf))
    end = msgs[-1][3] if msgs else 0
    print(f'server stream {len(buf)} bytes; {len(msgs)} messages; parsed to offset {end}'
          f' ({"exactly the end" if end == len(buf) else "TRAILING " + str(len(buf) - end)})')

    results = []
    for n, (off, mt, b0, b1) in enumerate(msgs):
        if mt != DRAW_COPY:
            continue
        body = bytes(buf[b0:b1])
        d = parse_draw_copy(body)
        so = d['src_off']
        iid, itype, iflags, iw, ih = struct.unpack('<QBBII', body[so:so + 18])
        if itype != IMAGE_TYPE_LZ4:
            continue
        (data_size,) = struct.unpack('<I', body[so + 18:so + 22])
        avail = len(body) - (so + 22)
        lz_body = body[so + 22:so + 22 + data_size]
        top_down, fmt, lengths, dec, problems = lz4_decode(lz_body, iw, ih)
        bpp = BPP.get(fmt, 0)
        want = iw * ih * bpp
        sum_ok = sum(lengths) + 4 * len(lengths) == data_size - 2
        wall = args.start_epoch + ts_at(idx, off)
        # The screendump to compare with: the first one taken after this draw arrived.
        after = [s for s in shots if s[0] > wall]
        cmp_txt = 'no later screendump'
        pix_diff = None
        if after and fmt in BPP and len(dec) == want:
            st, sp = after[0]
            if sp not in shot_cache:
                shot_cache[sp] = read_ppm(sp)
            rows = to_rgb_rows(dec, iw, ih, fmt, top_down)
            sl, stp, sr, sb = d['src']
            # Crop the image to src_area; it lands at box.
            crop = [r[sl * 3:sr * 3] for r in rows[stp:sb]]
            pix_diff, total = compare(crop, shot_cache[sp], d['box'])
            cmp_txt = f'vs {sp.rsplit("/", 1)[-1]} (+{st - wall:.3f}s): {pix_diff}/{total} px differ'
        r = {
            'msg': n, 'offset': off, 't_rel': ts_at(idx, off), 'wall': wall, 'id': iid,
            'flags': iflags, 'w': iw, 'h': ih, 'box': d['box'], 'src': d['src'],
            'clip_type': d['clip_type'], 'rop': d['rop'], 'scale': d['scale'], 'mask': d['mask'],
            'src_off': so, 'fixed_end': d['fixed_end'], 'data_size': data_size, 'avail': avail,
            'top_down': top_down, 'fmt': fmt, 'blocks': lengths, 'sum_ok': sum_ok,
            'decoded': len(dec), 'want': want, 'problems': problems, 'pix_diff': pix_diff,
        }
        results.append(r)
        print(f'msg {n:4d} t={r["t_rel"]:7.3f} id={iid:#x} flags={iflags} {iw}x{ih} box={d["box"]}'
              f' src={d["src"]} clip={d["clip_type"]} rop={d["rop"]} scale={d["scale"]}'
              f' mask={d["mask"]} src_off={so} fixed_end={d["fixed_end"]}')
        print(f'    data_size={data_size} avail={avail} ({"==" if data_size == avail else "!="})'
              f' top_down={top_down} fmt={fmt} blocks={len(lengths)} lengths={lengths}'
              f' sum+4n==data_size-2: {sum_ok} decoded={len(dec)} want={want}'
              f' ({"==" if len(dec) == want else "!="}) problems={problems}')
        print(f'    {cmp_txt}')
        if args.dump_dir:
            with open(f'{args.dump_dir}/img-{n:04d}.bin', 'wb') as f:
                f.write(lz_body)

    print()
    print(f'LZ4 images: {len(results)}')
    print('data_size == avail:', collections.Counter(r['data_size'] == r['avail'] for r in results))
    print('top_down:', collections.Counter(r['top_down'] for r in results))
    print('fmt:', collections.Counter(r['fmt'] for r in results))
    print('block counts:', collections.Counter(len(r['blocks']) for r in results))
    print('lengths sum ok:', collections.Counter(r['sum_ok'] for r in results))
    print('decoded == want:', collections.Counter(r['decoded'] == r['want'] for r in results))
    print('problems:', collections.Counter(bool(r['problems']) for r in results))
    print('pixel exact:', collections.Counter(r['pix_diff'] == 0 for r in results))
    print('mask offsets:', collections.Counter(r['mask'][3] for r in results))
    print('src == full image:', collections.Counter(
        r['src'] == (0, 0, r['w'], r['h']) for r in results))
    print('box size == image size:', collections.Counter(
        (r['box'][2] - r['box'][0], r['box'][3] - r['box'][1]) == (r['w'], r['h']) for r in results))
    print('src_off == fixed_end:', collections.Counter(r['src_off'] == r['fixed_end'] for r in results))
    if args.dump_dir:
        with open(f'{args.dump_dir}/results.json', 'w') as f:
            json.dump(results, f, indent=1)


if __name__ == '__main__':
    main()
