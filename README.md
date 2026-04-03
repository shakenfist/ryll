# Ryll - A Rust SPICE VDI Test Client

Ryll is a Rust implementation of a SPICE (Simple Protocol for Independent Computing Environments) client, designed for testing the Kerbside SPICE proxy. It provides both a GUI mode using egui and a headless mode for automated testing.

## Features

- **Immediate mode rendering** - Uses egui for efficient display rendering without accumulating objects
- **Image decompression** - LZ, GLZ, ZLIB_GLZ_RGB, LZ4, JPEG, and Pixmap image types
- **Multi-channel support** - Handles main, display, cursor, inputs, and usbredir channels
- **USB device redirection** - Forward physical USB devices or present RAW disk images as virtual USB mass storage devices via `--usb-disk`
- **TLS support** - Secure connections with inline CA certificates from .vv files
- **Cursor rendering** - Server cursor shapes with fallback default arrow
- **Headless mode** - Run without GUI for automated testing and benchmarking
- **Cadence mode** - Automatic keystroke injection every 2 seconds for latency testing
- **Display channel capabilities** - Advertises COMPOSITE, MONITORS_CONFIG, SIZED_STREAM, and A8_SURFACE so the guest QXL driver uses efficient rendering paths instead of falling back to slow software blits
- **Statistics tracking** - Sliding-window FPS (from MARK boundaries), throughput, and latency measurements
- **Bandwidth sparkline** - Real-time bandwidth graph in the status bar showing rolling bytes/sec history
- **File logging** - Verbose mode writes to `/tmp/ryll.log` for debugging
- **Graceful Ctrl+C shutdown** - Cross-platform signal handling via `ctrlc` crate; the GUI and headless event loops check a flag and shut down cleanly, ensuring capture files are finalized
- **Unbuffered pcap I/O** - Packet writes go directly to disk so pcap data survives abrupt termination
- **Traffic ring buffer** - Always-active per-channel ring buffer (50 MB total) retaining recent protocol traffic for bug reports

## Installation

