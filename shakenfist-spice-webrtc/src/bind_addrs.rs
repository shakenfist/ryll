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
//! reach. See `docs/plans/PLAN-webrtc-0.20-upgrade.md` for the
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
//! exactly the host-coupled flake that the `RYLL_GATHERING_SOAK`
//! environment variable exists to hide — a CI runner's
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
//! question for whoever decides what a deployment should advertise.
//!
//! ## Policy versus mechanism
//!
//! That distinction is the split this module enforces. The three
//! exclusions above are two different kinds of thing:
//!
//! - **Mechanical**: unspecified (`0.0.0.0`, `::`) and zoneless
//!   `fe80::/10`. A [`SocketAddr`] cannot represent either in a way
//!   ICE can use, so no configuration may re-enable them. See
//!   [`UdpBindPolicy::validate`].
//! - **Policy**: loopback, and by omission everything that is *not*
//!   filtered — IPv4 link-local, IPv6 ULA, RFC 1918. These are
//!   defaults about what is worth advertising, and an operator who
//!   names addresses or interfaces explicitly overrides them.
//!
//! So [`UdpBindPolicy`] with no selectors reproduces the default
//! exactly, while `--web-media-addr 127.0.0.1` gets a loopback-only
//! deployment its bind address, and `--web-media-addr 0.0.0.0` is
//! refused rather than read as "all interfaces" — that is already
//! what no flag at all means, and quietly reinterpreting an address
//! as a wildcard is how a host ends up advertising an interface its
//! operator believed they had excluded.
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
//! The corollary is that a caller cannot state *which* of the two
//! happened, and must not write an error message that implies it
//! knows. The `warn` above is the only place the distinction survives,
//! so the callers' messages point at it.
//!
//! It is the *caller's* job to decide whether an empty result is
//! fatal. `WebrtcBridge::new` constructs a peer connection from this
//! list, and a peer connection that can never produce a routable
//! candidate is useless — so it rejects an empty list rather than
//! building a bridge nobody can connect to.
//! This module does not make that call itself, because a caller with
//! explicit selectors has a different empty case to report: not "this
//! host has no usable interface" but "nothing on this host matched
//! what you asked for", which has a different fix.

use std::net::{IpAddr, SocketAddr};

use anyhow::{bail, Result};

/// One entry in a [`UdpBindPolicy`]'s selector list: either a literal
/// address to bind, or the name of an interface whose addresses to
/// bind.
///
/// An operator supplies these as strings (`--web-media-addr`), and
/// anything that does not parse as an [`IpAddr`] is taken to be an
/// interface name — no interface name is a valid IP literal, so the
/// two cannot collide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindSelector {
    /// Bind this address exactly, whatever the default policy would
    /// have made of it. Subject only to the mechanical rejections.
    Addr(IpAddr),
    /// Bind every address this interface reports, matched by name
    /// against [`if_addrs::Interface::name`].
    Interface(String),
}

/// Which local addresses the WebRTC UDP sockets bind to, and on which
/// port.
///
/// [`Default`] — no selectors, port 0 — is the policy this module
/// shipped before it was configurable: every interface address that
/// is not loopback, unspecified or IPv6 link-local, each on an
/// ephemeral port. Any deployment that sets no flags gets exactly
/// that.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UdpBindPolicy {
    /// Restrict binding to these addresses and interfaces. Empty
    /// means "the default policy": enumerate and filter.
    pub selectors: Vec<BindSelector>,
    /// Port to bind on every selected address. 0 asks the kernel for
    /// an ephemeral port per socket, which is the default; a non-zero
    /// value pins the port so a firewall rule can name it.
    pub port: u16,
}

