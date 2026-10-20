# shakenfist-spice-renderer

Shared SPICE rendering substrate for the
[ryll](https://github.com/shakenfist/ryll) SPICE client: the
software framebuffer, the per-channel message handlers
(display, inputs, cursor, playback, and optionally usbredir
and webdav), and the session orchestration that drives them.

Both front ends build on this crate rather than reimplementing
the channel logic: the GUI client and the WebRTC bridge
([shakenfist-spice-webrtc](https://github.com/shakenfist/ryll))
share the same rendering and session code through it.

## Cargo features

All but `digest-decode` are on by default, so a plain dependency
builds the full client. A consumer that only needs display decode
(for example a server-role frontend) uses `default-features = false`
and names what it needs.

| Feature | Gates |
|---------|-------|
| `audio` | Local audio output (cpal, rtrb); the playback channel and Opus decode are always built |
| `usbredir` | The usbredir channel and USB device backends |
| `webdav` | The WebDAV folder-sharing channel and server |
| `encoder` | The live H.264 encoder (openh264, vendored C++) |
| `h264-decode` | H.264 stream decode |
| `mozjpeg` | SIMD JPEG decode (vendored libjpeg-turbo) |
| `digest-decode` | Visual-digest QR detection (off by default) |

## Source

Extracted from the
[ryll](https://github.com/shakenfist/ryll) SPICE client.
Internal consumers within the shakenfist project depend on this
crate via workspace paths.

## License

Apache-2.0
