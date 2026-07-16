//! Expected-certificate-subject ("host subject") parsing and matching.
//!
//! SPICE deployments commonly pin the server's certificate identity by
//! subject DN rather than hostname, because SPICE server certificates
//! often lack SAN extensions. The expected subject travels as a string
//! of comma-separated `key=value` pairs (the `.vv` file `host-subject`
//! field, oVirt's `host.certificate.subject`, and so on).
//!
//! The parsing and comparison semantics here deliberately replicate the
//! canonical implementation shared by spice-gtk and virt-viewer:
//! `subject_to_x509_name` and `verify_subject` in spice-common's
//! `common/ssl_verify.c`. In particular:
//!
//! - A backslash escapes exactly `\` and `,`; any other character after
//!   a backslash is a parse error. An escaped comma is a literal within
//!   a value, not an entry separator.
//! - Spaces immediately before a key are skipped; all other whitespace
//!   is literal.
//! - An entry with no `=`, or with an empty value, is a parse error. A
//!   trailing comma terminates cleanly.
//! - Matching requires the certificate subject to carry exactly the
//!   same number of attributes, with the same attribute types in the
//!   same order. Values compare with leading/trailing ASCII whitespace
//!   trimmed, internal ASCII whitespace runs collapsed to one space,
//!   and ASCII case ignored — approximating OpenSSL's `X509_NAME_cmp`
//!   canonical form. Unicode case folding is deliberately out of scope.
//! - Anything the matcher cannot decode fails closed: an unsupported
//!   attribute value encoding (TeletexString, BMPString) is a mismatch,
//!   never a skip.
//!
//! Accepted attribute keys are the OpenSSL short names `C`, `ST`, `L`,
//! `O`, `OU`, `CN`, `DC`, and `emailAddress` (case-sensitive). Unknown
//! keys are a parse error so a typo can never silently weaken the pin.

use std::fmt;

use thiserror::Error;
use x509_parser::certificate::X509Certificate;
use x509_parser::oid_registry::{
    Oid, OID_DOMAIN_COMPONENT, OID_PKCS9_EMAIL_ADDRESS, OID_X509_COMMON_NAME,
    OID_X509_COUNTRY_NAME, OID_X509_LOCALITY_NAME, OID_X509_ORGANIZATIONAL_UNIT,
    OID_X509_ORGANIZATION_NAME, OID_X509_STATE_OR_PROVINCE_NAME,
};
use x509_parser::prelude::FromDer;

/// Errors from parsing an expected-subject string or matching it
/// against a certificate.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum HostSubjectError {
    /// A backslash escaped something other than `\` or `,` (including a
    /// trailing backslash at end of input).
    #[error("invalid escape in host subject: backslash may only escape '\\' and ','")]
    InvalidEscape,

    /// A `,` appeared in an entry before any `=`.
    #[error("host subject entry {0:?} has a ',' before any '=' (assignment is missing)")]
    MissingAssignment(String),

    /// The string ended part-way through an entry (a key with no `=`).
    #[error("host subject ends inside entry {0:?} (missing '=' and value)")]
    TruncatedEntry(String),

    /// An entry had an `=` but nothing after it.
    #[error("host subject key {0:?} has an empty value")]
    EmptyValue(String),

    /// A key was not one of the accepted attribute short names.
    #[error(
        "host subject key {0:?} is not a recognised attribute \
         (expected one of C, ST, L, O, OU, CN, DC, emailAddress)"
    )]
    UnknownKey(String),

    /// The certificate itself could not be parsed as DER.
    #[error("could not parse the server certificate: {0}")]
    CertificateParse(String),

    /// The certificate parsed but its subject does not match.
    #[error("certificate subject does not match expected {expected:?}: {reason}")]
    Mismatch {
        /// The original expected-subject string.
        expected: String,
        /// Which check failed, for the log line.
        reason: String,
    },
}

/// The subject attributes an expected-subject string may name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubjectAttr {
    CountryName,
    StateOrProvinceName,
    LocalityName,
    OrganizationName,
    OrganizationalUnitName,
    CommonName,
    DomainComponent,
    EmailAddress,
}

