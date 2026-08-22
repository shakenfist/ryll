use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use clap::Parser;
use configparser::ini::Ini;
use shakenfist_spice_protocol::ConnectionConfig;
use shakenfist_spice_webrtc::{BindSelector, UdpBindPolicy};
use tracing::warn;

// Device-shaped configuration (the value types passed into the
// channel constructors) lives in the renderer crate. The
// CLI-shaped `Args` and `Config` definitions stay here, alongside
// the path-validation helpers.
pub use shakenfist_spice_renderer::device_config::{ShareDirConfig, VirtualDiskConfig};

/// Ryll - A Rust SPICE VDI client
#[derive(Parser, Debug)]
#[command(author, version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("RYLL_GIT_SHA"), ")"), about, long_about = None)]
pub struct Args {
    /// URL to fetch .vv configuration file from
    #[arg(long, group = "config_source")]
    pub url: Option<String>,

    /// Path to local .vv configuration file
    #[arg(long, group = "config_source")]
    pub file: Option<String>,

    /// Direct connection string: HOST:PORT or HOST:INSECURE_PORT:SECURE_PORT
    #[arg(long, group = "config_source")]
    pub direct: Option<String>,

    /// Run in headless mode (no GUI)
    #[arg(long, default_value_t = false)]
    pub headless: bool,

