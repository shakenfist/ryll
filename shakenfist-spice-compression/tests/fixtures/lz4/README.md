# SPICE LZ4 image fixtures

Test inputs for the SPICE LZ4 image decoder (`SPICE_IMAGE_TYPE_LZ4`,
type 109). Every `.bin` file here is the body of the image's
`BinaryData`: the bytes *after* the little-endian u32 `data_size` that
follows the 18-byte image descriptor. That body is:

- byte 0: top-down flag, 0 or 1;
- byte 1: `SPICE_BITMAP_FMT_*` (6 = 16-bit x555, 7 = 24-bit B,G,R,
  8 = 32-bit B,G,R,X, 9 = RGBA as B,G,R,A);
- then one or more blocks, each a **big-endian** u32 length followed by
  one raw LZ4 block (no frame header, no checksum). Blocks are
  dependent: each may reference the output of the blocks before it, so
  they decode in order into one contiguous buffer (spice-common uses a
  single `LZ4_streamDecode_t`).

The decoded buffer is `height` rows of `width * bytes_per_pixel` bytes
(no padding) in memory order: the first row is the top row when byte 0
is 1 and the bottom row when it is 0. Width and height come from the
image descriptor, which is not in these files; they are listed below.

liblz4 1.9.4 through the Python `lz4` package 4.4.5, the library
spice-common's decoder uses, was the independent reference used to
make and check these files. The tests in `src/lz4.rs` now decode
every one of them with ryll's own decoder as well.

## Captured images

Captured from spice-server by ryll (`--preferred-compression lz4
--capture`) against `make test-qemu` (the UEFI latency guest,
`testdata/uefi-latency-guest.qcow2`, QXL, 1280x800). The guest repaints
its screen in a new colour on each keystroke; keys were sent over QMP
and a QMP `screendump` was taken three seconds after each.

| File | Width x height | Format | Top-down | Blocks | Destination box | Content |
|------|----------------|--------|----------|--------|-----------------|---------|
| `capture-32x11.bin` | 32 x 11 | 8 (32-bit) | 1 | 1 (208 bytes) | (224,109)-(256,120) | text fragment, 000000 / 009898 |
| `capture-32x800.bin` | 32 x 800 | 8 (32-bit) | 1 | 1 (607 bytes) | (224,0)-(256,800) | screen column with text, 000000 / 980000 |
| `capture-1280x800.bin` | 1280 x 800 | 8 (32-bit) | 1 | 1 (16264 bytes) | (0,0)-(1280,800) | full screen, 000000 / 000098 |

Each `<name>.rgba.lz4` holds the expected decode: the screendump's
pixels in the draw's destination box (`src_area` was the whole image in
every case) as R,G,B,A with A = 255, `width x height`, top row first,
compressed with `lz4.block.compress(rgba, store_size=True)` (a 4-byte
little-endian size, then one LZ4 block; lz4_flex's
`decompress_size_prepended` reads it). These come from the
**screendump**, not from a decode, and the liblz4 decode matched them
exactly. In the captured traffic the X byte of every pixel was 0.

`capture-32x11` is vertically symmetric, so it does not catch a
flipped row order; the other two do (a flipped decode differs in 88
pixels), and `capture-32x800` and `capture-1280x800` both catch a
swapped R and B.

What the capture showed, for every one of 683 LZ4 images over two
captures: `data_size` equalled the bytes remaining in the DRAW_COPY
message after it (no mask; mask bitmap offset 0); byte 0 was 1; byte 1
was 8; there was exactly one block; the block lengths plus 4 summed to
`data_size - 2`; the decode was exactly `width * height * 4` bytes; and
replaying every decoded draw onto a 1280x800 framebuffer reproduced all
18 screendumps (7 and 11 in the two captures) with no differing pixel. The test
guest never produced a bottom-up image, a multi-block image or a format
other than 8; the synthetic files below cover those.

- spice-server: `libspice-server1` 0.15.2-1+b1 (Debian)
- QEMU: 10.0.13 (Debian 1:10.0.13+ds-0+deb13u1)
- ryll: 82786ae, plus a local, uncommitted workaround for #399
  (`--capture` panics because `CaptureSession::new` spawns outside a
  tokio runtime)

## Synthetic streams

All 37 x 16, made with liblz4 (`lz4.block.compress(...,
store_size=False)`) from

