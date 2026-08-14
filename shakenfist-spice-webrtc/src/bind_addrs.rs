//! Choosing which local addresses to bind the WebRTC UDP sockets to.
//!
//! ## Why this exists
//!
//! webrtc-rs 0.17 binds its own sockets and enumerates the host's
//! interfaces internally, so a caller never had to think about this.
//! 0.20's `PeerConnectionBuilder::with_udp_addrs` inverts that: *we*
//! bind the sockets and hand the bound addresses in, and those
//! addresses are the only input to ICE host-candidate generation
//! (`webrtc-0.20.2/src/peer_connection/mod.rs:691-697`). There is no
//! unspecified-address filtering anywhere downstream in the stack.
//!
//! The obvious placeholder, `0.0.0.0:0`, binds without error and then
//! produces a literal `a=candidate:... 0.0.0.0 <port> typ host` in
//! the answer SDP, which every browser discards. It is also
//! undetectable by our own test suite: two Rust peers on one host
//! both see `0.0.0.0`, agree about it, and connect happily, so
//! `tests/loopback.rs` would stay green on a build no browser could
//! reach. See Decision 4 of
//! `docs/plans/PLAN-webrtc-0.20-upgrade-phase-02-bump.md` for the
//! full argument. This module is the fix: reproduce what 0.17 did
//! internally, so 0.20 gets addresses that produce routable
//! candidates.
//!
//! ## Why the filter and the enumeration are separate functions
//!
//! [`if_addrs::Interface`] carries a Windows-only `adapter_name`
//! field and derives no `Default`, so building synthetic `Interface`
//! values in a unit test would be platform-conditional and ugly.
//! [`bindable_udp_addrs`] sidesteps that entirely by taking plain
//! [`std::net::IpAddr`] values, which are trivial to construct on any
//! platform. All of the filtering policy — what counts as "worth
//! binding" — lives there, and it is exercised with synthetic
//! addresses only. [`host_udp_bind_addrs`] is a thin, essentially
//! untestable wrapper around [`if_addrs::get_if_addrs`]; keeping it
//! thin means the untested part carries no logic worth testing.
//!
//! Deliberately not covered by an automated test here: enumerating
//! the *real* interfaces of the host running the test. That is
//! exactly the host-coupled flake that phase 01 had to hide behind
//! the `RYLL_GATHERING_SOAK` environment variable — a CI runner's
//! interface set (typically just `lo` and a container bridge) says
//! nothing about a deployment's, and a test that passed only because
//! of what happened to be plugged in that day is worse than no test.
//!
//! ## What gets filtered, and why
//!
//! - **Loopback** (`127.0.0.0/8`, `::1`): only reachable from the
//!   same host, never from a browser across the network.
//! - **Unspecified** (`0.0.0.0`, `::`): the broken placeholder this
//!   module exists to replace. A bound-but-unspecified address is
//!   not a routable candidate, it is the bug.
//! - **IPv6 link-local** (`fe80::/10`): only valid alongside a zone
//!   (scope) id, which the socket address types used here — and ICE
//!   candidate SDP itself — have no way to carry. Advertising one
//!   without a zone id is ambiguous on any multi-interface host.
//!
//! Nothing else is filtered. In particular, IPv4 link-local
//! (`169.254.0.0/16`) and IPv6 unique-local (`fc00::/7`) addresses
//! are passed through: both are plain unicast addresses with well-
//! defined, unambiguous socket representations, so there is no
//! *mechanical* reason to reject them the way there is for the three
//! cases above. Whether they are *useful* candidates is a policy
//! question for whoever decides what a deployment should advertise,
//! which is phase 03's job (see Decision 4), not this module's.
//!
//! ## What an empty result means
//!
//! [`host_udp_bind_addrs`] returns an empty `Vec` rather than an
//! error — both when [`if_addrs::get_if_addrs`] itself fails and
//! when it succeeds but every address it reports gets filtered out
//! (a host with only `lo`, e.g. a bare container). Enumeration
//! failures are logged at `warn` so they are visible, but are not
//! escalated here, because the two cases collapse to the same
//! actionable fact for a caller: "no address to bind". A host with
//! only `lo` is a real deployment shape, not a malfunction, so this
//! module does not treat it as one.
//!
//! It is the *caller's* job to decide whether an empty result is
//! fatal. Step 2d constructs a peer connection from this list, and a
//! peer connection that can never produce a routable candidate is
//! useless — that construction should fail loudly, so 2d rejects an
//! empty list rather than building a bridge nobody can connect to.
//! This module does not make that call itself, because it has no way
//! to know whether a future caller might have a different bind
//! source (e.g. a phase 03 configuration override) to fall back to.