    /// Bind a Unix-domain control socket at <PATH> for external
    /// NDJSON-based session control. Valid with --headless or
    /// --web; not with the GUI. The socket is created with mode
    /// 0600 (owner read/write only). See
    /// ryll/docs/control-socket-protocol.md for the wire format.
    ///
    /// Unix-only: tokio::net::UnixListener has no equivalent on
    /// Windows; the flag is omitted from the CLI on non-Unix builds.
    #[cfg(unix)]
    #[arg(long, value_name = "PATH",
          value_parser = clap::value_parser!(PathBuf))]
    pub control_socket: Option<PathBuf>,

    /// Enable cadence mode (automatic keystroke every 2 seconds)
    #[arg(long, default_value_t = false)]
    pub cadence: bool,

    /// Enable verbose logging
    #[arg(short, long, default_value_t = false)]
    pub verbose: bool,

    /// Log intimate details (keystrokes, mouse movements)
    #[arg(long, default_value_t = false)]
    pub intimate: bool,

    /// Path to write latency measurements
    #[arg(long)]
    pub latency_file: Option<String>,

    /// Directory for protocol/display capture output (pcap + video)
    #[cfg(feature = "capture")]
    #[arg(long)]
    pub capture: Option<String>,

    /// Number of monitors to connect (default: 1)
    #[arg(long, default_value_t = 1)]
    pub monitors: u8,

    /// Present a RAW disk image as a USB mass storage device (repeatable)
    #[arg(long = "usb-disk")]
    pub usb_disk: Vec<String>,

    /// Present a RAW disk image as a read-only USB mass storage device (repeatable)
    #[arg(long = "usb-disk-ro")]
    pub usb_disk_ro: Vec<String>,

    /// Share a local directory with the guest via WebDAV (SPICE folder sharing)
    #[arg(long = "share-dir")]
    pub share_dir: Option<String>,

    /// Make the shared directory read-only
    #[arg(long = "share-dir-ro")]
    pub share_dir_ro: bool,

    /// Auto-write a bug report zip the first time each
    /// distinct protocol gap is seen. Capped at 50 reports
    /// per session to bound disk use.
    #[arg(long)]
    pub pedantic: bool,

    /// Directory for --pedantic-mode bug reports. Created if
    /// missing. If unset, falls back to --bug-report-dir, then
    /// to the historical default of ./ryll-pedantic-reports.
    #[arg(long)]
    pub pedantic_dir: Option<std::path::PathBuf>,

    /// Default directory for bug-report zip files (manual F12
    /// reports, pedantic reports, and auto-disconnect
    /// snapshots). Created if missing. Each flavour can be
    /// overridden individually (e.g. by --pedantic-dir). If
    /// neither this nor a flavour-specific flag is set, the
    /// per-flavour fallback applies (cwd for manual /
    /// auto-disconnect, ./ryll-pedantic-reports for pedantic).
    #[arg(long)]
    pub bug_report_dir: Option<std::path::PathBuf>,

    /// Automatically save a complete bug-report zip every N seconds
    /// into <bug-report-dir>/auto-snapshots/. Use this as a
    /// "flight-data-recorder" for intermittent issues: set it before
    /// the session, walk away, and the evidence is captured by
    /// construction even if the symptom is transient.
    ///
    /// Minimum recommended interval: 10 s (BugReport::new blocks for
    /// ~2 s sampling runtime metrics; shorter intervals cause overlapping
    /// samples which is harmless but wasteful). Values below 10 s log
    /// a warning at startup.
    #[arg(long)]
    pub auto_snapshot_interval: Option<u64>,

    /// Maximum number of auto-snapshot zips to keep on disk
    /// (default: 20). Oldest zips are pruned when the cap is
    /// exceeded. Only meaningful when --auto-snapshot-interval is set.
    #[arg(long)]
    pub auto_snapshot_cap: Option<usize>,

    /// Diagnostic flag for a hang investigation. When set, ryll's
    /// per-connection tokio runtime is built with
    /// `Builder::new_current_thread()` instead of the default
    /// multi-threaded `Runtime::new()`. Disambiguates a real blocking
    /// call (would still hang) from a multi-threaded scheduler / Waker
    /// registration bug (would not hang). Will be removed once that
    /// investigation is closed.
    #[arg(long)]
    pub debug_single_thread_runtime: bool,

    /// Enable paste-as-keystrokes fallback for guests without vdagent
    #[arg(long)]
    pub enable_paste_as_keystrokes: bool,

    /// String to type as keystrokes in headless mode (implies --enable-paste-as-keystrokes)
    #[arg(long = "paste-text")]
    pub paste_text: Option<String>,

    /// Inter-character delay for paste-as-keystrokes in milliseconds
    #[arg(long = "paste-char-delay-ms", default_value_t = 16)]
    pub paste_char_delay_ms: u32,

    /// Start with "obey guest size hints" turned off so the
    /// window does not auto-fit when the guest changes
    /// resolution. Equivalent to opening the hamburger menu
    /// and unchecking the checkbox after launch.
    #[arg(long, default_value_t = false)]
    pub no_obey_guest_size: bool,

    /// Run as a SPICE → browser transcoder. Listens on an
    /// ephemeral HTTP port, prints a URL with a per-launch
    /// random token, and serves a browser shell that consumes
    /// the SPICE display via WebRTC. Mutually exclusive with
    /// --headless and the GUI default.
    #[arg(long)]
    pub web: bool,

    /// Bind address for --web mode (default 127.0.0.1).
    #[arg(long, default_value = "127.0.0.1")]
    pub web_host: String,

    /// Listen port for --web mode (default ephemeral).
    #[arg(long, default_value_t = 0u16)]
    pub web_port: u16,

    /// PEM-encoded TLS certificate chain for --web mode. If
    /// supplied, --web-tls-key is also required and the web
    /// frontend serves over HTTPS instead of plain HTTP.
    #[arg(long, requires = "web_tls_key")]
    pub web_tls_cert: Option<PathBuf>,

    /// PEM-encoded TLS private key for --web mode. Required if
    /// --web-tls-cert is supplied.
    #[arg(long, requires = "web_tls_cert")]
    pub web_tls_key: Option<PathBuf>,

    /// Local address or interface name to bind the WebRTC media
    /// (UDP) sockets to in --web mode. Repeatable. Defaults to
    /// every interface address that is not loopback, unspecified
    /// or IPv6 link-local. Naming an address explicitly overrides
    /// that default, which is how a loopback-only host is served:
    /// --web-media-addr 127.0.0.1. Note this is not --web-host,
    /// which binds only the HTTP listener.
    #[arg(long = "web-media-addr", value_name = "ADDR|IFACE")]
    pub web_media_addr: Vec<String>,

    /// UDP port for the WebRTC media sockets in --web mode.
    /// Defaults to 0, an ephemeral port per bound address. Pin it
    /// so a firewall rule can name one port instead of the whole
    /// ephemeral range; the pinned port applies to every bound
    /// address, and a port already in use is a hard error rather
    /// than a silent fallback.
    #[arg(long, default_value_t = 0u16)]
    pub web_media_port: u16,

    /// STUN or TURN server URL for --web mode, as
    /// stun:host:port or turn:host:port. Repeatable. Empty by
    /// default: ryll assumes browser and host share a LAN, so ICE
    /// host candidates are usually enough.
    #[arg(long = "web-ice-server", value_name = "URL")]
    pub web_ice_server: Vec<String>,

    /// Maximum total bytes for the SPICE display image cache,
    /// in MiB. Defaults to 256. The cache holds decoded RGBA
    /// for images the server flagged with CACHE_ME; without a
    /// cap, video workloads can consume gigabytes (see
    /// session-002g).
    #[arg(long, default_value_t = 256)]
    pub image_cache_cap_mib: u64,

    /// Maximum total bytes for the shared SPICE GLZ dictionary, in MiB.
    /// Defaults to 256. The dictionary holds decoded RGBA for GLZ
    /// images so cross-frame references resolve; without a cap,
    /// full-screen ZlibGlzRgb workloads observed in sessions 003a /
    /// 004d-g consumed gigabytes (~30 MiB/s of growth).
    #[arg(long, default_value_t = 256)]
    pub glz_dictionary_cap_mib: u64,
}

