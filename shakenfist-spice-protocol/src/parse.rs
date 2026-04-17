//! Byte-parsing helpers for reading little-endian fields
//! from raw protocol payloads.

/// Read a little-endian `u16` from `data` at `offset`.
#[inline]
pub fn read_u16_le(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

/// Read a little-endian `u32` from `data` at `offset`.
#[inline]
pub fn read_u32_le(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

/// Read a little-endian `i32` from `data` at `offset`.
#[inline]
pub fn read_i32_le(data: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

/// Read a little-endian `u64` from `data` at `offset`.
#[inline]
pub fn read_u64_le(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_u16_le() {
        let data = [0x34, 0x12];
        assert_eq!(read_u16_le(&data, 0), 0x1234);
    }

    #[test]
    fn test_read_u32_le() {
        let data = [0x78, 0x56, 0x34, 0x12];
        assert_eq!(read_u32_le(&data, 0), 0x12345678);
    }

    #[test]
    fn test_read_i32_le() {
        let data = [0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(read_i32_le(&data, 0), -1);
    }

    #[test]
    fn test_read_u64_le() {
        let data = [0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(read_u64_le(&data, 0), 1);
    }

    #[test]
    fn test_read_at_offset() {
        let data = [0x00, 0x00, 0x34, 0x12];
        assert_eq!(read_u16_le(&data, 2), 0x1234);
    }
}
