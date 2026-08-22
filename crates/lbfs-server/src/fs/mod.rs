//! The backend boundary.
//!
//! [`FileSystem`] has exactly one method per protocol opcode that touches a
//! filesystem, so the RPC layer can dispatch a decoded request straight onto a
//! trait object without knowing what is behind it. [`local::LocalFs`] is the
//! only implementation in v1; the trait is what lets an authorization
//! decorator, a passthrough cache, or a different storage backend take its
//! place later without the wire layer noticing (spec §5.2).
//!
//! Errors are raw Linux errnos ([`Errno`]) rather than a bespoke taxonomy:
//! whatever the backing syscall reported travels to the client's FUSE reply
//! unchanged (spec §8).

pub mod local;

use lbfs_proto::ops::{ReaddirReply, ReaddirplusReply};
use lbfs_proto::types::{Entry, Fh, FileAttr, NodeId, SetattrArgs, StatfsReply};
use lbfs_proto::Errno;

use crate::fs::local::buffers::PooledBuf;

/// Every backend operation either succeeds or reports the errno the client's
/// FUSE reply will carry.
pub type FsResult<T> = Result<T, Errno>;

/// One method per filesystem-touching opcode.
///
/// Names cross the wire as bytes, never as `str`: a Linux filename is any byte
/// sequence without `/` or NUL, and re-encoding it through UTF-8 would corrupt
/// perfectly legal names. Implementations validate them.
///
/// `Send + Sync + 'static` is what lets the connection task hold an
/// `Arc<dyn FileSystem>` and run requests concurrently.
#[async_trait::async_trait]
pub trait FileSystem: Send + Sync + 'static {
    async fn lookup(&self, parent: NodeId, name: &[u8]) -> FsResult<Entry>;
    async fn forget(&self, node: NodeId, nlookup: u64);
    async fn getattr(&self, node: NodeId, fh: Option<Fh>) -> FsResult<FileAttr>;
    async fn setattr(&self, node: NodeId, args: SetattrArgs) -> FsResult<FileAttr>;
    async fn readlink(&self, node: NodeId) -> FsResult<Vec<u8>>;
    async fn symlink(&self, parent: NodeId, name: &[u8], target: &[u8]) -> FsResult<Entry>;
    async fn mkdir(&self, parent: NodeId, name: &[u8], mode: u32) -> FsResult<Entry>;
    async fn unlink(&self, parent: NodeId, name: &[u8]) -> FsResult<()>;
    async fn rmdir(&self, parent: NodeId, name: &[u8]) -> FsResult<()>;
    async fn rename(
        &self,
        parent: NodeId,
        name: &[u8],
        newparent: NodeId,
        newname: &[u8],
        flags: u32,
    ) -> FsResult<()>;
    async fn link(&self, node: NodeId, newparent: NodeId, newname: &[u8]) -> FsResult<Entry>;
    async fn open(&self, node: NodeId, flags: u32) -> FsResult<Fh>;
    async fn create(
        &self,
        parent: NodeId,
        name: &[u8],
        mode: u32,
        flags: u32,
    ) -> FsResult<(Entry, Fh)>;
    /// Returns the pooled buffer the read filled; a short read at EOF comes
    /// back with the smaller length, not an error.
    async fn read(&self, node: NodeId, fh: Fh, offset: u64, size: u32) -> FsResult<PooledBuf>;
    /// `kill_suidgid` carries the kernel's `FUSE_WRITE_KILL_SUIDGID`: the
    /// writer holds no `CAP_FSETID`, so the file loses set-user-ID — and
    /// set-group-ID when it also carries group execute — before these bytes
    /// land. Under `FUSE_HANDLE_KILLPRIV_V2` the client's kernel does nothing
    /// about it, so this flag is the only notice a backend receives.
    async fn write(
        &self,
        node: NodeId,
        fh: Fh,
        offset: u64,
        data: PooledBuf,
        len: u32,
        kill_suidgid: bool,
    ) -> FsResult<u32>;
    async fn flush(&self, node: NodeId, fh: Fh) -> FsResult<()>;
    async fn release(&self, node: NodeId, fh: Fh) -> FsResult<()>;
    async fn fsync(&self, node: NodeId, fh: Fh, datasync: bool) -> FsResult<()>;
    async fn fallocate(
        &self,
        node: NodeId,
        fh: Fh,
        offset: u64,
        length: u64,
        mode: u32,
    ) -> FsResult<()>;
    async fn lseek(&self, node: NodeId, fh: Fh, offset: u64, whence: u32) -> FsResult<u64>;
    /// Eight arguments because the opcode has eight: two (node, handle,
    /// offset) triples and a length. Collapsing them into a struct would only
    /// move the same fields somewhere else.
    #[allow(clippy::too_many_arguments)]
    async fn copy_file_range(
        &self,
        node_in: NodeId,
        fh_in: Fh,
        off_in: u64,
        node_out: NodeId,
        fh_out: Fh,
        off_out: u64,
        len: u64,
    ) -> FsResult<u64>;
    async fn opendir(&self, node: NodeId) -> FsResult<Fh>;
    async fn readdir(
        &self,
        node: NodeId,
        dh: Fh,
        offset: u64,
        max_bytes: u32,
    ) -> FsResult<ReaddirReply>;
    async fn readdirplus(
        &self,
        node: NodeId,
        dh: Fh,
        offset: u64,
        max_bytes: u32,
    ) -> FsResult<ReaddirplusReply>;
    async fn releasedir(&self, node: NodeId, dh: Fh) -> FsResult<()>;
    async fn fsyncdir(&self, node: NodeId, dh: Fh, datasync: bool) -> FsResult<()>;
    async fn statfs(&self, node: NodeId) -> FsResult<StatfsReply>;
    /// `(value_size, bytes)`. Per FUSE convention `size == 0` asks for the
    /// length alone and the returned byte vector is empty.
    async fn getxattr(&self, node: NodeId, name: &[u8], size: u32) -> FsResult<(u32, Vec<u8>)>;
    async fn setxattr(&self, node: NodeId, name: &[u8], value: &[u8], flags: u32) -> FsResult<()>;
    /// `(list_size, bytes)`, with the same `size == 0` length-probe rule as
    /// [`FileSystem::getxattr`].
    async fn listxattr(&self, node: NodeId, size: u32) -> FsResult<(u32, Vec<u8>)>;
    async fn removexattr(&self, node: NodeId, name: &[u8]) -> FsResult<()>;
}
