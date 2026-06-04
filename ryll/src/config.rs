use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use clap::Parser;
use configparser::ini::Ini;
use shakenfist_spice_protocol::ConnectionConfig;
use tracing::warn;

// Device-shaped configuration (the value types passed into the
// channel constructors) lives in the renderer crate. The
// CLI-shaped `Args` and `Config` definitions stay here, alongside
// the path-validation helpers.
pub use shakenfist_spice_renderer::device_config::{ShareDirConfig, VirtualDiskConfig};

/// Ryll - A Rust SPICE VDI test client
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
    /// NDJSON-based session control. Only valid with --headless;
    /// incompatible with --web and the GUI default. The socket is
    /// created with mode 0600 (owner read/write only). See
    /// ryll/docs/control-socket-protocol.md for the wire format.
    #[arg(long, value_name = "PATH", requires = "headless",
          conflicts_with = "web",
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

    /// Default directory for bug-report zip files (F8 reports,
    /// pedantic reports, and auto-disconnect snapshots).
    /// Created if missing. Each flavour can be overridden
    /// individually (e.g. by --pedantic-dir). If neither this
    /// nor a flavour-specific flag is set, the per-flavour
    /// fallback applies (cwd for F8 / auto-disconnect,
    /// ./ryll-pedantic-reports for pedantic).
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

    /// Diagnostic flag for the K1 hang investigation
    /// (PLAN-session-001-feedback Phase 02). When set, ryll's
    /// per-connection tokio runtime is built with
    /// `Builder::new_current_thread()` instead of the default
    /// multi-threaded `Runtime::new()`. Disambiguates a real
    /// blocking call (would still hang) from a multi-threaded
    /// scheduler / Waker registration bug (would not hang).
    /// Will be removed after K1 is closed.
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

    /// Maximum total bytes for the SPICE display image cache,
    /// in MiB. Defaults to 256. The cache holds decoded RGBA
    /// for images the server flagged with CACHE_ME; without a
    /// cap, video workloads can consume gigabytes (see
    /// session-002g).
    #[arg(long, default_value_t = 256)]
    pub image_cache_cap_mib: u64,

    /// Maximum total bytes for the shared SPICE GLZ dictionary,
    /// in MiB. Defaults to 256. The dictionary holds decoded
    /// RGBA for GLZ images so cross-frame references resolve;
    /// without a cap, full-screen ZlibGlzRgb workloads observed
    /// in sessions 003a / 004d-g consumed gigabytes (~30 MiB/s
    /// of growth). Phase 12E.
    #[arg(long, default_value_t = 256)]
    pub glz_dictionary_cap_mib: u64,
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
    /// Phase 5 step 5a tightened `--web` to require a real
    /// connection source (`--url` / `--file` / `--direct`).
    /// Phase 4 had returned a placeholder stub here so the
    /// HTTP server could come up without a SPICE backend; that
    /// expedient is gone now that `run_web` actually spawns
    /// `run_connection`.
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
}
