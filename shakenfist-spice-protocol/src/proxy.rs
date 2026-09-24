//! HTTP CONNECT proxy support: proxy address parsing and the tunnel
//! handshake.
//!
//! Some SPICE deployments do not expose the hypervisor's SPICE port to
//! clients directly. Proxmox VE, for example, hands out a `.vv` file whose
//! `proxy=` key names an HTTP proxy (its `spiceproxy` daemon on port 3128)
//! and whose `host=` is an opaque, signed pseudo-hostname the proxy
//! resolves to the right node and port. The client opens a TCP connection
//! to the proxy, asks it to `CONNECT` to the pseudo-hostname, and then runs
//! TLS and the SPICE link handshake through the resulting tunnel.
//!
//! This module provides the two halves of that:
//!
//! - [`parse_proxy_uri`] turns a `proxy=` value into a typed
//!   [`HttpProxy`]. Its rules replicate `spice_uri_parse` in spice-gtk's
//!   `src/spice-uri.c`, which is what remote-viewer uses for the same key:
//!   - The scheme defaults to `http`, and is matched case-insensitively.
//!   - The port defaults to [`DEFAULT_HTTP_PROXY_PORT`] (3128) for `http`.
//!   - Any scheme other than `http` or `https` is refused.
//!   - Trailing slashes are stripped.
//!   - A host in square brackets is an IPv6 literal. The brackets are
//!     removed, a missing `]` is an error, and anything after the `]`
//!     other than `:port` is an error.
//!   - Otherwise the host ends at the first `:`, and everything after it
//!     is the port.
//!   - An empty host, an empty port, a non-numeric port, and a port
//!     outside `1..=65535` are errors.
//!
//!   It deliberately departs from spice-gtk in four places. An `https`
//!   proxy (TLS to the proxy itself) and userinfo (`user:pass@`) are both
//!   refused with an error naming the unsupported feature, where spice-gtk
//!   accepts them; neither is needed by any deployment this crate supports
//!   today. A scheme is only recognised when followed by `://`, so
//!   `proxy.example:3128` is a host and port rather than the unknown
//!   scheme `proxy.example` that spice-gtk's `g_uri_parse_scheme` call
//!   reports. And a port must be plain ASCII digits, where spice-gtk's
//!   `g_ascii_strtoll` also accepts a leading sign or whitespace.
//!
//! - [`write_connect_request`], [`read_connect_response`] and the pure
//!   [`parse_connect_response`] perform the CONNECT exchange. They follow
//!   GLib's `gio/ghttpproxy.c`, which performs the CONNECT on behalf of
//!   spice-gtk and so is the client proxies like Proxmox's are tested
//!   against: an `HTTP/1.0` request carrying `Host`, `Proxy-Connection`
//!   and `User-Agent` headers, a response read one byte at a time so that
//!   nothing past the header block is consumed, and any `2xx` status
//!   accepted as an established tunnel.
//!
//! Nothing here performs DNS resolution, TLS, or the SPICE handshake; the
//! caller dials the proxy, runs the exchange, and then treats the stream
//! as if it were connected to the target.

use std::fmt;
use std::net::Ipv6Addr;

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The port an `http` proxy URI defaults to when it names none, as in
/// spice-gtk.
pub const DEFAULT_HTTP_PROXY_PORT: u16 = 3128;

/// The largest CONNECT response header block [`read_connect_response`]
/// will buffer, including the terminating blank line. A CONNECT response
/// is a status line and a handful of headers; anything approaching this
/// size is a misbehaving or hostile proxy, not a slow one.
pub const MAX_CONNECT_RESPONSE_BYTES: usize = 16 * 1024;

/// The longest status line an error message quotes. The rest is elided,
/// so a hostile proxy cannot fill a log line with 16 KiB of its choosing.
const MAX_QUOTED_STATUS_LINE: usize = 256;

/// The value sent in the CONNECT request's `User-Agent` header.
const USER_AGENT: &str = concat!("shakenfist-spice-protocol/", env!("CARGO_PKG_VERSION"));

