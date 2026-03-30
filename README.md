# Ryll - A Rust SPICE VDI Test Client

Ryll is a Rust implementation of a SPICE (Simple Protocol for Independent Computing Environments) client, designed for testing the Kerbside SPICE proxy. It provides both a GUI mode using egui and a headless mode for automated testing.

## Features

- **Immediate mode rendering** - Uses egui for efficient display rendering without accumulating objects
- **GLZ and LZ decompression** - Full support for SPICE image compression formats
- **Multi-channel support** - Handles main, display, cursor, and inputs channels
- **TLS support** - Secure connections with certificate validation
- **Headless mode** - Run without GUI for automated testing and benchmarking
- **Cadence mode** - Automatic keystroke injection every 2 seconds for latency testing
- **Statistics tracking** - Frame counts, throughput, and latency measurements

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
  --latency-file <PATH>  Path to write latency measurements
  -h, --help             Print help
  -V, --version          Print version
```

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
├── main.rs              # CLI entry point
├── app.rs               # egui App implementation
├── config.rs            # Configuration parsing
├── protocol/
│   ├── constants.rs     # SPICE protocol constants
│   ├── messages.rs      # Binary message structures
│   ├── link.rs          # Handshake and authentication
│   └── client.rs        # Connection management
├── channels/
│   ├── main_channel.rs  # Session management
│   ├── display.rs       # Display rendering
│   ├── cursor.rs        # Cursor tracking
│   └── inputs.rs        # Keyboard/mouse input
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
- [Configuration](docs/configuration.md) - CLI options and .vv file format
- [Troubleshooting](docs/troubleshooting.md) - Common issues and debugging
- [Binary Portability](docs/portability.md) - How to share binaries between machines

## License

Apache-2.0
