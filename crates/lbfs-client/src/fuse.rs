//! The kernel's side of the mount: `fuser` callbacks onto the multiplexer.
//!
//! ```text
//!   /dev/fuse ──▶ fuser session thread ──▶ callback (returns at once)
//!                                            │ spawn
//!                                            ▼
//!                                    tokio runtime ──▶ Connection::<op>()
//!                                            │
//!                                    reply.<kind>() / reply.error()
//! ```
//!
//! # One dispatch thread, arbitrarily many requests in flight
//!
//! `fuser`'s session loop is a single thread reading one request at a time out
//! of `/dev/fuse`, and every callback it makes is synchronous. Nothing here may
//! wait: a callback that blocked on a network round trip would make the whole
//! mount strictly serial, and one slow `READ` would hold up every `GETATTR`
//! behind it — the precise property the pipelined multiplexer underneath exists
//! to avoid. So each callback copies its parameters, moves the (owned, `Send`)
//! reply object into a task, hands the task to the tokio runtime and returns.
//! Concurrency in FUSE then maps one-to-one onto concurrency on the wire
//! (spec §7). The cost of the shape is that ordering between callbacks is the
//! kernel's business, not ours — which is already true of FUSE.
//!
//! # `st_ino` is the node id, and it has to be
//!
//! FUSE carries two numbers per inode: `nodeid`, the handle the kernel names
//! the file by in every later request, and `attr.ino`, the `st_ino` userspace
//! sees. `fuser` 0.15 conflates them — [`fuser::ReplyEntry::entry`],
//! `ReplyCreate::created` and `ReplyDirectoryPlus::add` all take the nodeid
//! from `attr.ino` and ignore any separately supplied one. Since the nodeid is
//! the load the server can act on, `attr.ino` here is the server's [`NodeId`],
//! never the exported file's real inode number.
//!
//! That is less of a compromise than it sounds. The server's node table keys on
//! `(st_dev, st_ino)`, so hard links to one file share one node id and still
//! report one `st_ino`; and because an export may span mounts, two files with
//! equal `st_ino` on different devices would otherwise look like one file to a
//! client that sees a single `st_dev`. The node id separates them. What is lost
//! is the *value*: `stat` inside the mount does not report the number `stat` on
//! the server would. Only `READDIR` — which carries no node ids at all — still
//! reports the server's inode numbers, so `ls -i` can disagree with itself
//! depending on whether the kernel served a listing from `READDIR` or
//! `READDIRPLUS`. Cosmetic, and the alternative is a mount that cannot address
//! its own files.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::consts::{
    FOPEN_KEEP_CACHE, FUSE_DO_READDIRPLUS, FUSE_READDIRPLUS_AUTO, FUSE_WRITEBACK_CACHE,
};
use fuser::{
    KernelConfig, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyLseek, ReplyOpen, ReplyStatfs, ReplyWrite,
    ReplyXattr, Request, TimeOrNow,
};
use lbfs_proto::ops::CopyFileRangeRequest;
use lbfs_proto::types::{Entry, FileAttr, FileKind, NodeId, SetattrArgs, StatfsReply, TimeSet};
use lbfs_proto::Errno;

use crate::conn::Connection;

/// How many bytes of directory page to ask the server for per round trip.
///
/// The kernel hands `READDIR` and `READDIRPLUS` a buffer of one page, so a
/// smaller ask would cost a round trip per 4 KiB of listing. Asking for a
/// little more fills that buffer in one call and leaves a tail the client
/// discards — cheap, because the server answers every page of a directory out
/// of the snapshot it took at `OPENDIR`, so re-requesting the tail on the next
/// call costs it no syscalls. The server clamps the ask to what a reply frame
/// can legally carry, so this number needs no relationship to the negotiated
/// limits.
const READDIR_PAGE_BYTES: u32 = 8 << 10;

// ---------------------------------------------------------------------------
// Attribute conversion
// ---------------------------------------------------------------------------

/// The `S_IFMT` bits as `fuser` spells them.
///
/// An unrecognized type becomes a regular file rather than an error. The
/// kernel's `fuse_invalid_attr` rejects an attribute whose mode names no known
/// type, and it rejects it by failing the *whole* request — a single odd entry
/// would take a directory listing down with it. A file that reads as regular is
/// the containable answer.
pub fn kind_of(mode: u32) -> fuser::FileType {
    match mode & libc::S_IFMT {
        libc::S_IFDIR => fuser::FileType::Directory,
        libc::S_IFLNK => fuser::FileType::Symlink,
        libc::S_IFIFO => fuser::FileType::NamedPipe,
        libc::S_IFSOCK => fuser::FileType::Socket,
        libc::S_IFCHR => fuser::FileType::CharDevice,
        libc::S_IFBLK => fuser::FileType::BlockDevice,
        libc::S_IFREG => fuser::FileType::RegularFile,
        other => {
            tracing::debug!(
                mode = format!("{other:#o}"),
                "unknown S_IFMT; reporting a regular file"
            );
            fuser::FileType::RegularFile
        }
    }
}

/// The wire's directory-entry type, for `READDIR`, which reports a kind rather
/// than a mode.
fn kind_of_wire(kind: FileKind) -> fuser::FileType {
    match kind {
        FileKind::Regular => fuser::FileType::RegularFile,
        FileKind::Directory => fuser::FileType::Directory,
        FileKind::Symlink => fuser::FileType::Symlink,
        FileKind::Socket => fuser::FileType::Socket,
        FileKind::Fifo => fuser::FileType::NamedPipe,
        FileKind::CharDevice => fuser::FileType::CharDevice,
        FileKind::BlockDevice => fuser::FileType::BlockDevice,
    }
}