/// An HTTP proxy to tunnel a SPICE connection through with `CONNECT`.
///
/// Build one with [`parse_proxy_uri`] where the proxy address arrives as a
/// string (a `.vv` file's `proxy=` key, say), so that a malformed value
/// fails there and never reaches a dial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpProxy {
    /// The proxy's host: a DNS name, an IPv4 literal, or a bare IPv6
    /// literal. An IPv6 literal is stored without the square brackets its
    /// URI form requires, so it can be handed to a resolver or a
    /// `(host, port)` socket address as is.
    pub host: String,
    /// The proxy's TCP port.
    pub port: u16,
}

/// Errors from parsing a proxy URI with [`parse_proxy_uri`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProxyError {
    /// The URI named a scheme other than `http` or `https`.
    #[error("proxy URI scheme {0:?} is not supported (only http is)")]
    UnsupportedScheme(String),

    /// The URI named the `https` scheme. spice-gtk supports TLS to the
    /// proxy itself; this crate does not yet.
    #[error("https proxies (TLS to the proxy itself) are not supported; use an http:// proxy")]
    HttpsUnsupported,

    /// The URI carried userinfo (`user:pass@`). spice-gtk sends it as
    /// `Proxy-Authorization`; this crate does not support proxy
    /// authentication yet.
    #[error("proxy credentials (user:pass@ in the proxy URI) are not supported")]
    CredentialsUnsupported,

    /// A host began with `[` but had no closing `]`.
    #[error("proxy URI has an IPv6 host with no closing ']'")]
    MissingBracket,

    /// Something other than `:port` followed the `]` of an IPv6 host.
    #[error("proxy URI has {0:?} after the IPv6 host, where only ':port' may follow")]
    TrailingAfterBracket(String),

    /// The host was empty.
    #[error("proxy URI has an empty host")]
    EmptyHost,

    /// A `:` followed the host but no port followed the `:`.
    #[error("proxy URI has a ':' but no port after it")]
    MissingPort,

    /// The port was not a plain decimal number.
    #[error("proxy URI port {0:?} is not a number")]
    InvalidPort(String),

    /// The port was a number outside `1..=65535`.
    #[error("proxy URI port {0} is out of range (1-65535)")]
    PortOutOfRange(String),
}

/// Errors from the CONNECT exchange: [`write_connect_request`],
/// [`read_connect_response`] and [`parse_connect_response`].
#[derive(Debug, Error)]
pub enum ConnectError {
    /// Reading from or writing to the proxy failed.
    #[error("I/O error talking to the HTTP proxy: {0}")]
    Io(#[from] std::io::Error),

    /// The CONNECT target host was empty, or contained whitespace or a
    /// control character. Refused before anything is written, because a
    /// line break in it would let the host inject request headers.
    #[error("CONNECT target host is empty or contains whitespace or control characters")]
    InvalidTarget,

    /// The proxy closed the connection before sending the blank line that
    /// ends the response headers.
    #[error("HTTP proxy closed the connection after {received} bytes, before the end of its CONNECT response")]
    ClosedBeforeHeaders {
        /// How many response bytes arrived before the close.
        received: usize,
    },

    /// The response header block exceeded [`MAX_CONNECT_RESPONSE_BYTES`]
    /// without ending.
    #[error("HTTP proxy CONNECT response headers exceed {limit} bytes")]
    HeadersTooLarge {
        /// The cap that was exceeded.
        limit: usize,
    },

    /// The first line of the response was not an `HTTP/1.0` or
    /// `HTTP/1.1` status line with a three-digit status code.
    #[error("HTTP proxy sent a malformed CONNECT status line: {0:?}")]
    MalformedStatusLine(String),

    /// The proxy answered `401 Unauthorized`. Proxmox's `spiceproxy`
    /// answers this when the ticket in the target pseudo-hostname has
    /// expired or is otherwise invalid.
    #[error(
        "HTTP proxy refused the CONNECT: {0:?}. If this is a Proxmox VE spiceproxy, the ticket in the \
         connection's host has probably expired: Proxmox tickets are valid for about 30 seconds, so \
         fetch a fresh .vv file and open it promptly"
    )]
    Unauthorized(String),

    /// The proxy answered `407 Proxy Authentication Required`.
    #[error("HTTP proxy requires authentication, which is not supported: {0:?}")]
    ProxyAuthenticationRequired(String),

    /// The proxy answered with any other non-`2xx` status.
    #[error("HTTP proxy refused the CONNECT: {status_line:?}")]
    Refused {
        /// The numeric status code.
        status: u16,
        /// The status line as received, truncated for quoting.
        status_line: String,
    },
}

