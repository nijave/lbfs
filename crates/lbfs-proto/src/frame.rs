pub const HEADER_LEN: usize = 24;
pub const MAGIC: [u8; 4] = *b"LBFS";
/// Version 3 adds session resumption: `HelloRequest.resume`,
/// `HelloReply.resume_grace_ms`, a ticket on the `ATTACH` reply, and the
/// `RESUME`/`DETACH` opcodes. The match stays exact for the reason version 2
/// made it exact — postcard ignores trailing bytes rather than refusing them,
/// so a version-2 server would decode a version-3 `HELLO` cleanly, drop the
/// resume request, and hand back a mount the client wrongly believes it can
/// re-attach to.
pub const PROTOCOL_VERSION: u32 = 3;
pub const DEFAULT_PORT: u16 = 9423;
pub const DEFAULT_MAX_INFLIGHT: u32 = 128;
pub const WINDOW_CLAMP: (u32, u32) = (8, 1024);
pub const DEFAULT_MAX_IO_SIZE: u32 = 1 << 20;
pub const MAX_BODY_SIZE: u32 = 64 << 10;

pub const FLAG_NO_REPLY: u16 = 1 << 0;
/// The forced-sync control, in both directions (spec §3.1, §6).
///
/// On a `FSYNC` or `FSYNCDIR` **request** it overrides the server's durability
/// policy: the sync that opcode names runs for real even under
/// `fsync = "ignore"`. On any other opcode it means nothing and the server
/// leaves it alone — unknown flag bits have never been fatal, which is what let
/// this bit go live without a protocol version.
///
/// On a **reply** it is the server's acknowledgement that it performed the
/// forced sync. A server built before the control existed answers `flags = 0`
/// while still reporting `STATUS_OK`, so this bit is the only way a client can
/// separate a sync that ran from one that was silently skipped.
pub const FLAG_FORCE_SYNC: u16 = 1 << 1;

pub const STATUS_OK: u16 = 0;
pub const STATUS_VERSION_MISMATCH: u16 = 0xFF01;
pub const STATUS_ATTACH_DENIED: u16 = 0xFF02;
pub const STATUS_NOT_EXPORTED: u16 = 0xFF03;
pub const STATUS_NO_SESSION: u16 = 0xFF04;
pub const STATUS_SESSION_BUSY: u16 = 0xFF05;
pub const STATUS_SESSION_MISMATCH: u16 = 0xFF06;

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

    /// The two live flags own one bit each, and never the same one.
    ///
    /// Worth pinning rather than reading off the shifts: bit 1 carries two
    /// meanings now — "force this sync" outbound and "I forced it" inbound — and
    /// a third flag that landed on top of either bit would be a wire bug no
    /// decoder could report, since a frame with a stray flag stays perfectly
    /// well formed.
    #[test]
    fn the_two_frame_flags_are_distinct_single_bits() {
        assert_eq!(FLAG_NO_REPLY, 0b01);
        assert_eq!(FLAG_FORCE_SYNC, 0b10);
        assert_eq!(FLAG_NO_REPLY & FLAG_FORCE_SYNC, 0);
        // Both survive the header round trip, together and apart.
        for flags in [
            0,
            FLAG_NO_REPLY,
            FLAG_FORCE_SYNC,
            FLAG_NO_REPLY | FLAG_FORCE_SYNC,
        ] {
            let h = FrameHeader {
                request_id: 9,
                op_or_status: 20,
                flags,
                body_len: 0,
                data_len: 0,
            };
            assert_eq!(FrameHeader::decode(&h.encode()).flags, flags);
        }
    }

    proptest! {
        #[test]
        fn decode_any_24_bytes_never_panics(bytes in prop::array::uniform24(any::<u8>())) {
            let _ = FrameHeader::decode(&bytes);
        }
    }

    /// Version 3 is the session-resumption protocol, and its three refusal
    /// statuses must be protocol statuses (>= 0xFF00), distinct from each
    /// other and from the three that already exist.
    #[test]
    fn version_three_and_the_session_statuses() {
        assert_eq!(PROTOCOL_VERSION, 3);
        let new = [
            STATUS_NO_SESSION,
            STATUS_SESSION_BUSY,
            STATUS_SESSION_MISMATCH,
        ];
        let old = [
            STATUS_VERSION_MISMATCH,
            STATUS_ATTACH_DENIED,
            STATUS_NOT_EXPORTED,
        ];
        for (i, s) in new.iter().enumerate() {
            assert!(*s >= 0xFF00, "a protocol status lives above 0xFF00");
            for later in &new[i + 1..] {
                assert_ne!(s, later);
            }
            for o in &old {
                assert_ne!(s, o);
            }
        }
    }
}
