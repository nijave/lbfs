use crate::types::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Opcode {
    Hello = 1,
    Attach = 2,
    Lookup = 3,
    Forget = 4,
    Getattr = 5,
    Setattr = 6,
    Readlink = 7,
    Symlink = 8,
    Mkdir = 9,
    Unlink = 10,
    Rmdir = 11,
    Rename = 12,
    Link = 13,
    Open = 14,
    Create = 15,
    Read = 16,
    Write = 17,
    Flush = 18,
    Release = 19,
    Fsync = 20,
    Fallocate = 21,
    Lseek = 22,
    CopyFileRange = 23,
    Opendir = 24,
    Readdir = 25,
    Readdirplus = 26,
    Releasedir = 27,
    Fsyncdir = 28,
    Statfs = 29,
    Getxattr = 30,
    Setxattr = 31,
    Listxattr = 32,
    Removexattr = 33,
}

impl TryFrom<u16> for Opcode {
    type Error = u16;

    fn try_from(v: u16) -> Result<Self, u16> {
        Ok(match v {
            1 => Self::Hello,
            2 => Self::Attach,
            3 => Self::Lookup,
            4 => Self::Forget,
            5 => Self::Getattr,
            6 => Self::Setattr,
            7 => Self::Readlink,
            8 => Self::Symlink,
            9 => Self::Mkdir,
            10 => Self::Unlink,
            11 => Self::Rmdir,
            12 => Self::Rename,
            13 => Self::Link,
            14 => Self::Open,
            15 => Self::Create,
            16 => Self::Read,
            17 => Self::Write,
            18 => Self::Flush,
            19 => Self::Release,
            20 => Self::Fsync,
            21 => Self::Fallocate,
            22 => Self::Lseek,
            23 => Self::CopyFileRange,
            24 => Self::Opendir,
            25 => Self::Readdir,
            26 => Self::Readdirplus,
            27 => Self::Releasedir,
            28 => Self::Fsyncdir,
            29 => Self::Statfs,
            30 => Self::Getxattr,
            31 => Self::Setxattr,
            32 => Self::Listxattr,
            33 => Self::Removexattr,
            other => return Err(other),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloRequest {
    pub magic: [u8; 4],
    pub version: u32,
    pub max_inflight: u32,
    pub max_io_size: u32,
    /// Whether the client mounted with `FUSE_WRITEBACK_CACHE`.
    ///
    /// Not a server option and not negotiable: it says whose kernel owns the
    /// page cache and the file size, which changes what an `OPEN` flag means
    /// on the server (`O_APPEND` and `O_WRONLY` in particular — see
    /// `LocalFs::mask_open_flags`). Only the client knows it, so it travels
    /// with the handshake rather than being guessed at attach.
    pub writeback: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloReply {
    pub version: u32,
    pub max_inflight: u32,
    pub max_io_size: u32,
    pub max_body_size: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachRequest {
    #[serde(with = "serde_bytes")]
    pub path: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachReply {
    pub root_attr: FileAttr,
}

/// Reply: [`Entry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookupRequest {
    pub parent: NodeId,
    #[serde(with = "serde_bytes")]
    pub name: Vec<u8>,
}

/// No reply (`FLAG_NO_REPLY`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetRequest {
    pub items: Vec<(NodeId, u64)>,
}

/// Reply: [`FileAttr`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetattrRequest {
    pub node: NodeId,
    pub fh: Option<Fh>,
}

/// Reply: [`FileAttr`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetattrRequest {
    pub node: NodeId,
    pub args: SetattrArgs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadlinkRequest {
    pub node: NodeId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadlinkReply {
    #[serde(with = "serde_bytes")]
    pub target: Vec<u8>,
}

/// Reply: [`Entry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymlinkRequest {
    pub parent: NodeId,
    #[serde(with = "serde_bytes")]
    pub name: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub target: Vec<u8>,
}

/// Reply: [`Entry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MkdirRequest {
    pub parent: NodeId,
    #[serde(with = "serde_bytes")]
    pub name: Vec<u8>,
    pub mode: u32,
}

/// Reply: `()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnlinkRequest {
    pub parent: NodeId,
    #[serde(with = "serde_bytes")]
    pub name: Vec<u8>,
}

/// Reply: `()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RmdirRequest {
    pub parent: NodeId,
    #[serde(with = "serde_bytes")]
    pub name: Vec<u8>,
}

/// Reply: `()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameRequest {
    pub parent: NodeId,
    #[serde(with = "serde_bytes")]
    pub name: Vec<u8>,
    pub newparent: NodeId,
    #[serde(with = "serde_bytes")]
    pub newname: Vec<u8>,
    pub flags: u32,
}