use std::net::{IpAddr, SocketAddr};

/// Every address in `addrs` worth binding a UDP socket to, as a
/// [`SocketAddr`] with port 0 (ephemeral — the kernel assigns the
/// actual port at bind time).
///
/// This is the entire filtering policy for this module; see the
/// module docs for what is excluded and why. Order is preserved from
/// the input.
fn bindable_udp_addrs(addrs: impl IntoIterator<Item = IpAddr>) -> Vec<SocketAddr> {
    addrs
        .into_iter()
        .filter(|addr| match addr {
            IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_unspecified(),
            IpAddr::V6(v6) => {
                !v6.is_loopback() && !v6.is_unspecified() && !v6.is_unicast_link_local()
            }
        })
        .map(|addr| SocketAddr::new(addr, 0))
        .collect()
}

/// The UDP addresses this host's network interfaces make worth
/// binding, filtered by [`bindable_udp_addrs`].
///
/// See the module docs for what an empty return means and who is
/// responsible for treating it as an error. Not yet called from
/// anywhere in this crate — step 2d of
/// `docs/plans/PLAN-webrtc-0.20-upgrade-phase-02-bump.md` is what
/// wires this into `WebrtcBridge`'s construction. `pub` (and
/// re-exported from `lib.rs`) so it is part of the crate's API
/// surface ahead of that, rather than living behind an `#[allow]`
/// for dead code that stops being true the moment 2d lands.
pub fn host_udp_bind_addrs() -> Vec<SocketAddr> {
    match if_addrs::get_if_addrs() {
        Ok(interfaces) => bindable_udp_addrs(interfaces.into_iter().map(|iface| iface.ip())),
        Err(e) => {
            tracing::warn!("host_udp_bind_addrs: interface enumeration failed: {}", e);
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    #[test]
    fn empty_input_yields_empty_output() {
        assert!(bindable_udp_addrs(Vec::new()).is_empty());
    }

    #[test]
    fn loopback_v4_is_rejected() {
        let addrs = bindable_udp_addrs([IpAddr::V4(Ipv4Addr::LOCALHOST)]);
        assert!(addrs.is_empty(), "127.0.0.1 must not be bound");
    }

    #[test]
    fn loopback_v6_is_rejected() {
        let addrs = bindable_udp_addrs([IpAddr::V6(Ipv6Addr::LOCALHOST)]);
        assert!(addrs.is_empty(), "::1 must not be bound");
    }

    #[test]
    fn unspecified_v4_is_rejected() {
        let addrs = bindable_udp_addrs([IpAddr::V4(Ipv4Addr::UNSPECIFIED)]);
        assert!(
            addrs.is_empty(),
            "0.0.0.0 is the exact bug this module fixes"
        );
    }

    #[test]
    fn unspecified_v6_is_rejected() {
        let addrs = bindable_udp_addrs([IpAddr::V6(Ipv6Addr::UNSPECIFIED)]);
        assert!(addrs.is_empty(), ":: is the exact bug this module fixes");
    }

    #[test]
    fn ipv6_link_local_is_rejected() {
        let link_local = IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1));
        let addrs = bindable_udp_addrs([link_local]);
        assert!(
            addrs.is_empty(),
            "fe80::/10 has no zone id in a SocketAddr and must not be bound"
        );
    }

    #[test]
    fn ordinary_private_v4_is_accepted() {
        let private = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42));
        let addrs = bindable_udp_addrs([private]);
        assert_eq!(addrs, vec![SocketAddr::new(private, 0)]);
    }

    #[test]
    fn global_v6_is_accepted() {
        let global = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
        let addrs = bindable_udp_addrs([global]);
        assert_eq!(addrs, vec![SocketAddr::new(global, 0)]);
    }

    #[test]
    fn every_returned_address_has_port_zero() {
        let inputs = [
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2)),
        ];
        let addrs = bindable_udp_addrs(inputs);
        assert_eq!(addrs.len(), 2);
        for addr in addrs {
            assert_eq!(addr.port(), 0, "bind port must be ephemeral, not pinned");
        }
    }

    #[test]
    fn mixed_input_keeps_only_the_bindable_addresses() {
        let inputs = [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)),
            IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
        ];
        let addrs = bindable_udp_addrs(inputs);
        assert_eq!(
            addrs,
            vec![
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)), 0),
                SocketAddr::new(
                    IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
                    0
                ),
            ]
        );
    }
}