```
r = (x * 7 + y) & 0xff
g = (y * 13) & 0xff
b = (x ^ y) & 0xff
a = (x + y * 3) & 0xff   (format 9 only; otherwise the expected A is 255)
```

stored as B,G,R,X with X = 0xAA (format 8, so a decoder that copies X
instead of forcing A = 255 fails), B,G,R (format 7), little-endian u16
`(r >> 3) << 10 | (g >> 3) << 5 | (b >> 3)` (format 6) or B,G,R,A
(format 9). For format 6 the expected value of each channel is the
5-bit value expanded as `(v << 3) | (v >> 2)`.

| File | Format | Top-down | Blocks | Notes |
|------|--------|----------|--------|-------|
| `synthetic-32-topdown-3blocks.bin` | 8 | 1 | 3 (744, 386, 462 bytes) | rows 0-4, 5-9, 10-15; dependent blocks, see below |
| `synthetic-32-bottomup.bin` | 8 | 0 | 1 (2379 bytes) | first stored row is y = 15 |
| `synthetic-24.bin` | 7 | 1 | 1 (1784 bytes) | |
| `synthetic-16.bin` | 6 | 1 | 1 (1189 bytes) | incompressible, so the block is longer than its 1184-byte output |
| `synthetic-rgba.bin` | 9 | 1 | 1 (2379 bytes) | |

`synthetic-32-topdown-3blocks.bin` uses a **modified formula**: for
`x >= 18` the formula is evaluated at row `y % 5` instead of `y` (pixels
with `x < 18` use the plain formula). With the plain formula no row
repeats a 4-byte run of an earlier row, so liblz4 never used the
dictionary and blocks 2 and 3 decoded fine on their own, which would
not test dependence. With the change, each block was compressed with
`dict=` all the input before it, and blocks 2 and 3 fail to decode
without that dictionary (liblz4 error 77) and decode correctly with it.

Every synthetic file was decoded back by `make_fixtures.py` below and
reproduces its formula exactly.

## Scripts

The captures, screendumps and run logs were kept outside the tree. The
commands were:

```
make build && make test-qemu
ryll --direct localhost:5900 --headless --verbose \
    --preferred-compression lz4 --capture <dir>/cap &
python qmp_drive.py <dir> 1,2,3,4,5,6,7,8,9,0 3
kill -INT <ryll>
python lz4_check.py <dir>/cap/display.pcap <capture-start-epoch> \
    <dir>/qmp-log.txt --dump-dir <dir>/imgs
python make_fixtures.py <scripts dir> <dir> <this dir> \
    capture-32x11=88:shot-02.ppm capture-32x800=136:shot-03.ppm \
    capture-1280x800=3:shot-00.ppm
make test-qemu-stop
```

`<capture-start-epoch>` is the time of ryll's `capture: wrote
.../metadata.json` log line; the pcap timestamps are relative to it.

### `lz4_check.py`

Parses the display pcap and checks and decodes each LZ4 image.

```python
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
```

### `make_fixtures.py`

Writes the captured and synthetic fixtures.

```python
"""Write the SPICE LZ4 test fixtures: captured images plus liblz4-made synthetic streams.

Usage: make_fixtures.py <scratch dir holding lz4_check.py> <run dir> <out dir> <name>=<msg>:<shot> ...

Each captured fixture <name>.bin is the BinaryData body (the bytes after data_size). Its
<name>.rgba.lz4 is the matching screendump region as RGBA (A=255), top row first, compressed
with lz4.block.compress(store_size=True). The script refuses to write a captured fixture whose
liblz4 decode does not match the screendump exactly.
"""
import json
import struct
import sys

import lz4.block

sys.path.insert(0, sys.argv[1])
import lz4_check as c  # noqa: E402

W, H = 37, 16
# For the three-block file only: pixels with x >= REPEAT_X take the formula at row y % 5, so the
# right part of every row in blocks 2 and 3 repeats bytes from an earlier block and liblz4 has
# to reference the dictionary. With the plain formula no row repeats a 4-byte run of an earlier
# row, so liblz4 never used the dictionary and the blocks decoded without it.
REPEAT_X = 18


def formula(x, y, repeat=False):
    if repeat and x >= REPEAT_X:
        y = y % 5
    return ((x * 7 + y) & 0xff, (y * 13) & 0xff, (x ^ y) & 0xff, (x + y * 3) & 0xff)