/// The WebRTC media socket binding policy `--web-media-addr` and
/// `--web-media-port` describe.
///
/// Called once at startup so a malformed or unusable address fails
/// the launch, rather than surfacing at the first viewer's
/// `POST /offer` — the policy itself is re-resolved per bridge, so
/// this is the only chance to reject bad input early.
///
/// Anything that does not parse as an `IpAddr` is taken to be an
/// interface name, which is what lets `--web-media-addr eth0` work
/// without a second flag. That fallback is narrowed by
/// [`reject_malformed_address`] first, because the interesting
/// failure is not an interface name that looks like an address —
/// there is no such thing — but an address that Rust's parser
/// rejects and which would otherwise be silently demoted to a name.
pub fn web_media_bind_policy(args: &Args) -> Result<UdpBindPolicy> {
    let mut selectors = Vec::with_capacity(args.web_media_addr.len());
    for value in &args.web_media_addr {
        selectors.push(match value.parse::<IpAddr>() {
            Ok(ip) => BindSelector::Addr(ip),
            Err(_) => {
                // Safe to name one flag here, unlike the whole-policy
                // check below: this only ever inspects a
                // `--web-media-addr` value.
                reject_malformed_address(value).map_err(|e| anyhow!("--web-media-addr: {}", e))?;
                BindSelector::Interface(value.clone())
            }
        });
    }
    let policy = UdpBindPolicy {
        selectors,
        port: args.web_media_port,
    };
    // Name the flag family rather than one flag: `validate` checks
    // the whole policy, and today only the selectors can fail — but a
    // port check added later would arrive here wearing
    // `--web-media-addr` if this named that flag directly.
    policy.validate().map_err(|e| {
        anyhow!(
            "web media binding (`--web-media-addr` / `--web-media-port`): {}",
            e
        )
    })?;
    Ok(policy)
}

/// Reject `--control-socket` in GUI mode.
///
/// The socket is valid with `--headless` or `--web`, both of which
/// run a session with no host window and can host the server. The
/// GUI cannot: it owns input and the surface itself, and a second
/// driver injecting events behind its back has no defined meaning.
///
/// Expressed here rather than as a clap `requires`, because clap
/// cannot say "one of these two flags" and the error it produces for
/// a single `requires` names the wrong flag.
#[cfg(unix)]
pub fn validate_control_socket(args: &Args) -> Result<()> {
    if args.control_socket.is_some() && !args.headless && !args.web {
        bail!(
            "--control-socket needs a session with no host window: pass --headless or \
             --web as well. It is not available in GUI mode, where the window owns input \
             and the surface."
        );
    }
    Ok(())
}

/// Reject a `--web-media-addr` value that is a failed *address*
/// rather than an interface name.
///
/// The address-or-interface fallback is ambiguous in exactly one
/// direction. No interface name parses as an IP literal, so a
/// successful parse is never a misread name — but an address the
/// parser rejects becomes an interface name that will never match,
/// and the operator gets "no interface named ..." for something that
/// is visibly an address.
///
/// The zone-scoped case is the one the docs actively steer people
/// into: `bind_addrs` and `docs/configuration.md` both explain that a
/// zoneless `fe80::/10` address is refused *because* it has no zone
/// id, so `fe80::1%eth0` is the natural next thing to try. Rust's
/// `Ipv6Addr` has no scope-id support, so it does not parse.
///
/// A single colon is left alone: `eth0:0` is a legitimate IPv4 alias
/// interface label, and `getifaddrs` reports it as the interface
/// name.
fn reject_malformed_address(value: &str) -> Result<()> {
    if value.contains('%') {
        bail!(
            "`{value}` cannot be used as a media bind address: a zone-scoped IPv6 literal \
             cannot be carried by a socket address or an ICE candidate, which is why the \
             zoneless form is refused too. Name the interface instead — `--web-media-addr \
             eth0` binds the addresses on that link"
        );
    }
    if value.matches(':').count() > 1 {
        bail!(
            "`{value}` is not a valid IPv6 address and cannot be an interface name either. \
             Pass an address literal, or an interface name such as `--web-media-addr eth0`"
        );
    }
    // The dot is what makes it an attempted address rather than a
    // name: a bare number is a legal interface name on Linux, so
    // `--web-media-addr 123` has to stay a name.
    if value.contains('.') && value.chars().all(|c| c.is_ascii_digit() || c == '.') {
        bail!(
            "`{value}` is not a valid IPv4 address and cannot be an interface name either. \
             Pass an address literal, or an interface name such as `--web-media-addr eth0`"
        );
    }
    Ok(())
}

