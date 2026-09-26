/// SPICE link protocol - handshake and authentication
use anyhow::{anyhow, Result};
use byteorder::{BigEndian, LittleEndian, ReadBytesExt};
use rand::rngs::OsRng;
use rsa::pkcs8::{DecodePublicKey, EncodePublicKey};
use rsa::{Oaep, RsaPrivateKey, RsaPublicKey};
use sha1::Sha1;
use std::io::{Cursor, IoSlice, Read};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream as ClientTlsStream;
use tokio_rustls::server::TlsStream as ServerTlsStream;

use super::constants::*;
use crate::reader::{BoundedReader, LinkError};

/// Sanity cap on the declared link-message body `size`, in bytes. This is a
/// DoS guard set far above any legitimate link message (real ones are only
/// tens of bytes); a client declaring more than this is treated as hostile
/// and rejected before any allocation.
pub(crate) const MAX_LINK_MESSAGE_SIZE: usize = 4096;

/// Sanity cap on the number of capability words in each of the common and
/// channel capability arrays. Real implementations send 1 word; 16 is
/// generous headroom while still bounding a hostile count before allocation.
pub(crate) const MAX_CAP_WORDS: usize = 16;

/// Either a plain TCP stream or a TLS-wrapped stream (client or server
/// role). `Tls` is the outbound (client) role used when ryll connects to
/// a SPICE server; `TlsServer` is the inbound (server) role used when a
/// proxy terminates a SPICE connection from a client.
#[allow(clippy::large_enum_variant)]
pub enum SpiceStream {
    Plain(TcpStream),
    Tls(ClientTlsStream<TcpStream>),
    TlsServer(ServerTlsStream<TcpStream>),
}

impl SpiceStream {
    pub async fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        match self {
            SpiceStream::Plain(s) => {
                s.read_exact(buf).await?;
            }
            SpiceStream::Tls(s) => {
                s.read_exact(buf).await?;
            }
            SpiceStream::TlsServer(s) => {
                s.read_exact(buf).await?;
            }
        }
        Ok(())
    }

    pub async fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        match self {
            SpiceStream::Plain(s) => s.write_all(buf).await?,
            SpiceStream::Tls(s) => s.write_all(buf).await?,
            SpiceStream::TlsServer(s) => s.write_all(buf).await?,
        }
        Ok(())
    }

    pub async fn flush(&mut self) -> Result<()> {
        match self {
            SpiceStream::Plain(s) => s.flush().await?,
            SpiceStream::Tls(s) => s.flush().await?,
            SpiceStream::TlsServer(s) => s.flush().await?,
        }
        Ok(())
    }
}

