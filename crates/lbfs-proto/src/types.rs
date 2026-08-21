use serde::{Deserialize, Serialize};

pub type NodeId = u64;
pub type Fh = u64;

pub const ROOT_NODE: NodeId = 1; // FUSE_ROOT_ID

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileKind {
    Regular,
    Directory,
    Symlink,
    Socket,
    Fifo,
    CharDevice,
    BlockDevice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FileAttr {
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime_sec: i64,
    pub atime_nsec: u32,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    pub ctime_sec: i64,
    pub ctime_nsec: u32,
    pub mode: u32, // full st_mode including file type bits
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u32,
    pub blksize: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub node: NodeId,
    pub generation: u64,
    pub attr: FileAttr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeSet {
    Omit,
    Now,
    Set { sec: i64, nsec: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetattrArgs {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime: TimeSet,
    pub mtime: TimeSet,
    pub fh: Option<Fh>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatfsReply {
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub bsize: u32,
    pub namelen: u32,
    pub frsize: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    #[serde(with = "serde_bytes")]
    pub name: Vec<u8>,
    pub kind: FileKind,
    pub offset: u64, // opaque resume cursor: pass back as ReaddirRequest.offset
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntryPlus {
    #[serde(with = "serde_bytes")]
    pub name: Vec<u8>,
    pub entry: Entry,
    pub offset: u64,
}

/// Xattr get/list use FUSE's two-phase shape: size == 0 asks for the value
/// length only; the reply then carries `size` and empty data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XattrReply {
    pub size: u32,
}
