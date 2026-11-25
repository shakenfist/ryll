//! Bounded wait for ICE gathering to complete.
//!
//! The bridge does not trickle ICE: `accept_offer` answers with every
//! candidate it has, so it has to wait for gathering to finish before
//! it reads the local description. That wait needs a ceiling. Without
//! one, a gathering that never reaches `Complete` — a wedged peer
//! connection driver, a STUN server that accepts the packet and never
//! answers — parks the `POST /offer` request forever, along with the
//! encoder restart that preceded it.
//!
//! The ceiling and its diagnosis live here, outside
//! [`crate::test_client`], because production needs them: the test
//! helpers had this bound from the start while `accept_offer` did
//! not, which is the wrong way round.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use webrtc::peer_connection::PeerConnection;

use crate::sticky::StickySignal;

/// How long to wait for ICE gathering to complete.
///
/// Not a performance budget: a host-candidates-only gather finishes
/// in milliseconds, and one that consults a reachable STUN server
/// takes a second or two. It is deliberately generous, because the
/// cost of being wrong in each direction is not symmetric — too short
/// fails an offer that would have worked on a slow or busy host, and
/// too long only delays the error on a gather that was never going to
/// complete.
pub const GATHER_TIMEOUT: Duration = Duration::from_secs(20);

/// How long [`wait_for_gathering`] spends reading the local
/// description for its error message. See there for why this needs a
/// bound of its own.
const GATHER_DIAGNOSIS_TIMEOUT: Duration = Duration::from_secs(2);

/// Marker in the error [`wait_for_gathering`] returns, so a caller can
/// tell a gathering timeout from the other ways accepting an offer can
/// fail without parsing prose. Ryll's `POST /offer` handler answers
/// `504 Gateway Timeout` on this, rather than reporting the browser's
/// offer as malformed.
pub const GATHERING_TIMEOUT_ERR: &str = "ice gathering did not complete";

/// Wait for `gathered` to be raised, or fail after `timeout` saying how
/// far gathering got.
///
/// Shared by [`crate::WebrtcBridge::accept_offer`], the bridge's
/// test-only offerer and `TestPeer::offer_and_gather`, which all wait
/// on the same kind of signal for the same reason. The candidate count
/// is the useful part of the error. None at all means the peer
/// connection's driver never acted on the gathering kick
/// `set_local_description` sends it; some means host candidates were
/// added but the end-of-candidates marker that moves the state to
/// `Complete` never followed.
///
/// Reading the local description takes the same lock the driver
/// holds while it works, so a wedged driver would hang the diagnosis
/// too. It gets its own short bound, and running out of that is
/// reported as what it is.
pub(crate) async fn wait_for_gathering(
    pc: &Arc<dyn PeerConnection>,
    gathered: &StickySignal,
    timeout: Duration,
) -> Result<()> {
    // Sticky wait; see `StickySignal` for the lost-wakeup reasoning
    // this encapsulates.
    if tokio::time::timeout(timeout, gathered.wait()).await.is_ok() {
        return Ok(());
    }

    let progress =
        match tokio::time::timeout(GATHER_DIAGNOSIS_TIMEOUT, pc.local_description()).await {
            Ok(Some(desc)) => {
                let candidates = desc
                    .sdp
                    .lines()
                    .filter(|line| line.starts_with("a=candidate:"))
                    .count();
                format!("{candidates} candidate line(s) in the local description so far")
            }
            Ok(None) => "no local description at all".to_owned(),
            Err(_) => "the peer connection did not yield its local description either, so its \
                   driver is probably wedged holding the lock"
                .to_owned(),
        };
    Err(anyhow!(
        "{GATHERING_TIMEOUT_ERR} within {timeout:?}: {progress}"
    ))
}
