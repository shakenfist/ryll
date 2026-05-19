# Libvirt / QEMU Settings for Best SPICE Performance with Ryll

Ryll is built to tolerate any reasonable SPICE server configuration, but the
guest configuration has a large effect on what the user perceives as
"display responsiveness." This document captures the settings we recommend
for QEMU guests fronted by SPICE, based on what we have learned from
dogfood sessions and from reading the upstream `spice-server` and
`qemu` source.

The recommendations below are written against a **libvirt domain XML**
because that is the most common deployment path; the equivalent direct
`qemu` command-line flags are noted in parentheses where helpful.

> **TL;DR.** Prefer `virtio-vga` over `qxl`. Set
> `image-compression=auto_lz`, drop the `*-wan-compression=always`
> overrides, and use `streaming-video=filter` rather than `all`. Install
> `spice-vdagent` in the guest. The rest of this document explains why.

## Video device

### Recommended: virtio-vga (or virtio-gpu-pci)

```xml
<video>
  <model type='virtio' heads='1' primary='yes'>
    <acceleration accel3d='no'/>
  </model>
</video>
```

`virtio-gpu` is the actively-maintained guest-side display path in modern
Linux kernels. It uses a normal virtio queue to push dirty rectangles
into the host, which gives the SPICE server clean small-region updates
to encode — exactly what the static-UI (terminal, text) path needs.

Trade-offs vs `qxl`:

- **No hardware-accelerated streaming video.** The SPICE server's
  "stream this region as MJPEG/H.264" path is tightly coupled to the
  QXL command ring. With `virtio-gpu`, video playback falls back to
  bitmap blits and is bandwidth-heavier.
- **3D acceleration is opt-in.** Set `accel3d='yes'` if you want
  virgl 3D forwarding; leave off for plain VDI workloads.

### Not recommended: qxl

```xml
<!-- avoid for new VMs -->
<video>
  <model type='qxl' ram='65536' vram='65536' vgamem='16384' heads='1' primary='yes'/>
</video>
```

The QXL display driver is in maintenance-only mode upstream — it still
works but is no longer where SPICE engineering investment is going.
We have seen QXL guests fall into pathological encoding patterns
(full-screen `ZlibGlzRgb` blasts for terminal cursor blinks) that
virtio-gpu does not exhibit. If you must use QXL (e.g. for the
streaming-video path), allocate generous `ram`/`vram` (64 MiB each in
the example above) and `vgamem` (16 MiB) so the driver does not page
its surfaces out from under SPICE's encoder.

### Cirrus, vmware-svga, bochs

Don't. Cirrus is 1990s hardware emulation; vmware-svga and bochs are
fallbacks for guests that lack better drivers. None of them offer the
SPICE-specific paths (dirty-rect queue, image cache, command ring) that
make the protocol worth using.

## SPICE channel settings

### Recommended graphics block

```xml
<graphics type='spice' port='-1' tls-port='-1' autoport='yes'
          listen='0.0.0.0'>
  <listen type='address' address='0.0.0.0'/>
  <image compression='auto_lz'/>
  <jpeg compression='auto'/>
  <zlib compression='auto'/>
  <playback compression='on'/>
  <streaming mode='filter'/>
</graphics>
```

Each of these maps to a `-spice` flag on the QEMU command line. The
defaults vary by distro, so spell them out explicitly.

### Why `image compression='auto_lz'`?

Default in many libvirt configurations is `auto_glz`. `glz` is a
dictionary-based zlib variant designed for static-UI surfaces — fine
in theory, but it produces large compressed payloads (~50% of raw
RGBA) and is slow to decode (~80 ms for a 2048×1152 frame on Apple
Silicon).

`auto_lz` switches the server's default to plain LZ, which is much
faster to decode (~10–20 ms for the same payload) at the cost of a
slightly worse compression ratio. Combined with ryll's advertised
`LZ4_COMPRESSION` capability (phase 2 of the stream-caps work), the
server will pick LZ4 — even faster — for any frames that benefit
from it.

If you actually need glz (e.g. you are bandwidth-constrained over a
WAN link), keep `auto_glz` but at least drop the
`zlib-glz-wan-compression=always` override mentioned below.

### Why drop `*-wan-compression=always`?

Older libvirt templates often include:

