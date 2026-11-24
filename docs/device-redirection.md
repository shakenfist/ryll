# Device redirection

Device redirection shares peripherals and data between the local machine
and the guest: USB devices over the usbredir channel, either as physical
pass-through on Linux or as virtual mass storage devices everywhere;
folder sharing over WebDAV; and clipboard text delivered as synthetic
keystrokes for guests without vdagent.

## USB Redirection

USB device redirection uses the SPICE usbredir channel (type 9) to
forward USB devices to the remote VM. The implementation spans
several protocol layers:

```mermaid
flowchart TB
    vmc["SPICE SpiceVMC<br/>(DATA / COMPRESSED_DATA messages)"]
    usbredir["usbredir protocol<br/>(hello, device_connect, control / bulk / interrupt packets)"]
    bot["USB Mass Storage Bulk-Only Transport<br/>(for virtual disks)"]
    scsi["SCSI commands<br/>(INQUIRY, READ/WRITE(10), and so on)"]
    raw["RAW file I/O<br/>(seek + read/write at LBA * 512)"]

    vmc -- carries --> usbredir
    usbredir -- carries --> bot
    bot -- carries --> scsi
    scsi -- carries --> raw
```

### Device backends

The `UsbDeviceBackend` trait (`shakenfist-spice-renderer/src/usb/mod.rs`)
abstracts over device types. The `DeviceBackend` enum provides
non-object-safe dispatch:

- **RealDevice** (`shakenfist-spice-renderer/src/usb/real.rs`): Physical USB
  device via the `nusb` crate. Linux only (`#[cfg(target_os = "linux")]`).
  Detaches kernel drivers, claims interfaces, forwards
  control/bulk/interrupt transfers. On non-Linux platforms, only virtual
  devices are available.
- **VirtualMsc** (`shakenfist-spice-renderer/src/usb/virtual_msc.rs`):
  Emulated USB mass storage device backed by a RAW disk image. Implements
  BOT protocol (CBW/CSW) and 8 SCSI commands. Reports as a USB 2.0 High
  Speed removable disk.

### Channel handler flow

1. Channel connects, sends usbredir hello with capabilities.
2. Server responds with hello.
3. If `--usb-disk` is configured, auto-connects after hello.
4. Device attachment sends `ep_info`, `interface_info`, `device_connect`.
5. Server sends lifecycle messages (`set_configuration`, `reset`, etc.)
   and data transfers (`control_packet`, `bulk_packet`).
6. Interrupt endpoints use background tokio polling tasks.
7. Disconnection aborts polling tasks and sends `device_disconnect`.

### CLI usage

```bash
ryll --file conn.vv --usb-disk /path/to/image.raw       # read-write
ryll --file conn.vv --usb-disk-ro /path/to/image.raw     # read-only
```

See the [configuration guide](configuration.md) for details. Use
`make test-qemu-usb` to start a QEMU instance with USB redirection
enabled.

### GUI Components

The USB panel is a right-side panel toggled by Menu → USB,
rendered alongside the traffic viewer panel (both use `egui::SidePanel::right`
with different IDs).

**State tracking on RyllApp:**

- `usb_tx` — mpsc sender to the UsbredirChannel, created in `RyllApp::new()`
  and threaded through `run_connection()`. Mirrors the `input_tx` pattern.
- `usb_channel_ready` — set when `UsbChannelReady` event arrives, cleared on
  usbredir channel disconnect.
- `usb_connecting` / `usb_disconnecting` — operation in progress flags, cleared
  on success/failure events.
- `usb_device_description` — set by `UsbDeviceConnected`, cleared by
  `UsbDeviceDisconnected` and channel disconnect.
- `usb_connected_at` — timestamp for the elapsed connection timer.
- `usb_available_devices` — enumerated device list, refreshed on panel open
  and via Refresh button.
- `usb_virtual_disks` — session-scoped virtual disk paths from CLI flags and
  runtime additions.

**Command flow:**

The GUI sends identity-based `UsbCommand` variants (`ConnectPhysical { bus,
address }` (Linux only), `ConnectVirtualDisk { path, read_only }`,
`DisconnectDevice`) via
`usb_tx`. The channel handler does async device lookup and open in its tokio
context, sending `UsbDeviceConnected`, `UsbDeviceDisconnected`, or
`UsbConnectFailed` events back to the app. If a device is already connected
when a connect command arrives, the handler disconnects it first.

**File picker:**

The "Add Disk..." button spawns `rfd::FileDialog` on a background thread. The
result is polled via `std::sync::mpsc::try_recv()` each frame. Selected files
are validated (regular file, >= 512 bytes) and added to the session's virtual
disk list.

**Bug report integration:**

USB errors show a "Report this as a bug" button that opens the bug report
dialog pre-populated with `BugReportType::Usb`, which captures the usbredir
channel's pcap traffic. Generic channel errors (displayed in the central
panel) also offer a bug report button pre-populated with
`BugReportType::Connection`.

