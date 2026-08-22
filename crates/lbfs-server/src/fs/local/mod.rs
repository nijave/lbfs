//! `LocalFs`: the passthrough backend, modeled on virtiofsd.
//!
//! Every operation is descriptor-relative. A [`NodeId`] resolves through
//! [`NodeTable`] to an `O_PATH` descriptor, and a name resolves from there with
//! `openat2(parent, name, RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)`. Nothing
//! ever walks an absolute path, so a server-side rename cannot break a node the
//! client still holds, and no lookup can leave the export tree — the kernel
//! refuses the resolution rather than us filtering the result (spec §5.3).
//!
//! # Why `O_PATH`
//!
//! A node descriptor is opened `O_PATH | O_NOFOLLOW`, which pins the inode
//! without granting read or write access and, crucially, without dereferencing
//! a symlink: `READLINK` hands the target string to the client and the client's
//! kernel resolves it. The cost is that `O_PATH` descriptors reject most
//! syscalls (`fchmod`, `read`, `fgetxattr`, ...), so operations that need real
//! access reopen through `/proc/self/fd/N` — the same trick virtiofsd uses.
//! `/proc` must therefore be mounted in the server's namespace.
//!
//! # Two safety rules this module owes the rest of the server
//!
//! * **One `register` per [`Entry`].** Every `Entry` handed to a client carries
//!   a lookup count, and the client retires it with exactly one `FORGET`.
//!   Registering without returning the entry leaks a node for the session;
//!   returning one without registering lets a later `FORGET` drop a node the
//!   client still believes in. `lookup_impl` is the single place that does
//!   both, and every entry-returning op goes through it.
//! * **`FileKey` is built one way.** Attach and lookup must agree bit for bit,
//!   or the export root would fail to dedup against itself and the loop guard
//!   below would never fire. [`make_key`] is the only constructor.

pub mod buffers;
pub mod nodes;

// The io_uring bridge is the single sanctioned home for `unsafe` in this
// workspace: raw SQE submission and raw pointers into slab-owned payloads
// cannot be expressed safely. Every other module inherits the crate root's
// `#![deny(unsafe_code)]`.
#[allow(unsafe_code)]
pub mod uring;

use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use lbfs_proto::ops::{ReaddirReply, ReaddirplusReply};
use lbfs_proto::types::{Entry, Fh, FileAttr, NodeId, SetattrArgs, StatfsReply, TimeSet};
use lbfs_proto::Errno;

use crate::config::FsyncPolicy;
use crate::fs::{FileSystem, FsResult};
use buffers::{BufferPool, PooledBuf};
use nodes::{FileKey, NodeTable};
use uring::UringExecutor;

/// Not in [`Errno`]'s constant list, and this is its only use: a child that
/// resolves back to the export root is a directory loop.
const ELOOP: Errno = Errno(libc::ELOOP as u16);

// ---------------------------------------------------------------------------
// Handles
// ---------------------------------------------------------------------------

/// Slab of open handles behind one mutex, keyed by a monotonic counter.
///
/// Ids are never reused, so an `Fh` a client kept past its `RELEASE` misses
/// rather than aliasing somebody else's handle.
pub struct HandleTable<T> {
    map: Mutex<HashMap<Fh, T>>,
    next: AtomicU64,
}

impl<T> Default for HandleTable<T> {
    fn default() -> HandleTable<T> {
        HandleTable {
            map: Mutex::new(HashMap::new()),
            // Zero is left free so it can stay a "no handle" sentinel on the
            // wire.
            next: AtomicU64::new(1),
        }
    }
}

impl<T> HandleTable<T> {
    pub fn new() -> HandleTable<T> {
        HandleTable::default()
    }

    pub fn insert(&self, v: T) -> Fh {
        let fh = self.next.fetch_add(1, Ordering::Relaxed);
        self.map.lock().unwrap().insert(fh, v);
        fh
    }

    pub fn get(&self, fh: Fh) -> Option<T>
    where
        T: Clone,
    {
        self.map.lock().unwrap().get(&fh).cloned()
    }

    pub fn remove(&self, fh: Fh) -> Option<T> {
        self.map.lock().unwrap().remove(&fh)
    }
}

/// Snapshot of one open directory.
///
/// Task 11 fills this in with the entry list taken at `OPENDIR` and the
/// reopened `O_RDONLY | O_DIRECTORY` descriptor `FSYNCDIR` needs.
pub struct DirHandle;

