# shakenfist-spice-webrtc

WebRTC bridge for the
[ryll](https://github.com/shakenfist/ryll) SPICE client. It
carries SPICE-encoded video and audio over an
`RTCPeerConnection` and exposes a control datachannel for
inputs and the cursor overlay, so a SPICE session can be
reached from any modern browser — this is what backs ryll's
`--web` mode.

It builds on
[shakenfist-spice-renderer](https://github.com/shakenfist/ryll)
for the shared channel handling and session orchestration.

## Source

Extracted from the
[ryll](https://github.com/shakenfist/ryll) SPICE client.
Internal consumers within the shakenfist project depend on this
crate via workspace paths.

## License

Apache-2.0