def pixel_bytes(fmt, x, y, repeat=False):
    r, g, b, a = formula(x, y, repeat)
    if fmt == 8:
        return bytes((b, g, r, 0xaa))
    if fmt == 9:
        return bytes((b, g, r, a))
    if fmt == 7:
        return bytes((b, g, r))
    v = ((r >> 3) << 10) | ((g >> 3) << 5) | (b >> 3)
    return struct.pack('<H', v)


def expected_rgba(fmt, x, y, repeat=False):
    r, g, b, a = formula(x, y, repeat)
    if fmt == 6:
        def ex(v):
            v >>= 3
            return (v << 3) | (v >> 2)
        return bytes((ex(r), ex(g), ex(b), 255))
    return bytes((r, g, b, a if fmt == 9 else 255))


def rows_bytes(fmt, ys, repeat=False):
    return [b''.join(pixel_bytes(fmt, x, y, repeat) for x in range(W)) for y in ys]


def build_stream(top_down, fmt, memory_rows, split):
    """memory_rows: encoded rows in memory order. split: rows per block. Dependent blocks."""
    out = bytearray((top_down, fmt))
    blocks = []
    prior = b''
    i = 0
    for n in split:
        chunk = b''.join(memory_rows[i:i + n])
        i += n
        if prior:
            comp = lz4.block.compress(chunk, store_size=False, dict=prior[-65536:])
        else:
            comp = lz4.block.compress(chunk, store_size=False)
        blocks.append((comp, chunk, prior))
        out += struct.pack('>I', len(comp)) + comp
        prior += chunk
    assert i == len(memory_rows)
    return bytes(out), blocks


def decode_rgba(body, width, height):
    """Independent decode to RGBA rows, top row first, using lz4_check.lz4_decode for the blocks."""
    top_down, fmt, lengths, dec, problems = c.lz4_decode(body, width, height)
    assert not problems, problems
    bpp = c.BPP[fmt]
    assert len(dec) == width * height * bpp, (len(dec), width * height * bpp)
    rows = []
    for r in range(height):
        row = dec[r * width * bpp:(r + 1) * width * bpp]
        out = bytearray()
        for x in range(width):
            p = row[x * bpp:(x + 1) * bpp]
            if fmt == 8:
                out += bytes((p[2], p[1], p[0], 255))
            elif fmt == 9:
                out += bytes((p[2], p[1], p[0], p[3]))
            elif fmt == 7:
                out += bytes((p[2], p[1], p[0], 255))
            else:
                (v,) = struct.unpack('<H', p)
                ch = [(v >> 10) & 31, (v >> 5) & 31, v & 31]
                out += bytes([(k << 3) | (k >> 2) for k in ch] + [255])
        rows.append(bytes(out))
    if not top_down:
        rows.reverse()
    return top_down, fmt, lengths, rows


def synthetic(outdir):
    made = []
    # 32-bit, top-down, three dependent blocks of 5, 5, 6 rows.
    rows = rows_bytes(8, range(H), repeat=True)
    body, blocks = build_stream(1, 8, rows, [5, 5, 6])
    for k, (comp, chunk, prior) in enumerate(blocks[1:], start=2):
        try:
            got = lz4.block.decompress(comp, uncompressed_size=len(chunk))
            dependent = got != chunk
            note = 'decoded without dict but WRONG' if dependent else 'decoded without dict CORRECTLY'
        except lz4.block.LZ4BlockError as e:
            dependent = True
            note = f'fails without dict: {e}'
        with_dict = lz4.block.decompress(comp, uncompressed_size=len(chunk), dict=prior[-65536:])
        print(f'  3blocks block {k}: {len(comp)} bytes; {note}; with dict correct: {with_dict == chunk}')
        assert dependent, 'block does not depend on the dictionary'
    made.append(('synthetic-32-topdown-3blocks.bin', body, 8, True))
    # 32-bit, bottom-up: memory order is bottom row first.
    rows = rows_bytes(8, reversed(range(H)))
    body, _ = build_stream(0, 8, rows, [H])
    made.append(('synthetic-32-bottomup.bin', body, 8, False))
    for name, fmt in (('synthetic-24.bin', 7), ('synthetic-16.bin', 6), ('synthetic-rgba.bin', 9)):
        body, _ = build_stream(1, fmt, rows_bytes(fmt, range(H)), [H])
        made.append((name, body, fmt, False))
    for name, body, fmt, repeat in made:
        top_down, f, lengths, rgba_rows = decode_rgba(body, W, H)
        exp = [b''.join(expected_rgba(fmt, x, y, repeat) for x in range(W)) for y in range(H)]
        ok = rgba_rows == exp
        print(f'  {name}: {len(body)} bytes top_down={top_down} fmt={f} blocks={lengths} '
              f'decode reproduces formula: {ok}')
        assert ok
        if fmt == 8:
            assert all(b == 0xaa for b in c.lz4_decode(body, W, H)[3][3::4]), 'X byte not 0xaa'
        with open(f'{outdir}/{name}', 'wb') as fh:
            fh.write(body)