impl SubjectAttr {
    fn from_key(key: &str) -> Option<Self> {
        match key {
            "C" => Some(SubjectAttr::CountryName),
            "ST" => Some(SubjectAttr::StateOrProvinceName),
            "L" => Some(SubjectAttr::LocalityName),
            "O" => Some(SubjectAttr::OrganizationName),
            "OU" => Some(SubjectAttr::OrganizationalUnitName),
            "CN" => Some(SubjectAttr::CommonName),
            "DC" => Some(SubjectAttr::DomainComponent),
            "emailAddress" => Some(SubjectAttr::EmailAddress),
            _ => None,
        }
    }

    fn oid(&self) -> Oid<'static> {
        match self {
            SubjectAttr::CountryName => OID_X509_COUNTRY_NAME,
            SubjectAttr::StateOrProvinceName => OID_X509_STATE_OR_PROVINCE_NAME,
            SubjectAttr::LocalityName => OID_X509_LOCALITY_NAME,
            SubjectAttr::OrganizationName => OID_X509_ORGANIZATION_NAME,
            SubjectAttr::OrganizationalUnitName => OID_X509_ORGANIZATIONAL_UNIT,
            SubjectAttr::CommonName => OID_X509_COMMON_NAME,
            SubjectAttr::DomainComponent => OID_DOMAIN_COMPONENT,
            SubjectAttr::EmailAddress => OID_PKCS9_EMAIL_ADDRESS,
        }
    }

    fn short_name(&self) -> &'static str {
        match self {
            SubjectAttr::CountryName => "C",
            SubjectAttr::StateOrProvinceName => "ST",
            SubjectAttr::LocalityName => "L",
            SubjectAttr::OrganizationName => "O",
            SubjectAttr::OrganizationalUnitName => "OU",
            SubjectAttr::CommonName => "CN",
            SubjectAttr::DomainComponent => "DC",
            SubjectAttr::EmailAddress => "emailAddress",
        }
    }
}

impl fmt::Display for SubjectAttr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.short_name())
    }
}

/// A parsed expected-subject: an ordered list of attribute/value pairs,
/// with values pre-normalised for comparison.
///
/// Construct with [`parse_host_subject`]; match a certificate with
/// [`ExpectedSubject::matches_cert_der`]. `Display` renders the
/// original string the value was parsed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedSubject {
    original: String,
    entries: Vec<(SubjectAttr, String)>,
}

impl ExpectedSubject {
    /// The number of attribute entries in the expected subject.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the expected subject has no entries. `parse_host_subject`
    /// never produces this (an empty input string is itself an entryless
    /// subject, but pinning to it would match only certificates with an
    /// empty subject, which is what spice-common does too).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Check an end-entity certificate (DER) against this expected
    /// subject. `Ok(())` on match; on any failure a descriptive
    /// [`HostSubjectError`] suitable for the connection log. Anything
    /// undecodable is a failure, never a skip.
    pub fn matches_cert_der(&self, der: &[u8]) -> Result<(), HostSubjectError> {
        let (rem, cert) = X509Certificate::from_der(der)
            .map_err(|e| HostSubjectError::CertificateParse(e.to_string()))?;
        if !rem.is_empty() {
            return Err(HostSubjectError::CertificateParse(format!(
                "{} bytes of trailing data after the certificate",
                rem.len()
            )));
        }

        // spice-common compares X509_NAME_entry_count, which counts the
        // attributes of every (possibly multi-valued) RDN, so flatten
        // the same way. Note X509_NAME_cmp additionally sorts attributes
        // within a multi-valued RDN before comparing; we preserve
        // document order instead, which can only reject certificates
        // OpenSSL would accept (fail closed), and multi-valued RDNs do
        // not occur in practice in SPICE deployments.
        let avas: Vec<_> = cert.subject().iter().flat_map(|rdn| rdn.iter()).collect();

        if avas.len() != self.entries.len() {
            return Err(self.mismatch(format!(
                "certificate subject has {} attributes, expected {}",
                avas.len(),
                self.entries.len()
            )));
        }

        for (i, (ava, (attr, want))) in avas.iter().zip(self.entries.iter()).enumerate() {
            if *ava.attr_type() != attr.oid() {
                return Err(self.mismatch(format!(
                    "attribute {} has type {}, expected {}",
                    i,
                    ava.attr_type(),
                    attr
                )));
            }
            let got = ava.as_str().map_err(|_| {
                self.mismatch(format!(
                    "attribute {i} ({attr}) has an unsupported string encoding"
                ))
            })?;
            if normalise(got) != *want {
                return Err(self.mismatch(format!(
                    "attribute {i} ({attr}) value {got:?} does not match"
                )));
            }
        }
        Ok(())
    }

