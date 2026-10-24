//! Credential checking: compares the username and password an RDP
//! client presents with the configured pair, ignoring the domain,
//! which clients fill inconsistently.

use std::sync::Arc;

use ironrdp_server::{
    CredentialDecision, CredentialValidationError, CredentialValidator, Credentials,
};
use rand::distr::{Alphanumeric, SampleString};
use tracing::{info, warn};

use crate::{Rejected, SessionHandles, SessionProvider, SessionSlot};

/// Length of a password minted by [`generate_password`]. 24 characters
/// drawn from 62 give about 143 bits, far beyond what an online
/// guessing attack against one listener can reach.
const GENERATED_PASSWORD_LEN: usize = 24;

/// A [`SessionProvider`] that serves one SPICE session to one
/// username and password.
///
/// The domain a client presents is ignored. mstsc always fills one in
/// (the client machine's name or workgroup), FreeRDP may send none,
/// and a SPICE session has no notion of a domain, so comparing it
/// would refuse correct credentials depending on which client was
/// used.
pub struct SingleSession {
    expected: Credentials,
    handles: Arc<SessionHandles>,
}

impl SingleSession {
    /// A provider serving `handles` to clients presenting the username
    /// and password in `expected`. The domain in `expected` is not
    /// used.
    pub fn new(expected: Credentials, handles: Arc<SessionHandles>) -> Self {
        Self { expected, handles }
    }
}

impl SessionProvider for SingleSession {
    fn session(&self, credentials: &Credentials) -> Result<Arc<SessionHandles>, Rejected> {
        // Both comparisons always run, so a wrong username takes as
        // long to refuse as a wrong password.
        let username = constant_time_eq(
            credentials.username.as_bytes(),
            self.expected.username.as_bytes(),
        );
        let password = constant_time_eq(
            credentials.password.as_bytes(),
            self.expected.password.as_bytes(),
        );
        if username & password {
            Ok(Arc::clone(&self.handles))
        } else {
            Err(Rejected)
        }
    }
}

/// Mint a password for one launch: 24 random ASCII letters and
/// digits from a cryptographically secure generator seeded by the
/// operating system, so it can be typed into any RDP client.
pub fn generate_password() -> String {
    Alphanumeric.sample_string(&mut rand::rng(), GENERATED_PASSWORD_LEN)
}

/// Compare two byte strings in time that depends only on their
/// lengths, so the position of the first wrong byte of a password
/// cannot be learned from how quickly it is refused.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// IronRDP's credential check for TLS connections, answered by a
/// [`SessionProvider`].
///
/// IronRDP calls this with the credentials from the client's Client
/// Info PDU once the TLS handshake is done. An accepted client's
/// session goes into the slot the display and input handlers read; a
/// refused one leaves the slot as it was. A refusal is a decision,
/// not a failure, so this never returns an error.
///
/// This replaces IronRDP's own exact-match check, which compares the
/// domain too.
pub(crate) struct ProviderValidator {
    provider: Arc<dyn SessionProvider>,
    slot: SessionSlot,
}

impl ProviderValidator {
    /// A validator asking `provider`, and filling `slot` with the
    /// session it chooses.
    pub(crate) fn new(provider: Arc<dyn SessionProvider>, slot: SessionSlot) -> Self {
        Self { provider, slot }
    }
}

