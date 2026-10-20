//! The server's TLS identity: a certificate and key loaded from PEM
//! files, or a self-signed certificate generated at launch, with the
//! SHA-256 fingerprint a user can match against what the RDP client
//! shows.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use ironrdp_server::TlsIdentityCtx;
use sha2::{Digest, Sha256};
use tokio_rustls::rustls::crypto::CryptoProvider;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

/// The certificate and private key the RDP server presents, and what
/// is derived from them.
///
/// Both RDP security modes run inside TLS, so every server needs one.
/// The public key is kept separately because CredSSP (NLA) binds its
/// authentication to the certificate's public key.
///
/// [`acceptor`](Self::acceptor) uses the process-wide rustls
/// [`CryptoProvider`], which the embedding process must install before
/// it builds one, as for the SPICE side's TLS. This crate does not
/// pick a provider on the embedder's behalf.
pub struct TlsIdentity {
    /// The certificate chain, leaf first.
    certs: Vec<CertificateDer<'static>>,
    priv_key: PrivateKeyDer<'static>,
    /// The contents of the leaf certificate's SubjectPublicKeyInfo
    /// BIT STRING, which is what CredSSP's public key check compares.
    pub(crate) pub_key: Vec<u8>,
    fingerprint: String,
}

impl TlsIdentity {
    /// Load a certificate chain and private key from PEM files.
    ///
    /// The certificate file may hold a chain, leaf first. A key file
    /// whose name ends in `.pem` may hold a PKCS#1, PKCS#8 or SEC1
    /// key; any other key file must hold a PKCS#8 key.
    pub fn from_pem_files(cert: &Path, key: &Path) -> Result<Self> {
        let ctx = TlsIdentityCtx::init_from_paths(cert, key)
            .with_context(|| format!("loading the RDP TLS identity from {cert:?} and {key:?}"))?;
        let leaf = ctx
            .certs
            .first()
            .with_context(|| format!("{cert:?} holds no certificate"))?;
        let fingerprint = sha256_fingerprint(leaf.as_ref());
        Ok(Self {
            certs: ctx.certs,
            priv_key: ctx.priv_key,
            pub_key: ctx.pub_key,
            fingerprint,
        })
    }

    /// Generate a self-signed certificate valid for `names` (host
    /// names or IP addresses as text), with a fresh key pair.
    ///
    /// The certificate exists only in memory for the life of the
    /// process, so every launch has a new fingerprint. An operator who
    /// wants clients to remember the certificate supplies one with
    /// [`from_pem_files`](Self::from_pem_files) instead.
    pub fn self_signed(names: &[String]) -> Result<Self> {
        let generated = rcgen::generate_simple_self_signed(names.to_vec())
            .context("generating a self-signed RDP certificate")?;
        let leaf = generated.cert.der().clone();
        // The raw public key is exactly what the certificate's
        // SubjectPublicKeyInfo carries in its BIT STRING.
        let pub_key = generated.signing_key.public_key_raw().to_vec();
        let priv_key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(
            generated.signing_key.serialize_der(),
        ));
        let fingerprint = sha256_fingerprint(leaf.as_ref());
        Ok(Self {
            certs: vec![leaf],
            priv_key,
            pub_key,
            fingerprint,
        })
    }

    /// A TLS acceptor presenting this identity.
    ///
    /// Fails if no process-wide rustls crypto provider is installed,
    /// or if the key does not match the certificate.
    pub fn acceptor(&self) -> Result<TlsAcceptor> {
        let provider = CryptoProvider::get_default().context(
            "no rustls crypto provider is installed; the process must install one \
             (for example rustls::crypto::ring::default_provider().install_default()) \
             before starting the RDP server",
        )?;
        let config = ServerConfig::builder_with_provider(Arc::clone(provider))
            .with_safe_default_protocol_versions()
            .context("the installed rustls crypto provider supports no safe TLS version")?
            .with_no_client_auth()
            .with_single_cert(self.certs.clone(), self.priv_key.clone_key())
            .context("the RDP TLS certificate and key are unusable or do not match")?;
        Ok(TlsAcceptor::from(Arc::new(config)))
    }

    /// The SHA-256 fingerprint of the leaf certificate, as colon-separated
    /// upper-case hex byte pairs: the form FreeRDP and mstsc show when
    /// they ask whether to trust a certificate.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