/// Reply: [`Entry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkRequest {
    pub node: NodeId,
    pub newparent: NodeId,
    #[serde(with = "serde_bytes")]
    pub newname: Vec<u8>,
}

/// Reply: [`OpenReply`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenRequest {
    pub node: NodeId,
    pub flags: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenReply {
    pub fh: Fh,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateRequest {
    pub parent: NodeId,
    #[serde(with = "serde_bytes")]
    pub name: Vec<u8>,
    pub mode: u32,
    pub flags: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateReply {
    pub entry: Entry,
    pub fh: Fh,
}

/// Reply: `()` plus the payload in the frame's data segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadRequest {
    pub node: NodeId,
    pub fh: Fh,
    pub offset: u64,
    pub size: u32,
}

/// The request carries the payload in the frame's data segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteRequest {
    pub node: NodeId,
    pub fh: Fh,
    pub offset: u64,
    /// The kernel's `FUSE_WRITE_KILL_SUIDGID`, forwarded verbatim.
    ///
    /// True means the writer holds no `CAP_FSETID`, so the file must lose
    /// set-user-ID — and set-group-ID too when it carries group-execute —
    /// before these bytes land. Under `FUSE_HANDLE_KILLPRIV_V2` this is the
    /// only notice the server gets, because the kernel has stopped doing the
    /// strip itself.
    pub kill_suidgid: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteReply {
    pub written: u32,
}

/// Reply: `()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlushRequest {
    pub node: NodeId,
    pub fh: Fh,
}

/// Reply: `()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseRequest {
    pub node: NodeId,
    pub fh: Fh,
}

/// Reply: `()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsyncRequest {
    pub node: NodeId,
    pub fh: Fh,
    pub datasync: bool,
}