```xml
<image compression='auto_glz'/>
<jpeg compression='always'/>
<zlib compression='always'/>
```

The `always` setting forces the server's "wan" code path on every
image, regardless of what the client advertises. This is what causes
ryll's `LZ4_COMPRESSION` and (once phase 7 lands) `PREF_COMPRESSION`
hints to be silently ignored — the server has been told "I don't care
what the client wants, always use this."

Use `auto` instead so the server picks dynamically based on the
client's capabilities and the actual image characteristics.

### Why `streaming mode='filter'`?

The QEMU defaults are usually `streaming=off` or `streaming=all`. Both
extremes are wrong for typical desktop workloads:

- `off` — server never streams MJPEG/H.264, so video playback
  decompresses every frame as a static image. Murders bandwidth.
- `all` — server eagerly creates a stream for any region that changes
  more than a few times per second, including terminal cursor blinks
  and window-drag rubber-banding. Then tears the stream down 0.7 s
  later when the region stops moving, leaving the client to redraw
  the affected area as a static image. We have observed
  ten-streams-in-a-minute flap counts on `all`.
- `filter` — server uses a heuristic to decide which regions are
  actually video-like (high frame rate, large continuous area,
  smooth motion). Stable streams for real video, dirty-rect updates
  for everything else. This is the right answer for almost every
  workload.

As of ryll Phase 6, both MJPEG and H.264 streams are decoded client-side.
For sustained video playback with `streaming-video=filter`, the server will
prefer H.264 when available, which is typically more bandwidth-efficient than
MJPEG and results in cheaper sustained-video transmission.

### TLS channels

```xml
<channel name='main' mode='secure'/>
<channel name='display' mode='secure'/>
<channel name='inputs' mode='secure'/>
<channel name='cursor' mode='secure'/>
<channel name='playback' mode='secure'/>
<channel name='record' mode='secure'/>
<channel name='smartcard' mode='secure'/>
<channel name='usbredir' mode='secure'/>
```

Force-secure all SPICE channels. The 2010s-vintage `mode='any'`
default lets the client pick plaintext for "non-sensitive" channels
(display, inputs, etc.) which is the wrong threat model — anyone who
can sniff the display can shoulder-surf the session.

Ryll has no preference for plaintext: TLS is a fixed cost at link
time and a negligible per-frame cost after. Always use TLS unless
you are doing protocol bring-up against a debug server.

## VD agent (clipboard, mouse mode, monitors config)

```xml
<channel type='spicevmc'>
  <target type='virtio' name='com.redhat.spice.0'/>
</channel>
```

`spicevmc` over a `virtserialport` named `com.redhat.spice.0` is the
channel that carries `VD_AGENT_*` messages. Install `spice-vdagent`
in the guest (`apt install spice-vdagent` on Debian/Ubuntu;
`dnf install spice-vdagent` on Fedora). Without it:

- The mouse pointer is stuck in server-mode (relative coordinates),
  which breaks tablet-style absolute positioning that the SPICE
  client expects.
- Clipboard sync between client and guest is broken.
- Dynamic monitor reconfiguration (e.g. window resize triggers a
  guest-side resolution change) does not work.

Ryll's bug-report `MainSnapshot::agent_request_count` and related
fields (phase 9 of the stream-caps work, when it lands) report
whether the agent is responding to probes. A `0` agent reply count
in a bug report usually means `spice-vdagent` is not installed or
not running.

## USB redirection (optional)

```xml
<redirdev bus='usb' type='spicevmc'/>
<redirdev bus='usb' type='spicevmc'/>
<redirdev bus='usb' type='spicevmc'/>
<controller type='usb' index='0' model='ich9-ehci1'/>
<controller type='usb' index='0' model='ich9-uhci1'>
  <master startport='0'/>
</controller>
<controller type='usb' index='0' model='nec-xhci'/>
```

USB redirection works fine with ryll (we ship a usbredir channel
handler), but each `<redirdev>` adds a `usbredir` channel that has
to be set up at link time even if no device is attached. Three is the
historical libvirt default and is fine for most desktops. Set to one
if you only ever attach a single device; remove entirely if your
deployment policy forbids USB passthrough.

The xHCI (`nec-xhci`) controller is required for USB 3 devices. EHCI
(`ich9-ehci1`) plus the three UHCI companions cover USB 2.