/// Parse a proxy URI, such as a `.vv` file's `proxy=` value, into an
/// [`HttpProxy`].
///
/// Accepts `host`, `host:port`, `http://host`, `http://host:port`, and the
/// same with an IPv6 literal in square brackets (`[::1]:3128`), optionally
/// followed by trailing slashes. See the [module documentation](self) for
/// the full rules and where they depart from spice-gtk.
///
/// # Errors
///
/// Returns a [`ProxyError`] naming what is wrong. An `https` scheme and
/// userinfo get their own variants, [`ProxyError::HttpsUnsupported`] and
/// [`ProxyError::CredentialsUnsupported`], so the message says which
/// feature is missing rather than that the URI is malformed.
pub fn parse_proxy_uri(uri: &str) -> Result<HttpProxy, ProxyError> {
    let rest = match uri.split_once("://") {
        Some((scheme, rest)) if is_uri_scheme(scheme) => {
            if scheme.eq_ignore_ascii_case("https") {
                return Err(ProxyError::HttpsUnsupported);
            }
            if !scheme.eq_ignore_ascii_case("http") {
                return Err(ProxyError::UnsupportedScheme(scheme.to_string()));
            }
            rest
        }
        _ => uri,
    };

    let rest = rest.trim_end_matches('/');

    if rest.contains('@') {
        return Err(ProxyError::CredentialsUnsupported);
    }

    let (host, port) = if let Some(bracketed) = rest.strip_prefix('[') {
        let (host, after) = bracketed
            .split_once(']')
            .ok_or(ProxyError::MissingBracket)?;
        let port = if after.is_empty() {
            None
        } else if let Some(port) = after.strip_prefix(':') {
            Some(port)
        } else {
            return Err(ProxyError::TrailingAfterBracket(after.to_string()));
        };
        (host, port)
    } else {
        match rest.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (rest, None),
        }
    };

    if host.is_empty() {
        return Err(ProxyError::EmptyHost);
    }

    let port = match port {
        None => DEFAULT_HTTP_PROXY_PORT,
        Some(port) => parse_port(port)?,
    };

    Ok(HttpProxy {
        host: host.to_string(),
        port,
    })
}

/// Whether `s` is a syntactically valid URI scheme (RFC 3986 section 3.1:
/// a letter, then letters, digits, `+`, `-` or `.`), which is the test
/// GLib's `g_uri_parse_scheme` applies.
fn is_uri_scheme(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() => {
            chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        }
        _ => false,
    }
}

/// Parse a proxy URI port: one or more ASCII digits, in `1..=65535`.
fn parse_port(port: &str) -> Result<u16, ProxyError> {
    if port.is_empty() {
        return Err(ProxyError::MissingPort);
    }
    if !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ProxyError::InvalidPort(port.to_string()));
    }
    // All digits, so the only way this fails is overflow.
    match port.parse::<u64>() {
        Ok(n) if (1..=u64::from(u16::MAX)).contains(&n) => {
            Ok(u16::try_from(n).expect("range checked above"))
        }
        _ => Err(ProxyError::PortOutOfRange(port.to_string())),
    }
}