    fn mismatch(&self, reason: String) -> HostSubjectError {
        HostSubjectError::Mismatch {
            expected: self.original.clone(),
            reason,
        }
    }
}

impl fmt::Display for ExpectedSubject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.original)
    }
}

/// Normalise a subject attribute value for comparison: trim
/// leading/trailing ASCII whitespace, collapse internal ASCII
/// whitespace runs to a single space, and fold ASCII case.
fn normalise(value: &str) -> String {
    value
        .split(|c: char| c.is_ascii_whitespace())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// Parse an expected-subject string of comma-separated `key=value`
/// pairs, per spice-common's `subject_to_x509_name` (see the module
/// documentation for the exact rules).
pub fn parse_host_subject(subject: &str) -> Result<ExpectedSubject, HostSubjectError> {
    let mut entries = Vec::new();
    let mut key = String::new();
    let mut value = String::new();
    let mut in_value = false;

    let mut chars = subject.chars();
    loop {
        let mut escaped = false;
        let mut c = chars.next();
        if c == Some('\\') {
            match chars.next() {
                next @ (Some('\\') | Some(',')) => {
                    c = next;
                    escaped = true;
                }
                _ => return Err(HostSubjectError::InvalidEscape),
            }
        }

        if !in_value {
            // Reading a key.
            match c {
                Some(' ') if key.is_empty() => continue,
                None => {
                    if key.is_empty() {
                        break;
                    }
                    return Err(HostSubjectError::TruncatedEntry(key));
                }
                Some(',') if !escaped => {
                    return Err(HostSubjectError::MissingAssignment(key));
                }
                Some('=') if !escaped => {
                    in_value = true;
                }
                Some(ch) => key.push(ch),
            }
        } else {
            // Reading a value. An unescaped ',' or the end of input
            // terminates the entry; everything else is a literal.
            let terminates = match c {
                None => true,
                Some(',') => !escaped,
                _ => false,
            };
            if terminates {
                if value.is_empty() {
                    return Err(HostSubjectError::EmptyValue(key));
                }
                let attr = SubjectAttr::from_key(&key)
                    .ok_or_else(|| HostSubjectError::UnknownKey(key.clone()))?;
                entries.push((attr, normalise(&value)));
                if c.is_none() {
                    break;
                }
                key.clear();
                value.clear();
                in_value = false;
            } else if let Some(ch) = c {
                value.push(ch);
            }
        }
    }

    Ok(ExpectedSubject {
        original: subject.to_string(),
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use rcgen::string::BmpString;
    use rcgen::{CertificateParams, DistinguishedName, DnType, DnValue, KeyPair};

    // ── Parser ──────────────────────────────────────────────────────

    #[test]
    fn parse_simple_single_entry() {
        let s = parse_host_subject("CN=myhost").unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s.to_string(), "CN=myhost");
    }

    #[test]
    fn parse_multi_entry() {
        let s = parse_host_subject("C=US,O=Shaken Fist,CN=hv1").unwrap();
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn parse_escaped_comma_is_literal() {
        let s = parse_host_subject("O=Acme\\, Inc,CN=hv").unwrap();
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn parse_escaped_backslash_is_literal() {
        let s = parse_host_subject("CN=back\\\\slash").unwrap();
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn parse_skips_spaces_before_keys() {
        let s = parse_host_subject("  CN=a").unwrap();
        assert_eq!(s.len(), 1);
        let s = parse_host_subject("C=US, CN=a").unwrap();
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn parse_trailing_comma_terminates_cleanly() {
        let s = parse_host_subject("CN=a,").unwrap();
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn parse_empty_string_is_entryless() {
        let s = parse_host_subject("").unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn parse_rejects_bad_escape() {
        assert_eq!(
            parse_host_subject("CN=a\\b").unwrap_err(),
            HostSubjectError::InvalidEscape
        );
        // A trailing backslash escapes end-of-input: also an error.
        assert_eq!(
            parse_host_subject("CN=a\\").unwrap_err(),
            HostSubjectError::InvalidEscape
        );
    }

    #[test]
    fn parse_rejects_empty_value() {
        assert_eq!(
            parse_host_subject("CN=").unwrap_err(),
            HostSubjectError::EmptyValue("CN".to_string())
        );
        assert_eq!(
            parse_host_subject("CN=,O=x").unwrap_err(),
            HostSubjectError::EmptyValue("CN".to_string())
        );
    }

    #[test]
    fn parse_rejects_missing_assignment() {
        assert_eq!(
            parse_host_subject("CN,O=x").unwrap_err(),
            HostSubjectError::MissingAssignment("CN".to_string())
        );
        // A leading comma is a missing assignment for an empty key.
        assert_eq!(
            parse_host_subject(",CN=x").unwrap_err(),
            HostSubjectError::MissingAssignment(String::new())
        );
    }

    #[test]
    fn parse_rejects_truncated_entry() {
        assert_eq!(
            parse_host_subject("CN").unwrap_err(),
            HostSubjectError::TruncatedEntry("CN".to_string())
        );
        assert_eq!(
            parse_host_subject("CN=a,O").unwrap_err(),
            HostSubjectError::TruncatedEntry("O".to_string())
        );
    }

    #[test]
    fn parse_rejects_unknown_key() {
        assert_eq!(
            parse_host_subject("XX=a").unwrap_err(),
            HostSubjectError::UnknownKey("XX".to_string())
        );
        // Keys are case-sensitive short names.
        assert_eq!(
            parse_host_subject("cn=a").unwrap_err(),
            HostSubjectError::UnknownKey("cn".to_string())
        );
        // An empty key mid-string is unknown too.
        assert_eq!(
            parse_host_subject("CN=a,=b").unwrap_err(),
            HostSubjectError::UnknownKey(String::new())
        );
    }

    // ── Matcher ─────────────────────────────────────────────────────

    /// The emailAddress attribute (1.2.840.113549.1.9.1); rcgen has no
    /// named DnType for it.
    fn email_dn_type() -> DnType {
        DnType::CustomDnType(vec![1, 2, 840, 113549, 1, 9, 1])
    }

    /// The domainComponent attribute (0.9.2342.19200300.100.1.25).
    fn dc_dn_type() -> DnType {
        DnType::CustomDnType(vec![0, 9, 2342, 19200300, 100, 1, 25])
    }

    /// Mint a self-signed certificate whose subject carries exactly the
    /// given attributes, in the given order, and return its DER.
    fn cert_with(entries: &[(DnType, DnValue)]) -> Vec<u8> {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        let mut dn = DistinguishedName::new();
        for (ty, value) in entries {
            dn.push(ty.clone(), value.clone());
        }
        params.distinguished_name = dn;
        let key = KeyPair::generate().unwrap();
        params.self_signed(&key).unwrap().der().to_vec()
    }

    fn utf8(s: &str) -> DnValue {
        DnValue::Utf8String(s.to_string())
    }

    #[test]
    fn match_exact() {
        let der = cert_with(&[
            (DnType::CountryName, utf8("US")),
            (DnType::OrganizationName, utf8("Shaken Fist")),
            (DnType::CommonName, utf8("hv1")),
        ]);
        let s = parse_host_subject("C=US,O=Shaken Fist,CN=hv1").unwrap();
        assert_eq!(s.matches_cert_der(&der), Ok(()));
    }

    #[test]
    fn match_ignores_ascii_case() {
        let der = cert_with(&[(DnType::CommonName, utf8("HV1"))]);
        let s = parse_host_subject("CN=hv1").unwrap();
        assert_eq!(s.matches_cert_der(&der), Ok(()));
    }

    #[test]
    fn match_collapses_whitespace_runs() {
        let der = cert_with(&[(DnType::OrganizationName, utf8("Shaken  Fist "))]);
        let s = parse_host_subject("O=Shaken Fist").unwrap();
        assert_eq!(s.matches_cert_der(&der), Ok(()));
    }

    #[test]
    fn match_escaped_comma_value() {
        let der = cert_with(&[
            (DnType::OrganizationName, utf8("Acme, Inc")),
            (DnType::CommonName, utf8("hv")),
        ]);
        let s = parse_host_subject("O=Acme\\, Inc,CN=hv").unwrap();
        assert_eq!(s.matches_cert_der(&der), Ok(()));
    }

    #[test]
    fn match_email_and_dc_attributes() {
        let der = cert_with(&[
            (dc_dn_type(), utf8("example")),
            (email_dn_type(), utf8("ops@example.com")),
            (DnType::CommonName, utf8("hv1")),
        ]);
        let s = parse_host_subject("DC=example,emailAddress=ops@example.com,CN=hv1").unwrap();
        assert_eq!(s.matches_cert_der(&der), Ok(()));
    }

    fn assert_mismatch(result: Result<(), HostSubjectError>, reason_contains: &str) {
        match result {
            Err(HostSubjectError::Mismatch { reason, .. }) => {
                assert!(
                    reason.contains(reason_contains),
                    "reason {reason:?} does not contain {reason_contains:?}"
                );
            }
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn mismatch_wrong_value() {
        let der = cert_with(&[(DnType::CommonName, utf8("other"))]);
        let s = parse_host_subject("CN=hv1").unwrap();
        assert_mismatch(s.matches_cert_der(&der), "does not match");
    }

    #[test]
    fn mismatch_extra_attribute_in_cert() {
        let der = cert_with(&[
            (DnType::CountryName, utf8("US")),
            (DnType::OrganizationName, utf8("Shaken Fist")),
            (DnType::CommonName, utf8("hv1")),
        ]);
        let s = parse_host_subject("C=US,CN=hv1").unwrap();
        assert_mismatch(s.matches_cert_der(&der), "3 attributes, expected 2");
    }

    #[test]
    fn mismatch_missing_attribute_in_cert() {
        let der = cert_with(&[(DnType::CommonName, utf8("hv1"))]);
        let s = parse_host_subject("C=US,CN=hv1").unwrap();
        assert_mismatch(s.matches_cert_der(&der), "1 attributes, expected 2");
    }

    #[test]
    fn mismatch_reordered_attributes() {
        let der = cert_with(&[
            (DnType::OrganizationName, utf8("Shaken Fist")),
            (DnType::CommonName, utf8("hv1")),
        ]);
        let s = parse_host_subject("CN=hv1,O=Shaken Fist").unwrap();
        assert_mismatch(s.matches_cert_der(&der), "has type");
    }

    #[test]
    fn mismatch_undecodable_value_encoding_fails_closed() {
        let der = cert_with(&[(
            DnType::CommonName,
            DnValue::BmpString(BmpString::try_from("hv1").unwrap()),
        )]);
        let s = parse_host_subject("CN=hv1").unwrap();
        assert_mismatch(s.matches_cert_der(&der), "unsupported string encoding");
    }

    #[test]
    fn garbage_der_is_a_parse_error() {
        let s = parse_host_subject("CN=hv1").unwrap();
        match s.matches_cert_der(b"not a certificate") {
            Err(HostSubjectError::CertificateParse(_)) => {}
            other => panic!("expected CertificateParse, got {other:?}"),
        }
    }
}