impl UdpBindPolicy {
    /// Check the parts of this policy that can be checked without
    /// touching the host, so a bad flag fails at launch rather than
    /// at the first viewer's `POST /offer`.
    ///
    /// Only [`BindSelector::Addr`] can be checked statically: an
    /// interface that is absent right now may be up by the time a
    /// viewer arrives, and refusing to start would be wrong.
    pub fn validate(&self) -> Result<()> {
        for selector in &self.selectors {
            let BindSelector::Addr(ip) = selector else {
                continue;
            };
            if ip.is_unspecified() {
                bail!(
                    "{ip} cannot be used as a media bind address: it binds successfully and then \
                     advertises itself verbatim as an ICE host candidate, which every browser \
                     discards — leaving a session that connects to nothing. Binding every \
                     suitable interface is already the default when no address is given; name \
                     the interfaces or addresses you want if you need to narrow it"
                );
            }
            if let IpAddr::V6(v6) = ip {
                if v6.is_unicast_link_local() {
                    bail!(
                        "{ip} cannot be used as a media bind address: an fe80::/10 address is \
                         only meaningful alongside a zone id, which neither a socket address nor \
                         an ICE candidate can carry, so the candidate would be ambiguous on any \
                         multi-interface host"
                    );
                }
            }
        }
        Ok(())
    }

    /// The addresses to bind, resolved against this host as it is
    /// right now.
    ///
    /// Called once per [`crate::WebrtcBridge::new`] rather than once
    /// per process, so a session that outlives a DHCP lease, a VPN
    /// coming up or an interface flap binds what exists at the time
    /// the viewer arrives rather than what existed at launch.
    ///
    /// An empty return is possible and is the caller's problem to
    /// report; see the module docs, and note that the two empty cases
    /// — "this host has nothing bindable" and "nothing matched your
    /// selectors" — want different error messages.
    pub fn resolve(&self) -> Vec<SocketAddr> {
        let addrs = if self.selectors.is_empty() {
            default_policy_addrs()
        } else {
            self.selected_addrs()
        };
        dedup_with_port(addrs, self.port)
    }

    /// Resolve an explicit selector list against this host.
    ///
    /// Enumeration is the only host-coupled part, so it happens here
    /// and the matching itself lives in [`select_from`], which takes
    /// the interface list as an argument and is therefore testable
    /// against a synthetic one.
    fn selected_addrs(&self) -> Vec<IpAddr> {
        // Only enumerate when a selector actually needs it. An
        // address-only policy must not fail differently just because
        // interface enumeration is broken on this host.
        let interfaces = if self
            .selectors
            .iter()
            .any(|s| matches!(s, BindSelector::Interface(_)))
        {
            enumerate_interfaces()
        } else {
            Vec::new()
        };
        select_from(&interfaces, &self.selectors)
    }
}

/// Every address in `interfaces` that `selectors` names, in selector
/// order.
///
/// Addresses are taken as given and interfaces are looked up by name;
/// both are subject only to the mechanical rejections, because naming
/// something explicitly *is* the override of the default policy. An
/// interface contributes every address it reports that survives those
/// rejections, so naming one whose only IPv6 address is link-local
/// contributes nothing rather than contributing something unusable.
///
/// Pure: `interfaces` is whatever the caller enumerated, which is what
/// lets the hit paths be tested without depending on the addresses of
/// whichever machine is running the suite. See the module docs for why
/// enumerating the real host in a test is not an option.
fn select_from(interfaces: &[(String, IpAddr)], selectors: &[BindSelector]) -> Vec<IpAddr> {
    let mut out = Vec::new();
    for selector in selectors {
        match selector {
            BindSelector::Addr(ip) => {
                if is_mechanically_bindable(ip) {
                    out.push(*ip);
                } else {
                    // validate() rejects these at startup, so
                    // reaching here means a caller skipped it.
                    tracing::warn!(
                        "UdpBindPolicy::resolve: ignoring {} — it can never produce a \
                         routable ICE candidate",
                        ip
                    );
                }
            }
            BindSelector::Interface(name) => {
                let mut matched = false;
                for (iface, ip) in interfaces {
                    if iface == name {
                        matched = true;
                        if is_mechanically_bindable(ip) {
                            out.push(*ip);
                        }
                    }
                }
                if !matched {
                    tracing::warn!(
                        "UdpBindPolicy::resolve: no interface named {} on this host",
                        name
                    );
                }
            }
        }
    }
    out
}

