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

/// Also absent from [`Errno`]'s list: a handle the client never opened, one it
/// already released, or one it is presenting against the wrong node.
const EBADF: Errno = Errno(libc::EBADF as u16);

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

/// One open file: the descriptor `OPEN`/`CREATE` produced, plus the node it
/// was opened on.
///
/// The node id is not bookkeeping. Nothing stops a client pairing any `Fh`
/// with any `NodeId`, and every operation here is descriptor-relative, so a
/// handle onto one file would otherwise read, truncate, or `copy_file_range`
/// into another — a handle the client legitimately owns becoming a key to a
/// file it never opened. [`LocalFs::file_fd`] is the only route to the
/// descriptor and it checks the pair.
#[derive(Clone)]
pub struct FileHandle {
    node: NodeId,
    fd: Arc<OwnedFd>,
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
    /// The read/write data path draws its buffers from here.
    pool: BufferPool,
    /// `OPEN`/`CREATE` park their data descriptors here, and `SETATTR` reads
    /// it so a truncate against an open handle uses that handle's descriptor.
    files: HandleTable<FileHandle>,
    /// Task 11: `OPENDIR` parks its directory snapshots here.
    #[allow(dead_code)]
    dirs: HandleTable<Arc<DirHandle>>,
    /// Masks `O_SYNC`/`O_DSYNC` and short-circuits `FSYNC` (spec §6).
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

    /// Resolves `(node, fh)` to the descriptor that handle owns.
    ///
    /// The pairing check is the whole point (see [`FileHandle`]): an `Fh` that
    /// is unknown, already released, or presented against a different node is
    /// `EBADF`, never somebody else's file.
    fn file_fd(&self, node: NodeId, fh: Fh) -> FsResult<Arc<OwnedFd>> {
        match self.files.get(fh) {
            Some(handle) if handle.node == node => Ok(handle.fd),
            _ => Err(EBADF),
        }
    }

    /// Reduces a client's `open` flags to the ones this server will honor.
    ///
    /// An allowlist, not a denylist. `flags` is a raw `u32` the client copied
    /// out of its own kernel's `f_flags`, and the bits that matter here are the
    /// ones nobody thought to name: `O_TMPFILE` (which carries `O_DIRECTORY`),
    /// `O_PATH`, `O_CREAT`. Enumerating what is safe is the only form of this
    /// function that stays correct as flags are added.
    ///
    /// What survives, and why:
    ///
    /// * the access mode, which is the request;
    /// * `O_APPEND`, `O_NOATIME`, `O_NONBLOCK`, which describe how the client
    ///   wants its own descriptor to behave and cost the server nothing;
    /// * `O_SYNC`/`O_DSYNC`, unless the durability policy is `ignore`, whose
    ///   whole purpose is to not pay for them (spec §6).
    ///
    /// What does not, beyond the unnamed rest: `O_CREAT`, `O_EXCL` and
    /// `O_TRUNC` (FUSE has `CREATE` and `SETATTR` for those, and honoring them
    /// on `OPEN` would let a plain open create or destroy data), `O_NOFOLLOW`
    /// and `O_DIRECTORY` (meaningless — the node is already resolved and
    /// reopened through `/proc`), and `O_DIRECT`, which v1 does not support:
    /// pooled buffers carry no alignment guarantee, so every read and write
    /// against such a descriptor would fail `EINVAL`.
    fn mask_open_flags(&self, flags: u32) -> i32 {
        const ALLOWED: i32 = libc::O_ACCMODE
            | libc::O_APPEND
            | libc::O_NOATIME
            | libc::O_NONBLOCK
            // O_SYNC's value already contains O_DSYNC's bit; both are named so
            // the intent survives someone reading only one line.
            | libc::O_SYNC
            | libc::O_DSYNC;

        let mut flags = (flags as i32) & ALLOWED;
        if self.fsync_policy == FsyncPolicy::Ignore {
            flags &= !(libc::O_SYNC | libc::O_DSYNC);
        }
        // Descriptors are the server's, never a child's.
        flags | libc::O_CLOEXEC
    }

    /// The durability policy in one place (spec §6).
    ///
    /// `honor` runs the real `fsync`/`fdatasync`; `ignore` acknowledges without
    /// touching disk, the same trade an NFS `async` export makes — latency for
    /// crash durability. Task 11's `FSYNCDIR` joins here once a directory
    /// handle owns a descriptor.
    async fn maybe_fsync(&self, fd: &Arc<OwnedFd>, datasync: bool) -> FsResult<()> {
        match self.fsync_policy {
            FsyncPolicy::Honor => self.uring.fsync(fd, datasync).await.map_err(errno),
            FsyncPolicy::Ignore => Ok(()),
        }
    }

