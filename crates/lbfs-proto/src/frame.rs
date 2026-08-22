pub const HEADER_LEN: usize = 24;
pub const MAGIC: [u8; 4] = *b"LBFS";
/// Version 2 added `WriteRequest.kill_suidgid`. postcard ignores trailing
/// bytes instead of refusing them, so a version-1 peer would decode a
/// version-2 `WRITE` body cleanly and drop the flag — losing a set-user-ID
/// strip in silence. The exact-match handshake is what turns that into a
/// visible startup failure.
pub const PROTOCOL_VERSION: u32 = 2;
pub const DEFAULT_PORT: u16 = 9423;
pub const DEFAULT_MAX_INFLIGHT: u32 = 128;
pub const WINDOW_CLAMP: (u32, u32) = (8, 1024);
pub const DEFAULT_MAX_IO_SIZE: u32 = 1 << 20;
pub const MAX_BODY_SIZE: u32 = 64 << 10;

pub const FLAG_NO_REPLY: u16 = 1 << 0;
/// Reserved for the forced-sync fast-follow (spec §11). Never set in product v1.
pub const FLAG_FORCE_SYNC_RESERVED: u16 = 1 << 1;

pub const STATUS_OK: u16 = 0;
pub const STATUS_VERSION_MISMATCH: u16 = 0xFF01;
pub const STATUS_ATTACH_DENIED: u16 = 0xFF02;
pub const STATUS_NOT_EXPORTED: u16 = 0xFF03;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub request_id: u64,
    pub op_or_status: u16,
    pub flags: u16,
    pub body_len: u32,
    pub data_len: u32,
}

impl FrameHeader {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..8].copy_from_slice(&self.request_id.to_le_bytes());
        b[8..10].copy_from_slice(&self.op_or_status.to_le_bytes());
        b[10..12].copy_from_slice(&self.flags.to_le_bytes());
        b[12..16].copy_from_slice(&self.body_len.to_le_bytes());
        b[16..20].copy_from_slice(&self.data_len.to_le_bytes());
        // b[20..24] reserved, zero
        b
    }

    pub fn decode(b: &[u8; HEADER_LEN]) -> Self {
        Self {
            request_id: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            op_or_status: u16::from_le_bytes(b[8..10].try_into().unwrap()),
            flags: u16::from_le_bytes(b[10..12].try_into().unwrap()),
            body_len: u32::from_le_bytes(b[12..16].try_into().unwrap()),
            data_len: u32::from_le_bytes(b[16..20].try_into().unwrap()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn header_is_24_bytes_and_round_trips() {
        let h = FrameHeader {
            request_id: 0x0102_0304_0506_0708,
            op_or_status: 16,
            flags: FLAG_NO_REPLY,
            body_len: 7,
            data_len: 131072,
        };
        let bytes = h.encode();
        assert_eq!(bytes.len(), HEADER_LEN);
        assert_eq!(FrameHeader::decode(&bytes), h);
    }

    #[test]
    fn header_layout_is_little_endian_per_spec() {
        let h = FrameHeader {
            request_id: 1,
            op_or_status: 2,
            flags: 3,
            body_len: 4,
            data_len: 5,
        };
        let b = h.encode();
        assert_eq!(&b[0..8], &1u64.to_le_bytes());
        assert_eq!(&b[8..10], &2u16.to_le_bytes());
        assert_eq!(&b[10..12], &3u16.to_le_bytes());
        assert_eq!(&b[12..16], &4u32.to_le_bytes());
        assert_eq!(&b[16..20], &5u32.to_le_bytes());
        assert_eq!(&b[20..24], &0u32.to_le_bytes()); // reserved
    }

    proptest! {
        #[test]
        fn decode_any_24_bytes_never_panics(bytes in prop::array::uniform24(any::<u8>())) {
            let _ = FrameHeader::decode(&bytes);
        }
    }
}
