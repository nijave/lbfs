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
use lbfs_proto::types::{
    DirEntry, DirEntryPlus, Entry, Fh, FileAttr, FileKind, NodeId, SetattrArgs, StatfsReply,
    TimeSet,
};
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

/// What an operation this backend declines to perform on this file type
/// answers, matching what the underlying syscall would have said. v1 uses it
/// for xattrs on anything that is not a regular file or a directory.
const EOPNOTSUPP: Errno = Errno(libc::EOPNOTSUPP as u16);

/// `setxattr(2)`'s own answer to a value past the ceiling.
const E2BIG: Errno = Errno(libc::E2BIG as u16);

/// `XATTR_SIZE_MAX`: the kernel's ceiling on one xattr value, and also the
/// bound spec §3.2 puts on the frame body that would have to carry it.
const MAX_XATTR_SIZE: usize = 65536;

/// `XATTR_LIST_MAX`: the same ceiling for a whole name list.
const MAX_XATTR_LIST: usize = 65536;

/// Bytes one [`DirEntry`] costs on the wire beyond its name: postcard writes a
/// length prefix (2 bytes at `NAME_MAX`), a one-byte [`FileKind`] tag, and two
/// varint u64s — the inode and the cursor — at 10 bytes each. An upper bound,
/// deliberately: a page that overshoots the client's budget is a protocol
/// violation, one that undershoots is only a wasted round trip.
const READDIR_ENTRY_OVERHEAD: usize = 32;

/// The same for [`DirEntryPlus`], which carries a whole [`Entry`]: two varint
/// u64s plus fifteen [`FileAttr`] fields, none wider than a 10-byte varint.
const READDIRPLUS_ENTRY_OVERHEAD: usize = 160;

/// The reply's own framing: the entry-count prefix and the `end` flag.
const READDIR_REPLY_OVERHEAD: usize = 8;

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

/// One name from the `OPENDIR` snapshot.
struct SnapshotEntry {
    name: Vec<u8>,
    /// `getdents64`'s `d_ino`, kept because nothing downstream can reconstruct
    /// it: `READDIR` returns no attributes, and glibc drops a dirent whose
    /// inode is zero.
    ino: u64,
    kind: FileKind,
    /// The `d_off` `getdents64` reported for this entry: the kernel's own
    /// cursor to the position *after* it. This is what a client passes back to
    /// resume, so it must be the kernel's number rather than an array index —
    /// see [`DirHandle::resume_at`].
    cookie: u64,
}

/// Snapshot of one open directory.
///
/// `OPENDIR` reads the whole listing in a single `getdents64` sweep and serves
/// every later `READDIR`/`READDIRPLUS` page out of it. POSIX permits snapshot
/// semantics for a directory mutated during a walk, and it buys two things
/// worth more than freshness here: a resume cursor that means the same thing
/// for the handle's whole life, and one syscall batch instead of one per page.
/// The cost is memory proportional to the directory — a million-entry
/// directory is a million names held until `RELEASEDIR`.
///
/// The node id is the same guard [`FileHandle`] carries and for the same
/// reason: nothing stops a client pairing any `Dh` with any `NodeId`, and
/// `FSYNCDIR` would otherwise sync a directory the handle never opened.
pub struct DirHandle {
    node: NodeId,
    /// The node's descriptor reopened `O_RDONLY | O_DIRECTORY`. `getdents64`
    /// needs the read access an `O_PATH` node fd does not carry, and
    /// `FSYNCDIR` needs a descriptor at all.
    fd: Arc<OwnedFd>,
    entries: Vec<SnapshotEntry>,
    /// `cookie -> index of the entry that follows it`, so resuming is a hash
    /// lookup rather than a scan of the snapshot. Built once at `OPENDIR`.
    resume: HashMap<u64, usize>,
}