// ---------------------------------------------------------------------------
// LocalFs
// ---------------------------------------------------------------------------

pub struct LocalFs {
    uring: UringExecutor,
    nodes: NodeTable,
    /// Task 10: the read/write data path draws its buffers from here.
    #[allow(dead_code)]
    pool: BufferPool,
    /// Task 10: `OPEN`/`CREATE` park their data descriptors here. `setattr`
    /// already reads it, so a truncate against an open handle is correct the
    /// moment the handles exist.
    files: HandleTable<Arc<OwnedFd>>,
    /// Task 11: `OPENDIR` parks its directory snapshots here.
    #[allow(dead_code)]
    dirs: HandleTable<Arc<DirHandle>>,
    /// Task 10: masks `O_SYNC`/`O_DSYNC` and short-circuits `FSYNC`.
    #[allow(dead_code)]
    fsync_policy: FsyncPolicy,
    /// `(dev, ino)` of the export root, kept only for the loop guard in
    /// [`LocalFs::lookup_impl`].
    root_key: FileKey,
}

impl LocalFs {
    /// Opens `export_root` as node 1 and records its identity.
    ///
    /// The root `statx` runs synchronously rather than through the ring: this
    /// is attach time, once per connection, and the executor's async path buys
    /// nothing before the connection is even serving.
    pub fn new(
        export_root: &Path,
        fsync: FsyncPolicy,
        uring: UringExecutor,
        pool: BufferPool,
    ) -> io::Result<LocalFs> {
        let root = rustix::fs::open(
            export_root,
            rustix::fs::OFlags::PATH | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?;
        let st = rustix::fs::statx(
            &root,
            "",
            rustix::fs::AtFlags::EMPTY_PATH,
            rustix::fs::StatxFlags::BASIC_STATS,
        )?;
        let root_key = make_key(st.stx_dev_major, st.stx_dev_minor, st.stx_ino);
        Ok(LocalFs {
            uring,
            nodes: NodeTable::new(root, root_key),
            pool,
            files: HandleTable::new(),
            dirs: HandleTable::new(),
            fsync_policy: fsync,
            root_key,
        })
    }

    /// Resolves a node id to its `O_PATH` descriptor.
    ///
    /// A miss means the client is using an id it already forgot, or one from a
    /// previous session: `ESTALE`, per spec §8.
    fn node_fd(&self, node: NodeId) -> FsResult<Arc<OwnedFd>> {
        self.nodes.get(node).map(|(fd, _)| fd).ok_or(Errno::ESTALE)
    }

    /// `openat2` with the escape-proof resolve flags.
    ///
    /// `RESOLVE_BENEATH` makes the kernel refuse any resolution that leaves
    /// `parent`, and `RESOLVE_NO_MAGICLINKS` stops a `/proc` magic link from
    /// teleporting out of the tree. `O_PATH | O_NOFOLLOW` means a symlink child
    /// becomes a node describing the link itself, never its target.
    async fn open_child(&self, parent: &Arc<OwnedFd>, name: &CString) -> io::Result<OwnedFd> {
        let how = io_uring::types::OpenHow::new()
            .flags((libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC) as u64)
            .resolve(libc::RESOLVE_BENEATH | libc::RESOLVE_NO_MAGICLINKS);
        self.uring.openat2(parent, name.clone(), how).await
    }

    /// `statx` on the node's own descriptor, via `AT_EMPTY_PATH`.
    ///
    /// Naming the descriptor rather than re-resolving the path is what keeps
    /// the attributes and the node describing the same inode: a rename or an
    /// unlink racing the reply cannot make them disagree.
    async fn statx_fd(&self, fd: &Arc<OwnedFd>) -> io::Result<libc::statx> {
        self.uring
            .statx(
                fd,
                CString::default(),
                libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
                libc::STATX_BASIC_STATS,
            )
            .await
    }

    /// Resolve one child and turn it into the [`Entry`] the client gets back.
    ///
    /// This is the only `register` call site, which is what keeps the lookup
    /// count the client owes exactly one `FORGET` per entry. `mkdir`,
    /// `symlink` and `link` reuse it after creating their name, so a created
    /// object is reported exactly like a looked-up one — and re-validating the
    /// name here would be redundant, hence the `CString` argument.
    async fn lookup_impl(&self, parent_fd: &Arc<OwnedFd>, name: &CString) -> FsResult<Entry> {
        let child = Arc::new(self.open_child(parent_fd, name).await.map_err(errno)?);
        let st = self.statx_fd(&child).await.map_err(errno)?;
        let key = file_key(&st);
        if key == self.root_key {
            // The export root bind-mounted inside itself resolves cleanly
            // through RESOLVE_BENEATH, and dedup would then hand the client
            // node 1 as its own child - a loop its dcache cannot represent.
            // Refusing the entry is narrower than RESOLVE_NO_XDEV, which would
            // also reject the legitimate cross-device children that (dev, ino)
            // keys already handle.
            return Err(ELOOP);
        }
        let owned = into_owned(child).map_err(errno)?;
        let (node, generation, _fd) = self.nodes.register(owned, key);
        Ok(Entry {
            node,
            generation,
            attr: attr_from_statx(&st),
        })
    }

    #[cfg(test)]
    async fn key_of_node_for_test(&self, node: NodeId) -> FileKey {
        let fd = self.node_fd(node).unwrap();
        file_key(&self.statx_fd(&fd).await.unwrap())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Validates one path component arriving from the wire.
///
/// Names are bytes, not `str`: any byte sequence is a legal Linux filename as
/// long as it holds no `/` and no NUL. `.` and `..` are rejected outright — a
/// FUSE lookup is single-component by construction, so their only use here
/// would be traversal.
fn valid_name(name: &[u8]) -> FsResult<CString> {
    if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') || name.contains(&0)
    {
        return Err(Errno::EINVAL);
    }
    CString::new(name).map_err(|_| Errno::EINVAL)
}

/// The one `FileKey` constructor, so attach and lookup cannot disagree.
///
/// `statx` reports the device as a major/minor pair rather than the packed
/// `st_dev` that `fstat` returns, and the two encodings are not
/// interchangeable. Funnelling every key through `makedev` here means the
/// export root and its children are keyed identically — without that the root
/// would not dedup against itself and the loop guard in
/// [`LocalFs::lookup_impl`] could never fire.
fn make_key(dev_major: u32, dev_minor: u32, ino: u64) -> FileKey {
    (libc::makedev(dev_major, dev_minor), ino)
}

fn file_key(st: &libc::statx) -> FileKey {
    make_key(st.stx_dev_major, st.stx_dev_minor, st.stx_ino)
}

fn attr_from_statx(st: &libc::statx) -> FileAttr {
    FileAttr {
        ino: st.stx_ino,
        size: st.stx_size,
        blocks: st.stx_blocks,
        atime_sec: st.stx_atime.tv_sec,
        atime_nsec: st.stx_atime.tv_nsec,
        mtime_sec: st.stx_mtime.tv_sec,
        mtime_nsec: st.stx_mtime.tv_nsec,
        ctime_sec: st.stx_ctime.tv_sec,
        ctime_nsec: st.stx_ctime.tv_nsec,
        // Full mode including the file-type bits: the client rebuilds the
        // FUSE attribute from this alone.
        mode: u32::from(st.stx_mode),
        nlink: st.stx_nlink,
        uid: st.stx_uid,
        gid: st.stx_gid,
        // FUSE carries rdev as 32 bits, so this is the protocol's width, not a
        // lossy choice of ours.
        rdev: libc::makedev(st.stx_rdev_major, st.stx_rdev_minor) as u32,
        blksize: st.stx_blksize,
    }
}

/// Path through `/proc` that names the file `fd` already refers to.
///
/// This is how an `O_PATH` descriptor gets used with the syscalls that reject
/// it. Resolution jumps straight to the pinned inode, so it is not a second
/// name lookup and cannot race a rename.
fn proc_path(fd: &OwnedFd) -> String {
    format!("/proc/self/fd/{}", fd.as_raw_fd())
}

/// Reopen an `O_PATH` descriptor with real access rights.
fn reopen(fd: &OwnedFd, flags: rustix::fs::OFlags) -> io::Result<OwnedFd> {
    Ok(rustix::fs::open(
        proc_path(fd),
        flags | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?)
}

/// Recover the sole owner of a descriptor the executor briefly shared.
///
/// The ring thread sends a completion *before* it drops its own clone of the
/// descriptor `Arc`, so an awaiting caller can win that race and find the
/// refcount still at two. Duplicating is the right answer in that case:
/// `dup(2)` names the same inode, and the ring thread's clone closes itself a
/// moment later.
fn into_owned(fd: Arc<OwnedFd>) -> io::Result<OwnedFd> {
    match Arc::try_unwrap(fd) {
        Ok(fd) => Ok(fd),
        Err(shared) => shared.try_clone(),
    }
}

fn errno(e: io::Error) -> Errno {
    Errno::from_io(&e)
}

fn rustix_errno(e: rustix::io::Errno) -> Errno {
    errno(io::Error::from(e))
}

/// A `spawn_blocking` panic or a shut-down runtime; neither has a truthful
/// errno, so the client sees the generic I/O failure.
fn join_errno(_e: tokio::task::JoinError) -> Errno {
    Errno::EIO
}

fn timespec(t: TimeSet) -> rustix::fs::Timespec {
    match t {
        TimeSet::Omit => rustix::fs::Timespec {
            tv_sec: 0,
            tv_nsec: rustix::fs::UTIME_OMIT,
        },
        TimeSet::Now => rustix::fs::Timespec {
            tv_sec: 0,
            tv_nsec: rustix::fs::UTIME_NOW,
        },
        TimeSet::Set { sec, nsec } => rustix::fs::Timespec {
            tv_sec: sec,
            tv_nsec: nsec.into(),
        },
    }
}

/// The blocking half of `SETATTR`.
///
/// None of these have io_uring opcodes, so they run on a blocking thread as one
/// batch rather than four round trips. Order is deliberate:
///
/// 1. **owner** first, because `chown` clears set-user-ID and set-group-ID;
/// 2. **mode** second, so a caller asking for both gets the mode it asked for;
/// 3. **size**, which is the only step needing write access;
/// 4. **times** last, so an explicit timestamp beats the implicit `mtime`
///    bump a truncate just caused.
fn apply_setattr(
    fd: &OwnedFd,
    write_fd: Option<&OwnedFd>,
    args: &SetattrArgs,
) -> Result<(), Errno> {
    if args.uid.is_some() || args.gid.is_some() {
        rustix::fs::chownat(
            fd,
            "",
            args.uid.map(rustix::fs::Uid::from_raw),
            args.gid.map(rustix::fs::Gid::from_raw),
            rustix::fs::AtFlags::EMPTY_PATH,
        )
        .map_err(rustix_errno)?;
    }
    if let Some(mode) = args.mode {
        // `fchmod` refuses an O_PATH descriptor, so go through /proc. On a
        // symlink node the kernel answers EOPNOTSUPP, which is the correct
        // reply: symlink permission bits are meaningless on Linux.
        rustix::fs::chmod(
            proc_path(fd),
            rustix::fs::Mode::from_bits_truncate(mode & 0o7777),
        )
        .map_err(rustix_errno)?;
    }
    if let Some(size) = args.size {
        match write_fd {
            Some(open_fd) => rustix::fs::ftruncate(open_fd, size).map_err(rustix_errno)?,
            None => {
                let w = reopen(fd, rustix::fs::OFlags::WRONLY).map_err(errno)?;
                rustix::fs::ftruncate(&w, size).map_err(rustix_errno)?;
            }
        }
    }
    if !matches!((args.atime, args.mtime), (TimeSet::Omit, TimeSet::Omit)) {
        let times = rustix::fs::Timestamps {
            last_access: timespec(args.atime),
            last_modification: timespec(args.mtime),
        };
        // `futimens` rejects an O_PATH descriptor and `AT_EMPTY_PATH` is a
        // recent addition to `utimensat`; the /proc path works everywhere and
        // still lands on the symlink itself for a symlink node.
        rustix::fs::utimensat(
            rustix::fs::CWD,
            proc_path(fd),
            &times,
            rustix::fs::AtFlags::empty(),
        )
        .map_err(rustix_errno)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// FileSystem
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl FileSystem for LocalFs {
    async fn lookup(&self, parent: NodeId, name: &[u8]) -> FsResult<Entry> {
        let name = valid_name(name)?;
        let parent_fd = self.node_fd(parent)?;
        self.lookup_impl(&parent_fd, &name).await
    }

    async fn forget(&self, node: NodeId, nlookup: u64) {
        self.nodes.forget(node, nlookup);
    }

    async fn getattr(&self, node: NodeId, _fh: Option<Fh>) -> FsResult<FileAttr> {
        // The `fh` is advisory: the node's own descriptor and any handle onto
        // it name the same inode, so statx-ing the node is already exact.
        let fd = self.node_fd(node)?;
        let st = self.statx_fd(&fd).await.map_err(errno)?;
        Ok(attr_from_statx(&st))
    }

    async fn setattr(&self, node: NodeId, args: SetattrArgs) -> FsResult<FileAttr> {
        let fd = self.node_fd(node)?;
        // A truncate against an open handle must use that handle's descriptor:
        // the file may have been unlinked, or opened with rights the node's
        // O_PATH reopen would not get back. Empty until Task 10 fills `files`.
        let write_fd = args.fh.and_then(|fh| self.files.get(fh));
        let owned = Arc::clone(&fd);
        tokio::task::spawn_blocking(move || apply_setattr(&owned, write_fd.as_deref(), &args))
            .await
            .map_err(join_errno)??;
        let st = self.statx_fd(&fd).await.map_err(errno)?;
        Ok(attr_from_statx(&st))
    }

    async fn readlink(&self, node: NodeId) -> FsResult<Vec<u8>> {
        let fd = self.node_fd(node)?;
        // No io_uring opcode for readlinkat (spec §5.3), so a blocking thread
        // takes it. An empty path with the symlink's own O_PATH descriptor is
        // the documented way to read a link you hold rather than name.
        let target = tokio::task::spawn_blocking(move || {
            rustix::fs::readlinkat(&*fd, "", Vec::new()).map_err(rustix_errno)
        })
        .await
        .map_err(join_errno)??;
        Ok(target.into_bytes())
    }

    async fn symlink(&self, parent: NodeId, name: &[u8], target: &[u8]) -> FsResult<Entry> {
        let name = valid_name(name)?;
        // The target is opaque to us - it may be absolute, may contain '/',
        // and is never dereferenced server-side. Only NUL is impossible.
        let target = CString::new(target).map_err(|_| Errno::EINVAL)?;
        let parent_fd = self.node_fd(parent)?;
        self.uring
            .symlinkat(target, &parent_fd, name.clone())
            .await
            .map_err(errno)?;
        self.lookup_impl(&parent_fd, &name).await
    }

    async fn mkdir(&self, parent: NodeId, name: &[u8], mode: u32) -> FsResult<Entry> {
        let name = valid_name(name)?;
        let parent_fd = self.node_fd(parent)?;
        self.uring
            .mkdirat(&parent_fd, name.clone(), mode)
            .await
            .map_err(errno)?;
        self.lookup_impl(&parent_fd, &name).await
    }

    async fn unlink(&self, parent: NodeId, name: &[u8]) -> FsResult<()> {
        let name = valid_name(name)?;
        let parent_fd = self.node_fd(parent)?;
        self.uring
            .unlinkat(&parent_fd, name, false)
            .await
            .map_err(errno)
    }

    async fn rmdir(&self, parent: NodeId, name: &[u8]) -> FsResult<()> {
        let name = valid_name(name)?;
        let parent_fd = self.node_fd(parent)?;
        self.uring
            .unlinkat(&parent_fd, name, true)
            .await
            .map_err(errno)
    }

    async fn rename(
        &self,
        parent: NodeId,
        name: &[u8],
        newparent: NodeId,
        newname: &[u8],
        flags: u32,
    ) -> FsResult<()> {
        let name = valid_name(name)?;
        let newname = valid_name(newname)?;
        let parent_fd = self.node_fd(parent)?;
        let newparent_fd = self.node_fd(newparent)?;
        // `flags` (RENAME_NOREPLACE, RENAME_EXCHANGE, ...) goes to the kernel
        // untouched; it validates combinations we have no business second
        // guessing.
        self.uring
            .renameat(&parent_fd, name, &newparent_fd, newname, flags)
            .await
            .map_err(errno)
    }

    async fn link(&self, node: NodeId, newparent: NodeId, newname: &[u8]) -> FsResult<Entry> {
        let newname = valid_name(newname)?;
        let node_fd = self.node_fd(node)?;
        let newparent_fd = self.node_fd(newparent)?;
        // `AT_EMPTY_PATH` links the inode the descriptor holds, with no second
        // name resolution to race. It needed CAP_DAC_READ_SEARCH before Linux
        // 6.10 relaxed it; the io_uring metadata opcodes this server is built
        // on already require a newer kernel than that (spec §2).
        self.uring
            .linkat(
                &node_fd,
                CString::default(),
                &newparent_fd,
                newname.clone(),
                libc::AT_EMPTY_PATH,
            )
            .await
            .map_err(errno)?;
        // Dedup on (dev, ino) means this returns the source node's id, and the
        // client owes a FORGET for this entry as well as the original.
        self.lookup_impl(&newparent_fd, &newname).await
    }

    // --- Task 10: file I/O -------------------------------------------------

    async fn open(&self, _node: NodeId, _flags: u32) -> FsResult<Fh> {
        Err(Errno::ENOSYS) // Task 10
    }

    async fn create(
        &self,
        _parent: NodeId,
        _name: &[u8],
        _mode: u32,
        _flags: u32,
    ) -> FsResult<(Entry, Fh)> {
        Err(Errno::ENOSYS) // Task 10
    }

    async fn read(&self, _node: NodeId, _fh: Fh, _offset: u64, _size: u32) -> FsResult<PooledBuf> {
        Err(Errno::ENOSYS) // Task 10
    }

    async fn write(
        &self,
        _node: NodeId,
        _fh: Fh,
        _offset: u64,
        _data: PooledBuf,
        _len: u32,
    ) -> FsResult<u32> {
        Err(Errno::ENOSYS) // Task 10
    }

    async fn flush(&self, _node: NodeId, _fh: Fh) -> FsResult<()> {
        Err(Errno::ENOSYS) // Task 10
    }

    async fn release(&self, _node: NodeId, _fh: Fh) -> FsResult<()> {
        Err(Errno::ENOSYS) // Task 10
    }

    async fn fsync(&self, _node: NodeId, _fh: Fh, _datasync: bool) -> FsResult<()> {
        Err(Errno::ENOSYS) // Task 10
    }

    async fn fallocate(
        &self,
        _node: NodeId,
        _fh: Fh,
        _offset: u64,
        _length: u64,
        _mode: u32,
    ) -> FsResult<()> {
        Err(Errno::ENOSYS) // Task 10
    }

    async fn lseek(&self, _node: NodeId, _fh: Fh, _offset: u64, _whence: u32) -> FsResult<u64> {
        Err(Errno::ENOSYS) // Task 10
    }

    async fn copy_file_range(
        &self,
        _node_in: NodeId,
        _fh_in: Fh,
        _off_in: u64,
        _node_out: NodeId,
        _fh_out: Fh,
        _off_out: u64,
        _len: u64,
    ) -> FsResult<u64> {
        Err(Errno::ENOSYS) // Task 10
    }

    // --- Task 11: directories, statfs, xattrs ------------------------------

    async fn opendir(&self, _node: NodeId) -> FsResult<Fh> {
        Err(Errno::ENOSYS) // Task 11
    }

    async fn readdir(
        &self,
        _node: NodeId,
        _dh: Fh,
        _offset: u64,
        _max_bytes: u32,
    ) -> FsResult<ReaddirReply> {
        Err(Errno::ENOSYS) // Task 11
    }

    async fn readdirplus(
        &self,
        _node: NodeId,
        _dh: Fh,
        _offset: u64,
        _max_bytes: u32,
    ) -> FsResult<ReaddirplusReply> {
        Err(Errno::ENOSYS) // Task 11
    }

    async fn releasedir(&self, _node: NodeId, _dh: Fh) -> FsResult<()> {
        Err(Errno::ENOSYS) // Task 11
    }

    async fn fsyncdir(&self, _node: NodeId, _dh: Fh, _datasync: bool) -> FsResult<()> {
        Err(Errno::ENOSYS) // Task 11
    }

    async fn statfs(&self, _node: NodeId) -> FsResult<StatfsReply> {
        Err(Errno::ENOSYS) // Task 11
    }

    async fn getxattr(&self, _node: NodeId, _name: &[u8], _size: u32) -> FsResult<(u32, Vec<u8>)> {
        Err(Errno::ENOSYS) // Task 11
    }

    async fn setxattr(
        &self,
        _node: NodeId,
        _name: &[u8],
        _value: &[u8],
        _flags: u32,
    ) -> FsResult<()> {
        Err(Errno::ENOSYS) // Task 11
    }

    async fn listxattr(&self, _node: NodeId, _size: u32) -> FsResult<(u32, Vec<u8>)> {
        Err(Errno::ENOSYS) // Task 11
    }

    async fn removexattr(&self, _node: NodeId, _name: &[u8]) -> FsResult<()> {
        Err(Errno::ENOSYS) // Task 11
    }
}

#[cfg(test)]
pub(crate) async fn test_fs(policy: crate::config::FsyncPolicy) -> (tempfile::TempDir, LocalFs) {
    let dir = tempfile::tempdir().unwrap();
    let uring = uring::UringExecutor::new(1, 64).unwrap();
    let pool = buffers::BufferPool::new(1 << 20, 8);
    let fs = LocalFs::new(dir.path(), policy, uring, pool).unwrap();
    (dir, fs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lbfs_proto::types::ROOT_NODE;

    #[tokio::test]
    async fn lookup_returns_entry_and_missing_returns_enoent() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        std::fs::write(dir.path().join("f"), b"abc").unwrap();
        let e = fs.lookup(ROOT_NODE, b"f").await.unwrap();
        assert_eq!(e.attr.size, 3);
        assert_eq!(e.attr.mode & libc::S_IFMT, libc::S_IFREG);
        assert_eq!(
            fs.lookup(ROOT_NODE, b"missing").await.unwrap_err(),
            Errno::ENOENT
        );
    }

    #[tokio::test]
    async fn lookup_rejects_dotdot_and_slashes() {
        let (_dir, fs) = test_fs(FsyncPolicy::Honor).await;
        assert!(fs.lookup(ROOT_NODE, b"..").await.is_err());
        assert!(fs.lookup(ROOT_NODE, b"a/b").await.is_err());
        assert!(fs.lookup(ROOT_NODE, b"").await.is_err());
    }

    #[tokio::test]
    async fn lookup_does_not_follow_symlinks_out_of_export() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        std::os::unix::fs::symlink("/etc", dir.path().join("escape")).unwrap();
        // The lookup itself succeeds and reports a symlink — the server hands
        // the target string to the client and never dereferences it.
        let e = fs.lookup(ROOT_NODE, b"escape").await.unwrap();
        assert_eq!(e.attr.mode & libc::S_IFMT, libc::S_IFLNK);
        assert_eq!(fs.readlink(e.node).await.unwrap(), b"/etc");
    }

    #[tokio::test]
    async fn namespace_ops_round_trip() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        let d = fs.mkdir(ROOT_NODE, b"sub", 0o755).await.unwrap();
        assert_eq!(d.attr.mode & libc::S_IFMT, libc::S_IFDIR);

        std::fs::write(dir.path().join("sub/x"), b"1").unwrap();
        let x = fs.lookup(d.node, b"x").await.unwrap();

        let l = fs.link(x.node, ROOT_NODE, b"x2").await.unwrap();
        assert_eq!(l.node, x.node); // hardlink dedup via (dev, ino)

        fs.rename(ROOT_NODE, b"x2", d.node, b"x3", 0).await.unwrap();
        fs.unlink(d.node, b"x3").await.unwrap();
        fs.unlink(d.node, b"x").await.unwrap();
        fs.rmdir(ROOT_NODE, b"sub").await.unwrap();
        assert_eq!(
            fs.lookup(ROOT_NODE, b"sub").await.unwrap_err(),
            Errno::ENOENT
        );
    }

    #[tokio::test]
    async fn setattr_chmod_truncate_times() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        std::fs::write(dir.path().join("f"), b"hello").unwrap();
        let e = fs.lookup(ROOT_NODE, b"f").await.unwrap();

        let a = fs
            .setattr(
                e.node,
                SetattrArgs {
                    mode: Some(0o600),
                    uid: None,
                    gid: None,
                    size: Some(2),
                    atime: TimeSet::Omit,
                    mtime: TimeSet::Set { sec: 1000, nsec: 0 },
                    fh: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(a.mode & 0o777, 0o600);
        assert_eq!(a.size, 2);
        assert_eq!(a.mtime_sec, 1000);
    }

    #[tokio::test]
    async fn stale_node_returns_estale() {
        let (_dir, fs) = test_fs(FsyncPolicy::Honor).await;
        assert_eq!(fs.getattr(999, None).await.unwrap_err(), Errno::ESTALE);
    }

    /// Timestamps must survive the truncate that `setattr` performs in the
    /// same call, and `TimeSet::Now` must not be mistaken for a literal zero.
    #[tokio::test]
    async fn setattr_chown_to_the_current_owner_and_atime_now() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        std::fs::write(dir.path().join("f"), b"hello").unwrap();
        let e = fs.lookup(ROOT_NODE, b"f").await.unwrap();
        // Changing the owner needs privilege; setting it to what it already is
        // is the unprivileged case and still runs the fchownat path.
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();

        let a = fs
            .setattr(
                e.node,
                SetattrArgs {
                    mode: None,
                    uid: Some(uid),
                    gid: Some(gid),
                    size: None,
                    atime: TimeSet::Now,
                    mtime: TimeSet::Omit,
                    fh: None,
                },
            )
            .await
            .unwrap();
        assert_eq!((a.uid, a.gid), (uid, gid));
        assert!(a.atime_sec > 0, "UTIME_NOW must stamp a real time");
    }

    /// Attach builds the root's `FileKey` from a synchronous `statx`, lookup
    /// builds a child's from the ring's. If those two ever disagreed the loop
    /// guard below could never fire and the root would fail to dedup against
    /// itself, so pin the agreement directly.
    #[tokio::test]
    async fn attach_and_lookup_agree_on_the_root_file_key() {
        let (_dir, fs) = test_fs(FsyncPolicy::Honor).await;
        assert_eq!(fs.root_key, fs.key_of_node_for_test(ROOT_NODE).await);
    }

    /// The directory-loop guard. The real shape is the export root
    /// bind-mounted inside itself, which needs root to build; forging the same
    /// `FileKey` collision by pointing `root_key` at a child reaches the same
    /// branch with the same inputs.
    #[tokio::test]
    async fn lookup_refuses_a_child_that_is_the_export_root() {
        let (dir, mut fs) = test_fs(FsyncPolicy::Honor).await;
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let sub = fs.lookup(ROOT_NODE, b"sub").await.unwrap();
        assert!(sub.node > ROOT_NODE);

        fs.root_key = fs.key_of_node_for_test(sub.node).await;
        assert_eq!(fs.lookup(ROOT_NODE, b"sub").await.unwrap_err(), ELOOP);
    }

    /// One `register` per `Entry`, so one `FORGET` retires each one and no
    /// node outlives the client's belief in it.
    #[tokio::test]
    async fn every_returned_entry_carries_exactly_one_lookup_count() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        std::fs::write(dir.path().join("f"), b"x").unwrap();

        for entry in [
            fs.lookup(ROOT_NODE, b"f").await.unwrap(),
            fs.mkdir(ROOT_NODE, b"sub", 0o755).await.unwrap(),
            fs.symlink(ROOT_NODE, b"lnk", b"f").await.unwrap(),
        ] {
            fs.forget(entry.node, 1).await;
            assert_eq!(
                fs.getattr(entry.node, None).await.unwrap_err(),
                Errno::ESTALE
            );
        }

        // `link` dedups onto the source inode's node, so the client holds two
        // entries for one id and owes two FORGETs.
        let a = fs.lookup(ROOT_NODE, b"f").await.unwrap();
        let b = fs.link(a.node, ROOT_NODE, b"f2").await.unwrap();
        assert_eq!(a.node, b.node);
        fs.forget(a.node, 1).await;
        assert!(fs.getattr(a.node, None).await.is_ok());
        fs.forget(a.node, 1).await;
        assert_eq!(fs.getattr(a.node, None).await.unwrap_err(), Errno::ESTALE);
    }

    #[tokio::test]
    async fn namespace_ops_reject_traversal_and_nul_names() {
        let (_dir, fs) = test_fs(FsyncPolicy::Honor).await;
        assert_eq!(
            fs.mkdir(ROOT_NODE, b"..", 0o755).await.unwrap_err(),
            Errno::EINVAL
        );
        assert_eq!(
            fs.unlink(ROOT_NODE, b"a/b").await.unwrap_err(),
            Errno::EINVAL
        );
        assert_eq!(fs.rmdir(ROOT_NODE, b".").await.unwrap_err(), Errno::EINVAL);
        assert_eq!(
            fs.symlink(ROOT_NODE, b"x\0y", b"t").await.unwrap_err(),
            Errno::EINVAL
        );
        assert_eq!(
            fs.rename(ROOT_NODE, b"a", ROOT_NODE, b"..", 0)
                .await
                .unwrap_err(),
            Errno::EINVAL
        );
        assert_eq!(
            fs.link(ROOT_NODE, ROOT_NODE, b"").await.unwrap_err(),
            Errno::EINVAL
        );
    }

    /// Unimplemented opcodes must say so rather than pretend to succeed.
    #[tokio::test]
    async fn unimplemented_opcodes_report_enosys() {
        let (_dir, fs) = test_fs(FsyncPolicy::Honor).await;
        assert_eq!(fs.open(ROOT_NODE, 0).await.unwrap_err(), Errno::ENOSYS);
        assert_eq!(fs.opendir(ROOT_NODE).await.unwrap_err(), Errno::ENOSYS);
        assert_eq!(fs.statfs(ROOT_NODE).await.unwrap_err(), Errno::ENOSYS);
    }
}