    #[cfg(test)]
    async fn key_of_node_for_test(&self, node: NodeId) -> FileKey {
        let fd = self.node_fd(node).unwrap();
        file_key(&self.statx_fd(&fd).await.unwrap())
    }

    #[cfg(test)]
    fn pool_for_test(&self) -> &BufferPool {
        &self.pool
    }

    /// The flags the stored descriptor was actually opened with, straight from
    /// the kernel — the only honest witness that [`LocalFs::mask_open_flags`]
    /// did what it claims.
    #[cfg(test)]
    fn file_flags_for_test(&self, fh: Fh) -> i32 {
        let handle = self.files.get(fh).expect("handle is open");
        rustix::fs::fcntl_getfl(&*handle.fd).unwrap().bits() as i32
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

/// Move the tail a short write did not consume to the front of the buffer.
///
/// The executor writes from the start of the buffer and takes no offset into
/// it, so this is what lets the next attempt continue where the last one
/// stopped. `done` bytes of the leading `len` went out; what is left is
/// `buf[done..len]`, and afterwards it is `buf[..len - done]`.
///
/// The alternative is a wider executor API carrying a buffer offset into the
/// slab; a memmove on the rare short write is the cheaper half of that trade.
fn slide_unwritten(buf: &mut PooledBuf, done: usize, len: usize) {
    buf.as_mut_slice().copy_within(done..len, 0);
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

/// One [`TimeSet`] as the kernel wants it.
///
/// `Set` is range-checked because `UTIME_NOW` and `UTIME_OMIT` are themselves
/// nanosecond values just below 2^30: an out-of-range `nsec` off the wire would
/// not be rejected by `utimensat`, it would quietly mean "now" or "leave it
/// alone" instead of the timestamp the client asked for.
fn timespec(t: TimeSet) -> FsResult<rustix::fs::Timespec> {
    Ok(match t {
        TimeSet::Omit => rustix::fs::Timespec {
            tv_sec: 0,
            tv_nsec: rustix::fs::UTIME_OMIT,
        },
        TimeSet::Now => rustix::fs::Timespec {
            tv_sec: 0,
            tv_nsec: rustix::fs::UTIME_NOW,
        },
        TimeSet::Set { sec, nsec } => {
            if nsec >= 1_000_000_000 {
                return Err(Errno::EINVAL);
            }
            rustix::fs::Timespec {
                tv_sec: sec,
                tv_nsec: nsec.into(),
            }
        }
    })
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
///
/// The one thing that happens out of order is validating the timestamps, which
/// runs before the first mutation: `SETATTR` is not atomic, so a request that
/// is going to be refused should be refused before it has half-applied.
fn apply_setattr(
    fd: &OwnedFd,
    write_fd: Option<&OwnedFd>,
    args: &SetattrArgs,
) -> Result<(), Errno> {
    let times = match (args.atime, args.mtime) {
        (TimeSet::Omit, TimeSet::Omit) => None,
        (atime, mtime) => Some(rustix::fs::Timestamps {
            last_access: timespec(atime)?,
            last_modification: timespec(mtime)?,
        }),
    };
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
    if let Some(times) = times {
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
        // O_PATH reopen would not get back. An `fh` that does not belong to
        // `node` is refused rather than ignored - silently falling back to the
        // node's own descriptor would truncate the right file for the wrong
        // reason, and hide the client's bug until it aims at a file it cannot
        // otherwise reach.
        let write_fd = match args.fh {
            Some(fh) => Some(self.file_fd(node, fh)?),
            None => None,
        };
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

    // --- File I/O ----------------------------------------------------------

    async fn open(&self, node: NodeId, flags: u32) -> FsResult<Fh> {
        let fd = self.node_fd(node)?;
        let flags = self.mask_open_flags(flags);
        // `reopen` is a blocking `open(2)`. On a regular file that is a fast
        // path, but a node can be a FIFO, where an open without O_NONBLOCK
        // waits for a peer - unbounded, on a runtime worker.
        let opened = tokio::task::spawn_blocking(move || {
            // Lossless: every bit `mask_open_flags` can return is one rustix
            // knows, so nothing is silently dropped here.
            reopen(&fd, rustix::fs::OFlags::from_bits_truncate(flags as u32))
        })
        .await
        .map_err(join_errno)?
        .map_err(errno)?;
        Ok(self.files.insert(FileHandle {
            node,
            fd: Arc::new(opened),
        }))
    }

    async fn create(
        &self,
        parent: NodeId,
        name: &[u8],
        mode: u32,
        flags: u32,
    ) -> FsResult<(Entry, Fh)> {
        let name = valid_name(name)?;
        let parent_fd = self.node_fd(parent)?;
        // O_CREAT is ours to add; O_EXCL and O_TRUNC are the client's to ask
        // for and are the two creation flags that mean something here - drop
        // O_EXCL and an exclusive create silently opens somebody else's file.
        // O_NOFOLLOW because the client's own kernel resolved the path: it only
        // asks to create a name it believes is negative, so a symlink in that
        // position is a race, and failing it is better than writing through it.
        let creation =
            libc::O_CREAT | libc::O_NOFOLLOW | ((flags as i32) & (libc::O_EXCL | libc::O_TRUNC));
        let how = io_uring::types::OpenHow::new()
            .flags((self.mask_open_flags(flags) | creation) as u64)
            .mode(u64::from(mode & 0o7777))
            .resolve(libc::RESOLVE_BENEATH | libc::RESOLVE_NO_MAGICLINKS);
        let data = Arc::new(
            self.uring
                .openat2(&parent_fd, name.clone(), how)
                .await
                .map_err(errno)?,
        );
        // Through `lookup_impl`, so `CREATE` inherits the loop guard and the
        // single `register` that pairs this entry with exactly one FORGET. The
        // cost is that the node is resolved by name a second time, so a rename
        // racing between the two steps yields an entry for a different file
        // than the handle - the same window `libfuse`'s passthrough has, and
        // one RESOLVE_BENEATH still confines to the export.
        let entry = self.lookup_impl(&parent_fd, &name).await?;
        let fh = self.files.insert(FileHandle {
            node: entry.node,
            fd: data,
        });
        Ok((entry, fh))
    }

    async fn read(&self, node: NodeId, fh: Fh, offset: u64, size: u32) -> FsResult<PooledBuf> {
        let fd = self.file_fd(node, fh)?;
        let buf = self.pool.get();
        // Pool buffers are sized to the negotiated max_io_size, so an
        // over-large ask is clamped rather than allocated for. The executor
        // asserts on a length past capacity, which would be a panic on the
        // ring thread.
        let cap = u32::try_from(buf.capacity()).unwrap_or(u32::MAX);
        let (buf, res) = self.uring.read(&fd, offset, buf, size.min(cap)).await;
        match res {
            // A short read at EOF is not an error, and the executor has already
            // published the transferred count as the buffer's length - the
            // recycled tail beyond it is not readable.
            Ok(_) => Ok(buf),
            Err(e) => Err(errno(e)),
        }
    }

    async fn write(
        &self,
        node: NodeId,
        fh: Fh,
        offset: u64,
        data: PooledBuf,
        len: u32,
    ) -> FsResult<u32> {
        let fd = self.file_fd(node, fh)?;
        // `len` is the caller's claim about how much of `data` is real; the
        // buffer's capacity is the only bound this layer can enforce.
        let len = len.min(u32::try_from(data.capacity()).unwrap_or(u32::MAX));
        let mut buf = data;
        let mut written = 0u32;
        while written < len {
            let remaining = len - written;
            // Saturating because the alternative is a debug-build panic on a
            // client-supplied number: an offset that far out fails in the
            // kernel either way, and a panic here would take the connection.
            let at = offset.saturating_add(u64::from(written));
            let (mut returned, res) = self.uring.write(&fd, at, buf, remaining).await;
            let n = match res {
                Ok(n) => n,
                // POSIX write semantics: bytes already on the file happened, so
                // report them. The client retries the tail and gets the real
                // errno then.
                Err(_) if written > 0 => return Ok(written),
                Err(e) => return Err(errno(e)),
            };
            written += n;
            if n == 0 {
                // No progress and no error. Nothing here can make the next
                // attempt differ, so stop rather than spin.
                break;
            }
            if written < len {
                slide_unwritten(&mut returned, n as usize, remaining as usize);
            }
            buf = returned;
        }
        Ok(written)
    }

    async fn flush(&self, node: NodeId, fh: Fh) -> FsResult<()> {
        // Nothing is buffered server-side - a WRITE reply already means the
        // bytes reached the page cache - so FLUSH only has to answer for the
        // handle. Durability is FSYNC's job, and stays FSYNC's job even when a
        // client closes a file it never synced.
        self.file_fd(node, fh)?;
        Ok(())
    }

    async fn release(&self, node: NodeId, fh: Fh) -> FsResult<()> {
        match self.files.get(fh) {
            Some(handle) if handle.node != node => return Err(EBADF),
            // A RELEASE whose reply was lost gets retried; the second one has
            // nothing to close and is not an error.
            None => return Ok(()),
            Some(_) => {}
        }
        if let Some(handle) = self.files.remove(fh) {
            // The last `close(2)` on an unlinked file frees its blocks inline -
            // journal work on ext4, tens of milliseconds for a large file - so
            // it does not run on a runtime worker. In-flight ring operations
            // hold their own reference, so this closes when they are done, not
            // under them.
            tokio::task::spawn_blocking(move || drop(handle))
                .await
                .map_err(join_errno)?;
        }
        Ok(())
    }

    async fn fsync(&self, node: NodeId, fh: Fh, datasync: bool) -> FsResult<()> {
        let fd = self.file_fd(node, fh)?;
        self.maybe_fsync(&fd, datasync).await
    }

    async fn fallocate(
        &self,
        node: NodeId,
        fh: Fh,
        offset: u64,
        length: u64,
        mode: u32,
    ) -> FsResult<()> {
        let fd = self.file_fd(node, fh)?;
        // `mode` (FALLOC_FL_KEEP_SIZE, PUNCH_HOLE, ...) goes to the kernel as
        // it arrived: it validates the combinations, and a filesystem that does
        // not support one answers EOPNOTSUPP, which is the client's answer too.
        self.uring
            .fallocate(&fd, mode as i32, offset, length)
            .await
            .map_err(errno)
    }

    async fn lseek(&self, node: NodeId, fh: Fh, offset: u64, whence: u32) -> FsResult<u64> {
        let fd = self.file_fd(node, fh)?;
        // The wire carries the offset unsigned; for the whences that take a
        // relative displacement it is an `off_t` that was cast, so cast it back
        // rather than rejecting the top half of the range.
        let pos = match whence as i32 {
            libc::SEEK_SET => rustix::fs::SeekFrom::Start(offset),
            libc::SEEK_CUR => rustix::fs::SeekFrom::Current(offset as i64),
            libc::SEEK_END => rustix::fs::SeekFrom::End(offset as i64),
            libc::SEEK_DATA => rustix::fs::SeekFrom::Data(offset),
            libc::SEEK_HOLE => rustix::fs::SeekFrom::Hole(offset),
            _ => return Err(Errno::EINVAL),
        };
        // No io_uring opcode for lseek (spec §5.3). Each handle owns its own
        // open file description, so moving this one's offset cannot disturb
        // another client's reads.
        tokio::task::spawn_blocking(move || rustix::fs::seek(&*fd, pos).map_err(rustix_errno))
            .await
            .map_err(join_errno)?
    }

    async fn copy_file_range(
        &self,
        node_in: NodeId,
        fh_in: Fh,
        off_in: u64,
        node_out: NodeId,
        fh_out: Fh,
        off_out: u64,
        len: u64,
    ) -> FsResult<u64> {
        let fd_in = self.file_fd(node_in, fh_in)?;
        let fd_out = self.file_fd(node_out, fh_out)?;
        // No io_uring opcode either, and this is the op most worth having: the
        // bytes never leave the server, so a reflink-capable filesystem can
        // make the whole copy metadata.
        tokio::task::spawn_blocking(move || {
            let (mut off_in, mut off_out) = (off_in, off_out);
            let mut copied = 0u64;
            while copied < len {
                // A single call is capped so the length is a `usize` on any
                // target and the kernel's own clamp never surprises us.
                let want = (len - copied).min(1 << 30) as usize;
                match rustix::fs::copy_file_range(
                    &*fd_in,
                    Some(&mut off_in),
                    &*fd_out,
                    Some(&mut off_out),
                    want,
                ) {
                    // Source EOF. The short count is the answer, not an error.
                    Ok(0) => break,
                    Ok(n) => copied += n as u64,
                    Err(rustix::io::Errno::INTR) => continue,
                    // Same rule as `write`: progress already made is reported,
                    // and the client learns the errno when it retries the tail.
                    Err(_) if copied > 0 => break,
                    Err(e) => return Err(rustix_errno(e)),
                }
            }
            Ok(copied)
        })
        .await
        .map_err(join_errno)?
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

    // --- Task 10: file I/O -------------------------------------------------

    /// The process umask, read rather than probed by setting it: `umask(2)` is
    /// process-wide and these tests run as parallel threads of one process, so
    /// a set-and-restore probe would corrupt whatever the neighbours are doing.
    /// The server clears its own umask at startup (Task 12); a test process
    /// keeps whatever it inherited, so an expected mode has to be masked.
    fn process_umask() -> u32 {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let field = status
            .lines()
            .find_map(|l| l.strip_prefix("Umask:"))
            .expect("/proc/self/status reports Umask");
        u32::from_str_radix(field.trim(), 8).unwrap()
    }

    #[tokio::test]
    async fn create_write_read_release() {
        let (_dir, fs) = test_fs(FsyncPolicy::Honor).await;
        let (entry, fh) = fs
            .create(ROOT_NODE, b"f", 0o644, libc::O_RDWR as u32)
            .await
            .unwrap();

        let mut buf = fs.pool_for_test().get();
        buf.as_mut_slice()[..5].copy_from_slice(b"hello");
        assert_eq!(fs.write(entry.node, fh, 0, buf, 5).await.unwrap(), 5);

        let out = fs.read(entry.node, fh, 1, 3).await.unwrap();
        assert_eq!(out.as_slice(), b"ell");

        // Short read at EOF, not an error.
        let out = fs.read(entry.node, fh, 3, 100).await.unwrap();
        assert_eq!(out.as_slice(), b"lo");

        fs.flush(entry.node, fh).await.unwrap();
        fs.fsync(entry.node, fh, true).await.unwrap();
        fs.release(entry.node, fh).await.unwrap();
        assert_eq!(fs.read(entry.node, fh, 0, 1).await.err(), Some(EBADF));
    }

    /// `CREATE` owes the client exactly one `FORGET` for the entry it returns,
    /// which only holds while it registers through `lookup_impl`.
    #[tokio::test]
    async fn create_registers_exactly_one_lookup_count() {
        let (_dir, fs) = test_fs(FsyncPolicy::Honor).await;
        let (entry, fh) = fs
            .create(ROOT_NODE, b"f", 0o644, libc::O_RDWR as u32)
            .await
            .unwrap();
        assert!(fs.getattr(entry.node, None).await.is_ok());
        fs.forget(entry.node, 1).await;
        assert_eq!(
            fs.getattr(entry.node, None).await.unwrap_err(),
            Errno::ESTALE
        );
        // The handle outlives the node, which is safe only because node ids are
        // never reused: the handle's own descriptor keeps the file open, and
        // the id it remembers can never come to mean a different file.
        assert!(fs.read(entry.node, fh, 0, 1).await.is_ok());
        fs.release(entry.node, fh).await.unwrap();
    }

    #[tokio::test]
    async fn create_applies_the_mode_through_the_process_umask() {
        let (_dir, fs) = test_fs(FsyncPolicy::Honor).await;
        let (entry, _fh) = fs
            .create(ROOT_NODE, b"f", 0o666, libc::O_RDWR as u32)
            .await
            .unwrap();
        assert_eq!(entry.attr.mode & libc::S_IFMT, libc::S_IFREG);
        assert_eq!(entry.attr.mode & 0o7777, 0o666 & !process_umask());
    }

    /// The two creation flags that do mean something on `CREATE` are the
    /// client's to set. Dropping `O_EXCL` would turn an exclusive create into
    /// an ordinary open of a file somebody else owns — the failure mode every
    /// lock file rests on not happening.
    #[tokio::test]
    async fn create_honors_o_excl_o_trunc_and_refuses_a_symlink() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        std::fs::write(dir.path().join("f"), b"hello").unwrap();

        assert_eq!(
            fs.create(ROOT_NODE, b"f", 0o644, (libc::O_RDWR | libc::O_EXCL) as u32)
                .await
                .unwrap_err(),
            Errno::EEXIST
        );

        // Without O_TRUNC the contents survive; with it they do not.
        let (e, _) = fs
            .create(ROOT_NODE, b"f", 0o644, libc::O_RDWR as u32)
            .await
            .unwrap();
        assert_eq!(e.attr.size, 5);
        let (e, _) = fs
            .create(
                ROOT_NODE,
                b"f",
                0o644,
                (libc::O_RDWR | libc::O_TRUNC) as u32,
            )
            .await
            .unwrap();
        assert_eq!(e.attr.size, 0);

        // A name that turned into a symlink between the client's lookup and
        // this create is a race, and O_NOFOLLOW makes it fail rather than
        // write through the link.
        std::os::unix::fs::symlink("target", dir.path().join("l")).unwrap();
        assert_eq!(
            fs.create(ROOT_NODE, b"l", 0o644, libc::O_RDWR as u32)
                .await
                .unwrap_err(),
            ELOOP
        );
    }

    /// `CREATE` is the only op that may create; `OPEN` never does, and never
    /// truncates either — FUSE sends `SETATTR` for that.
    #[tokio::test]
    async fn open_strips_creation_and_direct_flags() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        std::fs::write(dir.path().join("f"), b"hello").unwrap();
        let e = fs.lookup(ROOT_NODE, b"f").await.unwrap();

        let fh = fs
            .open(
                e.node,
                (libc::O_WRONLY | libc::O_TRUNC | libc::O_CREAT | libc::O_DIRECT) as u32,
            )
            .await
            .unwrap();
        assert_eq!(
            fs.getattr(e.node, Some(fh)).await.unwrap().size,
            5,
            "O_TRUNC from OPEN must not truncate"
        );
        let flags = fs.file_flags_for_test(fh);
        assert_eq!(flags & libc::O_DIRECT, 0, "O_DIRECT is not supported in v1");
        assert_eq!(flags & libc::O_ACCMODE, libc::O_WRONLY);

        // A missing name still cannot be conjured: OPEN takes a node, and an
        // unknown one is ESTALE rather than a fresh file.
        assert_eq!(
            fs.open(9999, libc::O_CREAT as u32).await.unwrap_err(),
            Errno::ESTALE
        );
    }

    #[tokio::test]
    async fn fsync_ignore_masks_osync_open_flags() {
        let (dir, fs) = test_fs(FsyncPolicy::Ignore).await;
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        let e = fs.lookup(ROOT_NODE, b"f").await.unwrap();
        let fh = fs
            .open(e.node, (libc::O_WRONLY | libc::O_SYNC) as u32)
            .await
            .unwrap();
        let real = fs.file_flags_for_test(fh); // fcntl(F_GETFL) on the stored fd
        assert_eq!(
            real & libc::O_SYNC,
            0,
            "O_SYNC must be masked under fsync=ignore"
        );
        fs.fsync(e.node, fh, false).await.unwrap(); // acked without touching disk
    }

    /// The counterpart: `honor` is the default and must leave a sync-opened
    /// file sync.
    #[tokio::test]
    async fn fsync_honor_keeps_osync_open_flags() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        let e = fs.lookup(ROOT_NODE, b"f").await.unwrap();
        let fh = fs
            .open(e.node, (libc::O_WRONLY | libc::O_SYNC) as u32)
            .await
            .unwrap();
        assert_eq!(
            fs.file_flags_for_test(fh) & libc::O_SYNC,
            libc::O_SYNC,
            "fsync=honor must keep the client's O_SYNC"
        );
        fs.fsync(e.node, fh, false).await.unwrap();
    }

    /// Whether `honor` really reaches the kernel is otherwise invisible from
    /// userspace — a successful `fsync` and a skipped one look identical. A
    /// FIFO is the witness: it is the file type that answers `fsync` with
    /// `EINVAL`, so the two policies give visibly different answers on it.
    #[tokio::test]
    async fn the_fsync_policy_decides_whether_the_syscall_happens() {
        for (policy, expected) in [
            (FsyncPolicy::Honor, Err(Errno(libc::EINVAL as u16))),
            (FsyncPolicy::Ignore, Ok(())),
        ] {
            let (dir, fs) = test_fs(policy).await;
            rustix::fs::mknodat(
                rustix::fs::CWD,
                dir.path().join("p"),
                rustix::fs::FileType::Fifo,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
                0,
            )
            .unwrap();
            let e = fs.lookup(ROOT_NODE, b"p").await.unwrap();
            // O_NONBLOCK, or opening the read end waits for a writer.
            let fh = fs
                .open(e.node, (libc::O_RDONLY | libc::O_NONBLOCK) as u32)
                .await
                .unwrap();
            assert_eq!(fs.fsync(e.node, fh, false).await, expected, "{policy:?}");
        }
    }

    #[tokio::test]
    async fn lseek_finds_data_and_holes() {
        let (_dir, fs) = test_fs(FsyncPolicy::Honor).await;
        let (e, fh) = fs
            .create(ROOT_NODE, b"sparse", 0o644, libc::O_RDWR as u32)
            .await
            .unwrap();
        let mut buf = fs.pool_for_test().get();
        buf.as_mut_slice()[..1].copy_from_slice(b"x");
        fs.write(e.node, fh, 1 << 20, buf, 1).await.unwrap(); // data at 1 MiB
        let data_at = fs
            .lseek(e.node, fh, 0, libc::SEEK_DATA as u32)
            .await
            .unwrap();
        assert!(data_at >= 4096, "leading hole expected, got {data_at}");

        assert_eq!(
            fs.lseek(e.node, fh, 0, libc::SEEK_END as u32)
                .await
                .unwrap(),
            (1 << 20) + 1
        );
        assert_eq!(
            fs.lseek(e.node, fh, 0, 999).await.unwrap_err(),
            Errno::EINVAL
        );
    }

    #[tokio::test]
    async fn copy_file_range_copies_server_side() {
        let (_dir, fs) = test_fs(FsyncPolicy::Honor).await;
        let (a, fha) = fs
            .create(ROOT_NODE, b"a", 0o644, libc::O_RDWR as u32)
            .await
            .unwrap();
        let mut buf = fs.pool_for_test().get();
        buf.as_mut_slice()[..4].copy_from_slice(b"data");
        fs.write(a.node, fha, 0, buf, 4).await.unwrap();
        let (b, fhb) = fs
            .create(ROOT_NODE, b"b", 0o644, libc::O_RDWR as u32)
            .await
            .unwrap();
        let n = fs
            .copy_file_range(a.node, fha, 0, b.node, fhb, 0, 4)
            .await
            .unwrap();
        assert_eq!(n, 4);
        let out = fs.read(b.node, fhb, 0, 4).await.unwrap();
        assert_eq!(out.as_slice(), b"data");
    }

    #[tokio::test]
    async fn fallocate_extends_the_file() {
        let (_dir, fs) = test_fs(FsyncPolicy::Honor).await;
        let (e, fh) = fs
            .create(ROOT_NODE, b"f", 0o644, libc::O_RDWR as u32)
            .await
            .unwrap();
        fs.fallocate(e.node, fh, 0, 4096, 0).await.unwrap();
        assert_eq!(fs.getattr(e.node, Some(fh)).await.unwrap().size, 4096);
    }

    /// A read larger than one pooled buffer is clamped, not allocated for and
    /// not passed to the ring — the executor asserts on an over-long read.
    #[tokio::test]
    async fn read_clamps_the_request_to_the_pool_buffer() {
        let (_dir, fs) = test_fs(FsyncPolicy::Honor).await;
        let (e, fh) = fs
            .create(ROOT_NODE, b"f", 0o644, libc::O_RDWR as u32)
            .await
            .unwrap();
        let mut buf = fs.pool_for_test().get();
        buf.as_mut_slice()[..2].copy_from_slice(b"hi");
        fs.write(e.node, fh, 0, buf, 2).await.unwrap();
        let out = fs.read(e.node, fh, 0, u32::MAX).await.unwrap();
        assert_eq!(out.as_slice(), b"hi");
    }

    /// A handle is only usable against the node it was opened on. Nothing stops
    /// a client sending any `Fh` with any `NodeId`, and every op here is
    /// descriptor-relative, so without the pairing check a handle onto one file
    /// would read, truncate, and `copy_file_range` into another.
    #[tokio::test]
    async fn a_handle_is_bound_to_the_node_it_was_opened_on() {
        let (_dir, fs) = test_fs(FsyncPolicy::Honor).await;
        let (a, fha) = fs
            .create(ROOT_NODE, b"a", 0o644, libc::O_RDWR as u32)
            .await
            .unwrap();
        let (b, fhb) = fs
            .create(ROOT_NODE, b"b", 0o644, libc::O_RDWR as u32)
            .await
            .unwrap();
        assert_ne!(a.node, b.node);

        assert_eq!(fs.read(b.node, fha, 0, 1).await.err(), Some(EBADF));
        let buf = fs.pool_for_test().get();
        assert_eq!(fs.write(b.node, fha, 0, buf, 1).await.unwrap_err(), EBADF);
        assert_eq!(fs.flush(b.node, fha).await.unwrap_err(), EBADF);
        assert_eq!(fs.fsync(b.node, fha, false).await.unwrap_err(), EBADF);
        assert_eq!(fs.fallocate(b.node, fha, 0, 1, 0).await.unwrap_err(), EBADF);
        assert_eq!(fs.lseek(b.node, fha, 0, 0).await.unwrap_err(), EBADF);
        assert_eq!(
            fs.copy_file_range(b.node, fha, 0, b.node, fhb, 0, 1)
                .await
                .unwrap_err(),
            EBADF
        );
        assert_eq!(fs.release(b.node, fha).await.unwrap_err(), EBADF);
        // Rejected, not consumed: the handle still works on its own node.
        fs.release(a.node, fha).await.unwrap();
        // Releasing twice is not an error - a client that retried a RELEASE
        // whose reply it lost must not see a failure.
        fs.release(a.node, fha).await.unwrap();
    }

    /// `SETATTR` takes an `fh` too, and a truncate through it writes to
    /// whatever descriptor it names.
    #[tokio::test]
    async fn setattr_rejects_a_handle_from_another_node() {
        let (_dir, fs) = test_fs(FsyncPolicy::Honor).await;
        let (a, fha) = fs
            .create(ROOT_NODE, b"a", 0o644, libc::O_RDWR as u32)
            .await
            .unwrap();
        let (b, _fhb) = fs
            .create(ROOT_NODE, b"b", 0o644, libc::O_RDWR as u32)
            .await
            .unwrap();
        let mut buf = fs.pool_for_test().get();
        buf.as_mut_slice()[..4].copy_from_slice(b"keep");
        fs.write(a.node, fha, 0, buf, 4).await.unwrap();

        let truncate = |fh| SetattrArgs {
            mode: None,
            uid: None,
            gid: None,
            size: Some(0),
            atime: TimeSet::Omit,
            mtime: TimeSet::Omit,
            fh,
        };
        assert_eq!(
            fs.setattr(b.node, truncate(Some(fha))).await.unwrap_err(),
            EBADF
        );
        assert_eq!(
            fs.setattr(b.node, truncate(Some(9999))).await.unwrap_err(),
            EBADF
        );
        assert_eq!(fs.getattr(a.node, None).await.unwrap().size, 4);
        // The matching pair still truncates through the open handle.
        fs.setattr(a.node, truncate(Some(fha))).await.unwrap();
        assert_eq!(fs.getattr(a.node, None).await.unwrap().size, 0);
    }

    /// `UTIME_NOW` and `UTIME_OMIT` are themselves nanosecond values, just
    /// under 2^30. An `nsec` off the wire landing on one of them is the case
    /// that matters: `utimensat` would accept it and quietly do something other
    /// than what the client asked, where an ordinary out-of-range value is
    /// merely refused twice.
    #[tokio::test]
    async fn setattr_rejects_an_out_of_range_nsec() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        let e = fs.lookup(ROOT_NODE, b"f").await.unwrap();
        let stamp = |sec, nsec| SetattrArgs {
            mode: None,
            uid: None,
            gid: None,
            size: None,
            atime: TimeSet::Omit,
            mtime: TimeSet::Set { sec, nsec },
            fh: None,
        };
        // A known-good timestamp first, so "nothing was applied" below is a
        // statement about this call rather than about the file's history.
        fs.setattr(e.node, stamp(1000, 0)).await.unwrap();

        for sentinel in [
            rustix::fs::UTIME_NOW as u32,
            rustix::fs::UTIME_OMIT as u32,
            1_000_000_000,
        ] {
            assert_eq!(
                fs.setattr(e.node, stamp(1, sentinel)).await.unwrap_err(),
                Errno::EINVAL,
                "nsec {sentinel} must be refused"
            );
            assert_eq!(
                fs.getattr(e.node, None).await.unwrap().mtime_sec,
                1000,
                "a refused SETATTR must not have stamped anything"
            );
        }
    }

    /// The one piece of `write`'s short-write loop that arithmetic can get
    /// wrong. A real short write to a regular file needs a full disk or a
    /// signal to provoke, so pin the slide directly instead.
    #[test]
    fn a_short_write_slides_its_unwritten_tail_to_the_front() {
        let pool = BufferPool::new(16, 1);
        let mut buf = pool.get();
        buf.as_mut_slice()[..6].copy_from_slice(b"abcdef");

        slide_unwritten(&mut buf, 2, 6); // "ab" went out, 4 left
        assert_eq!(&buf.as_mut_ref_for_test()[..4], b"cdef");
        slide_unwritten(&mut buf, 1, 4); // "c" went out, 3 left
        assert_eq!(&buf.as_mut_ref_for_test()[..3], b"def");
        slide_unwritten(&mut buf, 3, 3); // all of it, nothing to move
        assert_eq!(&buf.as_mut_ref_for_test()[..3], b"def");
    }

    /// [`into_owned`]'s fallback: the ring thread sends its completion before
    /// dropping its own clone, so a caller can find the refcount still at two.
    /// The duplicate must name the same file.
    #[test]
    fn into_owned_duplicates_a_still_shared_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        let fd = Arc::new(
            rustix::fs::open(
                dir.path().join("f"),
                rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .unwrap(),
        );
        let still_shared = Arc::clone(&fd);
        let owned = into_owned(fd).unwrap();

        assert_ne!(
            owned.as_raw_fd(),
            still_shared.as_raw_fd(),
            "the shared case must dup, not steal"
        );
        let a = rustix::fs::fstat(&owned).unwrap();
        let b = rustix::fs::fstat(&*still_shared).unwrap();
        assert_eq!((a.st_dev, a.st_ino), (b.st_dev, b.st_ino));
    }

    /// Unimplemented opcodes must say so rather than pretend to succeed.
    #[tokio::test]
    async fn unimplemented_opcodes_report_enosys() {
        let (_dir, fs) = test_fs(FsyncPolicy::Honor).await;
        assert_eq!(fs.opendir(ROOT_NODE).await.unwrap_err(), Errno::ENOSYS);
        assert_eq!(fs.statfs(ROOT_NODE).await.unwrap_err(), Errno::ENOSYS);
    }
}
