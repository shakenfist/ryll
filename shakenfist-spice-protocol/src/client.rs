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
use tracing::{debug, info, warn};

use crate::host_subject::{parse_host_subject, ExpectedSubject};
use crate::link::{perform_auth, perform_link, SpiceStream};
use crate::{ChannelType, ConnectionConfig, SpiceError};

/// TLS certificate verifier that trusts a custom CA but skips hostname
/// verification. SPICE self-signed certificates typically lack SAN
/// extensions, so standard hostname checking always fails. The CA
/// trust itself validates the server identity — optionally strengthened
/// by pinning the certificate subject: when an expected subject is
/// configured, the end-entity certificate must match it or the
/// handshake fails (subject pinning substitutes for hostname
/// verification, exactly as in spice-gtk).
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
    expected_subject: Option<ExpectedSubject>,
}

impl SpiceCaVerifier {
    fn new(
        roots: Arc<RootCertStore>,
        provider: Arc<CryptoProvider>,
        expected_subject: Option<ExpectedSubject>,
    ) -> Result<Self> {
        let webpki =
            WebPkiServerVerifier::builder_with_provider(roots, provider.clone()).build()?;
        Ok(SpiceCaVerifier {
            webpki,
            provider,
            expected_subject,
        })
    }

    /// Enforce the pinned subject, if one is configured, against the
    /// end-entity certificate. Runs only after the chain has validated;
    /// any failure (mismatch, undecodable subject) rejects the
    /// handshake — fail closed, never a skip.
    fn check_subject(&self, end_entity: &CertificateDer<'_>) -> std::result::Result<(), Error> {
        let Some(expected) = &self.expected_subject else {
            return Ok(());
        };
        match expected.matches_cert_der(end_entity.as_ref()) {
            Ok(()) => {
                debug!("TLS: certificate subject matches pinned host_subject {expected}");
                Ok(())
            }
            Err(e) => {
                warn!("TLS: rejecting certificate: pinned host_subject {expected}: {e}");
                Err(Error::InvalidCertificate(CertificateError::NotValidForName))
            }
        }
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
        // The pinned-subject check (if configured) runs on every
        // accept path, so it cannot be bypassed by a certificate
        // that happens to carry a matching SAN.
        match self.webpki.verify_server_cert(
            end_entity,
            intermediates,
            _server_name,
            _ocsp_response,
            now,
        ) {
            Ok(v) => {
                self.check_subject(end_entity)?;
                Ok(v)
            }
            // webpki only checks the name after the chain has validated,
            // so a hostname mismatch means "valid chain, wrong name".
            Err(Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => {
                self.check_subject(end_entity)?;
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
    /// Create a new SPICE client from configuration.
    ///
    /// Fails if `config.host_subject` is set but malformed: a broken
    /// pin must refuse to start rather than silently downgrade to an
    /// unpinned connection. The pin is validated even when no TLS port
    /// is configured yet, so the error surfaces on the first (possibly
    /// plaintext) connection attempt, not only after a `need_secured`
    /// retry upgrades to TLS.
    pub fn new(config: ConnectionConfig) -> Result<Self> {
        let expected_subject = config
            .host_subject
            .as_deref()
            .map(parse_host_subject)
            .transpose()
            .map_err(|e| anyhow!("refusing to connect with a malformed host_subject: {e}"))?;

        let tls_connector = if config.tls_port.is_some() {
            Some(Self::create_tls_connector(&config, expected_subject)?)
        } else {
            None
        };

        Ok(SpiceClient {
            config,
            tls_connector,
        })
    }

    /// Create TLS connector with optional CA certificate and optional
    /// pinned certificate subject.
    fn create_tls_connector(
        config: &ConnectionConfig,
        expected_subject: Option<ExpectedSubject>,
    ) -> Result<TlsConnector> {
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

        let tls_config = if has_custom_ca || expected_subject.is_some() {
            // SPICE self-signed certs typically lack SAN extensions, so
            // standard hostname verification always fails. Use a custom
            // verifier that checks the CA chain but allows hostname mismatch;
            // when a host_subject is pinned, the verifier enforces it in
            // place of the hostname check (so the custom verifier is also
            // needed when a subject is pinned without a custom CA).
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
            let verifier = SpiceCaVerifier::new(Arc::new(root_store), provider, expected_subject)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, DnValue, IsCa, Issuer,
        KeyPair,
    };

    // ── Test helpers ────────────────────────────────────────────────

    /// A CryptoProvider for `SpiceCaVerifier::new`. Tests pass it
    /// directly rather than going through the process-wide
    /// `CryptoProvider::install_default()`/`get_default()` machinery:
    /// `SpiceCaVerifier` only uses whatever provider it is handed, and
    /// none of these tests exercise `SpiceClient::create_tls_connector`
    /// (the only caller that consults the global default), so a fresh
    /// instance per test avoids any cross-test install race entirely.
    fn crypto_provider() -> Arc<CryptoProvider> {
        Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider())
    }

    fn utf8(s: &str) -> DnValue {
        DnValue::Utf8String(s.to_string())
    }

    /// Mint a self-signed CA certificate (DER) with
    /// `BasicConstraints::Unconstrained`, plus the key pair and params
    /// needed to sign leaf certificates under it.
    fn make_ca() -> (Vec<u8>, KeyPair, CertificateParams) {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let key = KeyPair::generate().unwrap();
        let der = params.self_signed(&key).unwrap().der().to_vec();
        (der, key, params)
    }

    /// Mint a leaf certificate signed by the given CA, carrying exactly
    /// the given subject attributes in order and no SAN entries at all
    /// (real SPICE server certificates typically lack SANs, which is
    /// exactly what makes hostname verification unusable for them).
    fn leaf_signed_by(
        ca_key: &KeyPair,
        ca_params: &CertificateParams,
        entries: &[(DnType, DnValue)],
    ) -> Vec<u8> {
        let issuer = Issuer::from_params(ca_params, ca_key);
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        let mut dn = DistinguishedName::new();
        for (ty, value) in entries {
            dn.push(ty.clone(), value.clone());
        }
        params.distinguished_name = dn;
        let leaf_key = KeyPair::generate().unwrap();
        params.signed_by(&leaf_key, &issuer).unwrap().der().to_vec()
    }

    /// Build a root store trusting exactly the given CA DER.
    fn root_store(ca_der: &[u8]) -> Arc<RootCertStore> {
        let mut store = RootCertStore::empty();
        store.add(CertificateDer::from(ca_der.to_vec())).unwrap();
        Arc::new(store)
    }

    /// Run `verify_server_cert` on a lone end-entity certificate (no
    /// intermediates, no OCSP response) against the given server name.
    fn verify(
        verifier: &SpiceCaVerifier,
        leaf_der: &[u8],
        server_name: &str,
    ) -> std::result::Result<ServerCertVerified, Error> {
        let end_entity = CertificateDer::from(leaf_der.to_vec());
        let name = ServerName::try_from(server_name.to_string()).unwrap();
        verifier.verify_server_cert(&end_entity, &[], &name, &[], UnixTime::now())
    }

    // ── SpiceCaVerifier ─────────────────────────────────────────────

    #[test]
    fn accept_matching_subject() {
        let (ca_der, ca_key, ca_params) = make_ca();
        let leaf = leaf_signed_by(
            &ca_key,
            &ca_params,
            &[
                (DnType::CountryName, utf8("US")),
                (DnType::OrganizationName, utf8("Kerbside CI")),
                (DnType::CommonName, utf8("hv1")),
            ],
        );
        let expected = parse_host_subject("C=US,O=Kerbside CI,CN=hv1").unwrap();
        let verifier =
            SpiceCaVerifier::new(root_store(&ca_der), crypto_provider(), Some(expected)).unwrap();

        assert!(verify(&verifier, &leaf, "localhost").is_ok());
    }

    #[test]
    fn reject_mismatching_subject() {
        let (ca_der, ca_key, ca_params) = make_ca();
        let leaf = leaf_signed_by(&ca_key, &ca_params, &[(DnType::CommonName, utf8("other"))]);
        let expected = parse_host_subject("CN=hv1").unwrap();
        let verifier =
            SpiceCaVerifier::new(root_store(&ca_der), crypto_provider(), Some(expected)).unwrap();

        let result = verify(&verifier, &leaf, "localhost");
        assert!(matches!(
            result,
            Err(Error::InvalidCertificate(CertificateError::NotValidForName))
        ));
    }

    #[test]
    fn subject_pin_substitutes_for_hostname() {
        // Every leaf here has no SAN entries, so webpki's hostname check
        // always fails and every accept flows through the
        // NotValidForName arm — the pinned subject is what actually
        // gates acceptance, exactly as it substitutes for hostname
        // verification in spice-gtk. Use a server name that cannot
        // possibly match anything to make that arm unambiguous, and
        // confirm the pin still lets a subject-matching certificate
        // through.
        let (ca_der, ca_key, ca_params) = make_ca();
        let leaf = leaf_signed_by(&ca_key, &ca_params, &[(DnType::CommonName, utf8("hv1"))]);
        let expected = parse_host_subject("CN=hv1").unwrap();
        let verifier =
            SpiceCaVerifier::new(root_store(&ca_der), crypto_provider(), Some(expected)).unwrap();

        assert!(verify(&verifier, &leaf, "definitely-not-the-cert.example").is_ok());
    }

    #[test]
    fn no_pin_preserves_relaxed_behaviour() {
        // Today's behaviour: with no host_subject configured, a
        // hostname mismatch against a custom-CA-signed cert is still
        // accepted (the CA trust is the only identity check).
        let (ca_der, ca_key, ca_params) = make_ca();
        let leaf = leaf_signed_by(
            &ca_key,
            &ca_params,
            &[(DnType::CommonName, utf8("whatever"))],
        );
        let verifier = SpiceCaVerifier::new(root_store(&ca_der), crypto_provider(), None).unwrap();

        assert!(verify(&verifier, &leaf, "definitely-not-the-cert.example").is_ok());
    }

    // ── SpiceClient::new ────────────────────────────────────────────

    #[test]
    fn spice_client_new_rejects_malformed_pin() {
        let config = ConnectionConfig {
            host_subject: Some("CN=".into()),
            tls_port: None,
            ..Default::default()
        };
        // SpiceClient does not derive Debug, so unwrap_err() (which
        // requires the Ok side to be Debug too) is not available here.
        let err = match SpiceClient::new(config) {
            Err(e) => e,
            Ok(_) => panic!("expected malformed host_subject to be rejected"),
        };
        assert!(
            err.to_string().contains("host_subject"),
            "error {err} does not mention host_subject"
        );

        let config = ConnectionConfig {
            host_subject: None,
            tls_port: None,
            ..Default::default()
        };
        assert!(SpiceClient::new(config).is_ok());
    }
}
