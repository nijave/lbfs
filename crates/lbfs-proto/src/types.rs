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
    /// The entry's inode number, straight from `getdents64`'s `d_ino`.
    ///
    /// Not decoration: glibc's `readdir(3)` *drops* a dirent whose `d_ino` is
    /// zero, treating it as a deleted slot. Without this field the client
    /// would have to invent a number for every entry, and inventing one that
    /// disagrees with the `LOOKUP` that follows is worse than carrying the
    /// real one. `..` therefore reports the true parent inode, which is the
    /// one place a directory's own attributes cannot supply it.
    pub ino: u64,
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

/// What a client presents to claim a session it was already attached to.
///
/// `id` is the registry key: a server-local counter, never reused inside one
/// process. `secret` is the whole of the authentication — 128 bits from
/// `getrandom(2)`, compared in constant time. v1 has no authentication at all
/// (spec §1), and this does not add one: a ticket is a bearer capability worth
/// roughly what a fresh `ATTACH` to the same export is worth, plus the open
/// descriptors a fresh attach could not reach. The session-resumption design
/// document prices it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTicket {
    pub id: u64,
    pub secret: [u8; 16],
    /// How long the server holds this session after its socket dies. Zero
    /// means the server retains nothing.
    pub grace_ms: u32,
}

impl SessionTicket {
    /// Constant-time secret comparison.
    ///
    /// A short-circuiting `==` leaks the length of the matching prefix through
    /// timing, which turns 2^128 guesses into 16 × 256. The loop below reads
    /// every byte whatever it finds.
    pub fn secret_eq(&self, other: &[u8; 16]) -> bool {
        let mut diff = 0u8;
        for (a, b) in self.secret.iter().zip(other.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ticket_round_trips_and_compares_in_constant_time() {
        let t = SessionTicket {
            id: 42,
            secret: [7u8; 16],
            grace_ms: 60_000,
        };
        let bytes = postcard::to_allocvec(&t).unwrap();
        let back: SessionTicket = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, t);

        // One flipped secret byte must fail the comparison, through the
        // constant-time helper rather than derived `==`.
        let mut other = t.secret;
        other[9] ^= 1;
        assert!(t.secret_eq(&t.secret));
        assert!(!t.secret_eq(&other));
    }
}