/// The STUN and TURN URLs `--web-ice-server` supplies, checked for
/// the one thing that can be checked without a network.
///
/// The sibling address flags fail at launch on bad input, and this
/// one needs it more, not less: an operator only reaches for
/// `--web-ice-server` on a deployment where host candidates already
/// do not work, so a URL that is quietly useless is indistinguishable
/// from "WebRTC is broken". Reachability is not checked — a STUN
/// server that is down now may be up when a viewer arrives, the same
/// reasoning that keeps interface names out of `validate`.
pub fn web_ice_servers(args: &Args) -> Result<Vec<String>> {
    const SCHEMES: [&str; 4] = ["stun:", "stuns:", "turn:", "turns:"];
    for url in &args.web_ice_server {
        let Some(scheme) = SCHEMES.iter().find(|s| url.starts_with(*s)) else {
            bail!(
                "--web-ice-server: `{url}` has no usable scheme — a STUN or TURN URL must \
                 start with `stun:`, `stuns:`, `turn:` or `turns:` (RFC 7064, RFC 7065), as \
                 in `stun:stun.example.com:3478`"
            );
        };
        if url[scheme.len()..].is_empty() {
            bail!("--web-ice-server: `{url}` names no host after `{scheme}`");
        }
    }
    Ok(args.web_ice_server.clone())
}

/// SPICE connection configuration
#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub tls_port: Option<u16>,
    pub password: Option<String>,
    pub ca_cert: Option<String>,
    #[allow(dead_code)]
    pub host_subject: Option<String>,
    /// True when the .vv file set `delete-this-file=1`. ryll
    /// treats this as "the ticket is single-use" — auto-reconnect
    /// skips Pending entirely and shows the `OneShotConsumed`
    /// modal, since the previous link consumed the ticket. The
    /// non-spec interpretation is documented in
    /// `kerbside-wt-docs/docs/spice/console-vv-extensions.md`.
    pub ticket_is_single_use: bool,
    /// Optional ticket expiry timestamp (`ticket-valid-until` —
    /// ryll-specific extension key, unix seconds). When the
    /// current time has passed this point, auto-reconnect is
    /// disabled regardless of the 3-attempt budget — the ticket
    /// is dead from the server's point of view, so retrying
    /// only produces failed attempts. `None` if absent or
    /// malformed in the .vv file.
    pub ticket_valid_until: Option<SystemTime>,
}

impl From<&Config> for ConnectionConfig {
    fn from(c: &Config) -> Self {
        ConnectionConfig {
            host: c.host.clone(),
            port: c.port,
            tls_port: c.tls_port,
            password: c.password.clone(),
            ca_cert: c.ca_cert.clone(),
            host_subject: c.host_subject.clone(),
        }
    }
}

/// Filter out configparser's literal "None" string for absent values
fn filter_none(s: String) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
        None
    } else {
        Some(s)
    }
}

/// Parse an optional u16 from an INI field, treating empty/"None" as absent
fn parse_optional_u16(ini: &Ini, section: &str, key: &str) -> Result<Option<u16>> {
    match ini.get(section, key).and_then(filter_none) {
        Some(s) => {
            let val: u16 = s
                .trim()
                .parse()
                .map_err(|e| anyhow!("Invalid {} '{}': {}", key, s, e))?;
            Ok(Some(val))
        }
        None => Ok(None),
    }
}

impl Config {
    /// Create configuration from command line arguments.
    ///
    /// `--web` requires a real connection source (`--url` /
    /// `--file` / `--direct`) like every other mode, because
    /// `run_web` spawns `run_connection` rather than serving a
    /// placeholder backend.
    pub fn from_args(args: &Args) -> Result<Self> {
        if let Some(url) = &args.url {
            Self::from_url(url)
        } else if let Some(file) = &args.file {
            Self::from_vv_file(file)
        } else if let Some(direct) = &args.direct {
            Self::from_direct(direct)
        } else {
            Err(anyhow!("Must specify one of --url, --file, or --direct"))
        }
    }

    /// Fetch and parse .vv file from URL
    fn from_url(url: &str) -> Result<Self> {
        // Use blocking reqwest for simplicity during startup
        let response = reqwest::blocking::get(url)?;
        let content = response.text()?;
        Self::parse_vv_content(&content)
    }

