/// SPICE link protocol - handshake and authentication
use anyhow::{anyhow, Result};
use byteorder::{BigEndian, LittleEndian, ReadBytesExt, WriteBytesExt};
use rand::rngs::OsRng;
use rsa::pkcs8::DecodePublicKey;
use rsa::{Oaep, RsaPublicKey};
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
        WriteBytesExt::write_u32::<LittleEndian>(&mut buf, SPICE_VERSION_MAJOR).unwrap();
        WriteBytesExt::write_u32::<LittleEndian>(&mut buf, SPICE_VERSION_MINOR).unwrap();

        // Size of following data
        WriteBytesExt::write_u32::<LittleEndian>(&mut buf, size as u32).unwrap();

        // Connection ID
        WriteBytesExt::write_u32::<LittleEndian>(&mut buf, self.connection_id).unwrap();

        // Channel type and ID
        WriteBytesExt::write_u8(&mut buf, self.channel_type).unwrap();
        WriteBytesExt::write_u8(&mut buf, self.channel_id).unwrap();

        // Number of capabilities
        WriteBytesExt::write_u32::<LittleEndian>(&mut buf, num_common_caps).unwrap();
        WriteBytesExt::write_u32::<LittleEndian>(&mut buf, num_channel_caps).unwrap();

        // Capabilities offset
        WriteBytesExt::write_u32::<LittleEndian>(&mut buf, caps_offset).unwrap();

        // Common capabilities
        for cap in &self.common_caps {
            WriteBytesExt::write_u32::<LittleEndian>(&mut buf, *cap).unwrap();
        }

        // Channel capabilities
        for cap in &self.channel_caps {
            WriteBytesExt::write_u32::<LittleEndian>(&mut buf, *cap).unwrap();
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
    #[allow(dead_code)]
    pub common_caps: Vec<u32>,
    #[allow(dead_code)]
    pub channel_caps: Vec<u32>,
}

impl SpiceLinkReply {
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 16 {
            return Err(anyhow!("Link reply too short"));
        }

        let mut cursor = Cursor::new(data);

        // Verify magic
        let mut magic = [0u8; 4];
        Read::read_exact(&mut cursor, &mut magic)?;
        if &magic != SPICE_MAGIC {
            return Err(anyhow!("Invalid magic in link reply"));
        }

        // Version
        let major = ReadBytesExt::read_u32::<LittleEndian>(&mut cursor)?;
        let minor = ReadBytesExt::read_u32::<LittleEndian>(&mut cursor)?;

        if major != SPICE_VERSION_MAJOR {
            return Err(anyhow!(
                "Version mismatch: expected {}.x, got {}.{}",
                SPICE_VERSION_MAJOR,
                major,
                minor
            ));
        }

        // Size
        let size = ReadBytesExt::read_u32::<LittleEndian>(&mut cursor)? as usize;

        if data.len() < 16 + size {
            return Err(anyhow!("Link reply truncated"));
        }

        // Error code
        let error_code = ReadBytesExt::read_u32::<LittleEndian>(&mut cursor)?;
        let error = SpiceError::from_u32(error_code);

        // RSA public key (162 bytes - DER format)
        let mut pub_key = vec![0u8; 162];
        Read::read_exact(&mut cursor, &mut pub_key)?;

        // Number of capabilities
        let num_common_caps = ReadBytesExt::read_u32::<LittleEndian>(&mut cursor)? as usize;
        let num_channel_caps = ReadBytesExt::read_u32::<LittleEndian>(&mut cursor)? as usize;

        // Caps offset (skip)
        let _caps_offset = ReadBytesExt::read_u32::<LittleEndian>(&mut cursor)?;

        // Read capabilities
        let mut common_caps = Vec::with_capacity(num_common_caps);
        for _ in 0..num_common_caps {
            common_caps.push(ReadBytesExt::read_u32::<LittleEndian>(&mut cursor)?);
        }

        let mut channel_caps = Vec::with_capacity(num_channel_caps);
        for _ in 0..num_channel_caps {
            channel_caps.push(ReadBytesExt::read_u32::<LittleEndian>(&mut cursor)?);
        }

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
        WriteBytesExt::write_u32::<LittleEndian>(&mut buf, SPICE_VERSION_MAJOR).unwrap();
        WriteBytesExt::write_u32::<LittleEndian>(&mut buf, SPICE_VERSION_MINOR).unwrap();
        WriteBytesExt::write_u32::<LittleEndian>(&mut buf, size as u32).unwrap();

        // Body.
        WriteBytesExt::write_u32::<LittleEndian>(&mut buf, self.error.to_u32()).unwrap();
        buf.extend_from_slice(&self.pub_key);
        WriteBytesExt::write_u32::<LittleEndian>(&mut buf, num_common_caps).unwrap();
        WriteBytesExt::write_u32::<LittleEndian>(&mut buf, num_channel_caps).unwrap();
        WriteBytesExt::write_u32::<LittleEndian>(&mut buf, caps_offset).unwrap();

        for cap in &self.common_caps {
            WriteBytesExt::write_u32::<LittleEndian>(&mut buf, *cap).unwrap();
        }
        for cap in &self.channel_caps {
            WriteBytesExt::write_u32::<LittleEndian>(&mut buf, *cap).unwrap();
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

/// Perform the link handshake
pub async fn perform_link<S>(
    stream: &mut S,
    connection_id: u32,
    channel_type: ChannelType,
    channel_id: u8,
) -> Result<SpiceLinkReply>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    // Per-channel capabilities.  The display caps are critical:
    // without COMPOSITE the guest QXL driver renders via a
    // software fallback that produces far fewer display updates.
    let channel_caps = match channel_type {
        ChannelType::Display => capabilities::DEFAULT_DISPLAY,
        ChannelType::Usbredir => capabilities::DEFAULT_SPICEVMC,
        _ => capabilities::DEFAULT_MAIN,
    };

    // Send link message
    let link_mess = SpiceLinkMess::new(
        connection_id,
        channel_type,
        channel_id,
        capabilities::DEFAULT_COMMON,
        channel_caps,
    );

    let data = link_mess.serialize();
    stream.write_all(&data).await?;
    stream.flush().await?;

    // Read reply header (16 bytes: magic + version + size)
    let mut header = [0u8; 16];
    stream.read_exact(&mut header).await?;

    // Parse size from header
    let size = {
        let mut cursor = Cursor::new(&header[12..16]);
        ReadBytesExt::read_u32::<LittleEndian>(&mut cursor)? as usize
    };

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
    let mut auth_select = Vec::new();
    WriteBytesExt::write_u32::<LittleEndian>(&mut auth_select, AUTH_MECHANISM_SPICE).unwrap();
    stream.write_all(&auth_select).await?;

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
        let size = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
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
            LinkError::BadMagic {
                found: [b'X', b'E', b'D', b'Q'],
            }
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
}
