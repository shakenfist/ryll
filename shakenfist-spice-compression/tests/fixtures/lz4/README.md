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

Every synthetic file was decoded back by `make_fixtures.py` and
reproduces its formula exactly.

## Scripts

The fixtures were made and checked with three scripts in
[`tools/lz4-fixtures/`](../../../../tools/lz4-fixtures/), which use
liblz4 through the `lz4` package pinned in its `requirements.txt` and
never ryll's own decoder:

- [`qmp_drive.py`](../../../../tools/lz4-fixtures/qmp_drive.py) sends
  keys to the test guest over QMP and takes a screendump after each.
- [`lz4_check.py`](../../../../tools/lz4-fixtures/lz4_check.py) parses
  ryll's display pcap, decodes each LZ4 image with liblz4 and compares
  it with the screendump taken after the draw.
- [`make_fixtures.py`](../../../../tools/lz4-fixtures/make_fixtures.py)
  writes the captured fixtures, refusing any whose decode does not match
  its screendump, and the synthetic ones.

The captures, screendumps and run logs were kept outside the tree. The
commands were:

```
make build && make test-qemu
ryll --direct localhost:5900 --headless --verbose \
    --preferred-compression lz4 --capture <dir>/cap &
tools/lz4-fixtures/qmp_drive.py <dir> 1,2,3,4,5,6,7,8,9,0 3
kill -INT <ryll>
tools/lz4-fixtures/lz4_check.py <dir>/cap/display.pcap <capture-start-epoch> \
    <dir>/qmp-log.txt --dump-dir <dir>/imgs
tools/lz4-fixtures/make_fixtures.py <this dir> --run-dir <dir> \
    capture-32x11=88:shot-02.ppm capture-32x800=136:shot-03.ppm \
    capture-1280x800=3:shot-00.ppm
make test-qemu-stop
```

`<capture-start-epoch>` is the time of ryll's `capture: wrote
.../metadata.json` log line; the pcap timestamps are relative to it.

The synthetic files need no capture. `tools/lz4-fixtures/make_fixtures.py
<this dir>` alone rewrites them, and with the pinned `lz4` it reproduces
the committed files byte for byte.