def captured(run, outdir, specs):
    res = {r['msg']: r for r in json.load(open(f'{run}/imgs/results.json'))}
    for spec in specs:
        name, rest = spec.split('=')
        msg, shot_name = rest.split(':')
        r = res[int(msg)]
        body = open(f'{run}/imgs/img-{int(msg):04d}.bin', 'rb').read()
        assert len(body) == r['data_size']
        assert r['src'] == (0, 0, r['w'], r['h']) or r['src'] == [0, 0, r['w'], r['h']]
        top_down, fmt, lengths, rows = decode_rgba(body, r['w'], r['h'])
        sw, _sh, pix = c.read_ppm(f'{run}/{shot_name}')
        left, top, right, bottom = r['box']
        assert (right - left, bottom - top) == (r['w'], r['h'])
        shot_rows = []
        for y in range(top, bottom):
            seg = pix[(y * sw + left) * 3:(y * sw + right) * 3]
            shot_rows.append(b''.join(seg[i:i + 3] + b'\xff' for i in range(0, len(seg), 3)))
        match = shot_rows == rows
        print(f'  {name}: msg {msg} {r["w"]}x{r["h"]} top_down={top_down} fmt={fmt} blocks={lengths} '
              f'box={r["box"]} vs {shot_name}: exact={match}')
        assert match, 'decode does not match screendump'
        rgba = b''.join(shot_rows)
        comp = lz4.block.compress(rgba, store_size=True)
        assert lz4.block.decompress(comp) == rgba
        with open(f'{outdir}/{name}.bin', 'wb') as fh:
            fh.write(body)
        with open(f'{outdir}/{name}.rgba.lz4', 'wb') as fh:
            fh.write(comp)
        print(f'    {name}.bin {len(body)} bytes, {name}.rgba.lz4 {len(comp)} bytes (from screendump)')


def main():
    run, outdir, specs = sys.argv[2], sys.argv[3], sys.argv[4:]
    print('captured:')
    captured(run, outdir, specs)
    print('synthetic:')
    synthetic(outdir)


if __name__ == '__main__':
    main()
```

### `qmp_drive.py`

Sends keys and takes screendumps over QMP.

```python
"""Drive the test guest over QMP: send keys, screendump after each settles, log wall-clock times."""
import json
import socket
import sys
import time

SOCK = '/tmp/ryll-test-qemu-qmp.sock'


class Qmp:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(path)
        self.f = self.s.makefile('rwb')
        self._read()
        self.cmd('qmp_capabilities')

    def _read(self):
        while True:
            line = self.f.readline()
            msg = json.loads(line)
            if 'event' in msg:
                continue
            return msg

    def cmd(self, name, **args):
        req = {'execute': name}
        if args:
            req['arguments'] = args
        self.f.write(json.dumps(req).encode() + b'\n')
        self.f.flush()
        r = self._read()
        if 'error' in r:
            raise RuntimeError(r)
        return r


def main():
    outdir = sys.argv[1]
    keys = sys.argv[2].split(',')
    settle = float(sys.argv[3]) if len(sys.argv) > 3 else 3.0
    q = Qmp(SOCK)
    log = open(f'{outdir}/qmp-log.txt', 'a')
    # Initial screendump of whatever is on screen.
    path = f'{outdir}/shot-00.ppm'
    t = time.time()
    q.cmd('screendump', filename=path)
    log.write(f'{t:.6f} screendump {path}\n')
    for i, k in enumerate(keys, start=1):
        t = time.time()
        q.cmd('send-key', keys=[{'type': 'qcode', 'data': k}])
        log.write(f'{t:.6f} send-key {k}\n')
        log.flush()
        time.sleep(settle)
        path = f'{outdir}/shot-{i:02d}.ppm'
        t = time.time()
        q.cmd('screendump', filename=path)
        log.write(f'{t:.6f} screendump {path}\n')
        log.flush()
    log.close()


if __name__ == '__main__':
    main()
```
