# shakenfist-spice-rdp

RDP frontend for the
[ryll](https://github.com/shakenfist/ryll) SPICE client. It
serves a SPICE session to stock RDP clients — mstsc on Windows,
FreeRDP, and the RDP apps on macOS, iOS and Android — so a
desktop can be reached from machines with no SPICE client. This
is what backs ryll's `--rdp` mode.

It is a reachability fallback, not a better path. The session
is transcoded from SPICE to RDP on the serving side, which costs
CPU there, and SPICE features such as USB redirection and folder
sharing do not cross it. Users who can run a SPICE client should.

The RDP server is
[IronRDP](https://github.com/Devolutions/IronRDP)'s
`ironrdp-server`, pinned to its 0.13 release; IronRDP's server
API changes between minor releases, so moving to a newer one is
a deliberate upgrade. The crate builds on
[shakenfist-spice-renderer](https://github.com/shakenfist/ryll)
for the session's surfaces and input.

## Source

Developed as part of the
[ryll](https://github.com/shakenfist/ryll) SPICE client.
Internal consumers within the shakenfist project depend on this
crate via workspace paths.

## License

Apache-2.0
