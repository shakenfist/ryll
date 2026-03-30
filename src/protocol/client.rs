/// SPICE client connection management
use anyhow::{anyhow, Result};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;
use tracing::{debug, info};

use crate::config::Config;

use super::constants::{ChannelType, SpiceError};
use super::link::{perform_auth, perform_link, SpiceStream};

/// SPICE client for managing connections to channels
pub struct SpiceClient {
    config: Config,
    tls_connector: Option<TlsConnector>,
}

impl SpiceClient {
    /// Create a new SPICE client from configuration
    pub fn new(config: Config) -> Result<Self> {
        let tls_connector = if config.tls_port.is_some() {
            Some(Self::create_tls_connector(&config)?)
        } else {
            None
        };

        Ok(SpiceClient {
            config,
            tls_connector,
        })
    }

    /// Create TLS connector with optional CA certificate
    fn create_tls_connector(config: &Config) -> Result<TlsConnector> {
        let mut root_store = RootCertStore::empty();

        // Add system roots
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

        // Add custom CA if provided (inline PEM from .vv file)
        if let Some(ca_cert) = &config.ca_cert {
            // The .vv ca= field contains inline PEM with literal "\n" sequences
            let pem_str = ca_cert.replace("\\n", "\n");
            let mut reader = std::io::BufReader::new(pem_str.as_bytes());
            let certs = rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()?;

            if certs.is_empty() {
                return Err(anyhow!("No certificates found in ca= field"));
            }

            for cert in certs {
                root_store.add(cert)?;
            }
        }

        let tls_config = ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();

        Ok(TlsConnector::from(Arc::new(tls_config)))
    }

    /// Connect to a specific channel
    pub async fn connect_channel(
        &self,
        connection_id: u32,
        channel_type: ChannelType,
        channel_id: u8,
    ) -> Result<SpiceStream> {
        // Determine if we should use TLS
        let use_tls = self.config.tls_port.is_some();
        let port = if use_tls {
            self.config.tls_port.unwrap()
        } else {
            self.config.port
        };

        let addr = format!("{}:{}", self.config.host, port);
        debug!("Connecting to {} (TLS: {})", addr, use_tls);

        // Connect TCP
        let tcp_stream = TcpStream::connect(&addr).await?;
        tcp_stream.set_nodelay(true)?;

        // Wrap in TLS if needed
        let mut stream = if use_tls {
            let connector = self
                .tls_connector
                .as_ref()
                .ok_or_else(|| anyhow!("TLS not configured"))?;

            let server_name = ServerName::try_from(self.config.host.clone())?;
            let tls_stream = connector.connect(server_name, tcp_stream).await?;
            SpiceStream::Tls(tls_stream)
        } else {
            SpiceStream::Plain(tcp_stream)
        };

        // Perform link handshake
        info!(
            "{}: performing link handshake (id={})",
            channel_type.name(),
            channel_id
        );

        let reply = perform_link(&mut stream, connection_id, channel_type, channel_id).await?;

        // Check for errors
        match reply.error {
            SpiceError::Ok => {}
            SpiceError::NeedSecured => {
                return Err(anyhow!(
                    "Server requires TLS connection. Use tls-port in config."
                ));
            }
            err => {
                return Err(anyhow!("Link error: {:?}", err));
            }
        }

        // Perform authentication
        info!("{}: authenticating...", channel_type.name());
        perform_auth(&mut stream, &reply.pub_key, self.config.password.as_deref()).await?;

        info!("{}: connected successfully", channel_type.name());

        Ok(stream)
    }
}
