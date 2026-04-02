# Installation

Pre-built packages are available from the
[GitHub Releases](https://github.com/shakenfist/ryll/releases) page
and as CI artifacts on pull requests.

## Debian / Ubuntu

Download the `.deb` package for your architecture and install:

```bash
sudo dpkg -i ryll_0.1.0-1_amd64.deb
sudo apt-get install -f   # install any missing dependencies
```

The package installs `ryll` to `/usr/bin/ryll`. Runtime dependencies
(libc, libssl) are detected automatically and will be pulled in by
`apt-get install -f` if missing.

## Red Hat / Fedora (RPM)

*Coming soon.* RPM packages will be built with `cargo-generate-rpm`.

## macOS (Homebrew)

*Coming soon.* A Homebrew tap will be available for Apple Silicon Macs.

## Windows

*Coming soon.* A `.zip` archive containing `ryll.exe` will be available.
Note that `--capture` mode is not available on Windows.

## Building from source

If no pre-built package is available for your platform, you can build
ryll from source. See the [README](../README.md) for build instructions
and the [portability guide](portability.md) for platform-specific notes.