/// Delegate `AsyncRead` to the inner stream so `SpiceStream` can be used
/// anywhere a generic `AsyncRead + AsyncWrite + Unpin` bound is required
/// (e.g. `perform_link`/`perform_auth`), alongside its inherent
/// `read_exact`/`write_all`/`flush` helpers used by channel handlers.
impl AsyncRead for SpiceStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            SpiceStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            SpiceStream::Tls(s) => Pin::new(s).poll_read(cx, buf),
            SpiceStream::TlsServer(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for SpiceStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            SpiceStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            SpiceStream::Tls(s) => Pin::new(s).poll_write(cx, buf),
            SpiceStream::TlsServer(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            SpiceStream::Plain(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            SpiceStream::Tls(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            SpiceStream::TlsServer(s) => Pin::new(s).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            SpiceStream::Plain(s) => s.is_write_vectored(),
            SpiceStream::Tls(s) => s.is_write_vectored(),
            SpiceStream::TlsServer(s) => s.is_write_vectored(),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            SpiceStream::Plain(s) => Pin::new(s).poll_flush(cx),
            SpiceStream::Tls(s) => Pin::new(s).poll_flush(cx),
            SpiceStream::TlsServer(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            SpiceStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            SpiceStream::Tls(s) => Pin::new(s).poll_shutdown(cx),
            SpiceStream::TlsServer(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// SpiceLinkMess - client link message
#[derive(Debug)]
pub struct SpiceLinkMess {
    pub connection_id: u32,
    pub channel_type: u8,
    pub channel_id: u8,
    pub common_caps: Vec<u32>,
    pub channel_caps: Vec<u32>,
}

impl SpiceLinkMess {
    /// Convenience constructor for the common case of one capability word
    /// per set. A sender with multi-word capability sets builds the struct
    /// directly, as [`perform_link_with_caps`] does.
    pub fn new(
        connection_id: u32,
        channel_type: ChannelType,
        channel_id: u8,
        common_caps: u32,
        channel_caps: u32,
    ) -> Self {
        SpiceLinkMess {
            connection_id,
            channel_type: channel_type as u8,
            channel_id,
            common_caps: vec![common_caps],
            channel_caps: vec![channel_caps],
        }
    }

    pub fn serialize(&self) -> Vec<u8> {
        let num_common_caps = self.common_caps.len() as u32;
        let num_channel_caps = self.channel_caps.len() as u32;

        // Capabilities offset (from start of capabilities area)
        let caps_offset = 18u32; // Size of fixed fields after magic/version

        // Calculate size
        let caps_size = (num_common_caps + num_channel_caps) as usize * 4;
        let size = caps_offset as usize + caps_size;

        let mut buf = Vec::with_capacity(16 + size);

        // Magic
        buf.extend_from_slice(SPICE_MAGIC);

        // Version
        buf.extend_from_slice(&SPICE_VERSION_MAJOR.to_le_bytes());
        buf.extend_from_slice(&SPICE_VERSION_MINOR.to_le_bytes());

        // Size of following data
        buf.extend_from_slice(&(size as u32).to_le_bytes());

        // Connection ID
        buf.extend_from_slice(&self.connection_id.to_le_bytes());

        // Channel type and ID
        buf.push(self.channel_type);
        buf.push(self.channel_id);

        // Number of capabilities
        buf.extend_from_slice(&num_common_caps.to_le_bytes());
        buf.extend_from_slice(&num_channel_caps.to_le_bytes());

        // Capabilities offset
        buf.extend_from_slice(&caps_offset.to_le_bytes());

        // Common capabilities
        for cap in &self.common_caps {
            buf.extend_from_slice(&(*cap).to_le_bytes());
        }

        // Channel capabilities
        for cap in &self.channel_caps {
            buf.extend_from_slice(&(*cap).to_le_bytes());
        }

        buf
    }

    /// Parse a full link message (16-byte header + body) received from a
    /// client.
    ///
    /// This parser faces the public internet: every length, count, and
    /// offset in `data` is treated as hostile. All bounds enforcement goes
    /// through [`BoundedReader`], so the function is panic-free for any
    /// input — there is no open-coded slice indexing or unchecked
    /// arithmetic.
    ///
    /// The capability words are addressed by `caps_offset`, measured from
    /// the start of the body (i.e. byte 16 of the whole message); `18` when
    /// they immediately follow the fixed fields. Any bytes after the
    /// capability words but within the declared `size` are tolerated and
    /// ignored (qemu clients may pad).
    ///
    /// # Errors
    ///
    /// - [`LinkError::BadMagic`] if the four-byte magic is not `REDQ`.
    /// - [`LinkError::UnsupportedVersion`] if `major` is not
    ///   [`SPICE_VERSION_MAJOR`] (any minor is accepted).
    /// - [`LinkError::TooLarge`] if the declared `size` exceeds
    ///   [`MAX_LINK_MESSAGE_SIZE`], or a capability count exceeds
    ///   [`MAX_CAP_WORDS`] (rejected before allocation).
    /// - [`LinkError::Truncated`] if the buffer ends before a declared field
    ///   or capability word could be read.
    /// - [`LinkError::BadOffset`] if `caps_offset` falls outside the body.
    pub fn parse(data: &[u8]) -> Result<Self, LinkError> {
        let mut reader = BoundedReader::new(data);

        // Header (16 bytes): magic, major, minor, size.
        let magic = reader.read_array::<4>()?;
        if &magic != SPICE_MAGIC {
            return Err(LinkError::BadMagic { found: magic });
        }
        let major = reader.read_u32()?;
        let minor = reader.read_u32()?;
        if major != SPICE_VERSION_MAJOR {
            return Err(LinkError::UnsupportedVersion { major, minor });
        }
        let size = reader.read_u32()? as usize;
        if size > MAX_LINK_MESSAGE_SIZE {
            return Err(LinkError::TooLarge {
                what: "link message size",
                value: size,
                max: MAX_LINK_MESSAGE_SIZE,
            });
        }

        // Body: `size` bytes starting at byte 16 of the message. A buffer
        // holding fewer than 16 + size bytes surfaces here as BadOffset
        // rather than a panic.
        let mut body = reader.sub_reader(16, size)?;

        // Fixed fields (18 bytes). `caps_offset` is relative to the body
        // start, so all subsequent addressing uses `body`.
        let connection_id = body.read_u32()?;
        let channel_type = body.read_u8()?;
        let channel_id = body.read_u8()?;
        let num_common_caps = body.read_u32()? as usize;
        let num_channel_caps = body.read_u32()? as usize;
        let caps_offset = body.read_u32()? as usize;

        // Capability words live at `caps_offset` within the body and run to
        // the end of the declared body. An offset past the body end yields
        // BadOffset via the checked subtraction; the bounded sub_reader then
        // guarantees the words fit inside the declared size.
        let caps_len = size.checked_sub(caps_offset).ok_or(LinkError::BadOffset {
            offset: caps_offset,
            len: 0,
            buffer_len: size,
        })?;
        let mut caps = body.sub_reader(caps_offset, caps_len)?;
        let common_caps = caps.read_vec_u32(num_common_caps, MAX_CAP_WORDS)?;
        let channel_caps = caps.read_vec_u32(num_channel_caps, MAX_CAP_WORDS)?;

        Ok(SpiceLinkMess {
            connection_id,
            channel_type,
            channel_id,
            common_caps,
            channel_caps,
        })
    }
}

/// SpiceLinkReply - server response
#[derive(Debug)]
pub struct SpiceLinkReply {
    pub error: SpiceError,
    pub pub_key: Vec<u8>,
    pub common_caps: Vec<u32>,
    pub channel_caps: Vec<u32>,
}

impl SpiceLinkReply {
    /// Check that the server supports the common capabilities this crate's
    /// client drivers depend on, so a mismatch fails here with a clear
    /// error rather than as a garbled exchange later.
    ///
    /// All three live in common-capability word 0:
    ///
    /// - [`capabilities::AUTH_SELECTION`]: [`perform_auth`] always writes
    ///   an auth mechanism selector before the ticket.
    /// - [`capabilities::AUTH_SPICE`]: the mechanism [`perform_auth`]
    ///   selects is always SPICE ticket auth. A server with SASL enabled
    ///   lists `AUTH_SASL` instead, and rejects the SPICE mechanism.
    /// - [`capabilities::MINI_HEADER`]: this crate only speaks the 6-byte
    ///   mini header, not the full data header.
    ///
    /// A server's reply lists the capabilities it supports, not the
    /// intersection with what the link message advertised (that is
    /// checked before sending, by [`perform_link_with_caps`]). So this
    /// catches a server, or an intermediary, that cannot speak the
    /// exchange this crate uses at all. Only meaningful on a successful
    /// reply; an error reply carries no capabilities.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::MissingRequiredCaps`] naming each missing
    /// capability.
    pub fn check_client_requirements(&self) -> Result<(), LinkError> {
        check_required_common_caps("server", REQUIRED_SERVER_CAPS, &self.common_caps)
    }

    /// Parse a link reply (16-byte header + body) received from a server.
    ///
    /// Although ryll only parses replies from SPICE servers it chose to
    /// connect to, the reply still arrives over the network, so counts and
    /// lengths are treated as untrusted: parsing goes through
    /// [`BoundedReader`], the declared size is capped at
    /// [`MAX_LINK_MESSAGE_SIZE`], and capability counts are capped at
    /// [`MAX_CAP_WORDS`] before any allocation. (A prior unbounded
    /// `Vec::with_capacity` on the wire-supplied count could be driven to
    /// an out-of-memory abort; this was found by the fuzz target added for
    /// this parser.)
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut reader = BoundedReader::new(data);

        // Header: magic, major, minor, size.
        let magic = reader.read_array::<4>()?;
        if &magic != SPICE_MAGIC {
            return Err(LinkError::BadMagic { found: magic }.into());
        }
        let major = reader.read_u32()?;
        let minor = reader.read_u32()?;
        if major != SPICE_VERSION_MAJOR {
            return Err(LinkError::UnsupportedVersion { major, minor }.into());
        }
        let size = reader.read_u32()? as usize;
        if size > MAX_LINK_MESSAGE_SIZE {
            return Err(LinkError::TooLarge {
                what: "link reply size",
                value: size,
                max: MAX_LINK_MESSAGE_SIZE,
            }
            .into());
        }

        // Body: `size` bytes at byte 16. A short buffer surfaces as
        // BadOffset rather than a panic.
        let mut body = reader.sub_reader(16, size)?;

        let error = SpiceError::from_u32(body.read_u32()?);

        // RSA public key: always 162 bytes (DER SubjectPublicKeyInfo).
        let pub_key = body.read_bytes(162)?.to_vec();

        let num_common_caps = body.read_u32()? as usize;
        let num_channel_caps = body.read_u32()? as usize;
        // caps_offset is advisory here: the reference server (and our own
        // serialize) place the words immediately after this field, so we
        // read them sequentially from the current position.
        let _caps_offset = body.read_u32()?;

        let common_caps = body.read_vec_u32(num_common_caps, MAX_CAP_WORDS)?;
        let channel_caps = body.read_vec_u32(num_channel_caps, MAX_CAP_WORDS)?;

        Ok(SpiceLinkReply {
            error,
            pub_key,
            common_caps,
            channel_caps,
        })
    }

    /// Build an error reply (zeroed 162-byte key, no capabilities), used
    /// for the need_secured TLS redirect and other link-time failures.
    pub fn error_reply(error: SpiceError) -> Self {
        SpiceLinkReply {
            error,
            pub_key: vec![0u8; 162],
            common_caps: Vec::new(),
            channel_caps: Vec::new(),
        }
    }

    /// Serialize a link reply (16-byte header + body) for sending to a
    /// client.
    ///
    /// The body layout mirrors [`parse`](Self::parse): a `u32` error code,
    /// the 162-byte DER RSA public key (all-zero on error replies),
    /// `num_common_caps`/`num_channel_caps`/`caps_offset` (all `u32`), then
    /// the common and channel capability words in turn. `caps_offset` is
    /// `178` (the fixed body prefix) when at least one capability word
    /// follows, or `0` when there are none — matching the reference
    /// implementation, which uses `0` for the no-caps error-redirect reply.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::BadKeyLength`] if `pub_key` is not exactly 162
    /// bytes.
    pub fn serialize(&self) -> Result<Vec<u8>, LinkError> {
        if self.pub_key.len() != 162 {
            return Err(LinkError::BadKeyLength {
                len: self.pub_key.len(),
            });
        }

        let num_common_caps = self.common_caps.len() as u32;
        let num_channel_caps = self.channel_caps.len() as u32;

        // Fixed body prefix: error (4) + pub_key (162) + three u32 fields
        // (12) = 178. Capability words immediately follow when present; the
        // reference implementation reports offset 0 when there are none.
        let caps_offset = if num_common_caps + num_channel_caps > 0 {
            178u32
        } else {
            0u32
        };
        let size = 178 + (num_common_caps + num_channel_caps) as usize * 4;

        let mut buf = Vec::with_capacity(16 + size);

        // Header: magic, version, size.
        buf.extend_from_slice(SPICE_MAGIC);
        buf.extend_from_slice(&SPICE_VERSION_MAJOR.to_le_bytes());
        buf.extend_from_slice(&SPICE_VERSION_MINOR.to_le_bytes());
        buf.extend_from_slice(&(size as u32).to_le_bytes());

        // Body.
        buf.extend_from_slice(&self.error.to_u32().to_le_bytes());
        buf.extend_from_slice(&self.pub_key);
        buf.extend_from_slice(&num_common_caps.to_le_bytes());
        buf.extend_from_slice(&num_channel_caps.to_le_bytes());
        buf.extend_from_slice(&caps_offset.to_le_bytes());

        for cap in &self.common_caps {
            buf.extend_from_slice(&(*cap).to_le_bytes());
        }
        for cap in &self.channel_caps {
            buf.extend_from_slice(&(*cap).to_le_bytes());
        }

        Ok(buf)
    }
}

/// Parse the RSA public key from the SPICE link reply.
///
/// QEMU sends the key as DER-encoded SubjectPublicKeyInfo (ASN.1 SEQUENCE
/// starting with 0x30). Older descriptions of the protocol mention a raw
/// format (4-byte BE length-prefixed modulus + exponent), so we try DER
/// first and fall back to the raw format.
fn parse_public_key(pub_key_bytes: &[u8]) -> Result<RsaPublicKey> {
    // DER/SPKI: starts with ASN.1 SEQUENCE tag 0x30
    if pub_key_bytes.first() == Some(&0x30) {
        if let Ok(key) = RsaPublicKey::from_public_key_der(pub_key_bytes) {
            return Ok(key);
        }
    }

    // Fallback: raw SPICE format (4-byte BE modulus length + modulus +
    // 4-byte BE exponent length + exponent)
    if pub_key_bytes.len() < 8 {
        return Err(anyhow!("Public key too short"));
    }

    let mut cursor = Cursor::new(pub_key_bytes);

    let mod_size = ReadBytesExt::read_u32::<BigEndian>(&mut cursor)? as usize;
    if mod_size > 256 || mod_size == 0 {
        return Err(anyhow!("Invalid modulus size: {}", mod_size));
    }

    let mut modulus = vec![0u8; mod_size];
    Read::read_exact(&mut cursor, &mut modulus)?;

    let exp_size = ReadBytesExt::read_u32::<BigEndian>(&mut cursor)? as usize;
    if exp_size > 8 || exp_size == 0 {
        return Err(anyhow!("Invalid exponent size: {}", exp_size));
    }

    let mut exponent = vec![0u8; exp_size];
    Read::read_exact(&mut cursor, &mut exponent)?;

    let n = rsa::BigUint::from_bytes_be(&modulus);
    let e = rsa::BigUint::from_bytes_be(&exponent);
    Ok(RsaPublicKey::new(n, e)?)
}

/// Perform SPICE authentication
pub fn encrypt_password(pub_key_bytes: &[u8], password: &str) -> Result<Vec<u8>> {
    let pub_key = parse_public_key(pub_key_bytes)?;

    // Encrypt password using RSA-OAEP with SHA1
    let padding = Oaep::new::<Sha1>();
    let mut rng = OsRng;

    // SPICE auth convention is a NUL-terminated plaintext.  Every
    // reference implementation does this and every spec-compliant
    // server depends on it:
    //   spice-gtk  (spice-channel.c:1265,1273,1282): encrypts
    //              `strlen(password) + 1` bytes.
    //   spice-html5 (spiceconn.js:274): sends
    //              `this.password + String.fromCharCode(0)`.
    //   spice-server (reds.cpp:2086): writes `password[len] = '\0'`
    //              before `strcmp`, treating the decrypted blob as
    //              a C string.
    // Omitting the NUL causes any server that strips a trailing
    // sentinel byte (kerbside does this too) to chop the last
    // character of the real password and reject the auth.
    let mut plaintext = Vec::with_capacity(password.len() + 1);
    plaintext.extend_from_slice(password.as_bytes());
    plaintext.push(0);
    let encrypted = pub_key.encrypt(&mut rng, padding, &plaintext)?;

    // Pad to 128 bytes (RSA block size)
    let mut result = vec![0u8; 128];
    let start = 128 - encrypted.len();
    result[start..].copy_from_slice(&encrypted);

    Ok(result)
}

/// Generate a fresh per-connection RSA keypair for SPICE ticket exchange.
///
/// Returns the private key and its DER SubjectPublicKeyInfo encoding
/// (exactly 162 bytes for the 1024-bit key SPICE uses), suitable for the
/// `pub_key` field of a [`SpiceLinkReply`].
///
/// Generate a fresh keypair for every connection and never reuse one. The
/// `rsa` crate's decryption is not constant-time (RUSTSEC-2023-0071, the
/// "Marvin" attack), so reusing a key across many decryptions would expose a
/// timing side-channel. A per-connection key limits an attacker to a single
/// decryption per key, far below what the attack requires.
pub fn generate_ticket_keypair() -> Result<(RsaPrivateKey, Vec<u8>)> {
    let mut rng = OsRng;

    // `RsaPrivateKey::new` uses the default public exponent of 65537, which
    // matches the Python reference (`ClientPassword` in kerbside's
    // proxy.py) and every other SPICE server implementation.
    let private_key = RsaPrivateKey::new(&mut rng, 1024)?;
    let public_key = private_key.to_public_key();
    let der = public_key.to_public_key_der()?.as_bytes().to_vec();

    // DER SubjectPublicKeyInfo encoding of a 1024-bit RSA key with a
    // 65537 exponent is always exactly 162 bytes -- the field width baked
    // into SpiceLinkReply's wire format. Key generation is our own trusted
    // input (not attacker-controlled), so a debug_assert! is sufficient:
    // this would only fire on a bug in our own key generation, never on
    // hostile input.
    debug_assert_eq!(der.len(), 162, "1024-bit RSA SPKI DER must be 162 bytes");

    Ok((private_key, der))
}

/// Decrypt a 128-byte RSA-OAEP(SHA-1) ticket blob received from a client
/// using the server's private key, returning the recovered password.
///
/// The SPICE convention appends a NUL terminator to the plaintext before
/// encryption (see [`encrypt_password`]); a single trailing NUL is
/// stripped if present.
pub fn decrypt_password(key: &RsaPrivateKey, blob: &[u8; 128]) -> Result<String, LinkError> {
    let padding = Oaep::new::<Sha1>();
    let mut decrypted = key
        .decrypt(padding, blob)
        .map_err(|_| LinkError::DecryptFailed)?;

    // encrypt_password always appends exactly one NUL terminator before
    // encrypting; strip it if present rather than unconditionally (as the
    // Python reference does), so a plaintext without one still round-trips.
    if decrypted.last() == Some(&0) {
        decrypted.pop();
    }

    String::from_utf8(decrypted).map_err(|_| LinkError::BadUtf8)
}

/// Common capabilities (all in word 0) a link message from this crate's
/// client drivers must advertise: the server picks its auth exchange and
/// message framing from these. See [`perform_link_with_caps`].
const REQUIRED_ADVERTISED_CAPS: &[(u32, &str)] = &[
    (capabilities::AUTH_SELECTION, "AUTH_SELECTION"),
    (capabilities::MINI_HEADER, "MINI_HEADER"),
];

/// Common capabilities (all in word 0) a server's link reply must list for
/// this crate's client drivers to talk to it. A server never consults
/// whether the client advertised `AUTH_SPICE`, but offers it only when
/// SASL is off. See [`SpiceLinkReply::check_client_requirements`].
const REQUIRED_SERVER_CAPS: &[(u32, &str)] = &[
    (capabilities::AUTH_SELECTION, "AUTH_SELECTION"),
    (capabilities::AUTH_SPICE, "AUTH_SPICE"),
    (capabilities::MINI_HEADER, "MINI_HEADER"),
];

/// Fail with [`LinkError::MissingRequiredCaps`] naming each of `required`
/// absent from word 0 of `common_caps`, the `set` of caps being checked.
fn check_required_common_caps(
    set: &'static str,
    required: &[(u32, &'static str)],
    common_caps: &[u32],
) -> Result<(), LinkError> {
    let word0 = common_caps.first().copied().unwrap_or(0);
    let missing: Vec<&'static str> = required
        .iter()
        .filter(|(bit, _)| word0 & bit == 0)
        .map(|(_, name)| *name)
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(LinkError::MissingRequiredCaps {
            set,
            missing,
            caps: common_caps.to_vec(),
        })
    }
}

/// Check capability words a caller wants advertised in a link message,
/// before anything is sent: each set must fit in [`MAX_CAP_WORDS`] words
/// (what this crate's own parsers accept), and the common set must include
/// [`REQUIRED_ADVERTISED_CAPS`]. See [`perform_link_with_caps`].
pub(crate) fn check_advertised_caps(
    common_caps: &[u32],
    channel_caps: &[u32],
) -> Result<(), LinkError> {
    for (what, caps) in [
        ("advertised common capability words", common_caps),
        ("advertised channel capability words", channel_caps),
    ] {
        if caps.len() > MAX_CAP_WORDS {
            return Err(LinkError::TooLarge {
                what,
                value: caps.len(),
                max: MAX_CAP_WORDS,
            });
        }
    }
    check_required_common_caps("advertised", REQUIRED_ADVERTISED_CAPS, common_caps)
}

/// The channel capability word ryll advertises by default for
/// `channel_type`, as sent by [`perform_link`].
///
/// The display caps are critical: without COMPOSITE the guest QXL driver
/// renders via a software fallback that produces far fewer display updates.
pub fn default_channel_caps(channel_type: ChannelType) -> u32 {
    match channel_type {
        ChannelType::Display => capabilities::DEFAULT_DISPLAY,
        ChannelType::Usbredir => capabilities::DEFAULT_SPICEVMC,
        _ => capabilities::DEFAULT_MAIN,
    }
}

/// Perform the link handshake, advertising ryll's default capabilities:
/// [`capabilities::DEFAULT_COMMON`] and [`default_channel_caps`] for
/// `channel_type`. See [`perform_link_with_caps`] to advertise others.
pub async fn perform_link<S>(
    stream: &mut S,
    connection_id: u32,
    channel_type: ChannelType,
    channel_id: u8,
) -> Result<SpiceLinkReply>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    perform_link_with_caps(
        stream,
        connection_id,
        channel_type,
        channel_id,
        &[capabilities::DEFAULT_COMMON],
        &[default_channel_caps(channel_type)],
    )
    .await
}

/// Perform the link handshake, advertising exactly the given common and
/// channel capability words.
///
/// Each slice is sent verbatim as the link message's capability array, so
/// multi-word capability sets are supported (word `n` carries bits
/// `32n..32n+31`). An empty slice advertises no capabilities in that set.
/// This is what a proxy uses to forward a real client's capabilities to
/// the server on its backend leg, rather than ryll's defaults.
///
/// `common_caps` must include [`capabilities::AUTH_SELECTION`] and
/// [`capabilities::MINI_HEADER`]. A SPICE server decides whether to expect
/// an auth mechanism selector, and whether to use the mini header, from
/// what the *client advertised* — its own reply lists the capabilities it
/// supports regardless — so leaving either out would have the server speak
/// a protocol that [`perform_auth`] and this crate's message framing do
/// not. Each slice must also be at most 16 words, the limit this crate's
/// own parsers accept. Both are refused here, before anything is written.
///
/// Returns the server's [`SpiceLinkReply`] as parsed, without judging it:
/// the caller inspects `reply.error`, and on success should call
/// [`SpiceLinkReply::check_client_requirements`] before [`perform_auth`].
///
/// # Errors
///
/// Fails without writing, with [`LinkError::MissingRequiredCaps`] if
/// `common_caps` lacks a required capability or [`LinkError::TooLarge`]
/// if either slice is over 16 words. Propagates write/read errors from
/// `stream`, rejects a reply whose declared size exceeds
/// [`MAX_LINK_MESSAGE_SIZE`] before allocating for it, and propagates any
/// parse error from [`SpiceLinkReply::parse`].
pub async fn perform_link_with_caps<S>(
    stream: &mut S,
    connection_id: u32,
    channel_type: ChannelType,
    channel_id: u8,
    common_caps: &[u32],
    channel_caps: &[u32],
) -> Result<SpiceLinkReply>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    check_advertised_caps(common_caps, channel_caps)?;

    // Send link message
    let link_mess = SpiceLinkMess {
        connection_id,
        channel_type: channel_type as u8,
        channel_id,
        common_caps: common_caps.to_vec(),
        channel_caps: channel_caps.to_vec(),
    };

    let data = link_mess.serialize();
    stream.write_all(&data).await?;
    stream.flush().await?;

    // Read reply header (16 bytes: magic + version + size)
    let mut header = [0u8; 16];
    stream.read_exact(&mut header).await?;

    // Parse size from header, and refuse an oversized one before
    // allocating the body buffer: the parser enforces the same cap, but
    // only after the allocation it would otherwise be asked to make.
    let size = {
        let mut cursor = Cursor::new(&header[12..16]);
        ReadBytesExt::read_u32::<LittleEndian>(&mut cursor)? as usize
    };
    if size > MAX_LINK_MESSAGE_SIZE {
        return Err(LinkError::TooLarge {
            what: "link reply size",
            value: size,
            max: MAX_LINK_MESSAGE_SIZE,
        }
        .into());
    }

    // Read rest of reply
    let mut reply_data = vec![0u8; 16 + size];
    reply_data[..16].copy_from_slice(&header);
    stream.read_exact(&mut reply_data[16..]).await?;

    SpiceLinkReply::parse(&reply_data)
}

/// Perform authentication
pub async fn perform_auth<S>(stream: &mut S, pub_key: &[u8], password: Option<&str>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    // Send auth mechanism selection
    stream
        .write_all(&AUTH_MECHANISM_SPICE.to_le_bytes())
        .await?;

    // Send encrypted password (or empty password)
    let encrypted = if let Some(pwd) = password {
        encrypt_password(pub_key, pwd)?
    } else {
        encrypt_password(pub_key, "")?
    };

    stream.write_all(&encrypted).await?;
    stream.flush().await?;

    // Read auth result
    let mut result = [0u8; 4];
    stream.read_exact(&mut result).await?;

    let error_code = {
        let mut cursor = Cursor::new(&result);
        ReadBytesExt::read_u32::<LittleEndian>(&mut cursor)?
    };

    let error = SpiceError::from_u32(error_code);
    if error != SpiceError::Ok {
        return Err(anyhow!("Authentication failed: {:?}", error));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Server-role handshake drivers
// ---------------------------------------------------------------------------
//
// These mirror the client drivers `perform_link`/`perform_auth`, but for the
// inbound (server / proxy) side of the wire. They are deliberately split into
// small, composable steps rather than one monolithic `perform_server_link`:
// the real SPICE proxy must perform a token lookup against an external
// authorization service *between* decrypting the client's ticket and deciding
// the auth verdict. `read_auth_ticket` therefore returns the recovered
// password and stops; the caller consults its authorization service and then
// calls `send_auth_result` with the verdict. Fusing these steps would force
// the token lookup into this crate, which has no business knowing about it.

/// Read a client's link message from `stream`.
///
/// Reads the 16-byte header, extracts the declared body `size` from bytes
/// `[12..16]` (little-endian `u32`), reads exactly `size` further bytes, then
/// hands the concatenated 16 + `size` byte buffer to [`SpiceLinkMess::parse`]
/// (which re-validates every field against hostile input).
///
/// The declared `size` is checked against [`MAX_LINK_MESSAGE_SIZE`] *before*
/// allocating the body buffer, so a hostile client cannot induce a huge
/// speculative allocation with an oversized length field.
///
/// This driver bounds *memory* but not *time*: `read_exact` will wait
/// indefinitely for a slow or stalled peer. Callers accepting untrusted
/// connections (e.g. a proxy accept loop) must impose an I/O timeout — for
/// example `tokio::time::timeout` — and cap the number of concurrent
/// connections; these primitives deliberately leave that policy to the caller.
///
/// # Errors
///
/// Propagates read errors from `stream`, and any [`LinkError`] produced by the
/// size cap or by [`SpiceLinkMess::parse`] (converted to `anyhow` via `?`).
pub async fn read_link_mess<S>(stream: &mut S) -> Result<SpiceLinkMess>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    // Header: magic, major, minor, size.
    let mut header = [0u8; 16];
    stream.read_exact(&mut header).await?;

    // Declared body size lives in bytes [12..16]. Reject an oversized value
    // before allocating the body buffer (DoS guard), reusing the same cap the
    // parser enforces.
    let size = u32::from_le_bytes(
        header[12..16]
            .try_into()
            .expect("4-byte slice of a 16-byte array"),
    ) as usize;
    if size > MAX_LINK_MESSAGE_SIZE {
        return Err(LinkError::TooLarge {
            what: "link message size",
            value: size,
            max: MAX_LINK_MESSAGE_SIZE,
        }
        .into());
    }

    // Concatenate header + body and hand the whole buffer to the parser.
    let mut buf = vec![0u8; 16 + size];
    buf[..16].copy_from_slice(&header);
    stream.read_exact(&mut buf[16..]).await?;

    Ok(SpiceLinkMess::parse(&buf)?)
}

/// Serialize `reply` and write it to `stream`, flushing afterwards.
///
/// # Errors
///
/// Propagates [`LinkError::BadKeyLength`] from serialization and any write
/// error from `stream`.
pub async fn send_link_reply<S>(stream: &mut S, reply: &SpiceLinkReply) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let data = reply.serialize()?;
    stream.write_all(&data).await?;
    stream.flush().await?;
    Ok(())
}

/// Send a `need_secured` error redirect, telling the client to reconnect over
/// the TLS port.
///
/// Convenience wrapper over [`send_link_reply`] with
/// [`SpiceLinkReply::error_reply`]`(SpiceError::NeedSecured)`.
///
/// # Errors
///
/// Propagates any write error from `stream`.
pub async fn send_need_secured<S>(stream: &mut S) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    send_link_reply(
        stream,
        &SpiceLinkReply::error_reply(SpiceError::NeedSecured),
    )
    .await
}

/// Read the client's auth mechanism selector and encrypted ticket, returning
/// the recovered password.
///
/// Reads a `u32` mechanism (little-endian); if it is not
/// [`AUTH_MECHANISM_SPICE`] the client asked for something we do not implement
/// and this errors with [`LinkError::UnsupportedAuthMechanism`]. Otherwise it
/// reads the fixed 128-byte RSA-OAEP ticket blob and decrypts it with `key`
/// via [`decrypt_password`].
///
/// This function intentionally stops at the recovered password rather than
/// deciding the auth verdict: the caller (a proxy) consults its external
/// authorization service with the password and then calls [`send_auth_result`]
/// with the outcome. See the module comment on the server-role split.
///
/// # Errors
///
/// Propagates read errors from `stream`, [`LinkError::UnsupportedAuthMechanism`]
/// on a mismatched mechanism, and [`LinkError::DecryptFailed`]/[`LinkError::BadUtf8`]
/// from decryption (all converted to `anyhow` via `?`).
pub async fn read_auth_ticket<S>(stream: &mut S, key: &RsaPrivateKey) -> Result<String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    // Auth mechanism selector (u32 LE).
    let mut mech = [0u8; 4];
    stream.read_exact(&mut mech).await?;
    let mechanism = u32::from_le_bytes(mech);
    if mechanism != AUTH_MECHANISM_SPICE {
        return Err(LinkError::UnsupportedAuthMechanism { mechanism }.into());
    }

    // Fixed 128-byte RSA-OAEP ticket blob.
    let mut blob = [0u8; 128];
    stream.read_exact(&mut blob).await?;

    Ok(decrypt_password(key, &blob)?)
}

/// Send the final auth result to the client: a single `u32` [`SpiceError`]
/// code, little-endian (`0` = ok). Flushes afterwards.
///
/// # Errors
///
/// Propagates any write error from `stream`.
pub async fn send_auth_result<S>(stream: &mut S, error: SpiceError) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    stream.write_all(&error.to_u32().to_le_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `perform_link` is generic over any `AsyncRead + AsyncWrite + Unpin
    /// + Send` type, not just `SpiceStream`. Drive its write side against
    /// a `tokio::io::duplex` in-memory stream (a non-`SpiceStream` type)
    /// and check the bytes that land on the peer: the 16-byte header
    /// (magic + version + size) followed by the link message body. The
    /// peer never replies, so `perform_link` blocks on the subsequent
    /// read; that's fine here since only the write side is under test.
    /// Full server-side handshake tests land in a later step.
    #[tokio::test]
    async fn perform_link_writes_valid_header_and_body_over_duplex_stream() {
        let (mut client, mut server) = tokio::io::duplex(4096);

        let handle = tokio::spawn(async move {
            let _ = perform_link(&mut client, 42, ChannelType::Main, 0).await;
        });

        // Read the 16-byte link message header.
        let mut header = [0u8; 16];
        server
            .read_exact(&mut header)
            .await
            .expect("read link header");

        assert_eq!(&header[0..4], SPICE_MAGIC, "link message magic mismatch");
        let major = u32::from_le_bytes(header[4..8].try_into().unwrap());
        let minor = u32::from_le_bytes(header[8..12].try_into().unwrap());
        assert_eq!(major, SPICE_VERSION_MAJOR);
        assert_eq!(minor, SPICE_VERSION_MINOR);

        // Read the rest of the link message body.
        let size = u32::from_le_bytes(
            header[12..16]
                .try_into()
                .expect("4-byte slice of a 16-byte array"),
        ) as usize;
        let mut body = vec![0u8; size];
        server
            .read_exact(&mut body)
            .await
            .expect("read link message body");

        // connection_id (u32 LE) is the first field of the body.
        let connection_id = u32::from_le_bytes(body[0..4].try_into().unwrap());
        assert_eq!(connection_id, 42);

        // Channel type/id follow the connection id.
        assert_eq!(body[4], ChannelType::Main as u8);
        assert_eq!(body[5], 0);

        // The wrapper advertises ryll's defaults, one word each.
        let mut whole = header.to_vec();
        whole.extend_from_slice(&body);
        let parsed = SpiceLinkMess::parse(&whole).expect("parse link message");
        assert_eq!(parsed.common_caps, vec![capabilities::DEFAULT_COMMON]);
        assert_eq!(parsed.channel_caps, vec![capabilities::DEFAULT_MAIN]);

        // Let the still-blocked perform_link task unwind cleanly.
        drop(server);
        handle.await.expect("perform_link task panicked");
    }

    /// Assemble a 16-byte header for the given version and body size. The
    /// body is supplied separately so adversarial tests can declare a size
    /// that disagrees with the real body length.
    fn header(major: u32, minor: u32, size: u32) -> Vec<u8> {
        let mut buf = Vec::with_capacity(16);
        buf.extend_from_slice(SPICE_MAGIC);
        buf.extend_from_slice(&major.to_le_bytes());
        buf.extend_from_slice(&minor.to_le_bytes());
        buf.extend_from_slice(&size.to_le_bytes());
        buf
    }

    /// Build the 18-byte fixed portion of a link body plus an arbitrary
    /// capability-region tail. `caps_offset` and the counts are written
    /// verbatim so tests can supply hostile values.
    fn body(
        connection_id: u32,
        channel_type: u8,
        channel_id: u8,
        num_common_caps: u32,
        num_channel_caps: u32,
        caps_offset: u32,
        tail: &[u8],
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&connection_id.to_le_bytes());
        buf.push(channel_type);
        buf.push(channel_id);
        buf.extend_from_slice(&num_common_caps.to_le_bytes());
        buf.extend_from_slice(&num_channel_caps.to_le_bytes());
        buf.extend_from_slice(&caps_offset.to_le_bytes());
        buf.extend_from_slice(tail);
        buf
    }

    /// Header + body glued together with `size` set to the body length.
    fn message(body: &[u8]) -> Vec<u8> {
        let mut buf = header(SPICE_VERSION_MAJOR, SPICE_VERSION_MINOR, body.len() as u32);
        buf.extend_from_slice(body);
        buf
    }

    #[test]
    fn round_trip_serialize_parse() {
        let cases = [
            // Main channel, connection id 0, single-word caps.
            SpiceLinkMess::new(0, ChannelType::Main, 0, 0x0000_000b, 0x0000_0001),
            // Display channel with a non-zero connection id.
            SpiceLinkMess::new(
                0xdead_beef,
                ChannelType::Display,
                3,
                0x0000_000b,
                0x1a2b_3c4d,
            ),
            // Multi-word caps: 2 common + 3 channel words.
            SpiceLinkMess {
                connection_id: 42,
                channel_type: ChannelType::Inputs as u8,
                channel_id: 1,
                common_caps: vec![0x1111_1111, 0x2222_2222],
                channel_caps: vec![0xaaaa_aaaa, 0xbbbb_bbbb, 0xcccc_cccc],
            },
        ];

        for original in &cases {
            let bytes = original.serialize();
            let parsed = SpiceLinkMess::parse(&bytes).expect("parse of own serialize");
            assert_eq!(parsed.connection_id, original.connection_id);
            assert_eq!(parsed.channel_type, original.channel_type);
            assert_eq!(parsed.channel_id, original.channel_id);
            assert_eq!(parsed.common_caps, original.common_caps);
            assert_eq!(parsed.channel_caps, original.channel_caps);
        }
    }

    #[test]
    fn byte_exact_layout() {
        // Hand-built message, independent of serialize(), so the two cannot
        // share a latent bug. caps_offset = 18 (words follow fixed fields).
        let tail: Vec<u8> = [0x0000_000bu32, 0x0000_0001, 0x0000_00ff]
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect();
        // num_common = 1, num_channel = 2.
        let b = body(7, ChannelType::Display as u8, 5, 1, 2, 18, &tail);
        let bytes = message(&b);

        let parsed = SpiceLinkMess::parse(&bytes).expect("parse hand-built message");
        assert_eq!(parsed.connection_id, 7);
        assert_eq!(parsed.channel_type, ChannelType::Display as u8);
        assert_eq!(parsed.channel_id, 5);
        assert_eq!(parsed.common_caps, vec![0x0000_000b]);
        assert_eq!(parsed.channel_caps, vec![0x0000_0001, 0x0000_00ff]);
    }

    #[test]
    fn trailing_bytes_after_caps_are_tolerated() {
        // caps_offset 18, one common word, then 8 bytes of padding that the
        // declared size covers but the parser must ignore.
        let mut tail = 0x1234_5678u32.to_le_bytes().to_vec();
        tail.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef, 0x00, 0x11, 0x22, 0x33]);
        let b = body(1, ChannelType::Main as u8, 0, 1, 0, 18, &tail);
        let bytes = message(&b);

        let parsed = SpiceLinkMess::parse(&bytes).expect("parse with padding");
        assert_eq!(parsed.common_caps, vec![0x1234_5678]);
        assert!(parsed.channel_caps.is_empty());
    }

    #[test]
    fn truncation_at_every_boundary_never_ok_never_panics() {
        let original = SpiceLinkMess {
            connection_id: 99,
            channel_type: ChannelType::Display as u8,
            channel_id: 2,
            common_caps: vec![0x0000_000b, 0x0000_0010],
            channel_caps: vec![0x0000_0001],
        };
        let bytes = original.serialize();

        for n in 0..bytes.len() {
            match SpiceLinkMess::parse(&bytes[..n]) {
                Err(LinkError::Truncated { .. }) | Err(LinkError::BadOffset { .. }) => {}
                other => panic!("prefix len {n} should be Truncated/BadOffset, got {other:?}"),
            }
        }
        // The full message still parses.
        SpiceLinkMess::parse(&bytes).expect("full message parses");
    }

    #[test]
    fn bad_magic_rejected() {
        let mut bytes = message(&body(0, ChannelType::Main as u8, 0, 0, 0, 18, &[]));
        bytes[0] = b'X';
        assert_eq!(
            SpiceLinkMess::parse(&bytes).unwrap_err(),
            LinkError::BadMagic { found: *b"XEDQ" }
        );
    }

    #[test]
    fn bad_major_version_rejected() {
        let mut bytes = header(3, 7, 18);
        bytes.extend_from_slice(&body(0, ChannelType::Main as u8, 0, 0, 0, 18, &[]));
        assert_eq!(
            SpiceLinkMess::parse(&bytes).unwrap_err(),
            LinkError::UnsupportedVersion { major: 3, minor: 7 }
        );
    }

    #[test]
    fn size_over_cap_rejected_as_too_large() {
        // Header declares a body of 4097 bytes; rejected before we even try
        // to address a body.
        let bytes = header(SPICE_VERSION_MAJOR, SPICE_VERSION_MINOR, 4097);
        assert_eq!(
            SpiceLinkMess::parse(&bytes).unwrap_err(),
            LinkError::TooLarge {
                what: "link message size",
                value: 4097,
                max: MAX_LINK_MESSAGE_SIZE,
            }
        );
    }

    #[test]
    fn adversarial_caps_offset_yields_bad_offset() {
        // caps_offset far larger than the body, in three flavours: u32::MAX
        // (would overflow a naive add), just past the body end, and a mid
        // sized value still outside the 18-byte body. All must be BadOffset,
        // never a panic.
        for offset in [u32::MAX, 19, 1000] {
            let b = body(0, ChannelType::Main as u8, 0, 0, 0, offset, &[]);
            let bytes = message(&b);
            match SpiceLinkMess::parse(&bytes) {
                Err(LinkError::BadOffset { .. }) => {}
                other => panic!("caps_offset {offset} should be BadOffset, got {other:?}"),
            }
        }
    }

    #[test]
    fn cap_count_flood_rejected_before_allocation() {
        // num_common_caps = u32::MAX with a valid (empty) caps region. The
        // count check fires before any Vec is built.
        let b = body(0, ChannelType::Main as u8, 0, u32::MAX, 0, 18, &[]);
        let bytes = message(&b);
        assert_eq!(
            SpiceLinkMess::parse(&bytes).unwrap_err(),
            LinkError::TooLarge {
                what: "count",
                value: u32::MAX as usize,
                max: MAX_CAP_WORDS,
            }
        );
    }

    #[test]
    fn caps_not_fitting_in_declared_size_is_truncated() {
        // Declares 2 common words but supplies only one word (4 bytes) of
        // capability region inside the declared size.
        let tail = 0x0000_000bu32.to_le_bytes();
        let b = body(0, ChannelType::Main as u8, 0, 2, 0, 18, &tail);
        let bytes = message(&b);
        match SpiceLinkMess::parse(&bytes) {
            Err(LinkError::Truncated { .. }) | Err(LinkError::BadOffset { .. }) => {}
            other => panic!("undersized caps region should be Truncated/BadOffset, got {other:?}"),
        }
    }

    /// Byte-exact match against the Python reference
    /// (`kerbside/linkmessages.py`):
    /// `struct.pack('<4sIIII162sIIIII', b'REDQ', 2, 2, 186, 0, der_pubkey,
    /// 1, 1, 178, common_caps, channel_caps)`. Built by hand here, not via
    /// any helper shared with `serialize`, so the two cannot share a latent
    /// bug.
    #[test]
    fn byte_exact_success() {
        let key = vec![0xABu8; 162];
        let reply = SpiceLinkReply {
            error: SpiceError::Ok,
            pub_key: key.clone(),
            common_caps: vec![11],
            channel_caps: vec![9],
        };

        let mut expected = Vec::new();
        expected.extend_from_slice(SPICE_MAGIC);
        expected.extend_from_slice(&2u32.to_le_bytes()); // major
        expected.extend_from_slice(&2u32.to_le_bytes()); // minor
        expected.extend_from_slice(&186u32.to_le_bytes()); // size
        expected.extend_from_slice(&0u32.to_le_bytes()); // error = Ok
        expected.extend_from_slice(&key); // pub_key
        expected.extend_from_slice(&1u32.to_le_bytes()); // num_common_caps
        expected.extend_from_slice(&1u32.to_le_bytes()); // num_channel_caps
        expected.extend_from_slice(&178u32.to_le_bytes()); // caps_offset
        expected.extend_from_slice(&11u32.to_le_bytes()); // common cap word
        expected.extend_from_slice(&9u32.to_le_bytes()); // channel cap word

        assert_eq!(reply.serialize().unwrap(), expected);
    }

    /// Byte-exact match against the Python reference need_secured error
    /// redirect: `struct.pack('<4sIIII162sIII', b'REDQ', 2, 2, 178, 5,
    /// b'', 0, 0, 0)` (the `b''` fills the 162-byte key field with zeros).
    #[test]
    fn byte_exact_need_secured() {
        let reply = SpiceLinkReply::error_reply(SpiceError::NeedSecured);

        let mut expected = Vec::new();
        expected.extend_from_slice(SPICE_MAGIC);
        expected.extend_from_slice(&2u32.to_le_bytes()); // major
        expected.extend_from_slice(&2u32.to_le_bytes()); // minor
        expected.extend_from_slice(&178u32.to_le_bytes()); // size
        expected.extend_from_slice(&5u32.to_le_bytes()); // error = NeedSecured
        expected.extend_from_slice(&[0u8; 162]); // zeroed pub_key
        expected.extend_from_slice(&0u32.to_le_bytes()); // num_common_caps
        expected.extend_from_slice(&0u32.to_le_bytes()); // num_channel_caps
        expected.extend_from_slice(&0u32.to_le_bytes()); // caps_offset

        assert_eq!(reply.serialize().unwrap(), expected);
    }

    #[test]
    fn round_trip_serialize_parse_link_reply() {
        let reply = SpiceLinkReply {
            error: SpiceError::Ok,
            pub_key: vec![0x5Au8; 162],
            common_caps: vec![11, 2],
            channel_caps: vec![9],
        };

        let bytes = reply.serialize().expect("serialize");
        let parsed = SpiceLinkReply::parse(&bytes).expect("parse of own serialize");

        assert_eq!(parsed.error, reply.error);
        assert_eq!(parsed.pub_key, reply.pub_key);
        assert_eq!(parsed.common_caps, reply.common_caps);
        assert_eq!(parsed.channel_caps, reply.channel_caps);
    }

    /// Regression test for the unbounded-allocation OOM the fuzz target
    /// found: a reply header declaring a huge capability count must be
    /// rejected without attempting to allocate that many words.
    #[test]
    fn link_reply_cap_flood_rejected_without_allocation() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(SPICE_MAGIC);
        bytes.extend_from_slice(&SPICE_VERSION_MAJOR.to_le_bytes());
        bytes.extend_from_slice(&SPICE_VERSION_MINOR.to_le_bytes());
        bytes.extend_from_slice(&178u32.to_le_bytes()); // size: fixed body prefix
        bytes.extend_from_slice(&0u32.to_le_bytes()); // error = Ok
        bytes.extend_from_slice(&[0u8; 162]); // pub_key
        bytes.extend_from_slice(&u32::MAX.to_le_bytes()); // num_common_caps (hostile)
        bytes.extend_from_slice(&0u32.to_le_bytes()); // num_channel_caps
        bytes.extend_from_slice(&178u32.to_le_bytes()); // caps_offset

        // Must return an error (TooLarge, surfaced through anyhow), not OOM
        // and not panic.
        assert!(SpiceLinkReply::parse(&bytes).is_err());
    }

    #[test]
    fn bad_key_length_returns_typed_error_not_panic() {
        for len in [161, 200] {
            let reply = SpiceLinkReply {
                error: SpiceError::Ok,
                pub_key: vec![0u8; len],
                common_caps: Vec::new(),
                channel_caps: Vec::new(),
            };
            assert_eq!(reply.serialize(), Err(LinkError::BadKeyLength { len }));
        }
    }

    /// Round-trip a password through the client's `encrypt_password` and
    /// the server's `decrypt_password` against a freshly generated
    /// keypair, for a given plaintext. This is the primary compatibility
    /// guarantee between the two sides of the ticket exchange.
    fn round_trip(password: &str) {
        let (private_key, der) = generate_ticket_keypair().expect("keypair generation");
        let blob = encrypt_password(&der, password).expect("encrypt");
        let blob: [u8; 128] = blob.try_into().expect("128-byte blob");
        let decrypted = decrypt_password(&private_key, &blob).expect("decrypt");
        assert_eq!(decrypted, password);
    }

    #[test]
    fn round_trip_empty() {
        round_trip("");
    }

    #[test]
    fn round_trip_short() {
        round_trip("hunter2");
    }

    #[test]
    fn round_trip_max() {
        // SPICE max password length is 60 bytes including the NUL
        // terminator, so 59 usable characters.
        let password = "a".repeat(59);
        round_trip(&password);
    }

    #[test]
    fn der_key_is_162_bytes() {
        let (_private_key, der) = generate_ticket_keypair().expect("keypair generation");
        assert_eq!(der.len(), 162);
    }

    #[test]
    fn decrypt_garbage_is_error() {
        let (private_key, _der) = generate_ticket_keypair().expect("keypair generation");
        let garbage = [0xFFu8; 128];
        assert_eq!(
            decrypt_password(&private_key, &garbage),
            Err(LinkError::DecryptFailed)
        );
    }

    /// The new server-role drivers and the existing client drivers must
    /// interoperate over a real bidirectional stream. Drive the client on a
    /// spawned task and the server inline over a `tokio::io::duplex` pair: the
    /// client `perform_link`/`perform_auth`s, the server
    /// `read_link_mess`/`send_link_reply`/`read_auth_ticket`/`send_auth_result`s,
    /// and both sides must agree on the recovered password.
    #[tokio::test]
    async fn end_to_end_handshake_succeeds() {
        let (mut client_end, mut server_end) = tokio::io::duplex(8192);

        // Client: existing drivers, generic over the duplex half.
        let client = tokio::spawn(async move {
            let reply = perform_link(&mut client_end, 0, ChannelType::Main, 0)
                .await
                .expect("client perform_link");
            perform_auth(&mut client_end, &reply.pub_key, Some("s3cret"))
                .await
                .expect("client perform_auth");
        });

        // Server: new drivers, inline.
        let (private_key, der) = generate_ticket_keypair().expect("keypair generation");
        let _link_mess = read_link_mess(&mut server_end)
            .await
            .expect("read_link_mess");

        // Mirror the Python proxy's success reply: common_caps=[11],
        // channel_caps=[9].
        let reply = SpiceLinkReply {
            error: SpiceError::Ok,
            pub_key: der,
            common_caps: vec![11],
            channel_caps: vec![9],
        };
        send_link_reply(&mut server_end, &reply)
            .await
            .expect("send_link_reply");

        let password = read_auth_ticket(&mut server_end, &private_key)
            .await
            .expect("read_auth_ticket");
        assert_eq!(password, "s3cret", "server recovered the client's password");

        send_auth_result(&mut server_end, SpiceError::Ok)
            .await
            .expect("send_auth_result");

        client.await.expect("client task panicked");
    }

    /// A `need_secured` redirect sent by the server surfaces to the client's
    /// `perform_link`. `perform_link` returns `Ok(reply)` with
    /// `reply.error == SpiceError::NeedSecured` (it does not early-return an
    /// `Err`): the error redirect is a well-formed link reply with a zeroed
    /// key and no capabilities, which `SpiceLinkReply::parse` accepts, so the
    /// caller inspects `reply.error` to see the verdict.
    #[tokio::test]
    async fn need_secured_redirect_surfaces_to_client() {
        let (mut client_end, mut server_end) = tokio::io::duplex(8192);

        let client = tokio::spawn(async move {
            perform_link(&mut client_end, 0, ChannelType::Main, 0)
                .await
                .expect("client perform_link")
        });

        // Server: read the link message, then redirect to TLS. No auth phase.
        let _link_mess = read_link_mess(&mut server_end)
            .await
            .expect("read_link_mess");
        send_need_secured(&mut server_end)
            .await
            .expect("send_need_secured");

        let reply = client.await.expect("client task panicked");
        assert_eq!(
            reply.error,
            SpiceError::NeedSecured,
            "perform_link returns Ok(reply) with the NeedSecured error field set"
        );
    }

    /// The server rejects a client that selects an auth mechanism other than
    /// SPICE. The client writes mechanism `2` (not [`AUTH_MECHANISM_SPICE`])
    /// followed by 128 zero bytes; the server's `read_auth_ticket` must error.
    #[tokio::test]
    async fn wrong_auth_mechanism_rejected() {
        let (mut client_end, mut server_end) = tokio::io::duplex(8192);

        // Client: real link, then a hand-written bad auth selector + blob.
        let client = tokio::spawn(async move {
            let reply = perform_link(&mut client_end, 0, ChannelType::Main, 0)
                .await
                .expect("client perform_link");
            // Mechanism 2 is not AUTH_MECHANISM_SPICE (1).
            client_end
                .write_all(&2u32.to_le_bytes())
                .await
                .expect("write bad mechanism");
            client_end
                .write_all(&[0u8; 128])
                .await
                .expect("write ticket blob");
            client_end.flush().await.expect("flush");
            reply
        });

        // Server: successful link reply, then attempt to read the ticket.
        let (private_key, der) = generate_ticket_keypair().expect("keypair generation");
        let _link_mess = read_link_mess(&mut server_end)
            .await
            .expect("read_link_mess");
        let reply = SpiceLinkReply {
            error: SpiceError::Ok,
            pub_key: der,
            common_caps: vec![11],
            channel_caps: vec![9],
        };
        send_link_reply(&mut server_end, &reply)
            .await
            .expect("send_link_reply");

        let result = read_auth_ticket(&mut server_end, &private_key).await;
        assert!(
            result.is_err(),
            "read_auth_ticket must reject a non-SPICE auth mechanism"
        );

        client.await.expect("client task panicked");
    }

    /// `read_link_mess` must reject an oversized declared `size` from the
    /// 16-byte header *before* trying to read (and allocate) the body — the
    /// DoS guard for a server accepting untrusted connections. We write only
    /// a header declaring a body far larger than `MAX_LINK_MESSAGE_SIZE` and
    /// never send a body; the call must return an error rather than block
    /// waiting for gigabytes that will never arrive.
    #[tokio::test]
    async fn read_link_mess_rejects_oversized_size_before_reading_body() {
        let (mut client_end, mut server_end) = tokio::io::duplex(64);

        // A well-formed header (magic, version) but a hostile size field.
        let mut header = Vec::new();
        header.extend_from_slice(SPICE_MAGIC);
        header.extend_from_slice(&SPICE_VERSION_MAJOR.to_le_bytes());
        header.extend_from_slice(&SPICE_VERSION_MINOR.to_le_bytes());
        header.extend_from_slice(&u32::MAX.to_le_bytes()); // declared body size
        client_end.write_all(&header).await.expect("write header");
        client_end.flush().await.expect("flush");
        // Deliberately send no body and drop the client so any attempt to read
        // the body would see EOF rather than hang.
        drop(client_end);

        let result = read_link_mess(&mut server_end).await;
        assert!(
            result.is_err(),
            "read_link_mess must reject an oversized declared size"
        );
    }

    /// `perform_link_with_caps` sends the caller's capability words
    /// verbatim, multi-word sets included, and hands back the server's
    /// reply with its caps intact; auth then completes over the same
    /// stream.
    #[tokio::test]
    async fn perform_link_with_caps_round_trips_multi_word_caps() {
        let (mut client_end, mut server_end) = tokio::io::duplex(8192);

        let common = vec![capabilities::DEFAULT_COMMON, 0x8000_0001];
        let channel = vec![0xdead_beef, 0, 0x0000_0001];
        let (sent_common, sent_channel) = (common.clone(), channel.clone());
        let client = tokio::spawn(async move {
            let reply = perform_link_with_caps(
                &mut client_end,
                7,
                ChannelType::Display,
                2,
                &sent_common,
                &sent_channel,
            )
            .await
            .expect("client perform_link_with_caps");
            reply
                .check_client_requirements()
                .expect("reply grants the required caps");
            perform_auth(&mut client_end, &reply.pub_key, Some("pw"))
                .await
                .expect("client perform_auth");
            reply
        });

        let (private_key, der) = generate_ticket_keypair().expect("keypair generation");
        let link_mess = read_link_mess(&mut server_end)
            .await
            .expect("read_link_mess");
        assert_eq!(link_mess.connection_id, 7);
        assert_eq!(link_mess.channel_type, ChannelType::Display as u8);
        assert_eq!(link_mess.channel_id, 2);
        assert_eq!(
            link_mess.common_caps, common,
            "server sees every common word"
        );
        assert_eq!(
            link_mess.channel_caps, channel,
            "server sees every channel word"
        );

        let reply = SpiceLinkReply {
            error: SpiceError::Ok,
            pub_key: der.clone(),
            common_caps: vec![capabilities::DEFAULT_COMMON, 0x0000_0004],
            channel_caps: vec![9, 0x0000_0002],
        };
        send_link_reply(&mut server_end, &reply)
            .await
            .expect("send_link_reply");
        let password = read_auth_ticket(&mut server_end, &private_key)
            .await
            .expect("read_auth_ticket");
        assert_eq!(password, "pw");
        send_auth_result(&mut server_end, SpiceError::Ok)
            .await
            .expect("send_auth_result");

        let got = client.await.expect("client task panicked");
        assert_eq!(got.error, SpiceError::Ok);
        assert_eq!(got.pub_key, der);
        assert_eq!(
            got.common_caps,
            vec![capabilities::DEFAULT_COMMON, 0x0000_0004]
        );
        assert_eq!(got.channel_caps, vec![9, 0x0000_0002]);
    }

    /// Advertising common caps without AUTH_SELECTION or MINI_HEADER, or
    /// more capability words than this crate's own parsers accept, is
    /// refused with a typed error before anything reaches the wire: a
    /// server would then expect a protocol this crate does not speak, or a
    /// peer built on this crate would reject the message.
    #[tokio::test]
    async fn perform_link_with_caps_refuses_unsendable_caps() {
        let too_many = vec![capabilities::DEFAULT_COMMON; MAX_CAP_WORDS + 1];
        let missing = |missing: Vec<&'static str>, caps: &[u32]| LinkError::MissingRequiredCaps {
            set: "advertised",
            missing,
            caps: caps.to_vec(),
        };
        let cases: Vec<(Vec<u32>, Vec<u32>, LinkError)> = vec![
            (
                vec![],
                vec![],
                missing(vec!["AUTH_SELECTION", "MINI_HEADER"], &[]),
            ),
            (
                vec![capabilities::AUTH_SPICE],
                vec![],
                missing(
                    vec!["AUTH_SELECTION", "MINI_HEADER"],
                    &[capabilities::AUTH_SPICE],
                ),
            ),
            (
                vec![capabilities::AUTH_SELECTION | capabilities::AUTH_SPICE],
                vec![],
                missing(
                    vec!["MINI_HEADER"],
                    &[capabilities::AUTH_SELECTION | capabilities::AUTH_SPICE],
                ),
            ),
            (
                vec![capabilities::AUTH_SPICE | capabilities::MINI_HEADER],
                vec![],
                missing(
                    vec!["AUTH_SELECTION"],
                    &[capabilities::AUTH_SPICE | capabilities::MINI_HEADER],
                ),
            ),
            (
                too_many.clone(),
                vec![],
                LinkError::TooLarge {
                    what: "advertised common capability words",
                    value: MAX_CAP_WORDS + 1,
                    max: MAX_CAP_WORDS,
                },
            ),
            (
                vec![capabilities::DEFAULT_COMMON],
                too_many,
                LinkError::TooLarge {
                    what: "advertised channel capability words",
                    value: MAX_CAP_WORDS + 1,
                    max: MAX_CAP_WORDS,
                },
            ),
        ];
        for (common, channel, expected) in cases {
            let (mut client_end, mut server_end) = tokio::io::duplex(8192);
            // Bounded, so a missing guard fails here rather than hanging
            // on a reply the silent peer never sends.
            let err = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                perform_link_with_caps(&mut client_end, 0, ChannelType::Main, 0, &common, &channel),
            )
            .await
            .expect("refusal must be immediate, not wait on the peer")
            .expect_err("unsendable caps must be refused");
            assert_eq!(
                err.downcast_ref::<LinkError>(),
                Some(&expected),
                "for {common:?} / {channel:?}"
            );

            // Nothing was written: the peer sees EOF, not a link message.
            drop(client_end);
            let mut buf = Vec::new();
            server_end.read_to_end(&mut buf).await.unwrap();
            assert!(buf.is_empty(), "no bytes may be sent for {common:?}");
        }

        // Exactly MAX_CAP_WORDS words in each set is sendable, and a
        // parser from this crate accepts the result.
        let (mut client_end, mut server_end) = tokio::io::duplex(8192);
        let full = vec![capabilities::DEFAULT_COMMON; MAX_CAP_WORDS];
        let sent = full.clone();
        let client = tokio::spawn(async move {
            perform_link_with_caps(&mut client_end, 0, ChannelType::Main, 0, &sent, &sent).await
        });
        let link_mess = read_link_mess(&mut server_end)
            .await
            .expect("a link message at the word limit parses");
        assert_eq!(link_mess.common_caps, full);
        assert_eq!(link_mess.channel_caps, full);
        drop(server_end);
        let _ = client.await.expect("client task panicked");
    }

    /// A reply that does not list AUTH_SELECTION, AUTH_SPICE or MINI_HEADER
    /// parses fine, but `check_client_requirements` fails with a typed
    /// error naming what is missing, which is how the connect path stops
    /// before auth. The SASL case is what a spice-server with SASL enabled
    /// sends: it offers AUTH_SASL in place of AUTH_SPICE.
    #[tokio::test]
    async fn reply_missing_required_caps_fails_check() {
        let cases: [(Vec<u32>, Vec<&'static str>); 4] = [
            (vec![], vec!["AUTH_SELECTION", "AUTH_SPICE", "MINI_HEADER"]),
            (
                vec![capabilities::AUTH_SELECTION | capabilities::AUTH_SPICE],
                vec!["MINI_HEADER"],
            ),
            (
                vec![
                    capabilities::AUTH_SPICE | capabilities::MINI_HEADER,
                    0xffff_ffff,
                ],
                vec!["AUTH_SELECTION"],
            ),
            (
                vec![
                    capabilities::AUTH_SELECTION
                        | capabilities::AUTH_SASL
                        | capabilities::MINI_HEADER,
                ],
                vec!["AUTH_SPICE"],
            ),
        ];
        for (granted, missing) in cases {
            let (mut client_end, mut server_end) = tokio::io::duplex(8192);
            let client = tokio::spawn(async move {
                perform_link_with_caps(
                    &mut client_end,
                    0,
                    ChannelType::Main,
                    0,
                    &[capabilities::DEFAULT_COMMON],
                    &[capabilities::DEFAULT_MAIN],
                )
                .await
                .expect("a well-formed reply parses")
            });

            let (_key, der) = generate_ticket_keypair().expect("keypair generation");
            read_link_mess(&mut server_end)
                .await
                .expect("read_link_mess");
            let reply = SpiceLinkReply {
                error: SpiceError::Ok,
                pub_key: der,
                common_caps: granted.clone(),
                channel_caps: vec![],
            };
            send_link_reply(&mut server_end, &reply)
                .await
                .expect("send_link_reply");

            let got = client.await.expect("client task panicked");
            assert_eq!(got.common_caps, granted);
            let err = got
                .check_client_requirements()
                .expect_err("missing caps must fail the check");
            let msg = err.to_string();
            for name in &missing {
                assert!(msg.contains(name), "error {msg:?} should name {name}");
            }
            assert_eq!(
                err,
                LinkError::MissingRequiredCaps {
                    set: "server",
                    missing,
                    caps: granted,
                }
            );
        }
    }

    /// The client refuses a link reply declaring an oversized body before
    /// allocating for it, rather than waiting for bytes that never come.
    #[tokio::test]
    async fn perform_link_rejects_oversized_reply_size() {
        let (mut client_end, mut server_end) = tokio::io::duplex(8192);
        let client =
            tokio::spawn(
                async move { perform_link(&mut client_end, 0, ChannelType::Main, 0).await },
            );

        read_link_mess(&mut server_end)
            .await
            .expect("read_link_mess");
        server_end
            .write_all(&header(SPICE_VERSION_MAJOR, SPICE_VERSION_MINOR, u32::MAX))
            .await
            .expect("write reply header");
        server_end.flush().await.expect("flush");

        let result = client.await.expect("client task panicked");
        assert!(result.is_err(), "an oversized reply size must be rejected");
    }

    /// `default_channel_caps` is public API: pin its whole mapping.
    #[test]
    fn default_channel_caps_by_channel_type() {
        assert_eq!(
            default_channel_caps(ChannelType::Display),
            capabilities::DEFAULT_DISPLAY
        );
        assert_eq!(
            default_channel_caps(ChannelType::Usbredir),
            capabilities::DEFAULT_SPICEVMC
        );
        for other in [ChannelType::Main, ChannelType::Inputs, ChannelType::Cursor] {
            assert_eq!(default_channel_caps(other), capabilities::DEFAULT_MAIN);
        }
    }
}
