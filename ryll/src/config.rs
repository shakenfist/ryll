use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use clap::Parser;
use configparser::ini::Ini;
use shakenfist_spice_protocol::ConnectionConfig;

// Device-shaped configuration (the value types passed into the
// channel constructors) lives in the renderer crate. The
// CLI-shaped `Args` and `Config` definitions stay here, alongside
// the path-validation helpers.
pub use shakenfist_spice_renderer::device_config::{ShareDirConfig, VirtualDiskConfig};

/// Ryll - A Rust SPICE VDI test client
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
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

    /// Directory for --pedantic-mode bug reports. Created
    /// if missing.
    #[arg(long, default_value = "./ryll-pedantic-reports")]
    pub pedantic_dir: std::path::PathBuf,

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
    /// Create configuration from command line arguments
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

        Ok(Config {
            host,
            port: port.unwrap_or(0),
            tls_port,
            password,
            ca_cert,
            host_subject,
        })
    }
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
}