/// Reply: `()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FallocateRequest {
    pub node: NodeId,
    pub fh: Fh,
    pub offset: u64,
    pub length: u64,
    pub mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LseekRequest {
    pub node: NodeId,
    pub fh: Fh,
    pub offset: u64,
    pub whence: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LseekReply {
    pub offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyFileRangeRequest {
    pub node_in: NodeId,
    pub fh_in: Fh,
    pub off_in: u64,
    pub node_out: NodeId,
    pub fh_out: Fh,
    pub off_out: u64,
    pub len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyFileRangeReply {
    pub copied: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpendirRequest {
    pub node: NodeId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpendirReply {
    pub dh: Fh,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReaddirRequest {
    pub node: NodeId,
    pub dh: Fh,
    pub offset: u64,
    pub max_bytes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReaddirReply {
    pub entries: Vec<DirEntry>,
    pub end: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReaddirplusReply {
    pub entries: Vec<DirEntryPlus>,
    pub end: bool,
}

/// Reply: `()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleasedirRequest {
    pub node: NodeId,
    pub dh: Fh,
}

/// Reply: `()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsyncdirRequest {
    pub node: NodeId,
    pub dh: Fh,
    pub datasync: bool,
}

/// Reply: [`StatfsReply`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatfsRequest {
    pub node: NodeId,
}

/// Reply: [`XattrReply`] plus the value in the frame's data segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetxattrRequest {
    pub node: NodeId,
    #[serde(with = "serde_bytes")]
    pub name: Vec<u8>,
    pub size: u32,
}

/// The request carries the value in the frame's data segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetxattrRequest {
    pub node: NodeId,
    #[serde(with = "serde_bytes")]
    pub name: Vec<u8>,
    pub flags: u32,
}

/// Reply: [`XattrReply`] plus the name list in the frame's data segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListxattrRequest {
    pub node: NodeId,
    pub size: u32,
}

/// Reply: `()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovexattrRequest {
    pub node: NodeId,
    #[serde(with = "serde_bytes")]
    pub name: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::MAGIC;

    fn round_trip<
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    >(
        v: &T,
    ) {
        let bytes = postcard::to_allocvec(v).unwrap();
        let back: T = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(&back, v);
    }

    #[test]
    fn opcode_round_trips_through_u16() {
        for raw in 1u16..=33 {
            let op = Opcode::try_from(raw).unwrap();
            assert_eq!(op as u16, raw);
        }
        assert!(Opcode::try_from(0).is_err());
        assert!(Opcode::try_from(34).is_err());
    }

    #[test]
    fn lookup_request_wire_stability_golden() {
        // Pins the postcard encoding: u64 varint parent, length-prefixed bytes.
        let req = LookupRequest {
            parent: 1,
            name: b"hello".to_vec(),
        };
        let bytes = postcard::to_allocvec(&req).unwrap();
        assert_eq!(bytes, vec![1, 5, b'h', b'e', b'l', b'l', b'o']);
    }

    #[test]
    fn all_bodies_round_trip() {
        round_trip(&HelloRequest {
            magic: MAGIC,
            version: 1,
            max_inflight: 128,
            max_io_size: 1 << 20,
            writeback: true,
        });
        round_trip(&HelloRequest {
            magic: MAGIC,
            version: 1,
            max_inflight: 8,
            max_io_size: 4096,
            writeback: false,
        });
        round_trip(&HelloReply {
            version: 1,
            max_inflight: 128,
            max_io_size: 1 << 20,
            max_body_size: 64 << 10,
        });
        round_trip(&AttachRequest {
            path: b"/srv/exports/a".to_vec(),
        });
        round_trip(&SetattrRequest {
            node: 3,
            args: SetattrArgs {
                mode: Some(0o644),
                uid: None,
                gid: None,
                size: Some(0),
                atime: TimeSet::Now,
                mtime: TimeSet::Set { sec: 1, nsec: 2 },
                fh: Some(9),
            },
        });
        round_trip(&ReaddirReply {
            entries: vec![DirEntry {
                name: b"x".to_vec(),
                ino: 42,
                kind: FileKind::Regular,
                offset: 1,
            }],
            end: true,
        });
        round_trip(&Entry {
            node: 2,
            generation: 7,
            attr: FileAttr::default(),
        });
        // Non-UTF8 names must survive.
        round_trip(&LookupRequest {
            parent: 1,
            name: vec![0xff, 0xfe, b'/'],
        });
    }

    #[test]
    fn write_request_wire_stability_golden() {
        // Pins the postcard encoding: three u64 varints then a one-byte bool.
        // A version-1 peer decoding this body stops after the third varint and
        // drops the flag, which is why PROTOCOL_VERSION moved to 2.
        let set = WriteRequest {
            node: 1,
            fh: 2,
            offset: 3,
            kill_suidgid: true,
        };
        assert_eq!(postcard::to_allocvec(&set).unwrap(), vec![1, 2, 3, 1]);

        let clear = WriteRequest {
            node: 1,
            fh: 2,
            offset: 3,
            kill_suidgid: false,
        };
        assert_eq!(postcard::to_allocvec(&clear).unwrap(), vec![1, 2, 3, 0]);
    }

    #[test]
    fn write_request_round_trips_both_flag_states() {
        for kill_suidgid in [false, true] {
            round_trip(&WriteRequest {
                node: 9,
                fh: 4,
                offset: 1 << 40,
                kill_suidgid,
            });
        }
    }

    #[test]
    fn protocol_version_is_two() {
        // Version 1 bodies cannot carry the flag, and postcard ignores
        // trailing bytes rather than refusing them, so the handshake is the
        // only place that can catch a half-deployed pair.
        assert_eq!(crate::frame::PROTOCOL_VERSION, 2);
    }
}
