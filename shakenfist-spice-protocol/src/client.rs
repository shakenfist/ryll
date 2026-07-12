/// SPICE client connection management
use anyhow::{anyhow, Result};
use socket2::SockRef;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::client::WebPkiServerVerifier;
use tokio_rustls::rustls::crypto::CryptoProvider;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error, RootCertStore, SignatureScheme,
};
use tokio_rustls::TlsConnector;
use tracing::{debug, info};

use crate::link::{perform_auth, perform_link, SpiceStream};
use crate::{ChannelType, ConnectionConfig, SpiceError};

/// TLS certificate verifier that trusts a custom CA but skips hostname
/// verification. SPICE self-signed certificates typically lack SAN
/// extensions, so standard hostname checking always fails. The CA
/// trust itself validates the server identity.
///
/// Signature verification is delegated to the process-wide rustls
/// [`CryptoProvider`], captured at construction. The embedding process
/// must install a default provider (e.g. `ring` or `aws-lc-rs`) before a
/// TLS connection is made; ryll and the kerbside proxy both install
/// `ring`. Capturing it here rather than hardcoding one provider keeps
/// this crate agnostic to the embedder's choice.
#[derive(Debug)]
struct SpiceCaVerifier {
    webpki: Arc<WebPkiServerVerifier>,
    provider: Arc<CryptoProvider>,
}

impl SpiceCaVerifier {
    fn new(roots: Arc<RootCertStore>, provider: Arc<CryptoProvider>) -> Result<Self> {
        let webpki =
            WebPkiServerVerifier::builder_with_provider(roots, provider.clone()).build()?;
        Ok(SpiceCaVerifier { webpki, provider })
    }
}

impl ServerCertVerifier for SpiceCaVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, Error> {
        // Verify the certificate chain against our CA roots, but
        // skip hostname checking (SPICE certs lack SAN extensions).
        match self.webpki.verify_server_cert(
            end_entity,
            intermediates,
            _server_name,
            _ocsp_response,
            now,
        ) {
            Ok(v) => Ok(v),
            // webpki only checks the name after the chain has validated,
            // so a hostname mismatch means "valid chain, wrong name".
            Err(Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => {
                info!("TLS: accepting certificate despite hostname mismatch (custom CA)");
                Ok(ServerCertVerified::assertion())
            }
            // Other errors (expired, unknown CA, bad signature) are
            // still fatal.
            Err(e) => Err(e),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        tokio_rustls::rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        tokio_rustls::rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// SPICE client for managing connections to channels
pub struct SpiceClient {
    config: ConnectionConfig,
    tls_connector: Option<TlsConnector>,
}

impl SpiceClient {
    /// Create a new SPICE client from configuration
    pub fn new(config: ConnectionConfig) -> Result<Self> {
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
    fn create_tls_connector(config: &ConnectionConfig) -> Result<TlsConnector> {
        let mut root_store = RootCertStore::empty();

        // Add system roots
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

        let has_custom_ca = config.ca_cert.is_some();

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

        let tls_config = if has_custom_ca {
            // SPICE self-signed certs typically lack SAN extensions, so
            // standard hostname verification always fails. Use a custom
            // verifier that checks the CA chain but allows hostname mismatch.
            // The verifier delegates signature checks to the process-wide
            // crypto provider, so one must be installed before we connect.
            let provider = CryptoProvider::get_default()
                .ok_or_else(|| {
                    anyhow!(
                        "no rustls CryptoProvider installed; the embedding process must call \
                         install_default() (e.g. rustls::crypto::ring::default_provider()) \
                         before establishing a SPICE TLS connection"
                    )
                })?
                .clone();
            let verifier = SpiceCaVerifier::new(Arc::new(root_store), provider)?;
            ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
                .with_no_client_auth()
        } else {
            ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth()
        };

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
        let (use_tls, port) = match self.config.tls_port {
            Some(tls_port) => (true, tls_port),
            None => (false, self.config.port),
        };

        let addr = format!("{}:{}", self.config.host, port);
        debug!("Connecting to {} (TLS: {})", addr, use_tls);

        // Connect TCP
        let tcp_stream = TcpStream::connect(&addr).await?;
        tcp_stream.set_nodelay(true)?;

        // Enable TCP keepalive to prevent NAT/firewall idle timeouts and
        // detect dead connections.  Values match spice-gtk behaviour:
        // 30 s idle before first probe, then 3 probes at 15 s intervals
        // (75 s total to detect a dead peer).
        let sock_ref = SockRef::from(&tcp_stream);
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(Duration::from_secs(30))
            .with_interval(Duration::from_secs(15))
            .with_retries(3);
        sock_ref.set_keepalive(true)?;
        sock_ref.set_tcp_keepalive(&keepalive)?;

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