## Audio (optional)

```xml
<audio id='1' type='spice'/>
<sound model='ich9'/>
```

`audio type='spice'` routes guest audio over the SPICE playback /
record channels. We ship handlers for both. If audio isn't needed in
your benchmark, omit both elements — they add channel-setup time and
playback decode load (~1 MiB/s on a typical media stream).

## CPU and memory sizing

These are not SPICE-specific, but they affect what the user perceives
through SPICE:

- **CPUs**: 4 vCPUs minimum for a desktop workload that includes a
  browser. 2 is enough for terminal-only testing. The guest's
  display server (X11 / Wayland) is single-threaded so adding cores
  beyond 8 rarely improves responsiveness.
- **RAM**: 4 GiB for terminal-only, 8 GiB for a desktop with a
  browser. The QXL display driver allocates from guest RAM for
  its image cache; under-provisioning it leads to thrashing that
  shows up as choppy redraws.
- **Disk**: virtio-blk with `cache='none'` and `io='native'` for
  benchmark VMs (predictable IO, no host page-cache effects).
  `cache='writeback'` is fine for daily-driver VMs where you care
  more about throughput than reproducibility.

## What the `-spice` flag looks like on the QEMU command line

If you are testing without libvirt, the equivalent QEMU flags for the
recommended block above:

```
-spice port=5930,tls-port=5931,addr=0.0.0.0,\
       disable-ticketing=on,\
       x509-dir=/etc/pki/libvirt-spice,\
       tls-channel=default,\
       image-compression=auto_lz,\
       jpeg-wan-compression=auto,\
       zlib-glz-wan-compression=auto,\
       playback-compression=on,\
       streaming-video=filter,\
       seamless-migration=on
```

(Substitute appropriate ports, x509 path, and ticketing policy for
your deployment.)

## Side-by-side testing recipe

To compare configurations cleanly:

1. Clone the VM (e.g. `virt-clone --original src --name dst --auto-clone`).
2. Apply one change to the cloned VM's XML (e.g. swap `qxl` for
   `virtio`, or change `image compression`).
3. Restart both VMs.
4. Connect with ryll to each in turn, run an identical workload
   (e.g. open a terminal, type for 60 seconds, then drag a window for
   60 seconds), and file a Display bug report at the end of each.
5. Compare the two bug reports' `recent_decodes`,
   `mjpeg_decode_recent_mean_us`, `decode_recent_mean_us`, and
   `streams_created_total` fields.

Useful single-change A/B pairs:

| A | B | What it tells you |
|---|---|---|
| `qxl` | `virtio` | Whether the QXL command ring is producing pathological full-frame updates. |
| `image compression='auto_glz'` | `image compression='auto_lz'` | Whether `ZlibGlzRgb` decode time is the dominant per-frame cost. |
| `streaming mode='all'` | `streaming mode='filter'` | Whether stream flapping is the dominant lag source. |
| `<jpeg compression='always'/>` | `<jpeg compression='auto'/>` | Whether the server is over-eagerly switching to MJPEG for non-video regions. |

The ryll bug-report fields make these comparisons cheap; the
`Display` report type in particular includes everything needed to
characterise an A/B difference.

**Alternative: Auto-snapshot mode for long-running tests** — Instead of
manually filing reports at specific points, use `ryll --auto-snapshot-interval 30`
to fire a bug-report zip every 30 seconds into a rolling cap throughout the test.
This eliminates the risk of missing a transient lag spike or flap event that
happens between manual report points. Compare metrics across snapshots before,
during, and after the test to see the full timeline of changes.

## What ryll cannot fix from the client side

The server makes the encoding decisions. Ryll can:

- Advertise capabilities (`LZ4_COMPRESSION`, `STREAM_REPORT`, etc.)
  so the server *can* use efficient paths.
- Send preference hints (`PREF_COMPRESSION`, `PREF_VIDEO_CODEC_TYPE`
  — phase 7 of the stream-caps work) to bias server choices.
- Decode whatever arrives as fast as the host hardware allows.

But ryll cannot override a server config that says
`zlib-glz-wan-compression=always`. If the server is hard-configured
to use a slow encoding, the client just has to decode it. The
right fix in that case is server-side: update the libvirt XML per
the recommendations above.
