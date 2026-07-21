# shakenfist-spice-renderer

Shared SPICE rendering substrate for the
[ryll](https://github.com/shakenfist/ryll) SPICE client: the
software framebuffer, the per-channel message handlers
(display, inputs, cursor, and — with the `audio` feature —
playback), and the session orchestration that drives them.

Both front ends build on this crate rather than reimplementing
the channel logic: the GUI client and the WebRTC bridge
([shakenfist-spice-webrtc](https://github.com/shakenfist/ryll))
share the same rendering and session code through it.

## Source

Extracted from the
[ryll](https://github.com/shakenfist/ryll) SPICE client.
Internal consumers within the shakenfist project depend on this
crate via workspace paths.

## License

Apache-2.0