    /// Parse local .vv file
    fn from_vv_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(path)?;
        Self::parse_vv_content(&content)
    }

    /// Parse direct connection string
    fn from_direct(direct: &str) -> Result<Self> {
        let parts: Vec<&str> = direct.split(':').collect();

        match parts.len() {
            2 => {
                let host = parts[0].to_string();
                let port: u16 = parts[1].parse()?;
                Ok(Config {
                    host,
                    port,
                    tls_port: None,
                    password: None,
                    ca_cert: None,
                    host_subject: None,
                    ticket_is_single_use: false,
                    ticket_valid_until: None,
                })
            }
            3 => {
                let host = parts[0].to_string();
                let port: u16 = parts[1].parse()?;
                let tls_port: u16 = parts[2].parse()?;
                Ok(Config {
                    host,
                    port,
                    tls_port: Some(tls_port),
                    password: None,
                    ca_cert: None,
                    host_subject: None,
                    ticket_is_single_use: false,
                    ticket_valid_until: None,
                })
            }
            _ => Err(anyhow!(
                "Invalid direct connection string. Expected HOST:PORT or HOST:PORT:TLS_PORT"
            )),
        }
    }

    /// Parse .vv file content (INI format)
    fn parse_vv_content(content: &str) -> Result<Self> {
        let mut ini = Ini::new();
        ini.read(content.to_string())
            .map_err(|e| anyhow!("Failed to parse .vv file: {}", e))?;

        let section = "virt-viewer";

        let host = ini
            .get(section, "host")
            .and_then(filter_none)
            .ok_or_else(|| anyhow!("Missing 'host' in .vv file"))?;

        let port = parse_optional_u16(&ini, section, "port")?;
        let tls_port = parse_optional_u16(&ini, section, "tls-port")?;

        if port.is_none() && tls_port.is_none() {
            return Err(anyhow!("Must specify 'port' or 'tls-port' in .vv file"));
        }

        let password = ini.get(section, "password").and_then(filter_none);
        let ca_cert = ini.get(section, "ca").and_then(filter_none);
        let host_subject = ini.get(section, "host-subject").and_then(filter_none);

        // `delete-this-file=1` → single-use ticket. Standard
        // virt-viewer key; ryll layers an extra interpretation
        // documented in console-vv-extensions.md. Any value
        // other than "1" (including absence) leaves
        // `ticket_is_single_use = false`.
        let ticket_is_single_use = ini
            .get(section, "delete-this-file")
            .and_then(filter_none)
            .map(|s| s.trim() == "1")
            .unwrap_or(false);

        // `ticket-valid-until=<unix-ts>` → ryll-specific
        // extension key. Malformed values do not fail the parse;
        // we log a warning and treat the field as absent.
        let ticket_valid_until = parse_ticket_valid_until(&ini, section);

        Ok(Config {
            host,
            port: port.unwrap_or(0),
            tls_port,
            password,
            ca_cert,
            host_subject,
            ticket_is_single_use,
            ticket_valid_until,
        })
    }
}

/// Parse the `ticket-valid-until` extension key from the .vv
/// file. Absent → `None`; malformed → log a warn and yield
/// `None` (do not propagate the error — connect should still
/// succeed even if the optional expiry hint cannot be parsed).
///
/// Hardening (pre-push audit, wave 2d / F1 + F2):
///
/// - **Overflow-safe arithmetic.** A near-`u64::MAX` value
///   would overflow the i64-backed `SystemTime` and panic on
///   the `+` operator. `checked_add` returns `None`, which we
///   treat as a parse failure.
///
/// - **Reject past timestamps at parse time.** A hostile or
///   buggy `.vv` with `ticket-valid-until=0` (or any value
///   already in the past) would short-circuit every disconnect
///   into the `Modal(TicketExpired)` variant on first
///   disconnect, even though the actual server-side ticket may
///   be perfectly valid. Treat already-past expiries the same
///   as malformed: log a warn and yield `None` so the session
///   proceeds normally.
fn parse_ticket_valid_until(ini: &Ini, section: &str) -> Option<SystemTime> {
    let raw = ini
        .get(section, "ticket-valid-until")
        .and_then(filter_none)?;
    let secs = match raw.trim().parse::<u64>() {
        Ok(s) => s,
        Err(e) => {
            warn!(
                ".vv: ticket-valid-until='{}' is not a valid unix timestamp: {}; ignoring",
                raw, e
            );
            return None;
        }
    };
    let expiry = match UNIX_EPOCH.checked_add(Duration::from_secs(secs)) {
        Some(t) => t,
        None => {
            warn!(
                ".vv: ticket-valid-until='{}' overflows SystemTime; ignoring",
                raw
            );
            return None;
        }
    };
    let now = SystemTime::now();
    if expiry <= now {
        warn!(
            ".vv: ticket-valid-until='{}' is already in the past at parse time; \
             ignoring (would otherwise short-circuit every disconnect to a \
             ticket-expired modal)",
            raw
        );
        return None;
    }
    Some(expiry)
}