/// Wire attributes to the kernel's, with `node` as the reported `st_ino`.
///
/// The node id is a parameter and not `a.ino` on purpose: `fuser` sends
/// `attr.ino` as the FUSE `nodeid`, so getting this wrong does not produce a
/// cosmetically odd `stat`, it produces a mount whose every follow-up request
/// names a file the server has never heard of. See the module documentation.
pub fn to_fuse_attr(node: NodeId, a: &FileAttr) -> fuser::FileAttr {
    fuser::FileAttr {
        ino: node,
        size: a.size,
        blocks: a.blocks,
        atime: to_system_time(a.atime_sec, a.atime_nsec),
        mtime: to_system_time(a.mtime_sec, a.mtime_nsec),
        ctime: to_system_time(a.ctime_sec, a.ctime_nsec),
        // Linux has no birth time in `struct stat`, and the wire carries none.
        crtime: UNIX_EPOCH,
        kind: kind_of(a.mode),
        // The full permission word, so setuid, setgid and the sticky bit
        // survive: `fuser` rebuilds the mode as `kind | perm`.
        perm: (a.mode & 0o7777) as u16,
        nlink: a.nlink,
        uid: a.uid,
        gid: a.gid,
        rdev: a.rdev,
        blksize: a.blksize,
        // macOS `chflags`; nothing on Linux sets it.
        flags: 0,
    }
}

/// POSIX `(sec, nsec)` to `SystemTime`, where `nsec` is always non-negative and
/// a pre-epoch time therefore has its seconds rounded *down*.
///
/// Saturating rather than panicking: these numbers come off the wire, and a
/// peer that reported year 300-billion should not abort the mount.
fn to_system_time(sec: i64, nsec: u32) -> SystemTime {
    let nsec = nsec.min(999_999_999);
    let whole = Duration::from_secs(sec.unsigned_abs());
    let base = if sec >= 0 {
        UNIX_EPOCH.checked_add(whole)
    } else {
        UNIX_EPOCH.checked_sub(whole)
    };
    base.and_then(|t| t.checked_add(Duration::new(0, nsec)))
        .unwrap_or(UNIX_EPOCH)
}

/// The inverse, for the times `SETATTR` carries back.
fn split_system_time(t: SystemTime) -> (i64, u32) {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(before) => {
            let d = before.duration();
            let secs = -(d.as_secs() as i64);
            match d.subsec_nanos() {
                0 => (secs, 0),
                // A fraction of a second before the epoch is one whole second
                // earlier plus its complement, because `nsec` cannot be
                // negative.
                nsec => (secs - 1, 1_000_000_000 - nsec),
            }
        }
    }
}

fn time_set(t: Option<TimeOrNow>) -> TimeSet {
    match t {
        None => TimeSet::Omit,
        Some(TimeOrNow::Now) => TimeSet::Now,
        Some(TimeOrNow::SpecificTime(t)) => {
            let (sec, nsec) = split_system_time(t);
            TimeSet::Set { sec, nsec }
        }
    }
}

