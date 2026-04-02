use anyhow::{anyhow, Result};
use clap::Parser;
use configparser::ini::Ini;
use std::fs;
use std::path::Path;

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
    #[arg(long)]
    pub capture: Option<String>,
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