impl DirHandle {
    /// Turn a client's cursor back into a position in the snapshot.
    ///
    /// Zero is the start of the listing, by FUSE convention. Anything else has
    /// to be a cookie this handle handed out — including the last entry's,
    /// which resolves to one past the end and yields the empty final page that
    /// tells the client it is done. A cursor we never issued is a client bug,
    /// and `EINVAL` says so rather than silently truncating the listing.
    fn resume_at(&self, offset: u64) -> FsResult<usize> {
        if offset == 0 {
            return Ok(0);
        }
        self.resume.get(&offset).copied().ok_or(Errno::EINVAL)
    }
}

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
    /// `OPENDIR` parks its directory snapshots here. `Arc` because `get`
    /// clones, and a snapshot is the one handle payload big enough to care.
    dirs: HandleTable<Arc<DirHandle>>,
    /// Masks `O_SYNC`/`O_DSYNC` and short-circuits `FSYNC` (spec §6).
    fsync_policy: FsyncPolicy,
    /// Whether the client mounted with `FUSE_WRITEBACK_CACHE`, which changes
    /// what an `OPEN` flag means. See [`LocalFs::mask_open_flags`]. Task 12
    /// feeds it from the negotiated `HELLO`.
    writeback: bool,
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
    ///
    /// `writeback` is the client's negotiated `FUSE_WRITEBACK_CACHE` state, not
    /// a server option: it changes how `OPEN` flags are read, and only the
    /// client knows whose kernel is computing append offsets.
    pub fn new(
        export_root: &Path,
        fsync: FsyncPolicy,
        writeback: bool,
        uring: UringExecutor,
        pool: BufferPool,
    ) -> io::Result<LocalFs> {
        let root = rustix::fs::open(
            export_root,
            rustix::fs::OFlags::PATH | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?;
        LocalFs::from_root_fd(root, fsync, writeback, uring, pool)
    }

    /// The same, for a root descriptor the caller already holds.
    ///
    /// This is the constructor `ATTACH` uses. The allowlist check opens the
    /// client's path itself so it can match against the name that descriptor
    /// resolves to (spec §3.2 step 3); handing the fd over rather than the
    /// path is the other half of that guarantee. Re-opening by name here would
    /// give a racing symlink swap a second chance and export a root the
    /// allowlist never approved — the fd that was verified must be the fd that
    /// is exported.
    pub fn from_root_fd(
        root: OwnedFd,
        fsync: FsyncPolicy,
        writeback: bool,
        uring: UringExecutor,
        pool: BufferPool,
    ) -> io::Result<LocalFs> {
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
            writeback,
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

    /// The same check, minus the demand that the handle still exist.
    ///
    /// `FLUSH` and `RELEASE` are the two ops that retire a handle rather than
    /// use it, and both can legitimately arrive for one that is already gone —
    /// a `RELEASE` whose reply was lost gets retried, and a `FLUSH` can follow
    /// it. Refusing those would surface `EBADF` from the application's
    /// `close(2)`. A handle belonging to *another* node is still refused.
    fn retiring_handle(&self, node: NodeId, fh: Fh) -> FsResult<Option<FileHandle>> {
        match self.files.get(fh) {
            Some(handle) if handle.node == node => Ok(Some(handle)),
            Some(_) => Err(EBADF),
            None => Ok(None),
        }
    }

    /// Resolves `(node, dh)` to the directory snapshot that handle owns.
    ///
    /// The directory twin of [`LocalFs::file_fd`], and refused for the same
    /// reason: a `Dh` that is unknown, already released, or presented against
    /// a different node is `EBADF`, never somebody else's directory.
    fn dir_handle(&self, node: NodeId, dh: Fh) -> FsResult<Arc<DirHandle>> {
        match self.dirs.get(dh) {
            Some(handle) if handle.node == node => Ok(handle),
            _ => Err(EBADF),
        }
    }

    /// The descriptor the xattr operations run against.
    ///
    /// `fgetxattr` and friends reject an `O_PATH` descriptor, so the node has
    /// to be reopened for real — and *that* is why v1 answers `EOPNOTSUPP` for
    /// everything but a regular file or a directory. Reopening the other file
    /// types through `/proc/self/fd/N` is not merely unhelpful, it is harmful:
    /// a symlink's magic link cannot be opened without `O_PATH` at all
    /// (`ELOOP`), a FIFO with no peer blocks the open indefinitely, and a
    /// device node runs its driver's `open` — a side effect no client asked
    /// for by reading an attribute. libfuse sidesteps this by calling the
    /// path-based `getxattr` on the `/proc` name; that route has no io_uring
    /// opcode, so v1 narrows the file types instead.
    ///
    /// Known limitation, inherent to the reopen: it needs *read* permission,
    /// so an unprivileged server exporting a mode-0222 file answers `EACCES`
    /// to an xattr op the backing filesystem would have allowed.
    async fn xattr_fd(&self, node: NodeId) -> FsResult<Arc<OwnedFd>> {
        let fd = self.node_fd(node)?;
        // Blocking: `reopen` is an `open(2)`, and the `fstat` that guards it
        // belongs on the same thread rather than costing a second round trip
        // through the ring.
        let opened = tokio::task::spawn_blocking(move || {
            // `fstat` is one of the few syscalls an O_PATH descriptor accepts.
            let st = rustix::fs::fstat(&*fd).map_err(rustix_errno)?;
            match rustix::fs::FileType::from_raw_mode(st.st_mode) {
                rustix::fs::FileType::RegularFile | rustix::fs::FileType::Directory => {}
                _ => return Err(EOPNOTSUPP),
            }
            reopen(&fd, rustix::fs::OFlags::RDONLY).map_err(errno)
        })
        .await
        .map_err(join_errno)??;
        Ok(Arc::new(opened))
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
    /// * `O_APPEND` and `O_NONBLOCK`, which describe how the client wants its
    ///   own descriptor to behave and cost the server nothing;
    /// * `O_SYNC`/`O_DSYNC`, unless the durability policy is `ignore`, whose
    ///   whole purpose is to not pay for them (spec §6).
    ///
    /// What does not, beyond the unnamed rest: `O_CREAT`, `O_EXCL` and
    /// `O_TRUNC` (FUSE has `CREATE` and `SETATTR` for those, and honoring them
    /// on `OPEN` would let a plain open create or destroy data), `O_NOFOLLOW`
    /// and `O_DIRECTORY` (meaningless — the node is already resolved and
    /// reopened through `/proc`), `O_DIRECT`, which v1 does not support
    /// (pooled buffers carry no alignment guarantee, so every read and write
    /// against such a descriptor would fail `EINVAL`), and `O_NOATIME`, which
    /// the kernel checks against the *server's* credentials: an unprivileged
    /// server exporting another user's files would turn a legal client open
    /// into `EPERM`. libfuse and virtiofsd keep it only because they run
    /// privileged.
    ///
    /// # Writeback caching changes two of these
    ///
    /// With `FUSE_WRITEBACK_CACHE` the client's kernel owns the page cache and
    /// the file size, and the server sees the flushes rather than the writes:
    ///
    /// * **`O_APPEND` is cleared.** The client computes append offsets itself
    ///   and flushes dirty pages at explicit offsets, but a positioned write to
    ///   an `O_APPEND` descriptor ignores its offset and appends. A page
    ///   flushed twice would therefore be appended twice. With writeback *off*
    ///   the opposite holds — server-side `O_APPEND` is what makes an append
    ///   atomic against a stale client `i_size` — so this cannot be
    ///   unconditional.
    /// * **`O_WRONLY` becomes `O_RDWR`.** A partial-page write makes the client
    ///   read the rest of the page back through the same handle, even for a
    ///   file the application opened write-only; a write-only descriptor here
    ///   answers that read with `EBADF`.
    ///
    /// Both are what libfuse's `lo_open` and virtiofsd's `open_inode` do.
    fn mask_open_flags(&self, flags: u32) -> i32 {
        const ALLOWED: i32 = libc::O_ACCMODE
            | libc::O_APPEND
            | libc::O_NONBLOCK
            // O_SYNC's value already contains O_DSYNC's bit; both are named so
            // the intent survives someone reading only one line.
            | libc::O_SYNC
            | libc::O_DSYNC;

        let mut flags = (flags as i32) & ALLOWED;
        if self.fsync_policy == FsyncPolicy::Ignore {
            flags &= !(libc::O_SYNC | libc::O_DSYNC);
        }
        if self.writeback {
            flags &= !libc::O_APPEND;
            if flags & libc::O_ACCMODE == libc::O_WRONLY {
                flags = (flags & !libc::O_ACCMODE) | libc::O_RDWR;
            }
        }
        // Descriptors are the server's, never a child's.
        flags | libc::O_CLOEXEC
    }

    /// The durability policy in one place (spec §6).
    ///
    /// `honor` runs the real `fsync`/`fdatasync`; `ignore` acknowledges without
    /// touching disk, the same trade an NFS `async` export makes — latency for
    /// crash durability. `FSYNC` and `FSYNCDIR` both land here, which is what
    /// keeps one policy from applying to files and another to directories.
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

/// Validates an xattr name arriving from the wire.
///
/// Unlike a filename this is not restricted to one path component — `user.k`,
/// `security.capability` and whatever else the backing filesystem holds are
/// all opaque bytes to us. NUL is the single byte that cannot travel, because
/// the syscall takes a NUL-terminated string: `user.a\0evil` would otherwise
/// reach the kernel as `user.a`, an attribute the client never named.
fn xattr_name(name: &[u8]) -> FsResult<CString> {
    CString::new(name).map_err(|_| Errno::EINVAL)
}

/// One `getdents64` sweep of an open directory, taken whole at `OPENDIR`.
///
/// Runs on a blocking thread: there is no io_uring opcode for `getdents`
/// (spec §5.3), and a large directory is many syscalls' worth of work.
fn snapshot_dir(fd: &OwnedFd) -> FsResult<Vec<SnapshotEntry>> {
    let mut dir = rustix::fs::Dir::read_from(fd).map_err(rustix_errno)?;
    let mut entries = Vec::new();
    while let Some(entry) = dir.read() {
        let entry = entry.map_err(rustix_errno)?;
        let name = entry.file_name();
        let kind = match file_kind(entry.file_type()) {
            Some(kind) => kind,
            None => resolve_kind(fd, name),
        };
        entries.push(SnapshotEntry {
            name: name.to_bytes().to_vec(),
            ino: entry.ino(),
            kind,
            // `d_off` is signed in the kernel's struct and opaque on the wire;
            // the cast is a reinterpretation, not a range claim.
            cookie: entry.offset() as u64,
        });
    }
    Ok(entries)
}

/// How many bytes a directory page may spend.
///
/// The client's ask, clamped to the largest body the protocol can carry. Spec
/// §3.1 makes a body over `MAX_BODY_SIZE` a *fatal* violation on receipt, so a
/// client that asked for a megabyte of entries and got an honest megabyte back
/// would kill its own mount. Dispatch should refuse the oversized ask too, but
/// the clamp belongs here as well: it is the backend that decides how much it
/// writes, and it stays correct however the wire layer is later rewired.
fn reply_budget(max_bytes: u32) -> usize {
    max_bytes.min(lbfs_proto::frame::MAX_BODY_SIZE) as usize
}

/// `cookie -> index of the entry that follows it`, for [`DirHandle::resume_at`].
///
/// Reverse order, so the *lowest* index wins a duplicate cookie. `d_off` is
/// unique in every filesystem this server has met, but ext4 packs an htree
/// hash into it and colliding names come back consecutively, so a collision is
/// structurally possible rather than merely unlikely. Lowest-index-wins turns
/// what would be a silently skipped name into a replayed one — which is what
/// a native `seekdir` onto a shared cookie does anyway.
fn resume_map(entries: &[SnapshotEntry]) -> HashMap<u64, usize> {
    entries
        .iter()
        .enumerate()
        .rev()
        .map(|(i, e)| (e.cookie, i + 1))
        .collect()
}

/// The entry a name gets when the server can list it but cannot resolve it.
///
/// Node 0 is FUSE's "no dentry, no lookup count": the client's kernel leaves
/// `fuse_direntplus_link` before it reads the attributes, so the zeroed
/// [`FileAttr`] never reaches anybody and no `FORGET` is owed. Reporting the
/// name this way rather than dropping it does two things. It keeps `READDIR`
/// and `READDIRPLUS` agreeing on which names a directory holds — the same
/// directory listed two ways should not give two answers. And it keeps a page
/// non-empty: [`ReaddirplusReply`] carries no cursor of its own, so a page
/// that dropped every entry would leave the client nothing to advance to, and
/// it would either re-send the same offset forever or read the empty page as
/// the end of a listing that is not over.
fn unresolved_entry() -> Entry {
    Entry {
        node: 0,
        generation: 0,
        attr: FileAttr::default(),
    }
}

/// `d_type` to the wire's [`FileKind`]; `None` for `DT_UNKNOWN`.
fn file_kind(t: rustix::fs::FileType) -> Option<FileKind> {
    Some(match t {
        rustix::fs::FileType::RegularFile => FileKind::Regular,
        rustix::fs::FileType::Directory => FileKind::Directory,
        rustix::fs::FileType::Symlink => FileKind::Symlink,
        rustix::fs::FileType::Fifo => FileKind::Fifo,
        rustix::fs::FileType::Socket => FileKind::Socket,
        rustix::fs::FileType::CharacterDevice => FileKind::CharDevice,
        rustix::fs::FileType::BlockDevice => FileKind::BlockDevice,
        rustix::fs::FileType::Unknown => return None,
    })
}

/// The type of an entry whose `d_type` the filesystem did not fill in.
///
/// `DT_UNKNOWN` is legal and real — XFS without `ftype`, and several network
/// filesystems, never populate the field. The wire [`FileKind`] has no
/// "unknown" to forward, and guessing `Regular` for a directory is not a
/// harmless default: it is what makes a client's `find -type d` miss subtrees
/// and `rm -r` decline to descend. One `statat` per such entry buys the truth,
/// and costs nothing on the filesystems that do fill `d_type`.
///
/// A name from `getdents64` holds no `/`, and `AT_SYMLINK_NOFOLLOW` keeps the
/// stat on the entry itself, so this cannot leave the directory. An entry
/// unlinked between the sweep and the stat falls back to `Regular`; the type
/// of a name that no longer exists is nobody's answer.
fn resolve_kind(dirfd: &OwnedFd, name: &std::ffi::CStr) -> FileKind {
    rustix::fs::statat(dirfd, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .ok()
        .and_then(|st| file_kind(rustix::fs::FileType::from_raw_mode(st.st_mode)))
        .unwrap_or(FileKind::Regular)
}

/// One `statfs` field narrowed to the width the wire carries.
///
/// `f_bsize`, `f_namelen` and `f_frsize` are `c_long` in the kernel's struct
/// and `u32` in [`StatfsReply`], matching FUSE's own reply. Real values are
/// kilobytes at most; saturating is a formality that keeps a hostile or
/// corrupt filesystem from wrapping the number into something small.
fn statfs_field<T: TryInto<u32>>(v: T) -> u32 {
    v.try_into().unwrap_or(u32::MAX)
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
        // over-large ask is clamped rather than allocated for. `UringExecutor`
        // asserts on a length past capacity - in this task, before the request
        // is ever submitted - so the clamp is what keeps a client's number from
        // panicking its own request handler. Refusing `size > max_io_size`
        // outright belongs to dispatch, where spec §3.1 makes it fatal.
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
        // Bounded by the buffer's *initialized prefix*, not its capacity.
        // Pooled storage is recycled without zeroing, so a `len` larger than
        // what the caller actually put there would write another request's
        // bytes into a user's file. Dispatch sets the length to the count it
        // read off the socket; anything past that is structurally unreachable
        // rather than merely unlikely.
        let len = len.min(u32::try_from(data.as_slice().len()).unwrap_or(u32::MAX));
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
        self.retiring_handle(node, fh)?;
        Ok(())
    }

    async fn release(&self, node: NodeId, fh: Fh) -> FsResult<()> {
        if self.retiring_handle(node, fh)?.is_none() {
            return Ok(());
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

    // --- Directories -------------------------------------------------------

    async fn opendir(&self, node: NodeId) -> FsResult<Fh> {
        let fd = self.node_fd(node)?;
        // One blocking hop for both halves: the reopen is an `open(2)` and the
        // sweep is a run of `getdents64`, and neither belongs on a runtime
        // worker. `O_DIRECTORY` is what makes a file node fail `ENOTDIR` here
        // rather than somewhere less legible; `O_RDONLY` is the access
        // `getdents64` and `FSYNCDIR` both need and an `O_PATH` node fd lacks.
        let (dir_fd, entries) = tokio::task::spawn_blocking(move || {
            let dir_fd = reopen(
                &fd,
                rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            )
            .map_err(errno)?;
            let entries = snapshot_dir(&dir_fd)?;
            Ok::<_, Errno>((dir_fd, entries))
        })
        .await
        .map_err(join_errno)??;

        let resume = resume_map(&entries);
        Ok(self.dirs.insert(Arc::new(DirHandle {
            node,
            fd: Arc::new(dir_fd),
            entries,
            resume,
        })))
    }

    async fn readdir(
        &self,
        node: NodeId,
        dh: Fh,
        offset: u64,
        max_bytes: u32,
    ) -> FsResult<ReaddirReply> {
        let handle = self.dir_handle(node, dh)?;
        let start = handle.resume_at(offset)?;
        let budget = reply_budget(max_bytes);
        let mut used = READDIR_REPLY_OVERHEAD;
        let mut entries = Vec::new();
        let mut cursor = start;

        while let Some(e) = handle.entries.get(cursor) {
            let cost = e.name.len() + READDIR_ENTRY_OVERHEAD;
            // The first entry of a page ignores the budget. A name long enough
            // to overshoot on its own is still under 300 bytes, far inside the
            // negotiated frame, and refusing it would leave the client
            // re-issuing the same offset forever.
            if !entries.is_empty() && used + cost > budget {
                break;
            }
            used += cost;
            entries.push(DirEntry {
                name: e.name.clone(),
                ino: e.ino,
                kind: e.kind,
                offset: e.cookie,
            });
            cursor += 1;
        }
        Ok(ReaddirReply {
            entries,
            end: cursor >= handle.entries.len(),
        })
    }

    async fn readdirplus(
        &self,
        node: NodeId,
        dh: Fh,
        offset: u64,
        max_bytes: u32,
    ) -> FsResult<ReaddirplusReply> {
        let handle = self.dir_handle(node, dh)?;
        let start = handle.resume_at(offset)?;
        // Before the loop, deliberately. `.` and `..` need the directory's own
        // attributes, and this is the last step that may fail the whole call:
        // once the loop below has registered an entry, returning an error
        // instead of the reply strands a lookup count no FORGET will retire.
        let dir_attr = attr_from_statx(&self.statx_fd(&handle.fd).await.map_err(errno)?);

        let budget = reply_budget(max_bytes);
        let mut used = READDIR_REPLY_OVERHEAD;
        let mut entries = Vec::new();
        let mut cursor = start;

        while let Some(e) = handle.entries.get(cursor) {
            let cost = e.name.len() + READDIRPLUS_ENTRY_OVERHEAD;
            if !entries.is_empty() && used + cost > budget {
                break;
            }
            cursor += 1;
            used += cost;

            let entry = if e.name == b"." || e.name == b".." {
                // Node 0 is FUSE's "attributes only": the client's kernel does
                // not instantiate a dentry for it and takes no lookup count.
                // Registering these would put two entries per directory on the
                // FORGET ledger that no client will ever retire.
                Entry {
                    node: 0,
                    generation: 0,
                    attr: dir_attr,
                }
            } else {
                // Through `lookup_impl`, because every `Entry` a client
                // receives is one lookup count it owes exactly one FORGET for,
                // and that is the only place a node is registered.
                match CString::new(e.name.clone()) {
                    Ok(name) => match self.lookup_impl(&handle.fd, &name).await {
                        Ok(entry) => entry,
                        Err(err) => {
                            // Reported, never fatal, never dropped. The
                            // snapshot is older than the directory, so a name
                            // unlinked since the sweep is ordinary rather than
                            // exceptional - and failing the page would strand
                            // every lookup count already registered into it.
                            // See `unresolved_entry` for why the name still
                            // travels instead of vanishing from the page.
                            tracing::debug!(?err, "readdirplus could not resolve an entry");
                            unresolved_entry()
                        }
                    },
                    // Unreachable: `getdents64` names hold no NUL. Reported
                    // like any other unresolvable name rather than dropped.
                    Err(_) => unresolved_entry(),
                }
            };
            entries.push(DirEntryPlus {
                name: e.name.clone(),
                entry,
                offset: e.cookie,
            });
        }
        Ok(ReaddirplusReply {
            entries,
            end: cursor >= handle.entries.len(),
        })
    }

    async fn releasedir(&self, node: NodeId, dh: Fh) -> FsResult<()> {
        match self.dirs.get(dh) {
            Some(handle) if handle.node == node => {}
            Some(_) => return Err(EBADF),
            // The same tolerance `release` extends: a RELEASEDIR whose reply
            // was lost gets retried, and refusing the retry would surface
            // EBADF from the application's `closedir(3)`.
            None => return Ok(()),
        }
        if let Some(handle) = self.dirs.remove(dh) {
            // Off the runtime for the same reason `release` moves its `close`
            // there, only more so: the descriptor is the cheap part, and
            // freeing a large directory's snapshot is a million small
            // deallocations plus the cookie map behind them.
            tokio::task::spawn_blocking(move || drop(handle))
                .await
                .map_err(join_errno)?;
        }
        Ok(())
    }

    async fn fsyncdir(&self, node: NodeId, dh: Fh, datasync: bool) -> FsResult<()> {
        let handle = self.dir_handle(node, dh)?;
        // The durability policy is one policy (spec §6), and this is the
        // descriptor `OPENDIR` reopened so a directory could honor it.
        self.maybe_fsync(&handle.fd, datasync).await
    }

    async fn statfs(&self, node: NodeId) -> FsResult<StatfsReply> {
        let fd = self.node_fd(node)?;
        // No io_uring opcode for statfs (spec §5.3), and it is a filesystem
        // round trip on anything but a local disk.
        let st = tokio::task::spawn_blocking(move || {
            // `fstatfs` is one of the few syscalls an O_PATH descriptor takes,
            // so the node's own fd answers without a reopen.
            rustix::fs::fstatfs(&*fd).map_err(rustix_errno)
        })
        .await
        .map_err(join_errno)??;
        Ok(StatfsReply {
            blocks: st.f_blocks,
            bfree: st.f_bfree,
            bavail: st.f_bavail,
            files: st.f_files,
            ffree: st.f_ffree,
            bsize: statfs_field(st.f_bsize),
            namelen: statfs_field(st.f_namelen),
            frsize: statfs_field(st.f_frsize),
        })
    }

    // --- Xattrs ------------------------------------------------------------

    async fn getxattr(&self, node: NodeId, name: &[u8], size: u32) -> FsResult<(u32, Vec<u8>)> {
        let name = xattr_name(name)?;
        let fd = self.xattr_fd(node).await?;
        let buf = self.pool.get();
        // The largest value this server can hand back: the kernel's own
        // ceiling and the pooled buffer, whichever binds first.
        let ceiling = buf.capacity().min(MAX_XATTR_SIZE);
        let probe = size == 0;
        // `size == 0` is FUSE's length probe, and it still goes to the kernel
        // with the full ceiling rather than a zero-length buffer. A
        // zero-length call would report the value's true size, but the
        // completion clamps its count to the buffer's capacity
        // (`finish_transfer`), so the reply could not distinguish "the value
        // is exactly this big" from "the value is bigger than anything we can
        // carry" - and that distinction is the whole of the check below.
        // Asking for the ceiling costs one copy of a value that is at most
        // 64 KiB, against a reopen and a blocking hop already paid.
        let want = if probe {
            ceiling
        } else {
            (size as usize).min(ceiling)
        } as u32;
        let (buf, res) = self.uring.fgetxattr(&fd, name, buf, want).await;
        let n = match res {
            Ok(n) => n,
            // A value past the ceiling can never travel. Reporting its true
            // size would send the client back for a fetch that answers ERANGE
            // and a probe that repeats the advice, forever. E2BIG ends the
            // exchange, and mirrors the ceiling `setxattr` enforces inbound.
            Err(e) if probe && e.raw_os_error() == Some(libc::ERANGE) => return Err(E2BIG),
            // On a real fetch a buffer shorter than the value is ERANGE
            // straight from the syscall, which is what the client's own
            // getxattr(2) expects to see.
            Err(e) => return Err(errno(e)),
        };
        if probe {
            // The client asked for the length alone; the bytes the kernel put
            // in the buffer are not part of this answer.
            return Ok((n, Vec::new()));
        }
        Ok((n, buf.as_slice().to_vec()))
    }

    async fn setxattr(&self, node: NodeId, name: &[u8], value: &[u8], flags: u32) -> FsResult<()> {
        let name = xattr_name(name)?;
        let mut buf = self.pool.get();
        // XATTR_SIZE_MAX is the kernel's ceiling and the pooled buffer is
        // ours; past either, E2BIG is what `setxattr(2)` itself would say.
        if value.len() > MAX_XATTR_SIZE || value.len() > buf.capacity() {
            return Err(E2BIG);
        }
        let fd = self.xattr_fd(node).await?;
        buf.as_mut_slice()[..value.len()].copy_from_slice(value);
        // The length is the contract the ring reads: nothing past it is sent,
        // so the recycled tail of the buffer cannot become part of the value.
        buf.set_len(value.len());
        let len = value.len() as u32;
        // `flags` (XATTR_CREATE, XATTR_REPLACE) goes to the kernel as it
        // arrived; it is the one that knows whether the attribute exists.
        let (_buf, res) = self
            .uring
            .fsetxattr(&fd, name, buf, len, flags as i32)
            .await;
        res.map_err(errno)
    }

    async fn listxattr(&self, node: NodeId, size: u32) -> FsResult<(u32, Vec<u8>)> {
        let fd = self.xattr_fd(node).await?;
        // No io_uring opcode for listxattr (spec §5.3).
        tokio::task::spawn_blocking(move || {
            if size == 0 {
                // The same length probe `getxattr` honors, with the same rule:
                // the reply is the count and nothing else.
                let n = rustix::fs::flistxattr(&*fd, &mut [0u8; 0]).map_err(rustix_errno)?;
                return Ok((n as u32, Vec::new()));
            }
            let mut list = vec![0u8; (size as usize).min(MAX_XATTR_LIST)];
            let n = rustix::fs::flistxattr(&*fd, &mut list[..]).map_err(rustix_errno)?;
            // The kernel filled `n` bytes; the zeros past them are ours, not
            // the filesystem's, and must not travel as names.
            list.truncate(n);
            Ok((n as u32, list))
        })
        .await
        .map_err(join_errno)?
    }

    async fn removexattr(&self, node: NodeId, name: &[u8]) -> FsResult<()> {
        let name = xattr_name(name)?;
        let fd = self.xattr_fd(node).await?;
        // No io_uring opcode for removexattr either (spec §5.3).
        tokio::task::spawn_blocking(move || {
            rustix::fs::fremovexattr(&*fd, name.as_c_str()).map_err(rustix_errno)
        })
        .await
        .map_err(join_errno)?
    }
}

/// A `LocalFs` over a fresh tempdir, with the writeback state a client that
/// never negotiated the cache would produce.
#[cfg(test)]
pub(crate) async fn test_fs(policy: crate::config::FsyncPolicy) -> (tempfile::TempDir, LocalFs) {
    test_fs_writeback(policy, false).await
}

#[cfg(test)]
pub(crate) async fn test_fs_writeback(
    policy: crate::config::FsyncPolicy,
    writeback: bool,
) -> (tempfile::TempDir, LocalFs) {
    test_fs_with(policy, writeback, 1 << 20).await
}

/// A `LocalFs` whose pooled buffers are deliberately small.
///
/// `max_io_size` has no lower clamp in the config, and Task 12 settles it as
/// `min(client, server)`, so the branches where a value outgrows the pool are
/// reachable by configuration rather than merely theoretical.
#[cfg(test)]
pub(crate) async fn test_fs_pool(
    policy: crate::config::FsyncPolicy,
    buf_size: usize,
) -> (tempfile::TempDir, LocalFs) {
    test_fs_with(policy, false, buf_size).await
}

#[cfg(test)]
async fn test_fs_with(
    policy: crate::config::FsyncPolicy,
    writeback: bool,
    buf_size: usize,
) -> (tempfile::TempDir, LocalFs) {
    let dir = tempfile::tempdir().unwrap();
    let uring = uring::UringExecutor::new(1, 64).unwrap();
    let pool = buffers::BufferPool::new(buf_size, 8);
    let fs = LocalFs::new(dir.path(), policy, writeback, uring, pool).unwrap();
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
        // The length is the contract: `write` will not send bytes past it, so a
        // caller that fills the buffer owes the count.
        buf.set_len(5);
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
        // A FLUSH can still arrive for a handle already retired - a RELEASE
        // whose reply was lost gets retried, and refusing the FLUSH would
        // surface EBADF from the application's close(2).
        fs.flush(entry.node, fh).await.unwrap();
    }

    /// Pooled storage is recycled without zeroing, so the buffer's length is
    /// the boundary between the client's bytes and the last request's. A `len`
    /// larger than that must not reach the file.
    #[tokio::test]
    async fn write_never_sends_bytes_past_the_initialized_prefix() {
        let (_dir, fs) = test_fs(FsyncPolicy::Honor).await;
        let (e, fh) = fs
            .create(ROOT_NODE, b"f", 0o644, libc::O_RDWR as u32)
            .await
            .unwrap();

        let mut buf = fs.pool_for_test().get();
        buf.as_mut_slice()[..5].copy_from_slice(b"hello");
        buf.set_len(2); // only "he" is the caller's
        assert_eq!(fs.write(e.node, fh, 0, buf, 5).await.unwrap(), 2);
        assert_eq!(fs.getattr(e.node, None).await.unwrap().size, 2);
        let out = fs.read(e.node, fh, 0, 5).await.unwrap();
        assert_eq!(out.as_slice(), b"he");
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
                (libc::O_WRONLY | libc::O_TRUNC | libc::O_CREAT | libc::O_DIRECT | libc::O_NOATIME)
                    as u32,
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
        assert_eq!(
            flags & libc::O_NOATIME,
            0,
            "O_NOATIME is checked against the server's credentials, not the client's"
        );
        assert_eq!(flags & libc::O_ACCMODE, libc::O_WRONLY);

        // A missing name still cannot be conjured: OPEN takes a node, and an
        // unknown one is ESTALE rather than a fresh file.
        assert_eq!(
            fs.open(9999, libc::O_CREAT as u32).await.unwrap_err(),
            Errno::ESTALE
        );
    }

    /// With `FUSE_WRITEBACK_CACHE` — the v1 client's default — the client's
    /// kernel owns the file size and flushes dirty pages at explicit offsets.
    /// A positioned write to an `O_APPEND` descriptor ignores its offset and
    /// appends, so a page flushed twice would land twice; and a partial-page
    /// write makes the client read the rest of the page back through a handle
    /// the application opened write-only.
    #[tokio::test]
    async fn writeback_clears_o_append_and_promotes_o_wronly() {
        let (dir, fs) = test_fs_writeback(FsyncPolicy::Honor, true).await;
        std::fs::write(dir.path().join("f"), b"hello").unwrap();
        let e = fs.lookup(ROOT_NODE, b"f").await.unwrap();
        let fh = fs
            .open(e.node, (libc::O_WRONLY | libc::O_APPEND) as u32)
            .await
            .unwrap();

        let flags = fs.file_flags_for_test(fh);
        assert_eq!(
            flags & libc::O_APPEND,
            0,
            "a positioned write to an O_APPEND fd would ignore the offset"
        );
        assert_eq!(
            flags & libc::O_ACCMODE,
            libc::O_RDWR,
            "writeback reads back through the write handle"
        );
        // The promotion is not cosmetic: this read is the one a partial-page
        // writeback issues, and an O_WRONLY descriptor answers it with EBADF.
        let out = fs.read(e.node, fh, 0, 5).await.unwrap();
        assert_eq!(out.as_slice(), b"hello");
    }

    /// Without writeback the opposite holds: server-side `O_APPEND` is what
    /// makes an append atomic against a client's stale idea of the file size,
    /// so stripping it unconditionally would be its own corruption.
    #[tokio::test]
    async fn without_writeback_o_append_and_the_access_mode_survive() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        std::fs::write(dir.path().join("f"), b"hello").unwrap();
        let e = fs.lookup(ROOT_NODE, b"f").await.unwrap();
        let fh = fs
            .open(e.node, (libc::O_WRONLY | libc::O_APPEND) as u32)
            .await
            .unwrap();

        let flags = fs.file_flags_for_test(fh);
        assert_eq!(flags & libc::O_APPEND, libc::O_APPEND);
        assert_eq!(flags & libc::O_ACCMODE, libc::O_WRONLY);
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
        buf.set_len(1);
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
        buf.set_len(4);
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

    /// A read larger than one pooled buffer is clamped, not allocated for.
    /// Without the clamp `UringExecutor::read`'s length assert fires in this
    /// task, before any submission, so the failure is a panicked request
    /// handler rather than anything the ring ever sees.
    #[tokio::test]
    async fn read_clamps_the_request_to_the_pool_buffer() {
        let (_dir, fs) = test_fs(FsyncPolicy::Honor).await;
        let (e, fh) = fs
            .create(ROOT_NODE, b"f", 0o644, libc::O_RDWR as u32)
            .await
            .unwrap();
        let mut buf = fs.pool_for_test().get();
        buf.as_mut_slice()[..2].copy_from_slice(b"hi");
        buf.set_len(2);
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
        buf.set_len(4);
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

    // --- Task 11: directories, statfs, xattrs ------------------------------

    #[tokio::test]
    async fn readdir_pages_and_terminates() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        for i in 0..10 {
            std::fs::write(dir.path().join(format!("f{i}")), b"").unwrap();
        }
        let dh = fs.opendir(ROOT_NODE).await.unwrap();
        let mut names = Vec::new();
        let mut offset = 0;
        loop {
            // Tiny budget forces pagination.
            let page = fs.readdir(ROOT_NODE, dh, offset, 64).await.unwrap();
            for e in &page.entries {
                names.push(e.name.clone());
                offset = e.offset;
            }
            if page.end {
                break;
            }
        }
        fs.releasedir(ROOT_NODE, dh).await.unwrap();
        for i in 0..10 {
            assert!(names.contains(&format!("f{i}").into_bytes()));
        }
        assert!(names.contains(&b".".to_vec()));
        assert!(names.contains(&b"..".to_vec()));
    }

    #[tokio::test]
    async fn readdirplus_returns_attrs_and_registers_nodes() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        std::fs::write(dir.path().join("f"), b"abc").unwrap();
        let dh = fs.opendir(ROOT_NODE).await.unwrap();
        let page = fs.readdirplus(ROOT_NODE, dh, 0, 1 << 16).await.unwrap();
        let f = page.entries.iter().find(|e| e.name == b"f").unwrap();
        assert_eq!(f.entry.attr.size, 3);
        assert!(f.entry.node > 1);
        let dot = page.entries.iter().find(|e| e.name == b".").unwrap();
        assert_eq!(dot.entry.node, 0); // attr-only, no lookup count
        assert_eq!(dot.entry.attr.mode & libc::S_IFMT, libc::S_IFDIR);
        // The registered node answers getattr (lookup count held).
        assert!(fs.getattr(f.entry.node, None).await.is_ok());
        fs.releasedir(ROOT_NODE, dh).await.unwrap();
    }

    /// The listing is snapshotted at `OPENDIR` — POSIX permits it, and it is
    /// what lets a resume cursor mean the same thing for the handle's life.
    #[tokio::test]
    async fn opendir_snapshots_the_directory() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        std::fs::write(dir.path().join("before"), b"").unwrap();
        let dh = fs.opendir(ROOT_NODE).await.unwrap();
        std::fs::write(dir.path().join("after"), b"").unwrap();

        let page = fs.readdir(ROOT_NODE, dh, 0, 1 << 16).await.unwrap();
        assert!(page.end);
        let names: Vec<_> = page.entries.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&b"before".to_vec()));
        assert!(!names.contains(&b"after".to_vec()));
        fs.releasedir(ROOT_NODE, dh).await.unwrap();
    }

    /// A page always advances. A budget too small for even one entry must
    /// still return one: a client re-issuing the same cursor against an empty
    /// page that says `end: false` never finishes.
    #[tokio::test]
    async fn a_page_too_small_for_one_entry_still_makes_progress() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        std::fs::write(dir.path().join("f"), b"").unwrap();
        let dh = fs.opendir(ROOT_NODE).await.unwrap();

        let page = fs.readdir(ROOT_NODE, dh, 0, 0).await.unwrap();
        assert_eq!(page.entries.len(), 1);
        assert!(!page.end);
        let plus = fs.readdirplus(ROOT_NODE, dh, 0, 0).await.unwrap();
        assert_eq!(plus.entries.len(), 1);
        assert!(!plus.end);
        fs.releasedir(ROOT_NODE, dh).await.unwrap();
    }

    /// A directory larger than one `getdents64` buffer, paged with a budget
    /// that forces dozens of round trips. The reassembled listing equalling
    /// the single-shot one only holds if every `d_off` cookie is distinct and
    /// each page resumes exactly one entry past the last it returned.
    #[tokio::test]
    async fn a_large_directory_pages_without_gaps_or_repeats() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        for i in 0..512 {
            std::fs::write(dir.path().join(format!("entry-{i:04}")), b"").unwrap();
        }
        let dh = fs.opendir(ROOT_NODE).await.unwrap();
        let whole = fs.readdir(ROOT_NODE, dh, 0, 1 << 20).await.unwrap();
        assert!(whole.end);
        assert_eq!(whole.entries.len(), 514); // 512 files, "." and ".."

        let mut paged = Vec::new();
        let mut offset = 0;
        loop {
            let page = fs.readdir(ROOT_NODE, dh, offset, 256).await.unwrap();
            assert!(!page.entries.is_empty() || page.end);
            if let Some(last) = page.entries.last() {
                offset = last.offset;
            }
            let end = page.end;
            paged.extend(page.entries);
            if end {
                break;
            }
        }
        assert_eq!(paged, whole.entries);
        fs.releasedir(ROOT_NODE, dh).await.unwrap();
    }

    /// A `READDIR` cursor is the `d_off` the kernel reported for that entry,
    /// so a client that stopped mid-listing resumes exactly where it stopped:
    /// no name repeated, none dropped. A cursor we never issued is a client
    /// bug, not a silently empty listing.
    #[tokio::test]
    async fn readdir_resumes_from_a_returned_cookie() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        for i in 0..8 {
            std::fs::write(dir.path().join(format!("f{i}")), b"").unwrap();
        }
        let dh = fs.opendir(ROOT_NODE).await.unwrap();
        let whole = fs.readdir(ROOT_NODE, dh, 0, 1 << 16).await.unwrap();
        assert!(whole.end);
        assert_eq!(whole.entries.len(), 10); // 8 files, "." and ".."

        let mut resumed = Vec::new();
        let mut offset = 0;
        loop {
            let page = fs.readdir(ROOT_NODE, dh, offset, 64).await.unwrap();
            if let Some(last) = page.entries.last() {
                offset = last.offset;
            }
            let end = page.end;
            resumed.extend(page.entries);
            if end {
                break;
            }
        }
        assert_eq!(resumed, whole.entries);
        assert!(resumed.len() > 2, "the budget must have forced pagination");

        assert_eq!(
            fs.readdir(ROOT_NODE, dh, u64::MAX, 4096).await.unwrap_err(),
            Errno::EINVAL
        );
        fs.releasedir(ROOT_NODE, dh).await.unwrap();
    }

    /// Every `Entry` in a `READDIRPLUS` page is a lookup the client's kernel
    /// counts, so each owes exactly one `FORGET` — and `.`/`..` are the two
    /// names it must *not* count, which is why they come back as node 0.
    #[tokio::test]
    async fn readdirplus_registers_one_lookup_per_entry_and_none_for_dots() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        for name in ["a", "b"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let dh = fs.opendir(ROOT_NODE).await.unwrap();
        let page = fs.readdirplus(ROOT_NODE, dh, 0, 1 << 16).await.unwrap();
        assert_eq!(page.entries.len(), 4);

        let mut dots = 0;
        for e in &page.entries {
            if e.name == b"." || e.name == b".." {
                dots += 1;
                assert_eq!(e.entry.node, 0, "{:?} must take no lookup count", e.name);
                continue;
            }
            assert!(e.entry.node > ROOT_NODE);
            assert!(fs.getattr(e.entry.node, None).await.is_ok());
            fs.forget(e.entry.node, 1).await;
            assert_eq!(
                fs.getattr(e.entry.node, None).await.unwrap_err(),
                Errno::ESTALE,
                "one FORGET must retire the entry readdirplus registered"
            );
        }
        assert_eq!(dots, 2);
        fs.releasedir(ROOT_NODE, dh).await.unwrap();
    }

    /// The snapshot is older than the directory it describes. A name unlinked
    /// between `OPENDIR` and the page that reports it comes back as node 0
    /// rather than failing the page — failing would strand every lookup count
    /// already registered into it, and no `FORGET` would arrive to retire
    /// them. Its neighbours resolve normally in the same page.
    #[tokio::test]
    async fn readdirplus_reports_a_vanished_name_beside_its_surviving_neighbours() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        for name in ["keep", "gone"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let dh = fs.opendir(ROOT_NODE).await.unwrap();
        std::fs::remove_file(dir.path().join("gone")).unwrap();

        let page = fs.readdirplus(ROOT_NODE, dh, 0, 1 << 16).await.unwrap();
        assert!(page.end);
        assert_eq!(page.entries.len(), 4); // "." ".." keep gone
        let node_of = |name: &[u8]| {
            page.entries
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("{name:?} is missing from the page"))
                .entry
                .node
        };
        assert!(node_of(b"keep") > ROOT_NODE);
        assert_eq!(node_of(b"gone"), 0, "a vanished name owes no lookup count");
        fs.releasedir(ROOT_NODE, dh).await.unwrap();
    }

    /// A page the server could resolve nothing in still has to carry names.
    /// `ReaddirplusReply` has no cursor of its own, so the only way a client
    /// can advance is the offset of an entry it received: an empty page that
    /// says `end: false` leaves it re-sending the same offset forever, or
    /// reading the gap as the end of a listing that is not over. One `rm -rf`
    /// racing one `ls -l` on a large directory is enough to produce a whole
    /// page of names that vanished after the snapshot.
    #[tokio::test]
    async fn readdirplus_reports_vanished_names_rather_than_emptying_a_page() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        for i in 0..16 {
            std::fs::write(dir.path().join(format!("f{i}")), b"x").unwrap();
        }
        let dh = fs.opendir(ROOT_NODE).await.unwrap();
        for i in 0..16 {
            std::fs::remove_file(dir.path().join(format!("f{i}"))).unwrap();
        }

        let mut names = Vec::new();
        let mut offset = 0;
        loop {
            // One entry per page, so every page past the dots is made
            // entirely of names that no longer resolve.
            let page = fs.readdirplus(ROOT_NODE, dh, offset, 0).await.unwrap();
            assert!(
                !page.entries.is_empty() || page.end,
                "a non-final page with no entries leaves the client no cursor"
            );
            if let Some(last) = page.entries.last() {
                offset = last.offset;
            }
            let end = page.end;
            for e in &page.entries {
                if e.name != b"." && e.name != b".." {
                    assert_eq!(
                        e.entry.node, 0,
                        "{:?} vanished, so it owes no lookup count",
                        e.name
                    );
                }
                names.push(e.name.clone());
            }
            if end {
                break;
            }
        }
        // Every name the snapshot held still travels, so READDIR and
        // READDIRPLUS agree on what the directory contains.
        assert_eq!(names.len(), 18);
        for i in 0..16 {
            assert!(names.contains(&format!("f{i}").into_bytes()));
        }
        fs.releasedir(ROOT_NODE, dh).await.unwrap();
    }

    /// Spec §3.1 makes an oversized body fatal on receipt, so a client that
    /// asks for a megabyte of entries and gets an honest megabyte back kills
    /// its own mount. The backend clamps regardless of what dispatch does.
    #[tokio::test]
    async fn a_page_never_exceeds_the_maximum_body_size() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        // Names long enough that a 64 KiB page cannot hold the directory.
        for i in 0..600 {
            std::fs::write(dir.path().join(format!("{:0>200}", i)), b"").unwrap();
        }
        let dh = fs.opendir(ROOT_NODE).await.unwrap();
        let max = lbfs_proto::frame::MAX_BODY_SIZE as usize;

        for ask in [u32::MAX, 1 << 20] {
            let page = fs.readdir(ROOT_NODE, dh, 0, ask).await.unwrap();
            assert!(!page.end, "the directory must not fit in one page");
            let body = postcard::to_allocvec(&page).unwrap();
            assert!(body.len() <= max, "readdir body {} > {max}", body.len());

            let plus = fs.readdirplus(ROOT_NODE, dh, 0, ask).await.unwrap();
            assert!(!plus.end);
            let body = postcard::to_allocvec(&plus).unwrap();
            assert!(body.len() <= max, "readdirplus body {} > {max}", body.len());
        }
        fs.releasedir(ROOT_NODE, dh).await.unwrap();
    }

    /// `getdents64` hands over an inode for every name and nothing downstream
    /// can reconstruct it: `READDIR` carries no attributes, and glibc's
    /// `readdir(3)` drops a dirent whose `d_ino` is zero. `..` is the case
    /// that proves it travels — its inode is the parent's, which the
    /// directory's own attributes cannot supply.
    #[tokio::test]
    async fn readdir_carries_the_inode_of_every_entry() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/f"), b"x").unwrap();
        let sub = fs.lookup(ROOT_NODE, b"sub").await.unwrap();
        let dh = fs.opendir(sub.node).await.unwrap();

        let page = fs.readdir(sub.node, dh, 0, 1 << 16).await.unwrap();
        let ino_of = |name: &[u8]| {
            page.entries
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("{name:?} is missing"))
                .ino
        };
        let root = fs.getattr(ROOT_NODE, None).await.unwrap();
        let f = fs.lookup(sub.node, b"f").await.unwrap();

        assert!(page.entries.iter().all(|e| e.ino != 0));
        assert_eq!(ino_of(b"f"), f.attr.ino);
        assert_eq!(ino_of(b"."), sub.attr.ino);
        assert_eq!(ino_of(b".."), root.ino, "'..' reports the parent's inode");
        fs.releasedir(sub.node, dh).await.unwrap();
    }

    /// `O_DIRECTORY` is not decoration. Without it the reopen of a FIFO node
    /// blocks inside `fifo_open` until a writer arrives, parking a
    /// `spawn_blocking` worker for as long as the client cares to wait — and a
    /// client that opens enough of them drains the pool and stalls the server.
    /// With it the kernel refuses before it reaches `may_open` at all.
    #[tokio::test]
    async fn opendir_on_a_fifo_fails_instead_of_blocking() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        rustix::fs::mknodat(
            rustix::fs::CWD,
            dir.path().join("p"),
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .unwrap();
        let p = fs.lookup(ROOT_NODE, b"p").await.unwrap();

        let opened = tokio::time::timeout(std::time::Duration::from_secs(5), fs.opendir(p.node))
            .await
            .expect("opendir on a FIFO must answer rather than park a worker");
        assert_eq!(opened.unwrap_err(), Errno(libc::ENOTDIR as u16));
    }

    /// `d_off` is unique on every filesystem this server has met, but ext4
    /// packs an htree hash into it, so a collision is structurally possible.
    /// Lowest-index-wins replays one name; highest-index-wins would drop the
    /// name between silently.
    #[test]
    fn a_duplicate_cookie_resumes_at_the_earlier_entry() {
        let entry = |name: &[u8], cookie| SnapshotEntry {
            name: name.to_vec(),
            ino: 1,
            kind: FileKind::Regular,
            cookie,
        };
        let entries = [entry(b"a", 7), entry(b"b", 7), entry(b"c", 9)];
        let map = resume_map(&entries);

        assert_eq!(map.get(&7), Some(&1), "resuming from 7 must replay \"b\"");
        assert_eq!(map.get(&9), Some(&3));
    }

    /// A directory handle is bound to the node it was opened on, exactly like
    /// a file handle: nothing stops a client pairing any `Dh` with any
    /// `NodeId`, and every directory op here is descriptor-relative.
    #[tokio::test]
    async fn a_dir_handle_is_bound_to_the_node_it_was_opened_on() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let sub = fs.lookup(ROOT_NODE, b"sub").await.unwrap();
        let dh = fs.opendir(ROOT_NODE).await.unwrap();

        assert_eq!(fs.readdir(sub.node, dh, 0, 4096).await.unwrap_err(), EBADF);
        assert_eq!(
            fs.readdirplus(sub.node, dh, 0, 4096).await.unwrap_err(),
            EBADF
        );
        assert_eq!(fs.fsyncdir(sub.node, dh, false).await.unwrap_err(), EBADF);
        assert_eq!(fs.releasedir(sub.node, dh).await.unwrap_err(), EBADF);
        assert_eq!(
            fs.readdir(ROOT_NODE, 9999, 0, 4096).await.unwrap_err(),
            EBADF
        );

        // Rejected, not consumed: the handle still works on its own node.
        fs.fsyncdir(ROOT_NODE, dh, true).await.unwrap();
        fs.releasedir(ROOT_NODE, dh).await.unwrap();
        // A RELEASEDIR whose reply was lost gets retried; the second one must
        // not fail the application's closedir(3).
        fs.releasedir(ROOT_NODE, dh).await.unwrap();
        assert_eq!(fs.readdir(ROOT_NODE, dh, 0, 4096).await.unwrap_err(), EBADF);
    }

    #[tokio::test]
    async fn opendir_refuses_a_non_directory_and_an_unknown_node() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        let f = fs.lookup(ROOT_NODE, b"f").await.unwrap();
        assert_eq!(
            fs.opendir(f.node).await.unwrap_err(),
            Errno(libc::ENOTDIR as u16)
        );
        assert_eq!(fs.opendir(9999).await.unwrap_err(), Errno::ESTALE);
    }

    /// `FSYNCDIR` answers to the same durability policy as `FSYNC` (spec §6),
    /// which it can only do because the handle owns a real descriptor.
    #[tokio::test]
    async fn fsyncdir_runs_through_the_durability_policy() {
        for policy in [FsyncPolicy::Honor, FsyncPolicy::Ignore] {
            let (_dir, fs) = test_fs(policy).await;
            let dh = fs.opendir(ROOT_NODE).await.unwrap();
            fs.fsyncdir(ROOT_NODE, dh, false).await.unwrap();
            fs.fsyncdir(ROOT_NODE, dh, true).await.unwrap();
            fs.releasedir(ROOT_NODE, dh).await.unwrap();
        }
    }

    #[tokio::test]
    async fn statfs_reports_filesystem_numbers() {
        let (_dir, fs) = test_fs(FsyncPolicy::Honor).await;
        let s = fs.statfs(ROOT_NODE).await.unwrap();
        assert!(s.bsize > 0);
        assert!(s.blocks > 0);
        assert!(s.namelen > 0);
        assert_eq!(fs.statfs(9999).await.unwrap_err(), Errno::ESTALE);
    }

    #[tokio::test]
    async fn xattr_set_get_list_remove() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        std::fs::write(dir.path().join("f"), b"").unwrap();
        let e = fs.lookup(ROOT_NODE, b"f").await.unwrap();

        match fs.setxattr(e.node, b"user.k", b"v1", 0).await {
            // A filesystem without user xattrs has nothing to prove here.
            Err(err) if err == EOPNOTSUPP => return,
            other => other.unwrap(),
        }
        let (size, val) = fs.getxattr(e.node, b"user.k", 64).await.unwrap();
        assert_eq!((size, val.as_slice()), (2, b"v1".as_slice()));
        let (size, val) = fs.getxattr(e.node, b"user.k", 0).await.unwrap(); // length probe
        assert_eq!(size, 2);
        assert!(
            val.is_empty(),
            "a probe writes nothing, so it returns nothing"
        );
        let (size, list) = fs.listxattr(e.node, 256).await.unwrap();
        assert_eq!(size as usize, list.len());
        assert!(list.windows(6).any(|w| w == b"user.k"));
        fs.removexattr(e.node, b"user.k").await.unwrap();
        assert_eq!(
            fs.getxattr(e.node, b"user.k", 64).await.unwrap_err(),
            Errno::ENODATA
        );
    }

    /// `size == 0` is FUSE's length probe, and it is the case that must never
    /// hand back a pooled buffer's recycled tail: the kernel reports the value
    /// size without writing a byte. A short buffer is `ERANGE` from the
    /// syscall itself.
    #[tokio::test]
    async fn getxattr_probes_the_length_and_refuses_a_short_buffer() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        // Dirty the pool, so a probe that leaked its buffer would show it.
        let mut dirty = fs.pool_for_test().get();
        dirty.as_mut_slice().fill(b'X');
        drop(dirty);

        std::fs::write(dir.path().join("f"), b"").unwrap();
        let e = fs.lookup(ROOT_NODE, b"f").await.unwrap();
        match fs.setxattr(e.node, b"user.k", b"value", 0).await {
            Err(err) if err == EOPNOTSUPP => return,
            other => other.unwrap(),
        }
        assert_eq!(
            fs.getxattr(e.node, b"user.k", 0).await.unwrap(),
            (5, Vec::new())
        );
        assert_eq!(
            fs.getxattr(e.node, b"user.k", 2).await.unwrap_err(),
            Errno::ERANGE
        );
        let (size, list) = fs.listxattr(e.node, 0).await.unwrap();
        assert!(size > 0);
        assert!(list.is_empty(), "a probe returns the length alone");
    }

    /// A `size` past one pooled buffer is clamped, not allocated for. Without
    /// the clamp `UringExecutor::fgetxattr`'s length assert fires in this
    /// task, before anything is submitted, so a client's number panics its own
    /// request handler. The same hazard, and the same fix, as `READ`.
    #[tokio::test]
    async fn getxattr_clamps_the_request_to_the_pool_buffer() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        std::fs::write(dir.path().join("f"), b"").unwrap();
        let e = fs.lookup(ROOT_NODE, b"f").await.unwrap();
        match fs.setxattr(e.node, b"user.k", b"v1", 0).await {
            Err(err) if err == EOPNOTSUPP => return,
            other => other.unwrap(),
        }
        let (size, val) = fs.getxattr(e.node, b"user.k", u32::MAX).await.unwrap();
        assert_eq!((size, val.as_slice()), (2, b"v1".as_slice()));
    }

    /// The pooled buffer is the server's own ceiling on a value, and
    /// `max_io_size` has no lower clamp — a small pool makes that ceiling the
    /// live one. Both ends of the exchange have to respect it: `setxattr`
    /// refuses before it copies past the buffer, and the `getxattr` probe
    /// refuses rather than reporting a size whose fetch can only answer
    /// `ERANGE`, which would leave the client probing forever.
    #[tokio::test]
    async fn a_value_the_pool_buffer_cannot_carry_is_e2big_at_both_ends() {
        let (dir, fs) = test_fs_pool(FsyncPolicy::Honor, 512).await;
        let path = dir.path().join("f");
        std::fs::write(&path, b"").unwrap();
        let e = fs.lookup(ROOT_NODE, b"f").await.unwrap();
        let value = vec![b'v'; 4096];

        // The size check precedes the reopen, so this holds even on a
        // filesystem with no user xattrs at all.
        assert_eq!(
            fs.setxattr(e.node, b"user.k", &value, 0).await.unwrap_err(),
            E2BIG
        );

        // Plant the same value out of band, so the probe has one to find.
        if rustix::fs::setxattr(&path, "user.k", &value, rustix::fs::XattrFlags::empty()).is_err() {
            return; // no user xattrs here; nothing left to prove
        }
        assert_eq!(
            fs.getxattr(e.node, b"user.k", 0).await.unwrap_err(),
            E2BIG,
            "a probe must not report a size the fetch can never carry"
        );
    }

    /// An xattr name is arbitrary bytes to this server, but the syscall takes
    /// a NUL-terminated string: `user.a\0evil` reaching the kernel as `user.a`
    /// is a name the client did not ask for.
    #[tokio::test]
    async fn xattr_rejects_an_embedded_nul_and_an_oversized_value() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        std::fs::write(dir.path().join("f"), b"").unwrap();
        let e = fs.lookup(ROOT_NODE, b"f").await.unwrap();

        for name in [b"user.a\0evil".as_slice(), b"\0".as_slice()] {
            assert_eq!(
                fs.getxattr(e.node, name, 64).await.unwrap_err(),
                Errno::EINVAL
            );
            assert_eq!(
                fs.setxattr(e.node, name, b"v", 0).await.unwrap_err(),
                Errno::EINVAL
            );
            assert_eq!(
                fs.removexattr(e.node, name).await.unwrap_err(),
                Errno::EINVAL
            );
        }

        let huge = vec![0u8; MAX_XATTR_SIZE + 1];
        assert_eq!(
            fs.setxattr(e.node, b"user.k", &huge, 0).await.unwrap_err(),
            E2BIG
        );
    }

    /// v1 scopes xattrs to regular files and directories. The reopen the ring
    /// ops need would dereference a symlink's magic link, block forever on a
    /// FIFO with no peer, or run a device driver's `open`.
    #[tokio::test]
    async fn xattrs_outside_files_and_directories_are_unsupported() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        std::os::unix::fs::symlink("target", dir.path().join("l")).unwrap();
        let l = fs.lookup(ROOT_NODE, b"l").await.unwrap();

        assert_eq!(
            fs.getxattr(l.node, b"user.k", 64).await.unwrap_err(),
            EOPNOTSUPP
        );
        assert_eq!(fs.listxattr(l.node, 64).await.unwrap_err(), EOPNOTSUPP);
        assert_eq!(
            fs.setxattr(l.node, b"user.k", b"v", 0).await.unwrap_err(),
            EOPNOTSUPP
        );
        assert_eq!(
            fs.removexattr(l.node, b"user.k").await.unwrap_err(),
            EOPNOTSUPP
        );

        // A directory is in scope, so its xattr ops reach the filesystem.
        assert!(!matches!(fs.listxattr(ROOT_NODE, 0).await, Err(e) if e == EOPNOTSUPP));
    }
}
