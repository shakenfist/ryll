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

/// TLS certificate verifier that trusts a caller-supplied CA and, for
/// that CA alone, tolerates a hostname mismatch. SPICE self-signed
/// certificates typically lack SAN extensions, so standard hostname
/// checking always fails against a backend reached by IP. The CA
/// trust itself validates the server identity — optionally strengthened
/// by pinning the certificate subject: when an expected subject is
/// configured, the end-entity certificate must match it or the
/// handshake fails (subject pinning substitutes for hostname
/// verification, exactly as in spice-gtk).
///
/// What keeps that tolerance honest is where this verifier is
/// installed: [`needs_spice_verifier`] admits it only for a connection
/// that identifies the server by something other than its hostname.
/// A connection with neither a private CA nor a pin gets the stock
/// webpki verifier and its name check, because against the public
/// roots — which will vouch for any domain a presenter controls — the
/// name is the only thing tying a certificate to this backend.
/// [`build_root_store`] explains why the two root sets are never mixed.
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
            // Forgivable here, and only here: this verifier runs for a
            // connection that identifies the server some other way (see
            // `needs_spice_verifier`), and such a backend is routinely
            // dialled by IP while its certificate carries a hostname CN
            // and no SAN. A connection with no other identity never
            // reaches this code — it gets the stock webpki verifier, so
            // forgiving the name is not something this arm can do to it.
            Err(Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => {
                self.check_subject(end_entity)?;
                info!("TLS: accepting certificate despite hostname mismatch");
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

/// The trust anchors for a SPICE TLS connection, and which of the two
/// kinds they are. They travel together because a caller needs both:
/// the store to verify the chain against, and whether it came from a
/// `.vv` `ca=` field to decide the rest of the connection's posture.
/// Returning the decision [`build_root_store`] already made keeps it
/// from being re-derived, and drifting, at the call site.
#[derive(Debug)]
struct TrustAnchors {
    store: RootCertStore,
    /// True when `store` holds exactly the certificates a `ca=` field
    /// supplied, false when it holds the public WebPKI roots.
    private_pki: bool,
}

/// Build the trust anchor set for a SPICE TLS connection.
///
/// A `.vv` file's `ca=` field asserts a private PKI, so the certificate
/// it carries is the *whole* trust anchor set for that connection
/// rather than an addition to the public WebPKI roots. Seeding both
/// meant any certificate issued by any public CA also chained
/// successfully, and because this crate deliberately relaxes hostname
/// verification for a custom CA (see [`SpiceCaVerifier`]) while
/// `host_subject` pinning is optional — Shaken Fist publishes no
/// subject unless the operator configured one — that chain check was
/// frequently the only check a connection made. An on-path attacker
/// between the viewer and the hypervisor needed nothing more than a
/// free publicly issued certificate for a domain of their own to
/// terminate the SPICE session: full keyboard, mouse and framebuffer
/// access to the guest.
///
/// The two cases are therefore exclusive. With a custom CA the store
/// holds exactly the certificates that CA field supplied; only without
/// one do the public roots apply, which is a reachable case —
/// `--direct HOST:PORT:TLS_PORT` and a `.vv` carrying `tls-port` with
/// no `ca=` both land there, and have nothing else to trust.
fn build_root_store(ca_cert: Option<&str>) -> Result<TrustAnchors> {
    let mut store = RootCertStore::empty();

    let Some(ca_cert) = ca_cert else {
        store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        return Ok(TrustAnchors {
            store,
            private_pki: false,
        });
    };

    // The .vv ca= field contains inline PEM with literal "\n" sequences
    let pem_str = ca_cert.replace("\\n", "\n");
    let mut reader = std::io::BufReader::new(pem_str.as_bytes());
    let certs = rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()?;

    if certs.is_empty() {
        return Err(anyhow!("No certificates found in ca= field"));
    }

    for cert in certs {
        store.add(cert)?;
    }

    Ok(TrustAnchors {
        store,
        private_pki: true,
    })
}

/// Whether a connection needs [`SpiceCaVerifier`], which forgives a
/// hostname mismatch, in place of the stock webpki verifier, which does
/// not.
///
/// Hostname verification is only worth anything where the hostname is
/// what identifies the server. A SPICE backend is routinely reached by
/// IP while its certificate carries a hostname CN and no SAN at all, so
/// the name check rejects exactly the right certificate — but that is
/// only safe to forgive where the connection knows the server some
/// other way. Two things supply that: a `ca=` field, which says the
/// certificate must have been issued by this cluster's own CA, and a
/// pinned `host-subject`, which says which certificate. Either will do,
/// and `host_subject` documents the pin as substituting for the
/// hostname check exactly as spice-gtk's `cert-subject` does.
///
/// With neither, the roots are the public ones and the name is all
/// there is. Forgiving it would accept any publicly issued certificate
/// for any domain the presenter controls, so that connection keeps the
/// stock verifier and its name check.
fn needs_spice_verifier(
    anchors: &TrustAnchors,
    expected_subject: Option<&ExpectedSubject>,
) -> bool {
    anchors.private_pki || expected_subject.is_some()
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
        // A custom CA replaces the public roots rather than joining
        // them; `build_root_store` reports which case it took so the
        // question is not asked twice.
        let anchors = build_root_store(config.ca_cert.as_deref())?;

        let tls_config = if needs_spice_verifier(&anchors, expected_subject.as_ref()) {
            // SPICE certificates typically lack SAN extensions, so
            // standard hostname verification always fails against a
            // backend reached by IP. This connection identifies the
            // server another way, so use the verifier that checks the
            // chain, enforces any pinned subject, and forgives the name.
            // It delegates signature checks to the process-wide crypto
            // provider, so one must be installed before we connect.
            let provider = CryptoProvider::get_default()
                .ok_or_else(|| {
                    anyhow!(
                        "no rustls CryptoProvider installed; the embedding process must call \
                         install_default() (e.g. rustls::crypto::ring::default_provider()) \
                         before establishing a SPICE TLS connection"
                    )
                })?
                .clone();
            let verifier =
                SpiceCaVerifier::new(Arc::new(anchors.store), provider, expected_subject)?;
            ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
                .with_no_client_auth()
        } else {
            // Public roots and nothing else to go on, so the stock
            // verifier's hostname check is the identity check.
            ClientConfig::builder()
                .with_root_certificates(anchors.store)
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

    /// A minted certificate authority: both encodings of the CA
    /// certificate, plus the material needed to sign leaves under it.
    struct TestCa {
        der: Vec<u8>,
        /// The same certificate as it would appear in a `.vv` file's
        /// `ca=` field: PEM with literal `\n` escape sequences rather
        /// than real newlines, so a test that feeds this to
        /// `build_root_store` drives the unescaping production does.
        vv_ca_field: String,
        key: KeyPair,
        params: CertificateParams,
    }

    /// Mint a self-signed CA certificate with
    /// `BasicConstraints::Unconstrained`, plus the key pair and params
    /// needed to sign leaf certificates under it. `cn` names the
    /// authority: two CAs in one test must not share rcgen's default
    /// subject, or webpki matches a leaf to the wrong trust anchor by
    /// name and reports `BadSignature` where the honest answer is
    /// `UnknownIssuer`.
    fn make_ca(cn: &str) -> TestCa {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, utf8(cn));
        params.distinguished_name = dn;
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        TestCa {
            der: cert.der().to_vec(),
            vv_ca_field: cert.pem().replace('\n', "\\n"),
            key,
            params,
        }
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

    /// Mint a leaf certificate carrying a subject alternative name.
    /// The stock webpki verifier will not match a name without one, so
    /// this exists purely to give the name check something it is
    /// capable of accepting. Real SPICE certificates look like
    /// `leaf_signed_by`'s output instead, which is the whole reason
    /// `SpiceCaVerifier` exists.
    fn leaf_with_san(ca_key: &KeyPair, ca_params: &CertificateParams, san: &str) -> Vec<u8> {
        let issuer = Issuer::from_params(ca_params, ca_key);
        let params = CertificateParams::new(vec![san.to_string()]).unwrap();
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
        let ca = make_ca("cluster ca");
        let leaf = leaf_signed_by(
            &ca.key,
            &ca.params,
            &[
                (DnType::CountryName, utf8("US")),
                (DnType::OrganizationName, utf8("Kerbside CI")),
                (DnType::CommonName, utf8("hv1")),
            ],
        );
        let expected = parse_host_subject("C=US,O=Kerbside CI,CN=hv1").unwrap();
        let verifier =
            SpiceCaVerifier::new(root_store(&ca.der), crypto_provider(), Some(expected)).unwrap();

        assert!(verify(&verifier, &leaf, "localhost").is_ok());
    }

    #[test]
    fn reject_mismatching_subject() {
        let ca = make_ca("cluster ca");
        let leaf = leaf_signed_by(&ca.key, &ca.params, &[(DnType::CommonName, utf8("other"))]);
        let expected = parse_host_subject("CN=hv1").unwrap();
        let verifier =
            SpiceCaVerifier::new(root_store(&ca.der), crypto_provider(), Some(expected)).unwrap();

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
        let ca = make_ca("cluster ca");
        let leaf = leaf_signed_by(&ca.key, &ca.params, &[(DnType::CommonName, utf8("hv1"))]);
        let expected = parse_host_subject("CN=hv1").unwrap();
        let verifier =
            SpiceCaVerifier::new(root_store(&ca.der), crypto_provider(), Some(expected)).unwrap();

        assert!(verify(&verifier, &leaf, "definitely-not-the-cert.example").is_ok());
    }

    #[test]
    fn no_pin_preserves_relaxed_behaviour() {
        // Today's behaviour: with no host_subject configured, a
        // hostname mismatch against a custom-CA-signed cert is still
        // accepted (the CA trust is the only identity check).
        let ca = make_ca("cluster ca");
        let leaf = leaf_signed_by(
            &ca.key,
            &ca.params,
            &[(DnType::CommonName, utf8("whatever"))],
        );
        let verifier = SpiceCaVerifier::new(root_store(&ca.der), crypto_provider(), None).unwrap();

        assert!(verify(&verifier, &leaf, "definitely-not-the-cert.example").is_ok());
    }

    // ── Trust anchor set ────────────────────────────────────────────

    #[test]
    fn custom_ca_replaces_the_public_roots() {
        // A `.vv` `ca=` field asserts a private PKI, so it is the whole
        // trust anchor set. Adding it to the public roots instead left
        // every publicly issued certificate chaining successfully,
        // which for an unpinned connection was the only check made.
        let ca = make_ca("cluster ca");
        let anchors = build_root_store(Some(&ca.vv_ca_field)).unwrap();
        assert_eq!(anchors.store.roots.len(), 1);
        assert!(anchors.private_pki);

        // Without one there is nothing else to trust, so the public
        // roots still apply there — `--direct HOST:PORT:TLS_PORT` and a
        // `.vv` with `tls-port` and no `ca=` both reach this branch.
        let anchors = build_root_store(None).unwrap();
        assert_eq!(
            anchors.store.roots.len(),
            webpki_roots::TLS_SERVER_ROOTS.len()
        );
        assert!(anchors.store.roots.len() > 1);
        assert!(!anchors.private_pki);
    }

    #[test]
    fn certificate_outside_the_custom_ca_is_rejected() {
        // `public_ca` stands in for a public CA: no test can sign a
        // leaf under a real one, but "an issuer the `.vv` never named"
        // is exactly the property that matters. Before the narrowing,
        // the store carried the Mozilla roots too, so a certificate of
        // this shape — a free one for a domain the attacker controls —
        // chained, its name mismatch was forgiven, and an on-path
        // attacker owned the session.
        let cluster_ca = make_ca("cluster ca");
        let public_ca = make_ca("public ca");
        let leaf = leaf_signed_by(
            &public_ca.key,
            &public_ca.params,
            &[(DnType::CommonName, utf8("attacker.example"))],
        );

        let store = Arc::new(
            build_root_store(Some(&cluster_ca.vv_ca_field))
                .unwrap()
                .store,
        );
        let verifier = SpiceCaVerifier::new(store, crypto_provider(), None).unwrap();
        let err = verify(&verifier, &leaf, "attacker.example").unwrap_err();
        assert!(
            matches!(
                err,
                Error::InvalidCertificate(CertificateError::UnknownIssuer)
            ),
            "expected an unknown-issuer rejection, got {err:?}"
        );

        // The same leaf against a store that does trust its issuer is
        // accepted, so the rejection above is the trust anchor set
        // talking rather than some unrelated defect in the fixture.
        let verifier =
            SpiceCaVerifier::new(root_store(&public_ca.der), crypto_provider(), None).unwrap();
        assert!(verify(&verifier, &leaf, "attacker.example").is_ok());
    }

    #[test]
    fn cluster_ca_by_ip_still_connects() {
        // The two cases the narrowing must not disturb: a cluster CA
        // and an IP address, with and without a pinned subject. The
        // unpinned one is the common Shaken Fist deployment, where no
        // `spice_server_cert_subject` is published.
        let ca = make_ca("cluster ca");
        let leaf = leaf_signed_by(&ca.key, &ca.params, &[(DnType::CommonName, utf8("hv1"))]);

        let store = Arc::new(build_root_store(Some(&ca.vv_ca_field)).unwrap().store);
        let verifier = SpiceCaVerifier::new(store.clone(), crypto_provider(), None).unwrap();
        assert!(verify(&verifier, &leaf, "192.0.2.10").is_ok());

        let expected = parse_host_subject("CN=hv1").unwrap();
        let verifier = SpiceCaVerifier::new(store, crypto_provider(), Some(expected)).unwrap();
        assert!(verify(&verifier, &leaf, "192.0.2.10").is_ok());
    }

    #[test]
    fn build_root_store_rejects_an_unusable_ca_field() {
        // What an operator hits when a `.vv` is truncated or its PEM
        // double-escaped. Neither may fall back to the public roots.

        // Input carrying no PEM at all parses cleanly to nothing, so it
        // reaches the explicit message — the string troubleshooting.md
        // sends an operator to look for.
        for empty in ["", "not a certificate at all"] {
            let err = build_root_store(Some(empty)).unwrap_err();
            assert!(
                err.to_string()
                    .contains("No certificates found in ca= field"),
                "expected the empty-ca= message for {empty:?}, got {err}"
            );
        }

        // A PEM envelope with unusable content inside fails earlier, in
        // rustls_pemfile, and carries its message instead.
        assert!(build_root_store(Some(
            "-----BEGIN CERTIFICATE-----\\nnot-base64\\n-----END CERTIFICATE-----\\n"
        ))
        .is_err());
    }

    #[test]
    fn a_ca_field_may_carry_a_bundle() {
        // Nothing stops a producer emitting a concatenated bundle, and
        // the loop already supports it; assert it rather than leaving
        // it to be discovered.
        let first = make_ca("cluster ca one");
        let second = make_ca("cluster ca two");
        let bundle = format!("{}{}", first.vv_ca_field, second.vv_ca_field);

        let anchors = build_root_store(Some(&bundle)).unwrap();
        assert_eq!(anchors.store.roots.len(), 2);
        assert!(anchors.private_pki);
    }

    // ── Which verifier a connection gets ────────────────────────────

    #[test]
    fn the_spice_verifier_is_installed_only_where_something_else_identifies_the_server() {
        // The relaxation this verifier performs is licensed entirely by
        // where it is installed, so this is the whole gate. A `ca=`
        // field or a pinned subject each supply an identity that is not
        // the hostname; with neither there is nothing but the hostname,
        // and the connection must keep the stock verifier that checks
        // it.
        let ca = make_ca("cluster ca");
        let private = build_root_store(Some(&ca.vv_ca_field)).unwrap();
        let public = build_root_store(None).unwrap();
        let pin = parse_host_subject("CN=hv1").unwrap();

        assert!(needs_spice_verifier(&private, None));
        assert!(needs_spice_verifier(&private, Some(&pin)));
        assert!(needs_spice_verifier(&public, Some(&pin)));
        assert!(!needs_spice_verifier(&public, None));
    }

    #[test]
    fn hostname_mismatch_is_fatal_with_neither_a_custom_ca_nor_a_pin() {
        // The other half of the gate: the verifier such a connection
        // does get rejects a name mismatch. Built here the way
        // `create_tls_connector` builds it — `WebPkiServerVerifier`
        // over the anchors, no relaxation anywhere — because the
        // security claim is about the composition, not about either
        // piece alone. The CA minted here stands in for a public root
        // (see `certificate_outside_the_custom_ca_is_rejected`).
        let ca = make_ca("public ca");
        let stock =
            WebPkiServerVerifier::builder_with_provider(root_store(&ca.der), crypto_provider())
                .build()
                .unwrap();
        let check = |der: &[u8], name: &str| {
            let end_entity = CertificateDer::from(der.to_vec());
            let name = ServerName::try_from(name.to_string()).unwrap();
            stock.verify_server_cert(&end_entity, &[], &name, &[], UnixTime::now())
        };
        let name_rejection = |err: &Error| {
            matches!(
                err,
                Error::InvalidCertificate(
                    CertificateError::NotValidForName
                        | CertificateError::NotValidForNameContext { .. }
                )
            )
        };

        // A certificate of the shape SPICE servers actually present — a
        // CN and no SAN at all — is refused whatever host was dialled,
        // which is precisely what `SpiceCaVerifier` forgives and what a
        // connection with nothing else to go on must not have forgiven.
        let spice_shaped =
            leaf_signed_by(&ca.key, &ca.params, &[(DnType::CommonName, utf8("hv1"))]);
        let err = check(&spice_shaped, "hv1").unwrap_err();
        assert!(
            name_rejection(&err),
            "expected a name rejection, got {err:?}"
        );

        // One that does carry a name is accepted under it and refused
        // under another, so the rejection above is the name check
        // talking rather than an unrelated defect in the fixture.
        let named = leaf_with_san(&ca.key, &ca.params, "hv1.example");
        assert!(check(&named, "hv1.example").is_ok());
        let err = check(&named, "definitely-not-the-cert.example").unwrap_err();
        assert!(
            name_rejection(&err),
            "expected a name rejection, got {err:?}"
        );
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