/// Collect virtual disk configs from CLI args and validate paths.
pub fn parse_virtual_disks(args: &Args) -> Result<Vec<VirtualDiskConfig>> {
    let mut disks = Vec::new();

    for path_str in &args.usb_disk {
        let path = PathBuf::from(path_str);
        validate_disk_path(&path)?;
        disks.push(VirtualDiskConfig {
            path,
            read_only: false,
        });
    }

    for path_str in &args.usb_disk_ro {
        let path = PathBuf::from(path_str);
        validate_disk_path(&path)?;
        disks.push(VirtualDiskConfig {
            path,
            read_only: true,
        });
    }

    if disks.len() > 1 {
        tracing::warn!(
            "Multiple USB disks specified; only the first will be connected \
             (one device per usbredir channel)"
        );
    }

    Ok(disks)
}

/// Parse shared directory config from CLI args, validating the path.
pub fn parse_share_dir(args: &Args) -> Result<Option<ShareDirConfig>> {
    match &args.share_dir {
        Some(path_str) => {
            let path = PathBuf::from(path_str);
            if !path.exists() {
                return Err(anyhow!("Shared directory not found: {}", path.display()));
            }
            if !path.is_dir() {
                return Err(anyhow!(
                    "Shared path is not a directory: {}",
                    path.display()
                ));
            }
            Ok(Some(ShareDirConfig {
                path,
                read_only: args.share_dir_ro,
            }))
        }
        None => Ok(None),
    }
}