/// Format `host` and `port` as an HTTP authority (`host:port`), bracketing
/// an IPv6 literal as RFC 9110 requires. Any other host, including a
/// Proxmox pseudo-hostname with colons in it, is used verbatim: such a
/// host is opaque to this crate and must reach the proxy byte for byte.
fn authority(host: &str, port: u16) -> String {
    if host.parse::<Ipv6Addr>().is_ok() {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Write an HTTP CONNECT request for `target_host:target_port` to
/// `stream` (a connection to the proxy) and flush it.
///
/// The request matches the one GLib's `ghttpproxy.c` sends for
/// remote-viewer:
///
/// ```text
/// CONNECT <target> HTTP/1.0
/// Host: <target>
/// Proxy-Connection: keep-alive
/// User-Agent: shakenfist-spice-protocol/<version>
/// ```
///
/// followed by a blank line, with CRLF line endings. `<target>` is
/// `target_host:target_port`, with an IPv6 literal bracketed. The `Host`
/// header is always sent: Proxmox's `spiceproxy` reads the target from
/// it rather than from the request line, and answers a CONNECT without
/// it with a `401` indistinguishable from an expired ticket.
///
/// # Errors
///
/// [`ConnectError::InvalidTarget`] if `target_host` is empty or contains
/// whitespace or a control character, before anything is written;
/// [`ConnectError::Io`] if the write fails.
pub async fn write_connect_request<S>(
    stream: &mut S,
    target_host: &str,
    target_port: u16,
) -> Result<(), ConnectError>
where
    S: AsyncWrite + Unpin,
{
    if target_host.is_empty()
        || target_host
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(ConnectError::InvalidTarget);
    }

    let target = authority(target_host, target_port);
    let request = format!(
        "CONNECT {target} HTTP/1.0\r\n\
         Host: {target}\r\n\
         Proxy-Connection: keep-alive\r\n\
         User-Agent: {USER_AGENT}\r\n\
         \r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

/// Read the proxy's response to a CONNECT request from `stream` and
/// decide whether the tunnel is established.
///
/// Reads one byte at a time until the blank line (`\r\n\r\n`) that ends
/// the header block, so that no byte after it is consumed: those bytes
/// belong to whatever runs through the tunnel (TLS, for SPICE), and a
/// buffered read here would swallow them. The header block is then
/// interpreted by [`parse_connect_response`].
///
/// On `Ok(())` the stream is positioned at the first byte from the
/// target, and the caller may start its own protocol on it.
///
/// # Errors
///
/// [`ConnectError::ClosedBeforeHeaders`] if the proxy closes the
/// connection before the blank line, [`ConnectError::HeadersTooLarge`]
/// if the header block reaches [`MAX_CONNECT_RESPONSE_BYTES`] without
/// ending, [`ConnectError::Io`] on a read error, and anything
/// [`parse_connect_response`] returns.
pub async fn read_connect_response<S>(stream: &mut S) -> Result<(), ConnectError>
where
    S: AsyncRead + Unpin,
{
    let mut block = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        if stream.read(&mut byte).await? == 0 {
            return Err(ConnectError::ClosedBeforeHeaders {
                received: block.len(),
            });
        }
        block.push(byte[0]);
        if block.ends_with(b"\r\n\r\n") {
            break;
        }
        if block.len() >= MAX_CONNECT_RESPONSE_BYTES {
            return Err(ConnectError::HeadersTooLarge {
                limit: MAX_CONNECT_RESPONSE_BYTES,
            });
        }
    }
    parse_connect_response(&block)
}

/// Interpret a buffered CONNECT response header block.
///
/// Only the status line (everything before the first `\r\n`, or the whole
/// input if there is none) is examined; headers are ignored, as GLib
/// ignores them. The status line must be `HTTP/1.0` or `HTTP/1.1`, one or
/// more spaces, a three-digit status code, and then either nothing or a
/// space and a reason phrase. Any `2xx` status establishes the tunnel,
/// as it does in GLib and RFC 9110 section 9.3.6.
///
/// This function is pure and total over arbitrary bytes, which makes it
/// the fuzzing entry point for the response side of the exchange.
///
/// # Errors
///
/// [`ConnectError::MalformedStatusLine`] if the status line does not
/// parse; [`ConnectError::Unauthorized`] for `401`, with a hint about
/// Proxmox ticket expiry; [`ConnectError::ProxyAuthenticationRequired`]
/// for `407`; and [`ConnectError::Refused`] for every other non-`2xx`
/// status. Each carries the status line, truncated for quoting.
pub fn parse_connect_response(block: &[u8]) -> Result<(), ConnectError> {
    let line = match block.windows(2).position(|w| w == b"\r\n") {
        Some(end) => &block[..end],
        None => block,
    };
    let quoted = quote_status_line(line);

    let status =
        parse_status_code(line).ok_or_else(|| ConnectError::MalformedStatusLine(quoted.clone()))?;
    match status {
        200..=299 => Ok(()),
        401 => Err(ConnectError::Unauthorized(quoted)),
        407 => Err(ConnectError::ProxyAuthenticationRequired(quoted)),
        _ => Err(ConnectError::Refused {
            status,
            status_line: quoted,
        }),
    }
}

/// Extract the status code from an HTTP/1.x status line, or `None` if the
/// line is not one.
fn parse_status_code(line: &[u8]) -> Option<u16> {
    let rest = line
        .strip_prefix(b"HTTP/1.0")
        .or_else(|| line.strip_prefix(b"HTTP/1.1"))?;
    let digits = rest.strip_prefix(b" ")?;
    let digits = &digits[digits.iter().take_while(|&&b| b == b' ').count()..];
    let (code, after) = digits.split_at_checked(3)?;
    if !code.iter().all(u8::is_ascii_digit) || !(after.is_empty() || after.starts_with(b" ")) {
        return None;
    }
    code.iter()
        .try_fold(0u16, |acc, &b| Some(acc * 10 + u16::from(b - b'0')))
}

/// Render a status line for an error message: lossily decoded, and
/// truncated to [`MAX_QUOTED_STATUS_LINE`] characters.
fn quote_status_line(line: &[u8]) -> String {
    let text = String::from_utf8_lossy(line);
    let mut chars = text.chars();
    let mut quoted: String = chars.by_ref().take(MAX_QUOTED_STATUS_LINE).collect();
    if chars.next().is_some() {
        quoted.push_str("...");
    }
    quoted
}

impl fmt::Display for HttpProxy {
    /// Formats the proxy as `http://host:port`, bracketing an IPv6 host.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "http://{}", authority(&self.host, self.port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, DuplexStream};

    fn proxy(host: &str, port: u16) -> HttpProxy {
        HttpProxy {
            host: host.to_string(),
            port,
        }
    }

    // ---- parse_proxy_uri: accepted forms ----

    #[test]
    fn bare_host_gets_http_default_port() {
        assert_eq!(
            parse_proxy_uri("pve1.example").unwrap(),
            proxy("pve1.example", 3128)
        );
    }

    #[test]
    fn host_and_port_without_scheme() {
        assert_eq!(
            parse_proxy_uri("pve1.example:8080").unwrap(),
            proxy("pve1.example", 8080)
        );
        assert_eq!(
            parse_proxy_uri("192.0.2.7:3128").unwrap(),
            proxy("192.0.2.7", 3128)
        );
    }

    #[test]
    fn http_scheme_with_port() {
        assert_eq!(
            parse_proxy_uri("http://pve1.example:3128").unwrap(),
            proxy("pve1.example", 3128)
        );
    }

    #[test]
    fn http_scheme_without_port_defaults_to_3128() {
        assert_eq!(
            parse_proxy_uri("http://pve1.example").unwrap(),
            proxy("pve1.example", 3128)
        );
        assert_eq!(DEFAULT_HTTP_PROXY_PORT, 3128);
    }

    #[test]
    fn scheme_is_case_insensitive() {
        assert_eq!(
            parse_proxy_uri("HTTP://pve1.example:81").unwrap(),
            proxy("pve1.example", 81)
        );
    }

    #[test]
    fn trailing_slashes_are_stripped() {
        assert_eq!(
            parse_proxy_uri("http://pve1.example:3128/").unwrap(),
            proxy("pve1.example", 3128)
        );
        assert_eq!(
            parse_proxy_uri("http://pve1.example///").unwrap(),
            proxy("pve1.example", 3128)
        );
        assert_eq!(parse_proxy_uri("[::1]:3128//").unwrap(), proxy("::1", 3128));
    }

    #[test]
    fn bracketed_ipv6_host_is_stored_bare() {
        assert_eq!(parse_proxy_uri("[::1]:3128").unwrap(), proxy("::1", 3128));
        assert_eq!(
            parse_proxy_uri("http://[2001:db8::5]").unwrap(),
            proxy("2001:db8::5", 3128)
        );
    }

    #[test]
    fn port_bounds_are_inclusive() {
        assert_eq!(parse_proxy_uri("h:1").unwrap(), proxy("h", 1));
        assert_eq!(parse_proxy_uri("h:65535").unwrap(), proxy("h", 65535));
    }

    #[test]
    fn display_round_trips() {
        for uri in ["http://pve1.example:3128", "http://[::1]:8080"] {
            let parsed = parse_proxy_uri(uri).unwrap();
            assert_eq!(parsed.to_string(), uri);
            assert_eq!(parse_proxy_uri(&parsed.to_string()).unwrap(), parsed);
        }
    }

    // ---- parse_proxy_uri: refusals ----

    #[test]
    fn https_scheme_is_refused_by_name() {
        assert_eq!(
            parse_proxy_uri("https://pve1.example:3129"),
            Err(ProxyError::HttpsUnsupported)
        );
        assert_eq!(
            parse_proxy_uri("HTTPS://pve1.example"),
            Err(ProxyError::HttpsUnsupported)
        );
        assert!(ProxyError::HttpsUnsupported.to_string().contains("https"));
    }

    #[test]
    fn userinfo_is_refused_by_name() {
        for uri in [
            "user:pass@pve1.example",
            "http://user:pass@pve1.example:3128",
            "http://user@h",
        ] {
            assert_eq!(
                parse_proxy_uri(uri),
                Err(ProxyError::CredentialsUnsupported),
                "{uri}"
            );
        }
        assert!(ProxyError::CredentialsUnsupported
            .to_string()
            .contains("credentials"));
    }

    #[test]
    fn other_schemes_are_refused() {
        assert_eq!(
            parse_proxy_uri("socks5://pve1.example:1080"),
            Err(ProxyError::UnsupportedScheme("socks5".to_string()))
        );
    }

    #[test]
    fn port_out_of_range_is_refused() {
        for port in ["0", "65536", "99999999999999999999999"] {
            assert_eq!(
                parse_proxy_uri(&format!("http://h:{port}")),
                Err(ProxyError::PortOutOfRange(port.to_string())),
                "{port}"
            );
        }
    }

    #[test]
    fn non_numeric_port_is_refused() {
        for port in ["http", "31a", "+80", " 80", "-1", "3128:1"] {
            assert_eq!(
                parse_proxy_uri(&format!("h:{port}")),
                Err(ProxyError::InvalidPort(port.to_string())),
                "{port}"
            );
        }
    }

    #[test]
    fn empty_port_is_refused() {
        assert_eq!(parse_proxy_uri("http://h:"), Err(ProxyError::MissingPort));
        assert_eq!(parse_proxy_uri("[::1]:"), Err(ProxyError::MissingPort));
    }

    #[test]
    fn empty_host_is_refused() {
        for uri in [
            "",
            "http://",
            "http:///",
            ":3128",
            "http://:3128",
            "[]:3128",
            "::1",
        ] {
            assert_eq!(parse_proxy_uri(uri), Err(ProxyError::EmptyHost), "{uri:?}");
        }
    }

    #[test]
    fn missing_close_bracket_is_refused() {
        assert_eq!(
            parse_proxy_uri("[::1:3128"),
            Err(ProxyError::MissingBracket)
        );
        assert_eq!(
            parse_proxy_uri("http://[::1"),
            Err(ProxyError::MissingBracket)
        );
    }

    #[test]
    fn garbage_after_close_bracket_is_refused() {
        assert_eq!(
            parse_proxy_uri("[::1]x3128"),
            Err(ProxyError::TrailingAfterBracket("x3128".to_string()))
        );
    }

    // ---- write_connect_request ----

    /// Write a request into one end of a duplex pair and return exactly
    /// the bytes that reached the other end.
    async fn written_request(host: &str, port: u16) -> Vec<u8> {
        let (mut client, mut peer) = duplex(4096);
        write_connect_request(&mut client, host, port)
            .await
            .unwrap();
        drop(client);
        let mut bytes = Vec::new();
        peer.read_to_end(&mut bytes).await.unwrap();
        bytes
    }

    #[tokio::test]
    async fn request_bytes_are_exact() {
        let expected = format!(
            "CONNECT pve1.example:61000 HTTP/1.0\r\n\
             Host: pve1.example:61000\r\n\
             Proxy-Connection: keep-alive\r\n\
             User-Agent: shakenfist-spice-protocol/{}\r\n\
             \r\n",
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(
            written_request("pve1.example", 61000).await,
            expected.as_bytes()
        );
    }

    /// A Proxmox pseudo-hostname has colons in it but is not an IPv6
    /// literal, so it must reach the proxy verbatim, in the request line
    /// and in the `Host` header Proxmox actually reads.
    #[tokio::test]
    async fn proxmox_pseudo_hostname_is_sent_verbatim_in_host_header() {
        let host = "pvespiceproxy:6aaf3e30:100:pve1:61000::0ce0f00d";
        let request = String::from_utf8(written_request(host, 61000).await).unwrap();
        let mut lines = request.split("\r\n");
        assert_eq!(
            lines.next(),
            Some(format!("CONNECT {host}:61000 HTTP/1.0").as_str())
        );
        assert_eq!(lines.next(), Some(format!("Host: {host}:61000").as_str()));
    }

    #[tokio::test]
    async fn ipv6_target_is_bracketed() {
        let request = String::from_utf8(written_request("2001:db8::5", 5901).await).unwrap();
        assert!(request
            .starts_with("CONNECT [2001:db8::5]:5901 HTTP/1.0\r\nHost: [2001:db8::5]:5901\r\n"));
    }

    #[tokio::test]
    async fn target_that_could_inject_headers_is_refused_before_writing() {
        for host in ["", "evil\r\nX-Injected: 1", "a b", "tab\there", "nul\0"] {
            let (mut client, mut peer) = duplex(4096);
            let result = write_connect_request(&mut client, host, 5901).await;
            assert!(
                matches!(result, Err(ConnectError::InvalidTarget)),
                "{host:?}"
            );
            drop(client);
            let mut bytes = Vec::new();
            peer.read_to_end(&mut bytes).await.unwrap();
            assert!(bytes.is_empty(), "{host:?} wrote {bytes:?}");
        }
    }

    // ---- read_connect_response ----

    /// A duplex pair whose peer end has already had `response` written to
    /// it. The peer is returned so the test decides when it closes.
    async fn responding_peer(response: &[u8]) -> (DuplexStream, DuplexStream) {
        let (client, mut peer) = duplex(64 * 1024);
        peer.write_all(response).await.unwrap();
        (client, peer)
    }

    async fn response_result(response: &[u8]) -> Result<(), ConnectError> {
        let (mut client, peer) = responding_peer(response).await;
        drop(peer);
        read_connect_response(&mut client).await
    }

    #[tokio::test]
    async fn status_200_establishes_the_tunnel() {
        response_result(b"HTTP/1.0 200 Connection established\r\n\r\n")
            .await
            .unwrap();
        response_result(b"HTTP/1.1 200 OK\r\nProxy-Agent: pve-api-daemon/3.0\r\n\r\n")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn status_401_carries_the_ticket_hint() {
        let err = response_result(b"HTTP/1.0 401 invalid ticket\r\n\r\n")
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ConnectError::Unauthorized(line) if line == "HTTP/1.0 401 invalid ticket"),
            "{err:?}"
        );
        let message = err.to_string();
        assert!(message.contains("HTTP/1.0 401 invalid ticket"), "{message}");
        assert!(message.contains("about 30 seconds"), "{message}");
        assert!(message.contains("Proxmox"), "{message}");
    }

    #[tokio::test]
    async fn status_407_names_proxy_authentication_as_unsupported() {
        let err = response_result(
            b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"x\"\r\n\r\n",
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ConnectError::ProxyAuthenticationRequired(_)),
            "{err:?}"
        );
        let message = err.to_string();
        assert!(message.contains("authentication"), "{message}");
        assert!(message.contains("not supported"), "{message}");
    }

    #[tokio::test]
    async fn status_502_is_refused_with_its_status_line() {
        let err = response_result(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
            .await
            .unwrap_err();
        match err {
            ConnectError::Refused {
                status,
                ref status_line,
            } => {
                assert_eq!(status, 502);
                assert_eq!(status_line, "HTTP/1.1 502 Bad Gateway");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn eof_mid_headers_is_an_error() {
        let response = b"HTTP/1.0 200 Connection established\r\nProxy-Agent: x\r\n";
        let err = response_result(response).await.unwrap_err();
        assert!(
            matches!(err, ConnectError::ClosedBeforeHeaders { received } if received == response.len()),
            "{err:?}"
        );
        let err = response_result(b"").await.unwrap_err();
        assert!(
            matches!(err, ConnectError::ClosedBeforeHeaders { received: 0 }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn oversize_header_block_is_an_error() {
        let mut response = b"HTTP/1.0 200 OK\r\nX-Padding: ".to_vec();
        response.resize(MAX_CONNECT_RESPONSE_BYTES + 1024, b'a');
        // The peer stays open: the cap, not EOF, must end the read.
        let (mut client, _peer) = responding_peer(&response).await;
        let err = read_connect_response(&mut client).await.unwrap_err();
        assert!(
            matches!(err, ConnectError::HeadersTooLarge { limit } if limit == MAX_CONNECT_RESPONSE_BYTES),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn header_block_exactly_at_the_cap_is_accepted() {
        let mut response = b"HTTP/1.0 200 OK\r\nX-Padding: ".to_vec();
        response.resize(MAX_CONNECT_RESPONSE_BYTES - 4, b'a');
        response.extend_from_slice(b"\r\n\r\n");
        assert_eq!(response.len(), MAX_CONNECT_RESPONSE_BYTES);
        response_result(&response).await.unwrap();
    }

    #[tokio::test]
    async fn non_http_status_line_is_an_error() {
        let err = response_result(b"SSH-2.0-OpenSSH_9.6\r\n\r\n")
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ConnectError::MalformedStatusLine(line) if line == "SSH-2.0-OpenSSH_9.6"),
            "{err:?}"
        );
    }

    /// The bytes after the blank line belong to the tunnel (the server's
    /// first TLS record, in practice). `read_connect_response` must leave
    /// every one of them for the caller.
    #[tokio::test]
    async fn bytes_after_the_headers_are_not_consumed() {
        let tunnel_bytes: &[u8] = b"\x16\x03\x03\x00\x2a first TLS record and more";
        let mut response =
            b"HTTP/1.0 200 Connection established\r\nProxy-Agent: test\r\n\r\n".to_vec();
        response.extend_from_slice(tunnel_bytes);
        let (mut client, peer) = responding_peer(&response).await;
        drop(peer);

        read_connect_response(&mut client).await.unwrap();

        let mut rest = Vec::new();
        client.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, tunnel_bytes);
    }

    // ---- parse_connect_response ----

    #[test]
    fn any_2xx_status_establishes_the_tunnel() {
        for block in [
            &b"HTTP/1.0 200 OK\r\n\r\n"[..],
            b"HTTP/1.1 204\r\n\r\n",
            b"HTTP/1.1 299 Whatever\r\n\r\n",
            b"HTTP/1.1   200 extra spaces as GLib allows\r\n\r\n",
        ] {
            parse_connect_response(block).unwrap();
        }
    }

    #[test]
    fn non_2xx_statuses_are_refused() {
        for (block, status) in [
            (&b"HTTP/1.1 100 Continue\r\n\r\n"[..], 100),
            (b"HTTP/1.1 301 Moved\r\n\r\n", 301),
            (b"HTTP/1.0 403 Forbidden\r\n\r\n", 403),
            (b"HTTP/1.0 500\r\n\r\n", 500),
        ] {
            let err = parse_connect_response(block).unwrap_err();
            assert!(
                matches!(err, ConnectError::Refused { status: s, .. } if s == status),
                "{err:?}"
            );
        }
    }

    #[test]
    fn malformed_status_lines_are_refused() {
        for block in [
            &b""[..],
            b"\r\n\r\n",
            b"HTTP/2 200\r\n\r\n",
            b"HTTP/1.2 200 OK\r\n\r\n",
            b"http/1.1 200 OK\r\n\r\n",
            b"HTTP/1.1200 OK\r\n\r\n",
            b"HTTP/1.1 20 OK\r\n\r\n",
            b"HTTP/1.1 2000 OK\r\n\r\n",
            b"HTTP/1.1 20x OK\r\n\r\n",
            b"HTTP/1.1 \r\n\r\n",
            b"HTTP/1.1 200OK\r\n\r\n",
        ] {
            let err = parse_connect_response(block).unwrap_err();
            assert!(
                matches!(err, ConnectError::MalformedStatusLine(_)),
                "{block:?}: {err:?}"
            );
        }
    }

    #[test]
    fn quoted_status_line_is_truncated() {
        let mut block = b"HTTP/1.1 502 ".to_vec();
        block.resize(4096, b'x');
        let err = parse_connect_response(&block).unwrap_err();
        let ConnectError::Refused { status_line, .. } = err else {
            panic!("expected Refused, got {err:?}");
        };
        assert_eq!(status_line.chars().count(), MAX_QUOTED_STATUS_LINE + 3);
        assert!(status_line.ends_with("..."));
    }

    #[test]
    fn non_utf8_status_line_does_not_panic() {
        let err = parse_connect_response(b"HTTP/1.1 502 \xff\xfe\r\n\r\n").unwrap_err();
        assert!(
            matches!(err, ConnectError::Refused { status: 502, .. }),
            "{err:?}"
        );
    }
}