/// What a `READDIRPLUS` entry reports as its inode, which `fuser` also sends as
/// its nodeid.
///
/// Three cases, and the middle one is the reason this is a function:
///
/// * A resolved entry reports its node id, because that is what the kernel will
///   name the file by from here on.
/// * `.` and `..` come back with node 0 and the *directory's* attributes. The
///   kernel skips linking a dentry for either name before it looks at the
///   nodeid, so passing the server's inode number is safe, and it keeps the
///   dots agreeing with what plain `READDIR` reports for them.
/// * Anything else with node 0 is a name the server listed but could not
///   resolve — unlinked between `OPENDIR` and this page. Its inode must be
///   zero. The server does send the snapshot's real one, which would keep
///   glibc from dropping the dirent, but `fuser` would then also send it as the
///   nodeid, and the kernel would instantiate a dentry naming a node the server
///   never registered — an inode number that may well collide with a live node
///   id. A name that may not survive `readdir(3)` is a smaller loss than a
///   dentry pointing at the wrong file.
fn direntplus_ino(name: &[u8], entry: &Entry) -> u64 {
    if entry.node != 0 {
        entry.node
    } else if name == b"." || name == b".." {
        entry.attr.ino
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Mount and init configuration
// ---------------------------------------------------------------------------

/// One capability this client asks the kernel for, and what its absence costs.
struct Capability {
    bit: u32,
    name: &'static str,
    /// Whether a kernel without it makes the mount wrong rather than merely
    /// slow.
    required: bool,
}

/// The capabilities this client asks the kernel for.
///
/// Deliberately short, and `FUSE_ATOMIC_O_TRUNC` is deliberately absent: the
/// server drops `O_TRUNC` from an `OPEN`'s flags because honoring it there
/// would let a plain open destroy data, and truncation rides `SETATTR` instead.
/// Asking for atomic `O_TRUNC` would tell the kernel to stop sending that
/// `SETATTR`, and `open(O_TRUNC)` would silently keep the old contents.
/// `fuser`'s own default set is `FUSE_ASYNC_READ | FUSE_BIG_WRITES` plus
/// `FUSE_MAX_PAGES`, and none of those is it either.
///
/// A list rather than one bitmask because [`KernelConfig::add_capabilities`] is
/// all or nothing: one unsupported bit in a combined ask makes it add none of
/// them, so a kernel too old for the writeback cache would quietly cost this
/// mount `READDIRPLUS` as well.
fn capabilities(writeback: bool) -> Vec<Capability> {
    let mut caps = vec![
        // What makes READDIRPLUS available at all. Without it a listing costs
        // one LOOKUP round trip per name, which is a throughput loss and
        // nothing worse.
        Capability {
            bit: FUSE_DO_READDIRPLUS,
            name: "FUSE_DO_READDIRPLUS",
            required: false,
        },
        // Lets the kernel drop back to plain READDIR for a listing whose
        // attributes nothing asked for.
        Capability {
            bit: FUSE_READDIRPLUS_AUTO,
            name: "FUSE_READDIRPLUS_AUTO",
            required: false,
        },
    ];
    if writeback {
        // Required, because the server has already been told. `HELLO` carried
        // this flag, and on the strength of it the server clears `O_APPEND`
        // from every open and promotes `O_WRONLY` to `O_RDWR`. A kernel that
        // then refused the writeback cache would compute append offsets from
        // an `i_size` nothing keeps fresh, against a descriptor whose
        // server-side `O_APPEND` — the thing that made an append atomic — has
        // been taken away. Mounting anyway would corrupt appends silently;
        // `--no-writeback` is the honest way to run on such a kernel.
        caps.push(Capability {
            bit: FUSE_WRITEBACK_CACHE,
            name: "FUSE_WRITEBACK_CACHE",
            required: true,
        });
    }
    caps
}

/// Flags on an `OPEN`/`CREATE` reply.
///
/// `FOPEN_KEEP_CACHE` unconditionally (spec §7): without it the kernel throws
/// away an inode's cached pages on every open, so a file read twice is fetched
/// twice. One client owns the export, so nothing can invalidate that cache
/// behind the mount's back, and this holds whether or not writes are cached —
/// `--no-writeback` turns off dirty-page aggregation, not reading.
fn open_flags() -> u32 {
    FOPEN_KEEP_CACHE
}

/// The mount options this client uses.
///
/// `max_read` is the one that is not decoration. The negotiated I/O ceiling
/// reaches the kernel's write path through `KernelConfig::set_max_write`, but
/// `fuser` exposes no equivalent for reads and the kernel's default is far
/// larger. Without this option the kernel would issue reads the multiplexer
/// refuses with `EINVAL` — an unreadable file for no visible reason — so the
/// number is fixed here, at mount time, from the same negotiation.
///
/// `default_permissions` is what makes the kernel enforce the server's
/// reported mode bits locally, which is why `ACCESS` is `ENOSYS` server-side
/// (spec §7).
pub fn mount_options(max_io_size: u32, allow_other: bool, auto_unmount: bool) -> Vec<MountOption> {
    let mut opts = vec![
        MountOption::FSName("lbfs".to_string()),
        MountOption::DefaultPermissions,
        MountOption::CUSTOM(format!("max_read={max_io_size}")),
    ];
    if allow_other {
        opts.push(MountOption::AllowOther);
    }
    if auto_unmount {
        // Off by default, and a flag rather than a constant, because
        // `fusermount3` only honors it for a mount that also carries
        // `allow_other` — `fuser` adds that implicitly — and only when
        // `user_allow_other` is set in `/etc/fuse.conf`. Widening a private
        // mount to every user on the machine is not a default, and on a host
        // without that line it is not a default that works.
        opts.push(MountOption::AutoUnmount);
    }
    opts
}

// ---------------------------------------------------------------------------
// The filesystem
// ---------------------------------------------------------------------------

/// The `fuser::Filesystem` implementation: one connection, one runtime, one
/// cache lifetime.
pub struct LbfsFuse {
    conn: Arc<Connection>,
    rt: tokio::runtime::Handle,
    /// Both the entry and the attribute timeout (spec §7). Zero disables
    /// kernel caching of both.
    ttl: Duration,
    writeback: bool,
}

impl LbfsFuse {
    pub fn new(
        conn: Arc<Connection>,
        rt: tokio::runtime::Handle,
        ttl: Duration,
        writeback: bool,
    ) -> LbfsFuse {
        LbfsFuse {
            conn,
            rt,
            ttl,
            writeback,
        }
    }

    /// What every callback captures before it spawns.
    fn ctx(&self) -> (Arc<Connection>, Duration) {
        (Arc::clone(&self.conn), self.ttl)
    }
}

/// A `u16` errno as the kernel wants it. Every error out of the multiplexer
/// arrives this way, including the `EIO` a dead connection answers with, so
/// disconnection needs no handling of its own here (spec §7).
fn errno(e: Errno) -> libc::c_int {
    e.0 as libc::c_int
}

fn reply_entry(reply: ReplyEntry, ttl: Duration, r: Result<Entry, Errno>) {
    match r {
        Ok(e) => reply.entry(&ttl, &to_fuse_attr(e.node, &e.attr), e.generation),
        Err(e) => reply.error(errno(e)),
    }
}

fn reply_attr(reply: ReplyAttr, ttl: Duration, node: NodeId, r: Result<FileAttr, Errno>) {
    match r {
        Ok(a) => reply.attr(&ttl, &to_fuse_attr(node, &a)),
        Err(e) => reply.error(errno(e)),
    }
}

fn reply_unit(reply: ReplyEmpty, r: Result<(), Errno>) {
    match r {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(errno(e)),
    }
}

fn reply_data(reply: ReplyData, r: Result<Vec<u8>, Errno>) {
    match r {
        Ok(bytes) => reply.data(&bytes),
        Err(e) => reply.error(errno(e)),
    }
}

/// FUSE's two-phase xattr read, both halves.
///
/// `size == 0` asks only for the length; anything else asks for the value and
/// wants `ERANGE` when it does not fit. The server answers the second case with
/// `ERANGE` itself, straight from the syscall — the check here is what keeps
/// the contract true whatever the backend does.
fn reply_xattr(reply: ReplyXattr, size: u32, r: Result<(u32, Vec<u8>), Errno>) {
    match r {
        Ok((len, _)) if size == 0 => reply.size(len),
        Ok((len, _)) if len > size => reply.error(libc::ERANGE),
        Ok((_, value)) => reply.data(&value),
        Err(e) => reply.error(errno(e)),
    }
}

fn reply_statfs(reply: ReplyStatfs, r: Result<StatfsReply, Errno>) {
    match r {
        Ok(s) => reply.statfs(
            s.blocks, s.bfree, s.bavail, s.files, s.ffree, s.bsize, s.namelen, s.frsize,
        ),
        Err(e) => reply.error(errno(e)),
    }
}

impl fuser::Filesystem for LbfsFuse {
    fn init(&mut self, _req: &Request<'_>, config: &mut KernelConfig) -> Result<(), libc::c_int> {
        let max_io = self.conn.limits.max_io_size;
        // The kernel's default write ceiling is 16 MiB, far above anything the
        // handshake settled on; a write over the negotiated size is refused by
        // the multiplexer with `EINVAL` rather than travelling, so this is not
        // tuning but the other half of the `max_read` mount option. Fatal
        // rather than warned about: a mount whose every large write returns
        // `EINVAL` is worse than one that never came up.
        if let Err(nearest) = config.set_max_write(max_io) {
            tracing::error!(
                max_io,
                nearest,
                "the kernel will not accept the negotiated write ceiling"
            );
            return Err(libc::EPROTO);
        }
        // Advisory: the kernel reports its own ceiling and refuses anything
        // above it, which is fine — readahead below the I/O size costs
        // throughput, never correctness.
        if let Err(nearest) = config.set_max_readahead(max_io) {
            tracing::debug!(max_io, nearest, "max_readahead clamped by the kernel");
        }
        for cap in capabilities(self.writeback) {
            if config.add_capabilities(cap.bit).is_ok() {
                continue;
            }
            if cap.required {
                tracing::error!(
                    capability = cap.name,
                    "this kernel does not support a capability the server has \
                     already been promised; re-run with --no-writeback"
                );
                return Err(libc::EPROTO);
            }
            tracing::warn!(capability = cap.name, "unsupported by this kernel");
        }
        tracing::info!(
            max_io,
            writeback = self.writeback,
            ttl = ?self.ttl,
            "mount initialized"
        );
        Ok(())
    }

    fn destroy(&mut self) {
        let dropped = self.conn.dropped_forgets();
        if dropped > 0 {
            tracing::warn!(dropped, "forgets were dropped during this mount");
        }
        tracing::info!("mount torn down");
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let (conn, ttl) = self.ctx();
        let name = name.as_bytes().to_vec();
        self.rt
            .spawn(async move { reply_entry(reply, ttl, conn.lookup(parent, &name).await) });
    }

    /// No reply object, no way to wait: [`Connection::send_forget`] is
    /// synchronous and batches behind the scenes, so this must not spawn.
    fn forget(&mut self, _req: &Request<'_>, ino: u64, nlookup: u64) {
        self.conn.send_forget(ino, nlookup);
    }

    fn batch_forget(&mut self, _req: &Request<'_>, nodes: &[fuser::fuse_forget_one]) {
        for node in nodes {
            self.conn.send_forget(node.nodeid, node.nlookup);
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, fh: Option<u64>, reply: ReplyAttr) {
        let (conn, ttl) = self.ctx();
        self.rt
            .spawn(async move { reply_attr(reply, ttl, ino, conn.getattr(ino, fh).await) });
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        // Dropped, and it has to be: there is no syscall that sets a ctime
        // directly, so no backend could honor it. The kernel sends it beside
        // an mtime when writeback caching is on, and the mtime it rides with
        // moves the ctime anyway.
        _ctime: Option<SystemTime>,
        fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        // BSD file flags. Linux has no `chflags`.
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let (conn, ttl) = self.ctx();
        let args = SetattrArgs {
            mode,
            uid,
            gid,
            size,
            atime: time_set(atime),
            mtime: time_set(mtime),
            fh,
        };
        self.rt
            .spawn(async move { reply_attr(reply, ttl, ino, conn.setattr(ino, args).await) });
    }

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        let (conn, _) = self.ctx();
        self.rt
            .spawn(async move { reply_data(reply, conn.readlink(ino).await) });
    }

    /// No `MKNOD` opcode: the protocol creates regular files through `CREATE`
    /// and everything else not at all. `ENOSYS` lets the kernel remember that.
    fn mknod(
        &mut self,
        _req: &Request<'_>,
        _parent: u64,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        reply.error(libc::ENOSYS);
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        // Already applied to `mode` by the kernel: this client does not request
        // `FUSE_DONT_MASK`, so there is nothing left to mask.
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let (conn, ttl) = self.ctx();
        let name = name.as_bytes().to_vec();
        self.rt
            .spawn(async move { reply_entry(reply, ttl, conn.mkdir(parent, &name, mode).await) });
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let (conn, _) = self.ctx();
        let name = name.as_bytes().to_vec();
        self.rt
            .spawn(async move { reply_unit(reply, conn.unlink(parent, &name).await) });
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let (conn, _) = self.ctx();
        let name = name.as_bytes().to_vec();
        self.rt
            .spawn(async move { reply_unit(reply, conn.rmdir(parent, &name).await) });
    }

    fn symlink(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        link_name: &OsStr,
        target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        let (conn, ttl) = self.ctx();
        let name = link_name.as_bytes().to_vec();
        let target = target.as_os_str().as_bytes().to_vec();
        self.rt.spawn(async move {
            reply_entry(reply, ttl, conn.symlink(parent, &name, &target).await)
        });
    }

    /// `flags` carries `RENAME_NOREPLACE` and `RENAME_EXCHANGE` straight
    /// through to the server's `renameat2`, which is what preserves the
    /// atomicity the caller asked for (spec §8).
    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        let (conn, _) = self.ctx();
        let name = name.as_bytes().to_vec();
        let newname = newname.as_bytes().to_vec();
        self.rt.spawn(async move {
            reply_unit(
                reply,
                conn.rename(parent, &name, newparent, &newname, flags).await,
            )
        });
    }

    fn link(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        newparent: u64,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let (conn, ttl) = self.ctx();
        let newname = newname.as_bytes().to_vec();
        self.rt.spawn(
            async move { reply_entry(reply, ttl, conn.link(ino, newparent, &newname).await) },
        );
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let (conn, _) = self.ctx();
        self.rt.spawn(async move {
            match conn.open(ino, flags as u32).await {
                Ok(fh) => reply.opened(fh, open_flags()),
                Err(e) => reply.error(errno(e)),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let (conn, ttl) = self.ctx();
        let name = name.as_bytes().to_vec();
        self.rt.spawn(async move {
            match conn.create(parent, &name, mode, flags as u32).await {
                Ok((e, fh)) => reply.created(
                    &ttl,
                    &to_fuse_attr(e.node, &e.attr),
                    e.generation,
                    fh,
                    open_flags(),
                ),
                Err(e) => reply.error(errno(e)),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let (conn, _) = self.ctx();
        self.rt.spawn(async move {
            match u64::try_from(offset) {
                Ok(offset) => reply_data(reply, conn.read(ino, fh, offset, size).await),
                Err(_) => reply.error(libc::EINVAL),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let (conn, _) = self.ctx();
        // The slice borrows the session's single receive buffer, which is
        // reused the moment this callback returns. The copy is what lets the
        // write outlive the callback.
        let data = data.to_vec();
        self.rt.spawn(async move {
            let Ok(offset) = u64::try_from(offset) else {
                reply.error(libc::EINVAL);
                return;
            };
            match conn.write(ino, fh, offset, data).await {
                Ok(written) => reply.written(written),
                Err(e) => reply.error(errno(e)),
            }
        });
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        let (conn, _) = self.ctx();
        self.rt
            .spawn(async move { reply_unit(reply, conn.flush(ino, fh).await) });
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let (conn, _) = self.ctx();
        self.rt
            .spawn(async move { reply_unit(reply, conn.release(ino, fh).await) });
    }

    fn fsync(&mut self, _req: &Request<'_>, ino: u64, fh: u64, datasync: bool, reply: ReplyEmpty) {
        let (conn, _) = self.ctx();
        self.rt
            .spawn(async move { reply_unit(reply, conn.fsync(ino, fh, datasync).await) });
    }

    fn opendir(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        let (conn, _) = self.ctx();
        self.rt.spawn(async move {
            match conn.opendir(ino).await {
                Ok(dh) => reply.opened(dh, 0),
                Err(e) => reply.error(errno(e)),
            }
        });
    }

    /// Pages from the server until the kernel's buffer is full or the listing
    /// ends.
    ///
    /// The offset is an opaque cursor, not a byte count: it is the `d_off` the
    /// server's `getdents64` reported, reinterpreted as `u64` on the way out
    /// and back again on the way in, so a filesystem that packs a hash into the
    /// high bit round-trips unchanged. Only zero has a meaning of its own —
    /// the start of the listing.
    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let (conn, _) = self.ctx();
        self.rt.spawn(async move {
            let mut cursor = offset as u64;
            loop {
                let page = match conn.readdir(ino, fh, cursor, READDIR_PAGE_BYTES).await {
                    Ok(page) => page,
                    Err(e) => return reply.error(errno(e)),
                };
                // An empty non-final page would leave the cursor where it is
                // and spin here forever. The server does not produce one; the
                // check is what keeps a server that did from hanging the mount
                // rather than merely truncating a listing.
                let done = page.end || page.entries.is_empty();
                for e in &page.entries {
                    cursor = e.offset;
                    // The real inode from the server's `getdents64`. Plain
                    // `READDIR` instantiates nothing, so this number is only
                    // ever `d_ino`, and a zero would have glibc drop the name.
                    if reply.add(
                        e.ino,
                        e.offset as i64,
                        kind_of_wire(e.kind),
                        OsStr::from_bytes(&e.name),
                    ) {
                        return reply.ok();
                    }
                }
                if done {
                    return reply.ok();
                }
            }
        });
    }

    fn readdirplus(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let (conn, ttl) = self.ctx();
        self.rt.spawn(async move {
            let mut cursor = offset as u64;
            loop {
                let page = match conn.readdirplus(ino, fh, cursor, READDIR_PAGE_BYTES).await {
                    Ok(page) => page,
                    Err(e) => return reply.error(errno(e)),
                };
                let done = page.end || page.entries.is_empty();
                for e in &page.entries {
                    cursor = e.offset;
                    // Every name the server sent is emitted, including the ones
                    // it could not resolve: dropping one would make the listing
                    // disagree with `READDIR`, and the kernel takes no lookup
                    // count for an entry whose nodeid is zero, so nothing is
                    // owed for it either.
                    let node = direntplus_ino(&e.name, &e.entry);
                    let attr = to_fuse_attr(node, &e.entry.attr);
                    if reply.add(
                        node,
                        e.offset as i64,
                        OsStr::from_bytes(&e.name),
                        &ttl,
                        &attr,
                        e.entry.generation,
                    ) {
                        return reply.ok();
                    }
                }
                if done {
                    return reply.ok();
                }
            }
        });
    }

    fn releasedir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        let (conn, _) = self.ctx();
        self.rt
            .spawn(async move { reply_unit(reply, conn.releasedir(ino, fh).await) });
    }

    fn fsyncdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        let (conn, _) = self.ctx();
        self.rt
            .spawn(async move { reply_unit(reply, conn.fsyncdir(ino, fh, datasync).await) });
    }

    fn statfs(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyStatfs) {
        let (conn, _) = self.ctx();
        self.rt
            .spawn(async move { reply_statfs(reply, conn.statfs(ino).await) });
    }

    #[allow(clippy::too_many_arguments)]
    fn setxattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        // macOS resource forks. Linux always sends zero.
        _position: u32,
        reply: ReplyEmpty,
    ) {
        let (conn, _) = self.ctx();
        let name = name.as_bytes().to_vec();
        let value = value.to_vec();
        self.rt.spawn(async move {
            reply_unit(reply, conn.setxattr(ino, &name, value, flags as u32).await)
        });
    }

    fn getxattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        name: &OsStr,
        size: u32,
        reply: ReplyXattr,
    ) {
        let (conn, _) = self.ctx();
        let name = name.as_bytes().to_vec();
        self.rt
            .spawn(async move { reply_xattr(reply, size, conn.getxattr(ino, &name, size).await) });
    }

    fn listxattr(&mut self, _req: &Request<'_>, ino: u64, size: u32, reply: ReplyXattr) {
        let (conn, _) = self.ctx();
        self.rt
            .spawn(async move { reply_xattr(reply, size, conn.listxattr(ino, size).await) });
    }

    fn removexattr(&mut self, _req: &Request<'_>, ino: u64, name: &OsStr, reply: ReplyEmpty) {
        let (conn, _) = self.ctx();
        let name = name.as_bytes().to_vec();
        self.rt
            .spawn(async move { reply_unit(reply, conn.removexattr(ino, &name).await) });
    }

    fn fallocate(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        length: i64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        let (conn, _) = self.ctx();
        self.rt.spawn(async move {
            let (Ok(offset), Ok(length)) = (u64::try_from(offset), u64::try_from(length)) else {
                reply.error(libc::EINVAL);
                return;
            };
            reply_unit(
                reply,
                conn.fallocate(ino, fh, offset, length, mode as u32).await,
            )
        });
    }

    fn lseek(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        whence: i32,
        reply: ReplyLseek,
    ) {
        let (conn, _) = self.ctx();
        self.rt.spawn(async move {
            let Ok(offset) = u64::try_from(offset) else {
                reply.error(libc::EINVAL);
                return;
            };
            match conn.lseek(ino, fh, offset, whence as u32).await {
                // The result is a file offset, so it fits an `i64` on any
                // filesystem the kernel can represent.
                Ok(off) => reply.offset(off as i64),
                Err(e) => reply.error(errno(e)),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_file_range(
        &mut self,
        _req: &Request<'_>,
        ino_in: u64,
        fh_in: u64,
        offset_in: i64,
        ino_out: u64,
        fh_out: u64,
        offset_out: i64,
        len: u64,
        // Reserved by the syscall; must be zero, and the kernel checks.
        _flags: u32,
        reply: ReplyWrite,
    ) {
        let (conn, _) = self.ctx();
        self.rt.spawn(async move {
            let (Ok(off_in), Ok(off_out)) = (u64::try_from(offset_in), u64::try_from(offset_out))
            else {
                reply.error(libc::EINVAL);
                return;
            };
            let req = CopyFileRangeRequest {
                node_in: ino_in,
                fh_in,
                off_in,
                node_out: ino_out,
                fh_out,
                off_out,
                len,
            };
            match conn.copy_file_range(&req).await {
                // FUSE's reply field is 32 bits wide, so a copy larger than
                // that could not be reported whole; the caller sees a short
                // copy and comes back for the rest, which is what
                // `copy_file_range(2)` already allows.
                Ok(copied) => reply.written(copied.min(u32::MAX as u64) as u32),
                Err(e) => reply.error(errno(e)),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_maps_all_s_ifmt_variants() {
        assert_eq!(kind_of(libc::S_IFREG | 0o644), fuser::FileType::RegularFile);
        assert_eq!(kind_of(libc::S_IFDIR | 0o755), fuser::FileType::Directory);
        assert_eq!(kind_of(libc::S_IFLNK | 0o777), fuser::FileType::Symlink);
        assert_eq!(kind_of(libc::S_IFIFO), fuser::FileType::NamedPipe);
        assert_eq!(kind_of(libc::S_IFSOCK), fuser::FileType::Socket);
        assert_eq!(kind_of(libc::S_IFCHR), fuser::FileType::CharDevice);
        assert_eq!(kind_of(libc::S_IFBLK), fuser::FileType::BlockDevice);
        // A mode with no type bits at all — a zeroed attribute — must still
        // name a type the kernel accepts, or `fuse_invalid_attr` fails the
        // whole request it rode in.
        assert_eq!(kind_of(0), fuser::FileType::RegularFile);
    }

    #[test]
    fn every_wire_kind_has_a_fuse_kind() {
        use FileKind::*;
        let pairs = [
            (Regular, fuser::FileType::RegularFile),
            (Directory, fuser::FileType::Directory),
            (Symlink, fuser::FileType::Symlink),
            (Socket, fuser::FileType::Socket),
            (Fifo, fuser::FileType::NamedPipe),
            (CharDevice, fuser::FileType::CharDevice),
            (BlockDevice, fuser::FileType::BlockDevice),
        ];
        for (wire, fuse) in pairs {
            assert_eq!(kind_of_wire(wire), fuse);
        }
    }

    #[test]
    fn attr_conversion_preserves_fields() {
        let a = FileAttr {
            ino: 7,
            size: 42,
            blocks: 1,
            mode: libc::S_IFREG | 0o640,
            nlink: 2,
            uid: 1000,
            gid: 1000,
            blksize: 4096,
            mtime_sec: 100,
            mtime_nsec: 5,
            ..Default::default()
        };
        let f = to_fuse_attr(7, &a);
        assert_eq!(f.ino, 7);
        assert_eq!(f.size, 42);
        assert_eq!(f.blocks, 1);
        assert_eq!(f.perm, 0o640);
        assert_eq!(f.nlink, 2);
        assert_eq!(f.uid, 1000);
        assert_eq!(f.gid, 1000);
        assert_eq!(f.blksize, 4096);
        assert_eq!(f.kind, fuser::FileType::RegularFile);
        assert_eq!(f.mtime, UNIX_EPOCH + Duration::new(100, 5));
    }

    /// The whole permission word travels, not just the low nine bits: a
    /// setuid binary that lost its bit crossing the mount would be a silent
    /// change of behaviour.
    #[test]
    fn attr_conversion_keeps_setuid_setgid_and_sticky() {
        let a = FileAttr {
            mode: libc::S_IFREG | 0o6755,
            ..Default::default()
        };
        assert_eq!(to_fuse_attr(1, &a).perm, 0o6755);
        let d = FileAttr {
            mode: libc::S_IFDIR | 0o1777,
            ..Default::default()
        };
        assert_eq!(to_fuse_attr(1, &d).perm, 0o1777);
    }

    /// The reported `st_ino` is the node id and never the server's inode
    /// number, because `fuser` sends the same field as the FUSE nodeid. A
    /// regression here does not look like a wrong number in `stat`; it looks
    /// like every operation after the first returning `ESTALE`.
    #[test]
    fn attr_conversion_reports_the_node_id_not_the_servers_inode() {
        let a = FileAttr {
            ino: 999_111,
            mode: libc::S_IFREG | 0o644,
            ..Default::default()
        };
        assert_eq!(to_fuse_attr(42, &a).ino, 42);
    }

    #[test]
    fn times_round_trip_across_the_epoch() {
        for (sec, nsec) in [
            (0i64, 0u32),
            (0, 1),
            (1_700_000_000, 999_999_999),
            (-1, 0),
            (-1, 500_000_000),
            (-1_000_000, 123),
        ] {
            let t = to_system_time(sec, nsec);
            assert_eq!(split_system_time(t), (sec, nsec), "for ({sec}, {nsec})");
        }
    }

    /// Wire values are a peer's word, not this process's arithmetic: an
    /// absurd one must saturate rather than panic inside a FUSE callback.
    #[test]
    fn absurd_times_saturate_instead_of_panicking() {
        let _ = to_system_time(i64::MAX, 999_999_999);
        let _ = to_system_time(i64::MIN, 0);
        // A nanosecond field out of range is clamped, not carried into the
        // seconds.
        assert_eq!(split_system_time(to_system_time(5, 2_000_000_000)).0, 5);
    }

    #[test]
    fn time_set_maps_all_three_shapes() {
        assert_eq!(time_set(None), TimeSet::Omit);
        assert_eq!(time_set(Some(TimeOrNow::Now)), TimeSet::Now);
        assert_eq!(
            time_set(Some(TimeOrNow::SpecificTime(
                UNIX_EPOCH + Duration::new(7, 8)
            ))),
            TimeSet::Set { sec: 7, nsec: 8 }
        );
    }

    fn entry(node: NodeId, ino: u64) -> Entry {
        Entry {
            node,
            generation: 0,
            attr: FileAttr {
                ino,
                mode: libc::S_IFREG | 0o644,
                ..Default::default()
            },
        }
    }

    #[test]
    fn readdirplus_reports_the_node_id_for_a_resolved_entry() {
        assert_eq!(direntplus_ino(b"file", &entry(5, 999)), 5);
    }

    /// The kernel skips the dots before it reads the nodeid, so the inode it
    /// carries is free to be the server's — and must be non-zero, or glibc
    /// drops `.` and `..` from every listing.
    #[test]
    fn readdirplus_reports_the_servers_inode_for_the_dots() {
        assert_eq!(direntplus_ino(b".", &entry(0, 77)), 77);
        assert_eq!(direntplus_ino(b"..", &entry(0, 78)), 78);
    }

    /// A name the server listed but could not resolve gets a zero, which
    /// `fuse_direntplus_link` reads as "do not instantiate anything". Handing
    /// over the snapshot's inode instead would make `fuser` send it as the
    /// nodeid, and the kernel would cache a dentry pointing at a node id the
    /// server never issued.
    #[test]
    fn readdirplus_reports_zero_for_an_unresolved_name() {
        assert_eq!(direntplus_ino(b"vanished", &entry(0, 4242)), 0);
        // Not fooled by a name that merely begins with a dot.
        assert_eq!(direntplus_ino(b".hidden", &entry(0, 4242)), 0);
        assert_eq!(direntplus_ino(b"...", &entry(0, 4242)), 0);
    }

    fn requested(writeback: bool) -> u32 {
        capabilities(writeback).iter().fold(0, |all, c| all | c.bit)
    }

    /// The server drops `O_TRUNC` from `OPEN` on purpose and expects the
    /// truncation as a `SETATTR`. Asking the kernel for atomic `O_TRUNC` would
    /// stop that `SETATTR` from ever being sent, and `open(O_TRUNC)` would
    /// leave the old contents in place.
    #[test]
    fn atomic_o_trunc_is_never_requested() {
        for writeback in [true, false] {
            assert_eq!(requested(writeback) & fuser::consts::FUSE_ATOMIC_O_TRUNC, 0);
        }
    }

    #[test]
    fn readdirplus_is_always_requested_and_writeback_only_when_asked() {
        for writeback in [true, false] {
            let caps = requested(writeback);
            assert_eq!(caps & FUSE_DO_READDIRPLUS, FUSE_DO_READDIRPLUS);
            assert_eq!(caps & FUSE_READDIRPLUS_AUTO, FUSE_READDIRPLUS_AUTO);
        }
        assert_eq!(requested(true) & FUSE_WRITEBACK_CACHE, FUSE_WRITEBACK_CACHE);
        assert_eq!(requested(false) & FUSE_WRITEBACK_CACHE, 0);
    }

    /// Only the writeback cache is worth refusing to mount over, because only
    /// it is something the server has already been told in `HELLO` and has
    /// already changed its own behaviour for. Losing `READDIRPLUS` costs round
    /// trips and nothing else.
    #[test]
    fn only_the_promised_capability_is_required() {
        let required: Vec<_> = capabilities(true)
            .into_iter()
            .filter(|c| c.required)
            .map(|c| c.bit)
            .collect();
        assert_eq!(required, vec![FUSE_WRITEBACK_CACHE]);
        assert!(capabilities(false).iter().all(|c| !c.required));
    }

    /// Re-reads stay local whether or not writes are cached: `--no-writeback`
    /// turns off dirty-page aggregation, not the page cache.
    #[test]
    fn opens_keep_the_page_cache() {
        assert_eq!(open_flags() & FOPEN_KEEP_CACHE, FOPEN_KEEP_CACHE);
        assert_eq!(open_flags() & fuser::consts::FOPEN_DIRECT_IO, 0);
    }

    /// `fuser` has no `set_max_read`, so the negotiated ceiling can only reach
    /// the kernel's read path as a mount option. Without it the kernel issues
    /// reads the multiplexer answers with `EINVAL`.
    #[test]
    fn the_mount_pins_max_read_to_the_negotiated_size() {
        let opts = mount_options(4096, false, false);
        assert!(opts.contains(&MountOption::CUSTOM("max_read=4096".to_string())));
        assert!(opts.contains(&MountOption::FSName("lbfs".to_string())));
        assert!(opts.contains(&MountOption::DefaultPermissions));
    }

    /// Neither widens access by default: `allow_other` exposes the mount to
    /// every user on the machine, and `auto_unmount` makes `fuser` add
    /// `allow_other` implicitly.
    #[test]
    fn access_widening_options_are_opt_in() {
        let plain = mount_options(1 << 20, false, false);
        assert!(!plain.contains(&MountOption::AllowOther));
        assert!(!plain.contains(&MountOption::AutoUnmount));

        let wide = mount_options(1 << 20, true, true);
        assert!(wide.contains(&MountOption::AllowOther));
        assert!(wide.contains(&MountOption::AutoUnmount));
    }

    /// `fuser` rejects a list holding two options that contradict each other,
    /// and it rejects it by failing the mount.
    #[test]
    fn the_option_list_holds_no_duplicates_or_conflicts() {
        for (allow_other, auto_unmount) in [(false, false), (true, false), (true, true)] {
            let opts = mount_options(1 << 20, allow_other, auto_unmount);
            let unique: std::collections::HashSet<_> = opts.iter().collect();
            assert_eq!(unique.len(), opts.len(), "{opts:?}");
            // The one pair `fuser` calls a conflict that this list could ever
            // produce.
            assert!(!opts.contains(&MountOption::AllowRoot));
        }
    }
}