fn validate_disk_path(path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(anyhow!("USB disk image not found: {}", path.display()));
    }
    let metadata = fs::metadata(path)?;
    if metadata.len() < 512 {
        return Err(anyhow!(
            "USB disk image too small ({} bytes, minimum 512): {}",
            metadata.len(),
            path.display()
        ));
    }
    if metadata.len() % 512 != 0 {
        tracing::warn!(
            "USB disk image {} is {} bytes (not a multiple of 512), {} bytes will be inaccessible",
            path.display(),
            metadata.len(),
            metadata.len() % 512,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The "obey guest size hints" toggle defaults to ON;
    // --no-obey-guest-size flips it OFF. main.rs inverts
    // the flag (`obey_guest_size = !args.no_obey_guest_size`),
    // so anchor the bool semantics here at the parse layer
    // and let the inversion stay implicit.
    #[test]
    fn no_obey_guest_size_default_is_false() {
        let args = Args::parse_from(["ryll", "--direct", "host:5900"]);
        assert!(!args.no_obey_guest_size);
    }

    #[test]
    fn no_obey_guest_size_flag_sets_true() {
        let args = Args::parse_from(["ryll", "--direct", "host:5900", "--no-obey-guest-size"]);
        assert!(args.no_obey_guest_size);
    }

    // ── .vv extension keys ──────────────────────────────────

    fn parse(content: &str) -> Config {
        Config::parse_vv_content(content).expect("parse")
    }

    #[test]
    fn vv_defaults_have_ticket_fields_unset() {
        let cfg = parse("[virt-viewer]\nhost=h\nport=5900\n");
        assert!(!cfg.ticket_is_single_use);
        assert!(cfg.ticket_valid_until.is_none());
    }

    #[test]
    fn vv_delete_this_file_1_sets_single_use() {
        let cfg = parse("[virt-viewer]\nhost=h\nport=5900\ndelete-this-file=1\n");
        assert!(cfg.ticket_is_single_use);
    }

    #[test]
    fn vv_delete_this_file_0_leaves_single_use_off() {
        let cfg = parse("[virt-viewer]\nhost=h\nport=5900\ndelete-this-file=0\n");
        assert!(!cfg.ticket_is_single_use);
    }

    #[test]
    fn vv_ticket_valid_until_parses_unix_ts() {
        // Use a far-future timestamp so the past-rejection
        // hardening doesn't trip the happy-path test. 33 years
        // from the unix epoch puts us comfortably past 2026 and
        // well within SystemTime's range. The exact number
        // doesn't matter beyond "not in the past at test time".
        let future = (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs())
            + 86_400;
        let vv = format!(
            "[virt-viewer]\nhost=h\nport=5900\nticket-valid-until={}\n",
            future
        );
        let cfg = parse(&vv);
        let t = cfg.ticket_valid_until.expect("ticket_valid_until set");
        let secs = t.duration_since(UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(secs, future);
    }

    #[test]
    fn vv_ticket_valid_until_malformed_logs_warn_and_yields_none() {
        // Garbage value: do not fail the parse, just drop the
        // optional hint so connect can still proceed.
        let cfg = parse("[virt-viewer]\nhost=h\nport=5900\nticket-valid-until=not-a-number\n");
        assert!(cfg.ticket_valid_until.is_none());
    }

    #[test]
    fn vv_ticket_valid_until_absent_yields_none() {
        let cfg = parse("[virt-viewer]\nhost=h\nport=5900\n");
        assert!(cfg.ticket_valid_until.is_none());
    }

    #[test]
    fn vv_ticket_valid_until_past_value_yields_none() {
        // Pre-push audit wave 2d / F2 hardening: a hostile or
        // buggy .vv with a past timestamp would otherwise lock
        // the user into Modal(TicketExpired) on first
        // disconnect. Reject at parse time so the session
        // proceeds with the full auto-reconnect budget.
        let cfg = parse("[virt-viewer]\nhost=h\nport=5900\nticket-valid-until=1\n");
        assert!(cfg.ticket_valid_until.is_none());
    }

    #[test]
    fn vv_ticket_valid_until_zero_yields_none() {
        // Boundary case for the past-rejection: 0 (the unix
        // epoch) is unambiguously in the past.
        let cfg = parse("[virt-viewer]\nhost=h\nport=5900\nticket-valid-until=0\n");
        assert!(cfg.ticket_valid_until.is_none());
    }

    #[test]
    fn vv_ticket_valid_until_overflow_yields_none() {
        // Pre-push audit wave 2d / F1 hardening:
        // u64::MAX seconds overflows the i64-backed SystemTime
        // and would panic on `UNIX_EPOCH + Duration::from_secs`.
        // checked_add returns None; we treat it like a parse
        // failure.
        let vv = format!(
            "[virt-viewer]\nhost=h\nport=5900\nticket-valid-until={}\n",
            u64::MAX
        );
        let cfg = parse(&vv);
        assert!(cfg.ticket_valid_until.is_none());
    }

    fn web_args(extra: &[&str]) -> Args {
        let mut argv = vec!["ryll", "--web", "--file", "x.vv"];
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv).expect("args parse")
    }

    /// A control socket is allowed in web mode.
    ///
    /// It was a CLI error until phase 04, and that is why the
    /// QR-digest scenario tests -- which drive a session through the
    /// control socket and read back what the guest received -- could
    /// never observe web mode, the one mode with its own scancode
    /// table. Four input bugs shipped behind that gap.
    #[cfg(unix)]
    #[test]
    fn a_control_socket_is_allowed_alongside_web() {
        let args = web_args(&["--control-socket", "/tmp/ryll-test.sock"]);
        validate_control_socket(&args).expect("web mode should accept a control socket");
    }

    #[cfg(unix)]
    #[test]
    fn a_control_socket_is_still_allowed_alongside_headless() {
        let args = Args::try_parse_from([
            "ryll",
            "--headless",
            "--file",
            "x.vv",
            "--control-socket",
            "/tmp/ryll-test.sock",
        ])
        .expect("args parse");
        validate_control_socket(&args).expect("headless should still accept a control socket");
    }

    /// The GUI still refuses one, and says which flag to add.
    ///
    /// The window owns input and the surface, so a second driver
    /// injecting events behind its back has no defined meaning.
    #[cfg(unix)]
    #[test]
    fn a_control_socket_without_headless_or_web_is_refused() {
        let args = Args::try_parse_from([
            "ryll",
            "--file",
            "x.vv",
            "--control-socket",
            "/tmp/ryll-test.sock",
        ])
        .expect("args parse");
        let err = validate_control_socket(&args)
            .expect_err("the GUI cannot host a control socket")
            .to_string();
        assert!(
            err.contains("--headless"),
            "error should name --headless: {}",
            err
        );
        assert!(err.contains("--web"), "error should name --web: {}", err);
    }

    #[test]
    fn media_addr_takes_an_address_or_an_interface_name() {
        let args = web_args(&[
            "--web-media-addr",
            "192.168.1.42",
            "--web-media-addr",
            "eth0",
        ]);
        let policy = web_media_bind_policy(&args).expect("policy");
        assert_eq!(
            policy.selectors,
            vec![
                BindSelector::Addr("192.168.1.42".parse::<IpAddr>().expect("addr")),
                BindSelector::Interface("eth0".to_string()),
            ]
        );
    }

    #[test]
    fn media_addr_accepts_loopback_explicitly() {
        // The loopback-only opt-in: the default policy filters
        // loopback, and naming it is the override.
        let args = web_args(&["--web-media-addr", "127.0.0.1"]);
        let policy = web_media_bind_policy(&args).expect("loopback is a supported explicit choice");
        assert_eq!(
            policy.selectors,
            vec![BindSelector::Addr(
                "127.0.0.1".parse::<IpAddr>().expect("addr")
            )]
        );
    }

    #[test]
    fn media_addr_rejects_the_wildcard_at_startup() {
        // 0.0.0.0 binds happily and then advertises itself as an ICE
        // candidate every browser discards, so it must fail the
        // launch rather than the first offer.
        let args = web_args(&["--web-media-addr", "0.0.0.0"]);
        let err = web_media_bind_policy(&args).expect_err("0.0.0.0 must be refused");
        assert!(
            err.to_string().contains("--web-media-addr"),
            "the error must name the flag the operator typed: {err}"
        );
    }

    #[test]
    fn media_port_defaults_to_ephemeral_and_can_be_pinned() {
        assert_eq!(
            web_media_bind_policy(&web_args(&[]))
                .expect("default policy")
                .port,
            0
        );
        assert_eq!(
            web_media_bind_policy(&web_args(&["--web-media-port", "41000"]))
                .expect("pinned policy")
                .port,
            41_000
        );
    }

    #[test]
    fn media_addr_rejects_a_zoneless_link_local_at_startup() {
        // The other half of the mechanical pair. Both docs tell the
        // operator this address needs a zone id, so the CLI has to be
        // the thing that says no.
        let args = web_args(&["--web-media-addr", "fe80::1"]);
        let err = web_media_bind_policy(&args).expect_err("a zoneless fe80::/10 must be refused");
        assert!(
            err.to_string().contains("zone id"),
            "the error must explain what is missing: {err}"
        );
    }

    #[test]
    fn media_addr_rejects_a_zone_scoped_literal_rather_than_reading_it_as_an_interface() {
        // The trap the zoneless error sets: an operator told that
        // fe80::1 needs a zone id types fe80::1%eth0, which Rust
        // cannot parse, so without this it becomes an interface name
        // and fails much later with "no interface named fe80::1%eth0".
        let args = web_args(&["--web-media-addr", "fe80::1%eth0"]);
        let err = web_media_bind_policy(&args).expect_err("a zone-scoped literal must be refused");
        let err = err.to_string();
        assert!(
            err.contains("zone-scoped"),
            "the error must name what was typed, not what it was demoted to: {err}"
        );
        assert!(
            err.contains("eth0"),
            "and must point at the interface-name form as the fix: {err}"
        );
    }

    #[test]
    fn media_addr_rejects_malformed_literals_rather_than_demoting_them() {
        // An address typo is not an interface name. Without this it
        // becomes one, and the operator is told no interface has that
        // name — for a value that is visibly an address.
        for value in ["2001:db8::zz", "192.168.1.999", "10.0.0."] {
            let err = web_media_bind_policy(&web_args(&["--web-media-addr", value]))
                .expect_err("a malformed address literal must be refused");
            assert!(
                err.to_string().contains("interface name"),
                "the error must say why it is not being read as a name: {err}"
            );
        }
    }

    #[test]
    fn media_addr_still_accepts_the_interface_names_that_look_like_addresses() {
        // `eth0:0` is a real IPv4 alias label and getifaddrs reports
        // it as the interface name, so the malformed-literal check
        // must not swallow a single colon. A bare number is also a
        // legal interface name, so digits alone are not enough to
        // call something a failed address either.
        for name in ["eth0:0", "123"] {
            let policy = web_media_bind_policy(&web_args(&["--web-media-addr", name]))
                .unwrap_or_else(|e| panic!("`{name}` is a legal interface name: {e}"));
            assert_eq!(
                policy.selectors,
                vec![BindSelector::Interface(name.to_string())]
            );
        }
    }

    #[test]
    fn ice_servers_are_empty_unless_given() {
        assert!(web_ice_servers(&web_args(&[]))
            .expect("no servers")
            .is_empty());
        assert_eq!(
            web_ice_servers(&web_args(&[
                "--web-ice-server",
                "stun:stun.example.com:3478"
            ]))
            .expect("a well-formed stun URL"),
            vec!["stun:stun.example.com:3478".to_string()]
        );
    }

    #[test]
    fn ice_servers_reject_a_url_with_no_scheme() {
        // The scheme is easy to omit because the help text writes it
        // inline. An ICE server is reached for precisely when host
        // candidates do not work, so one that is silently useless
        // looks identical to WebRTC being broken.
        let err = web_ice_servers(&web_args(&["--web-ice-server", "stun.example.com:3478"]))
            .expect_err("a URL with no scheme must be refused");
        assert!(
            err.to_string().contains("--web-ice-server"),
            "the error must name the flag: {err}"
        );
    }

    #[test]
    fn ice_servers_reject_a_scheme_with_no_host() {
        let err = web_ice_servers(&web_args(&["--web-ice-server", "turn:"]))
            .expect_err("a scheme with no host must be refused");
        assert!(err.to_string().contains("names no host"), "{err}");
    }

    #[test]
    fn ice_servers_accept_every_documented_scheme() {
        for url in [
            "stun:stun.example.com:3478",
            "stuns:stun.example.com:5349",
            "turn:turn.example.com:3478",
            "turns:turn.example.com:5349",
        ] {
            web_ice_servers(&web_args(&["--web-ice-server", url]))
                .unwrap_or_else(|e| panic!("{url} should be accepted: {e}"));
        }
    }
}