/// True unless binding `ip` could only ever advertise a candidate no
/// remote peer can use. See the module docs' policy-versus-mechanism
/// section: this is the half no configuration may override.
fn is_mechanically_bindable(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_unspecified(),
        IpAddr::V6(v6) => !v6.is_unspecified() && !v6.is_unicast_link_local(),
    }
}

/// Every address in `addrs` the *default* policy considers worth
/// binding: the mechanical rejections plus loopback.
///
/// Order is preserved from the input.
fn bindable_udp_addrs(addrs: impl IntoIterator<Item = IpAddr>) -> Vec<IpAddr> {
    addrs
        .into_iter()
        .filter(|addr| {
            is_mechanically_bindable(addr)
                && match addr {
                    IpAddr::V4(v4) => !v4.is_loopback(),
                    IpAddr::V6(v6) => !v6.is_loopback(),
                }
        })
        .collect()
}

/// Pair every address with `port`, dropping any repeat.
///
/// Duplicates are possible either way round — an operator can name an
/// interface and one of its addresses, and a host can report one
/// address on two interfaces — and with a pinned port a duplicate is
/// not cosmetic: the second `UdpSocket::bind` on the same
/// address:port fails and takes the whole peer connection with it.
fn dedup_with_port(addrs: Vec<IpAddr>, port: u16) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = Vec::with_capacity(addrs.len());
    for addr in addrs {
        let sock = SocketAddr::new(addr, port);
        if !out.contains(&sock) {
            out.push(sock);
        }
    }
    out
}

/// This host's interfaces as `(name, address)` pairs.
///
/// Enumeration failure is logged at `warn` and reported as an empty
/// list, for the reasons in the module docs.
fn enumerate_interfaces() -> Vec<(String, IpAddr)> {
    match if_addrs::get_if_addrs() {
        Ok(interfaces) => interfaces
            .into_iter()
            .map(|iface| (iface.name.clone(), iface.ip()))
            .collect(),
        Err(e) => {
            tracing::warn!("bind_addrs: interface enumeration failed: {}", e);
            Vec::new()
        }
    }
}

/// The addresses the default policy selects on this host.
fn default_policy_addrs() -> Vec<IpAddr> {
    bindable_udp_addrs(enumerate_interfaces().into_iter().map(|(_, ip)| ip))
}

/// The UDP addresses this host's network interfaces make worth
/// binding under the default policy, on ephemeral ports.
///
/// Exactly `UdpBindPolicy::default().resolve()`, kept as a function
/// because `TestPeerBuilder` (`crate::test_client`) wants the default
/// and has no configuration surface of its own. `WebrtcBridge::new`
/// goes through the policy instead, since `--web-media-addr` and
/// `--web-media-port` reach it.
///
/// See the module docs for what an empty return means and who is
/// responsible for treating it as an error.
pub fn host_udp_bind_addrs() -> Vec<SocketAddr> {
    UdpBindPolicy::default().resolve()
}