impl fmt::Debug for TlsIdentity {
    /// Shows the fingerprint only, so the private key cannot reach a
    /// log through a debug print.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsIdentity")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

/// SHA-256 of `der`, as colon-separated upper-case hex byte pairs.
fn sha256_fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        vec!["127.0.0.1".to_string(), "localhost".to_string()]
    }

    #[test]
    fn self_signed_fingerprint_is_32_colon_separated_hex_bytes() {
        let identity = TlsIdentity::self_signed(&names()).expect("self-signed identity");
        let parts: Vec<&str> = identity.fingerprint().split(':').collect();
        assert_eq!(parts.len(), 32, "{}", identity.fingerprint());
        for part in parts {
            assert_eq!(part.len(), 2, "{}", identity.fingerprint());
            assert!(
                part.chars()
                    .all(|c| c.is_ascii_digit() || ('A'..='F').contains(&c)),
                "{}",
                identity.fingerprint()
            );
        }
    }

    #[test]
    fn self_signed_identities_differ_per_launch() {
        let first = TlsIdentity::self_signed(&names()).expect("self-signed identity");
        let second = TlsIdentity::self_signed(&names()).expect("self-signed identity");
        assert_ne!(first.fingerprint(), second.fingerprint());
    }

    #[test]
    fn self_signed_public_key_is_in_the_certificate() {
        let identity = TlsIdentity::self_signed(&names()).expect("self-signed identity");
        let der = identity.certs[0].as_ref();
        assert!(!identity.pub_key.is_empty());
        assert!(
            der.windows(identity.pub_key.len())
                .any(|window| window == identity.pub_key.as_slice()),
            "the public key CredSSP checks must be the certificate's"
        );
    }

    #[test]
    fn fingerprint_matches_known_digest() {
        // SHA-256 of the empty string.
        assert_eq!(
            sha256_fingerprint(b""),
            "E3:B0:C4:42:98:FC:1C:14:9A:FB:F4:C8:99:6F:B9:24:\
             27:AE:41:E4:64:9B:93:4C:A4:95:99:1B:78:52:B8:55"
        );
    }

    #[test]
    fn debug_does_not_print_the_key() {
        let identity = TlsIdentity::self_signed(&names()).expect("self-signed identity");
        let printed = format!("{identity:?}");
        assert!(printed.contains(identity.fingerprint()));
        assert!(!printed.contains("priv_key"));
    }

    #[test]
    fn pem_files_round_trip_to_the_same_fingerprint() {
        let generated = rcgen::generate_simple_self_signed(names()).expect("rcgen");
        let dir = std::env::temp_dir().join(format!(
            "shakenfist-spice-rdp-tls-{}-{}",
            std::process::id(),
            crate::generate_password()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        std::fs::write(&cert_path, generated.cert.pem()).expect("write cert");
        std::fs::write(&key_path, generated.signing_key.serialize_pem()).expect("write key");

        let loaded = TlsIdentity::from_pem_files(&cert_path, &key_path);
        let _ = std::fs::remove_dir_all(&dir);
        let loaded = loaded.expect("load PEM identity");

        assert_eq!(
            loaded.fingerprint(),
            sha256_fingerprint(generated.cert.der().as_ref())
        );
        assert_eq!(loaded.pub_key, generated.signing_key.public_key_raw());
    }

    #[test]
    fn acceptor_builds_once_a_provider_is_installed() {
        let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
        let identity = TlsIdentity::self_signed(&names()).expect("self-signed identity");
        identity.acceptor().expect("acceptor");
    }
}