### Why the USB panel is shaped this way

These are constraints and rejected alternatives behind the panel; the
[plan that built it](https://github.com/shakenfist/ryll/blob/8cf54b550713b2f65c6397b3ab4bde7b31817f84/docs/plans/PLAN-usb-ui.md)
(retired) holds the phase-by-phase history.

- **One device, one usbredir channel.** SPICE carries one redirected
  device per usbredir channel. A server may advertise several channels,
  but the session opens only the first and skips the rest (the command
  receiver is `take()`n once), so the panel manages one device at a time
  and disables Connect while the channel is not ready. Supporting
  several simultaneous redirections would need a command sender per
  channel and per-channel panel state; it was deferred, not rejected.
- **A panel, not a modal dialog.** It stays open while the user works in
  the guest and shows live status, as the Traffic viewer does. The status
  bar's `USB: ...` label is kept as a glance indicator; the panel adds
  the management detail. Both are driven by `usb_device_description`.
- **Enumeration is on demand, never per frame.** `enumerate_devices()`
  calls `nusb::list_devices()`, which is synchronous (under about 100 ms
  in practice), so the panel enumerates on first open and on Refresh. It
  deliberately does not clear the list when the panel closes: re-listing
  on every open would stall the render loop more often than slightly
  stale data with a Refresh button costs. A timer or the `nusb` hotplug
  watcher was weighed and deferred as added complexity; Refresh is the
  contract.
- **The GUI sends identity, the channel opens the device.** `nusb`
  device handles cannot cross the command channel, and opening is async
  while the render loop is not. `ConnectPhysical { bus, address }`
  therefore makes the handler re-enumerate and find the device again. A
  device unplugged between listing and clicking Connect produces a
  `UsbConnectFailed("Device not found ... may have been unplugged")`
  rather than a silent mismatch. This is why `enumerate_physical()` must
  report `busnum()` in `bus`: it originally stored `device_address()` in
  both fields, which would have made the re-lookup match the wrong device
  or none. `bus` stays a `u8` rather than the string `bus_id()` because
  physical pass-through is Linux only; revisit the type if another
  platform gains it.
- **Failure must always have a way out.** `UsbConnectFailed` exists so the
  panel can never sit in "Connecting..." with no event to clear it, and a
  `Disconnected` event for the usbredir channel resets all panel state
  (ready flag, connected device, in-progress flags) so a dropped channel
  does not leave a stale "Ready". A connect while a device is attached
  (for example after a `--usb-disk` auto-connect) disconnects the old
  device first.
- **No double-queued commands.** The in-progress flags disable every
  button until the matching event arrives, so at most one command is in
  flight; a full command channel is reported as an error rather than
  waited on.
- **A native file picker, not a typed path.** The panel is GUI only
  (headless users have `--usb-disk`), so a typed path would be a poor
  experience for the only people who see it. `rfd` is used without its
  `gtk3` feature (the build container has no GTK3 headers), so on Linux
  the dialog depends on a desktop portal service at run time; without
  one it does not open and the CLI flags remain. The blocking
  `FileDialog` runs on its own thread and the result is polled with
  `try_recv()` each frame.
- **Host permissions are not diagnosed.** An empty or failed enumeration
  is shown as it is; the panel does not guess at udev rules or container
  pass-through.
- **Known gap.** The connected row is matched by comparing the backend's
  `description()` with the device's `label()`. They are equal for virtual
  disks but not for physical devices (`vid:pid` against the full label),
  so a connected physical device is not highlighted and offers no
  Disconnect button in the list. Tracked in
  [#775](https://github.com/shakenfist/ryll/issues/775).

## WebDAV Folder Sharing

WebDAV folder sharing uses the SPICE WebDAV channel (type 11) to export a
local directory to the guest VM. Like usbredir, it uses the SpiceVMC transport
(`SPICEVMC_DATA` / `SPICEVMC_COMPRESSED_DATA` messages). The guest's
`spice-webdavd` daemon issues HTTP WebDAV requests through the channel;
ryll runs an embedded WebDAV server that fulfils them against the local
filesystem.

### Protocol layers

```mermaid
flowchart TB
    vmc["SPICE SpiceVMC<br/>(DATA / COMPRESSED_DATA messages)"]
    mux["Mux protocol<br/>(client_id + size + HTTP data)"]
    http["HTTP/1.1<br/>(parsed by hyper)"]
    webdav["WebDAV<br/>(RFC 4918, handled by dav-server with LocalFs)"]
    fs["Local filesystem I/O"]

    vmc -- carries --> mux
    mux -- carries --> http
    http -- carries --> webdav
    webdav -- carries --> fs
```

### Mux protocol

The WebDAV channel multiplexes multiple concurrent HTTP clients over a
single byte stream. Each frame is:

```
client_id:  i64 LE  (8 bytes) — identifies the HTTP client
data_size:  u16 LE  (2 bytes) — payload size (0 = disconnect)
data:       [u8]    (data_size bytes) — raw HTTP bytes
```

The `MuxDemuxer` (`shakenfist-spice-renderer/src/webdav/mux.rs`) accumulates
bytes and extracts complete frames, handling frames that span VMC messages
or are packed together.

### Per-client architecture

Each mux client gets a `tokio::io::DuplexStream` pair:

```mermaid
flowchart TB
    req["Guest HTTP request bytes"]
    subgraph ce["DuplexStream (client end, split)"]
        wh["write half<br/>held in MuxClient; the main loop writes request data"]
        rh["read half<br/>reader task reads response data, sends it via mpsc"]
    end
    se["DuplexStream (server end)"]
    hyper["TokioIo → hyper http1::serve_connection()<br/>→ dav-server DavHandler"]
    fs["Local filesystem (via dav-server LocalFs)"]

    req --> wh
    wh --> se
    se --> hyper
    hyper --> fs
    fs -. response .-> hyper
    hyper -. response .-> se
    se -. response .-> rh
```

Response data flows back through an `mpsc::Sender<MuxResponse>` from
the per-client reader task to the main `run()` loop, which muxes the
responses back to the guest. This is the same pattern used by usbredir's
interrupt polling tasks.

### Server lifecycle

The `WebdavServer` (`shakenfist-spice-renderer/src/webdav/server.rs`) wraps
`dav-server::DavHandler` with `LocalFs` and is cheaply cloneable (inner
`Arc`). It is created when
a `ShareDirectory` command arrives from the UI or `--share-dir` is
specified on the CLI, and destroyed on `StopSharing`. Read-only mode uses
`DavMethodSet::WEBDAV_RO` to restrict allowed HTTP methods.

### CLI usage

```bash
ryll --file conn.vv --share-dir /path/to/dir          # read-write
ryll --file conn.vv --share-dir /path/to/dir --share-dir-ro  # read-only
```

See the [configuration guide](configuration.md) for details. Use
`make test-qemu-webdav` to start a QEMU instance with WebDAV enabled.

### GUI Components

The Folders panel is a right-side panel toggled by
Menu → Folders. It mirrors the USB panel structure:
channel status indicator, active
share display with elapsed timer, error display with auto-clear, read-only
checkbox, and native directory picker via `rfd::FileDialog::pick_folder`.


## Paste-as-Keystrokes

The inputs channel includes a cooperative paste state machine for
typing text into guests that lack a vdagent clipboard channel.
Characters are translated to US-QWERTY AT scancodes via
`char_to_scancode()` and `translate_paste()` (both in `inputs.rs`),
capped at 4096 characters per paste.

The state machine (`PasteState`) runs as a conditional third arm in
the inputs channel's `tokio::select!` loop. A `tokio::time::sleep_until`
future fires on schedule; each firing sends one sub-step (press or
release) and yields back to the loop so the other two arms (server
reads and UI input events) remain responsive.

Per-character event sequence:
1. If shifted: KeyDown(Left Shift)
2. KeyDown(scancode)
3. Sleep half the inter-character delay
4. KeyUp(scancode)
5. If shifted: KeyUp(Left Shift)
6. Sleep the remaining half

At paste start, held modifier keys (Ctrl, Shift, Alt) are released
and saved; at paste end they are restored. Translation errors
(non-ASCII characters) emit `ChannelEvent::PasteFailed` and cause
a non-zero exit in headless mode. A paste that arrives while another
is still typing is refused with a `PasteFailed` of kind `InProgress`
before anything is typed; the GUI shows that one as a notification
rather than the paste-error dialog, since nothing needs fixing.

CLI flags: `--enable-paste-as-keystrokes` (master gate),
`--paste-text TEXT` (headless trigger, implies enable; refused without
`--headless`),
`--paste-char-delay-ms N` (default 16ms).

GUI surface: When enabled, a "Paste" entry appears in the hamburger
menu with "Ctrl+Alt+V" shortcut text. The entry is disabled (greyed
out) when vdagent is connected, with a tooltip explaining to use
normal Ctrl+V. The Ctrl+Alt+V shortcut is detected before
`handle_input()` to prevent the V keypress from reaching the guest.
Off macOS egui-winit reports that press as `Event::Paste`, not as a
key press, so `paste_shortcut_pressed()` in `ryll/src/input_egui.rs`
accepts either. Once the paste starts, `HeldKeys::withhold_paste_release()`
keeps the V release that follows from reaching the guest too.
Pre-validation via `translate_paste()` catches unrepresentable
characters and shows an error dialog listing up to three sample
codepoints. The clipboard is read via `arboard::Clipboard` (lazily
initialised in `RyllApp::clipboard()`, separate from the
`MainChannel` instance).