#[async_trait::async_trait]
impl CredentialValidator for ProviderValidator {
    async fn validate(
        &self,
        credentials: &Credentials,
    ) -> Result<CredentialDecision, CredentialValidationError> {
        // Only the username is logged: the password must never reach
        // a log line, at any level.
        match self.provider.session(credentials) {
            Ok(handles) => {
                info!(
                    username = %credentials.username,
                    session = %handles.label,
                    "RDP client authenticated"
                );
                self.slot.set(handles);
                Ok(CredentialDecision::Accept)
            }
            Err(Rejected) => {
                warn!(
                    username = %credentials.username,
                    "RDP client presented wrong credentials"
                );
                Ok(CredentialDecision::Reject)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::LazyLock;

    use shakenfist_spice_renderer::SurfaceMirror;
    use tokio::sync::{mpsc, watch};

    use crate::CursorState;

    /// The password the tests' session expects: minted once per test
    /// run rather than written into the source, so no literal
    /// password sits in the tree.
    static PASSWORD: LazyLock<String> = LazyLock::new(generate_password);

    /// A password of the same length as [`PASSWORD`] that differs in
    /// its first character.
    fn wrong_password() -> String {
        let mut other = PASSWORD.clone();
        let first = if other.starts_with('x') { "y" } else { "x" };
        other.replace_range(0..1, first);
        other
    }

    /// [`PASSWORD`] with every letter's case flipped.
    fn case_flipped_password() -> String {
        PASSWORD
            .chars()
            .map(|c| {
                if c.is_ascii_lowercase() {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect()
    }

    fn handles() -> Arc<SessionHandles> {
        let (input_tx, _) = mpsc::channel(1);
        let (_, mouse_mode) = watch::channel(0);
        let (_, cursor) = watch::channel(CursorState::default());
        let (resize_tx, _) = mpsc::channel(1);
        let mirror = SurfaceMirror::with_test_surface(0, 0, 16, 16, &[]);
        Arc::new(SessionHandles {
            surface_mirror: Arc::new(tokio::sync::Mutex::new(mirror)),
            input_tx,
            mouse_mode,
            resize_tx,
            cursor,
            clipboard: None,
            label: "test".to_string(),
        })
    }

    fn creds(username: &str, password: &str, domain: Option<&str>) -> Credentials {
        Credentials {
            username: username.to_string(),
            password: password.to_string(),
            domain: domain.map(str::to_string),
        }
    }

    fn validator(slot: &SessionSlot) -> ProviderValidator {
        let provider = SingleSession::new(creds("ryll", &PASSWORD, None), handles());
        ProviderValidator::new(Arc::new(provider), slot.clone())
    }

    #[tokio::test]
    async fn validator_accepts_matching_pair_with_any_domain() {
        for domain in [None, Some(""), Some("WORKGROUP"), Some("client-pc")] {
            let slot = SessionSlot::new();
            let decision = validator(&slot)
                .validate(&creds("ryll", &PASSWORD, domain))
                .await
                .expect("validator never errors");
            assert_eq!(decision, CredentialDecision::Accept, "domain {domain:?}");
            assert!(slot.current().is_some(), "domain {domain:?}");
        }
    }

    #[tokio::test]
    async fn validator_rejects_wrong_password_and_leaves_slot_empty() {
        let slot = SessionSlot::new();
        let decision = validator(&slot)
            .validate(&creds("ryll", &wrong_password(), None))
            .await
            .expect("validator never errors");
        assert_eq!(decision, CredentialDecision::Reject);
        assert!(slot.current().is_none());
    }

    #[tokio::test]
    async fn validator_rejects_wrong_username() {
        let slot = SessionSlot::new();
        let decision = validator(&slot)
            .validate(&creds("other", &PASSWORD, None))
            .await
            .expect("validator never errors");
        assert_eq!(decision, CredentialDecision::Reject);
        assert!(slot.current().is_none());
    }

    #[tokio::test]
    async fn provider_rejection_rejects_and_leaves_slot_empty() {
        struct Refuser;
        impl SessionProvider for Refuser {
            fn session(&self, _: &Credentials) -> Result<Arc<SessionHandles>, Rejected> {
                Err(Rejected)
            }
        }
        let slot = SessionSlot::new();
        let validator = ProviderValidator::new(Arc::new(Refuser), slot.clone());
        let decision = validator
            .validate(&creds("ryll", &PASSWORD, None))
            .await
            .expect("validator never errors");
        assert_eq!(decision, CredentialDecision::Reject);
        assert!(slot.current().is_none());
    }

    #[test]
    fn single_session_ignores_domain_but_not_case() {
        let provider = SingleSession::new(creds("ryll", &PASSWORD, Some("EXPECTED")), handles());
        assert!(provider
            .session(&creds("ryll", &PASSWORD, Some("OTHER")))
            .is_ok());
        assert!(provider
            .session(&creds("ryll", &case_flipped_password(), None))
            .is_err());
        assert!(provider.session(&creds("Ryll", &PASSWORD, None)).is_err());
        assert!(provider
            .session(&creds("ryll", &format!("{}2", *PASSWORD), None))
            .is_err());
    }

    #[test]
    fn generated_passwords_are_long_alphanumeric_and_distinct() {
        let first = generate_password();
        let second = generate_password();
        assert_eq!(first.len(), GENERATED_PASSWORD_LEN);
        // Plain asserts, so a failure never prints a password.
        assert!(first.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(first != second);
    }

    #[test]
    fn constant_time_eq_compares_contents_and_length() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }
}
