/// SPICE link protocol - handshake and authentication
use anyhow::{anyhow, Result};
use byteorder::{BigEndian, LittleEndian, ReadBytesExt, WriteBytesExt};
use rand::rngs::OsRng;
use rsa::pkcs8::DecodePublicKey;
use rsa::{Oaep, RsaPublicKey};
use sha1::Sha1;
use std::io::{Cursor, Read};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

use super::constants::*;

/// Either a plain TCP stream or TLS-wrapped stream
#[allow(clippy::large_enum_variant)]
pub enum SpiceStream {
    Plain(TcpStream),
    Tls(TlsStream<TcpStream>),
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
        }
        Ok(())
    }

    pub async fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        match self {
            SpiceStream::Plain(s) => s.write_all(buf).await?,
            SpiceStream::Tls(s) => s.write_all(buf).await?,
        }
        Ok(())
    }

    pub async fn flush(&mut self) -> Result<()> {
        match self {
            SpiceStream::Plain(s) => s.flush().await?,
            SpiceStream::Tls(s) => s.flush().await?,
        }
        Ok(())
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

    let encrypted = pub_key.encrypt(&mut rng, padding, password.as_bytes())?;

    // Pad to 128 bytes (RSA block size)
    let mut result = vec![0u8; 128];
    let start = 128 - encrypted.len();
    result[start..].copy_from_slice(&encrypted);

    Ok(result)
}

/// Perform the link handshake
pub async fn perform_link(
    stream: &mut SpiceStream,
    connection_id: u32,
    channel_type: ChannelType,
    channel_id: u8,
) -> Result<SpiceLinkReply> {
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
pub async fn perform_auth(
    stream: &mut SpiceStream,
    pub_key: &[u8],
    password: Option<&str>,
) -> Result<()> {
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