/// The default policy, or an explicit loopback bind on a host that
/// has nothing else.
///
/// A test peer and a bridge under test both need *some* address to
/// bind, and they do not care which: the handshake they exercise
/// happens between two peers inside one process, where a loopback
/// candidate works exactly as well as a routable one. A build
/// sandbox with no network namespace — `docker run --network none`,
/// which is how this workspace compiles and tests untrusted build
/// scripts — reports only `lo`, so the default policy correctly
/// resolves to nothing and every such test fails on an error that is
/// right about the host and irrelevant to what the test asserts.
///
/// This is the loopback-only deployment shape the module docs
/// describe, and the override is the sanctioned one: loopback is
/// policy, so naming it explicitly is how a caller opts in. That the
/// production default still refuses to guess is the point — a server
/// that silently bound loopback would advertise candidates no browser
/// could reach, which is the failure `--web-media-addr 127.0.0.1`
/// exists to make deliberate.
///
/// Gated to tests and the `test-support` feature so it cannot become
/// a production caller's shortcut past that decision.
#[cfg(any(test, feature = "test-support"))]
pub fn bind_policy_for_tests() -> UdpBindPolicy {
    let default = UdpBindPolicy::default();
    if !default.resolve().is_empty() {
        return default;
    }
    tracing::debug!(
        "bind_addrs: no routable address on this host — binding loopback so in-process peers \
         can still reach each other"
    );
    UdpBindPolicy {
        selectors: vec![BindSelector::Addr(IpAddr::V4(
            std::net::Ipv4Addr::LOCALHOST,
        ))],
        port: 0,
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    /// The default policy's filter, expressed as the bound socket
    /// addresses it would produce. Every test below that predates the
    /// policy type goes through this, so they still assert what they
    /// always asserted.
    fn bound(addrs: impl IntoIterator<Item = IpAddr>) -> Vec<SocketAddr> {
        dedup_with_port(bindable_udp_addrs(addrs), 0)
    }

    #[test]
    fn empty_input_yields_empty_output() {
        assert!(bound(Vec::new()).is_empty());
    }

    #[test]
    fn loopback_v4_is_rejected() {
        let addrs = bound([IpAddr::V4(Ipv4Addr::LOCALHOST)]);
        assert!(addrs.is_empty(), "127.0.0.1 must not be bound");
    }

    #[test]
    fn loopback_v6_is_rejected() {
        let addrs = bound([IpAddr::V6(Ipv6Addr::LOCALHOST)]);
        assert!(addrs.is_empty(), "::1 must not be bound");
    }

    #[test]
    fn unspecified_v4_is_rejected() {
        let addrs = bound([IpAddr::V4(Ipv4Addr::UNSPECIFIED)]);
        assert!(
            addrs.is_empty(),
            "0.0.0.0 is the exact bug this module fixes"
        );
    }

    #[test]
    fn unspecified_v6_is_rejected() {
        let addrs = bound([IpAddr::V6(Ipv6Addr::UNSPECIFIED)]);
        assert!(addrs.is_empty(), ":: is the exact bug this module fixes");
    }

    #[test]
    fn ipv6_link_local_is_rejected() {
        let link_local = IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1));
        let addrs = bound([link_local]);
        assert!(
            addrs.is_empty(),
            "fe80::/10 has no zone id in a SocketAddr and must not be bound"
        );
    }

    #[test]
    fn ordinary_private_v4_is_accepted() {
        let private = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42));
        let addrs = bound([private]);
        assert_eq!(addrs, vec![SocketAddr::new(private, 0)]);
    }

    #[test]
    fn global_v6_is_accepted() {
        let global = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
        let addrs = bound([global]);
        assert_eq!(addrs, vec![SocketAddr::new(global, 0)]);
    }

    #[test]
    fn every_returned_address_has_port_zero() {
        let inputs = [
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2)),
        ];
        let addrs = bound(inputs);
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
        let addrs = bound(inputs);
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

    #[test]
    fn default_policy_selects_nothing_of_its_own() {
        // The default carries no selectors and no pinned port, which
        // is what makes `host_udp_bind_addrs` a thin alias for it.
        let policy = UdpBindPolicy::default();
        assert!(policy.selectors.is_empty());
        assert_eq!(policy.port, 0);
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn explicit_loopback_overrides_the_default_filter() {
        // Loopback is policy, so naming it explicitly is the
        // override. A loopback-only deployment needs no flag of its
        // own.
        let policy = UdpBindPolicy {
            selectors: vec![BindSelector::Addr(IpAddr::V4(Ipv4Addr::LOCALHOST))],
            port: 0,
        };
        assert!(policy.validate().is_ok());
        assert_eq!(
            policy.resolve(),
            vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)]
        );
    }

    #[test]
    fn explicit_unspecified_is_refused_by_validate() {
        for ip in [
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        ] {
            let policy = UdpBindPolicy {
                selectors: vec![BindSelector::Addr(ip)],
                port: 0,
            };
            let err = policy
                .validate()
                .expect_err("an unspecified address is mechanically unusable, not a preference");
            let msg = err.to_string();
            assert!(
                msg.contains("ICE host candidate"),
                "the error must say why, not just that it is refused: {msg}"
            );
        }
    }

    #[test]
    fn explicit_zoneless_link_local_is_refused_by_validate() {
        let policy = UdpBindPolicy {
            selectors: vec![BindSelector::Addr(IpAddr::V6(Ipv6Addr::new(
                0xfe80, 0, 0, 0, 0, 0, 0, 1,
            )))],
            port: 0,
        };
        assert!(policy.validate().is_err());
    }

    #[test]
    fn a_pinned_port_lands_on_every_selected_address() {
        let policy = UdpBindPolicy {
            selectors: vec![
                BindSelector::Addr(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42))),
                BindSelector::Addr(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
            ],
            port: 41_000,
        };
        let addrs = policy.resolve();
        assert_eq!(addrs.len(), 2);
        for addr in addrs {
            assert_eq!(addr.port(), 41_000);
        }
    }

    #[test]
    fn a_repeated_selection_is_bound_once() {
        // With a pinned port a duplicate is fatal, not cosmetic: the
        // second bind on the same address:port fails.
        let addr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42));
        let policy = UdpBindPolicy {
            selectors: vec![BindSelector::Addr(addr), BindSelector::Addr(addr)],
            port: 41_000,
        };
        assert_eq!(policy.resolve(), vec![SocketAddr::new(addr, 41_000)]);
    }

    #[test]
    fn an_unmatched_interface_name_selects_nothing() {
        // Safe to run anywhere: no host has an interface by this
        // name, so this asserts the miss path without depending on
        // what the test machine happens to have plugged in.
        let policy = UdpBindPolicy {
            selectors: vec![BindSelector::Interface(
                "ryll-no-such-interface-0".to_string(),
            )],
            port: 0,
        };
        assert!(
            policy.validate().is_ok(),
            "an absent interface may appear later"
        );
        assert!(policy.resolve().is_empty());
    }

    /// A synthetic interface table with the three shapes that make
    /// selector matching interesting: an interface carrying more than
    /// one address, a link-local-only interface, and one address
    /// present on two interfaces.
    fn fixture() -> Vec<(String, IpAddr)> {
        vec![
            (
                "eth0".to_string(),
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)),
            ),
            (
                "eth0".to_string(),
                IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
            ),
            (
                "wg0".to_string(),
                IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)),
            ),
            (
                "eth1".to_string(),
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)),
            ),
        ]
    }

    #[test]
    fn an_interface_selector_takes_every_address_it_reports() {
        let selected = select_from(&fixture(), &[BindSelector::Interface("eth0".to_string())]);
        assert_eq!(
            selected,
            vec![
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)),
                IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
            ],
            "both of eth0's addresses, and neither of any other interface's"
        );
    }

    #[test]
    fn a_link_local_only_interface_selects_nothing() {
        // The mechanical rejection still applies to an explicitly
        // named interface: an fe80::/10 address cannot carry its zone
        // id into a candidate however it was chosen. The caller sees
        // the same empty list an unmatched name produces, which is
        // why `WebrtcBridge::new` phrases that error as "nothing
        // matched" rather than "no such interface".
        let selected = select_from(&fixture(), &[BindSelector::Interface("wg0".to_string())]);
        assert!(selected.is_empty());
    }

    #[test]
    fn an_address_on_two_interfaces_is_bound_once() {
        // Naming both interfaces selects the same address twice, and
        // with a pinned port the second bind would fail. `resolve`
        // dedups; `select_from` deliberately does not, so the dedup
        // stays in one place.
        let policy = UdpBindPolicy {
            selectors: vec![
                BindSelector::Interface("eth0".to_string()),
                BindSelector::Interface("eth1".to_string()),
            ],
            port: 5004,
        };
        let selected = select_from(&fixture(), &policy.selectors);
        assert_eq!(selected.len(), 3, "select_from keeps the repeat");
        assert_eq!(
            dedup_with_port(selected, policy.port),
            vec![
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)), 5004),
                SocketAddr::new(
                    IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
                    5004
                ),
            ]
        );
    }

    #[test]
    fn selectors_are_resolved_in_the_order_given() {
        let selected = select_from(
            &fixture(),
            &[
                BindSelector::Addr(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
                BindSelector::Interface("eth1".to_string()),
            ],
        );
        assert_eq!(
            selected,
            vec![
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)),
            ],
            "an address selector needs no interface to match, and order follows the flag order"
        );
    }
}
