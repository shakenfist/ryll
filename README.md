# Ryll - A Rust SPICE VDI Client

Ryll is a Rust implementation of a SPICE (Simple Protocol for Independent
Computing Environments) client. SPICE is one of the two virtual desktop
(VDI) protocols supported by qemu / KVM on Linux, so a SPICE client will
mostly be of interest to people looking for ways to access Linux virtual
machines running graphical environments. Ryll began life as a test
client for the [Kerbside](https://github.com/shakenfist/kerbside) SPICE
proxy, and it retains deep instrumentation from that heritage, but it is
now a client for everyday use as well.

Ryll is intended to be a **multi-modal SPICE client**: every delivery
mode is a first-class citizen and shares as much functionality as the
mode itself can physically support. The supported modes today are a
**GUI** (egui / eframe desktop window) for interactive day-to-day use, a
**headless** mode for automated testing, CI, and cadence latency
probing, and a **web** mode (browser frontend over WebRTC with native
TLS) that lets any modern browser connect to a SPICE session without
installing software.

## Highlights

- Broad SPICE protocol coverage: display, cursor, inputs, audio
  playback, USB redirection, and WebDAV folder sharing channels, with
  LZ / GLZ / LZ4 / JPEG / QUIC image decompression and hardware-accelerated
  MJPEG streaming.
- Built for automation and testing: headless and cadence modes, latency and bandwidth
  instrumentation, per-channel pcap capture, and a Unix-socket control
  interface for driving sessions from external tools.
- Deep diagnostics: one-keystroke bug-report zips, a live protocol
  traffic viewer, protocol-gap tracking, and a flight-data-recorder
  auto-snapshot mode.
- Cross-platform: Linux, macOS (Apple Silicon), and Windows, as a
  single static binary.

See [docs/features.md](https://github.com/shakenfist/ryll/blob/develop/docs/features.md)
for the full feature catalogue.

## Installation

Pre-built packages are available from
[GitHub Releases](https://github.com/shakenfist/ryll/releases):

```bash
# Debian / Ubuntu
sudo dpkg -i ryll_0.1.0-1_amd64.deb

# macOS (Apple Silicon)
brew install shakenfist/tap/ryll

# Linux via pip (bundled manylinux binary)
pip install ryll
```

RPM and Windows packages are also published. See
[docs/installation.md](https://github.com/shakenfist/ryll/blob/develop/docs/installation.md)
for all platforms, runtime dependencies, and building from source.

## Usage

```bash
# Connect using a .vv configuration file, from a URL or local file
ryll --url http://example.com/vm.vv
ryll --file /path/to/connection.vv

# Direct connection (host:port, or host:port:tls-port)
ryll --direct 192.168.1.100:5900

# Headless latency probe
ryll --file connection.vv --headless --cadence

# Serve the session to a web browser
ryll --web connection.vv
```

See [docs/configuration.md](https://github.com/shakenfist/ryll/blob/develop/docs/configuration.md)
for all command-line options and the .vv file format.

## Documentation

In the [docs/](https://github.com/shakenfist/ryll/blob/develop/docs/index.md)
directory:

- [Documentation Index](https://github.com/shakenfist/ryll/blob/develop/docs/index.md) - What ryll is and why it exists
- [Features](https://github.com/shakenfist/ryll/blob/develop/docs/features.md) - The detailed feature catalogue and mode guides
- [Installation](https://github.com/shakenfist/ryll/blob/develop/docs/installation.md) - Pre-built packages and install instructions
- [Configuration](https://github.com/shakenfist/ryll/blob/develop/docs/configuration.md) - CLI options and .vv file format
- [Web frontend guide](https://github.com/shakenfist/ryll/blob/develop/docs/web-frontend.md) - Operator guide for `--web` mode
- [Development](https://github.com/shakenfist/ryll/blob/develop/docs/development.md) - Building, testing, CI, and contributing
- [macOS Development](https://github.com/shakenfist/ryll/blob/develop/docs/development-macos.md) - Build and test locally on macOS
- [Troubleshooting](https://github.com/shakenfist/ryll/blob/develop/docs/troubleshooting.md) - Common issues and debugging
- [Releasing](https://github.com/shakenfist/ryll/blob/develop/docs/releasing.md) - How to publish a new release

Project reference files:

- [ARCHITECTURE.md](https://github.com/shakenfist/ryll/blob/develop/ARCHITECTURE.md) - Technical design and data flow
- [AGENTS.md](https://github.com/shakenfist/ryll/blob/develop/AGENTS.md) - Guide for AI coding assistants
- [STYLEGUIDE.md](https://github.com/shakenfist/ryll/blob/develop/STYLEGUIDE.md) - Code conventions and patterns

## License

Apache-2.0