Pre-built `.deb` packages for Debian/Ubuntu are available from
[GitHub Releases](https://github.com/shakenfist/ryll/releases). See
[docs/installation.md](docs/installation.md) for all platforms.

## CI

GitHub Actions CI builds and tests ryll on Linux, macOS (Apple Silicon),
and Windows on every push to `develop` and on pull requests. The workflow
is at `.github/workflows/ci.yml`.

## Building

### Using the devcontainer (recommended)

The project includes a devcontainer for consistent builds:

```bash
# Build debug version
make build

# Build release version
make release

# Run tests
make test

# Run linting (rustfmt + clippy)
make lint

# Run linting with auto-fix
make lint-fix

# Start a test QEMU SPICE server (UEFI latency guest, downloads on first run)
make test-qemu

# Stop the test QEMU instance
make test-qemu-stop
```

### Pre-commit hooks

The project uses pre-commit hooks to enforce code quality:

```bash
# Install pre-commit hooks
pre-commit install

# Run checks manually on all files
pre-commit run --all-files

# Or use the script directly
./scripts/check-rust.sh check   # Check mode
./scripts/check-rust.sh fix     # Auto-fix mode
```

The pre-commit hooks run:
- **rustfmt** - Code formatting
- **clippy** - Linting with warnings as errors
- **shellcheck** - Shell script linting

### Using local Rust installation

If you have Rust installed locally with the required dependencies:

```bash
cargo build --release
```

**Required system dependencies** (Debian/Ubuntu):
```bash
apt-get install -y \
    libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxcb1-dev \
    libx11-dev libxkbcommon-dev libgl1-mesa-dev libegl1-mesa-dev \
    libwayland-dev libssl-dev pkg-config
```

## Usage

### Connect using a .vv configuration file

```bash
# From URL
ryll --url http://example.com/vm.vv

# From local file
ryll --file /path/to/connection.vv
```

### Direct connection

```bash
# Insecure connection
ryll --direct 192.168.1.100:5900

# With TLS port
ryll --direct 192.168.1.100:5900:5901
```

### Options

```
Options:
  --url <URL>            URL to fetch .vv configuration file from
  --file <PATH>          Path to local .vv configuration file
  --direct <HOST:PORT>   Direct connection string
  --headless             Run in headless mode (no GUI)
  --cadence              Enable cadence mode (automatic keystroke every 2 seconds)
  -v, --verbose          Enable verbose logging
  --capture <DIR>        Write pcap + video capture to directory
  --latency-file <PATH>  Path to write latency measurements
  -h, --help             Print help
  -V, --version          Print version
```

### Capture mode

Record protocol traffic and display frames for debugging:

```bash
ryll --file connection.vv --capture /tmp/capture
```

This writes:
- `metadata.json` — session context (ryll version, platform, target host)
  for self-describing capture directories in bug reports
- `main.pcap`, `display.pcap`, `cursor.pcap`, `inputs.pcap`, `usbredir.pcap` —
  per-channel pcap files with fake TCP/IP headers, openable in Wireshark
- `display.mp4` — H.264 video of the display surface at real timing

See [STYLEGUIDE.md](STYLEGUIDE.md) for capture conventions.

### Headless mode

For automated testing without a GUI:

```bash
ryll --file connection.vv --headless --cadence
```

This will:
- Connect to the SPICE server
- Process display updates (decompress images)
- Send automatic keystrokes every 2 seconds
- Print statistics periodically

## Architecture

```
src/
├── main.rs              # CLI entry point, Ctrl+C handler
├── app.rs               # egui App, bandwidth sparkline
├── bugreport.rs         # Traffic ring buffer for bug reports
├── capture.rs           # Pcap + MP4 capture session
├── config.rs            # Configuration parsing
├── protocol/
│   ├── constants.rs     # SPICE protocol constants, capabilities
│   ├── messages.rs      # Binary message structures
│   ├── link.rs          # Handshake, auth, capability negotiation
│   └── client.rs        # Connection management
├── channels/
│   ├── main_channel.rs  # Session management
│   ├── display.rs       # Display rendering, GLZ dictionary
│   ├── cursor.rs        # Cursor tracking
│   ├── inputs.rs        # Keyboard/mouse input
│   └── usbredir.rs      # USB redirection (SpiceVMC transport)
├── usbredir/
│   ├── constants.rs     # usbredir message types, capabilities
│   ├── messages.rs      # Wire format structs, read/write
│   └── parser.rs        # Byte-stream parser
├── usb/
│   ├── mod.rs           # UsbDeviceBackend trait, device enumeration
│   ├── real.rs          # Physical USB device backend (nusb)
│   └── virtual_msc.rs   # Virtual mass storage (RAW disk images)
├── decompression/
│   ├── glz.rs           # GLZ decompression
│   └── lz.rs            # LZ decompression
└── display/
    └── surface.rs       # Surface buffer management
```

## Dependencies

- **eframe/egui** - Immediate mode GUI
- **tokio** - Async runtime
- **tokio-rustls** - TLS support
- **clap** - CLI parsing
- **rsa/sha1** - Authentication encryption
- **image** - JPEG decoding (via the `image` crate with jpeg feature)
- **nusb** - USB device access (pure Rust, no libusb)
- **ctrlc** - Cross-platform Ctrl+C handling for graceful shutdown

## Comparison with Python version

| Feature | Python (ryll) | Rust (ryll) |
|---------|--------------|-------------|
| Threading | Python threads + queues | Tokio async + channels |
| GUI | Tkinter (retained mode) | egui (immediate mode) |
| Image handling | Pillow | Direct pixel buffers |
| Memory efficiency | Object accumulation issues | No accumulation |
| Deployment | Requires Python env | Single binary |

## Documentation

Additional documentation is available:

- [ARCHITECTURE.md](ARCHITECTURE.md) - Technical design and data flow
- [AGENTS.md](AGENTS.md) - Guide for AI coding assistants
- [STYLEGUIDE.md](STYLEGUIDE.md) - Code conventions and patterns

In the `docs/` directory:

- [Documentation Index](docs/index.md) - What ryll is and why it exists
- [Installation](docs/installation.md) - Pre-built packages and install instructions
- [Configuration](docs/configuration.md) - CLI options and .vv file format
- [Troubleshooting](docs/troubleshooting.md) - Common issues and debugging
- [Binary Portability](docs/portability.md) - How to share binaries between machines
- [Releasing](docs/releasing.md) - How to publish a new release

## License

Apache-2.0
