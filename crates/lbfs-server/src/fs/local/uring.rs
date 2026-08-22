//! io_uring completion bridge.
//!
//! This is the only module in the workspace that is allowed to contain
//! `unsafe` (see the `#[allow(unsafe_code)]` on its declaration in
//! [`super`]). Everything the rest of the server does to a file goes through
//! [`UringExecutor`].
//!
//! # Architecture
//!
//! Each *lane* is one OS thread owning one [`IoUring`] and one eventfd.
//! Async callers never touch the ring:
//!
//! 1. A caller builds an [`OpDesc`] plus an [`OpPayload`] that **owns every
//!    allocation the kernel will touch**, pairs them with a
//!    [`oneshot::Sender`], and pushes the pair down an [`mpsc`] channel.
//! 2. The caller writes `1` to the lane's eventfd, then awaits the oneshot.
//! 3. The ring thread drains the channel, moves each task into a slab keyed by
//!    `user_data`, builds the SQE from pointers into the *slab-owned* payload,
//!    and blocks in `submit_and_wait(1)`.
//! 4. On each CQE the slab entry is removed and `(result, payload)` is handed
//!    back through the oneshot; a `user_data` of `0` means the eventfd read
//!    fired and is re-armed.
//!
//! The ordering in steps 1-2 is what makes wakeups lossless: the task is
//! visible in the channel *before* the eventfd counter is bumped, so a ring
//! thread that is about to sleep either sees the task when it drains the
//! channel or is woken by the already-nonzero counter the moment its eventfd
//! read is submitted.
//!
//! # Memory safety contract
//!
//! Two invariants carry the whole module:
//!
//! * **The slab owns every pointer target.** Each pointer handed to an SQE
//!   addresses a heap allocation reachable from the slab entry - a
//!   [`CString`]'s buffer, a [`PooledBuf`]'s `Box<[u8]>`, a
//!   `Box<libc::statx>`, a `Box<OpenHow>`. The slab entry itself may move when
//!   the slab's backing `Vec` grows; the heap allocations it points at do not.
//!   The entry is removed only when its CQE arrives, so the kernel is finished
//!   with the memory before it can be freed - including when the caller drops
//!   the future, because the payload is then dropped on the ring thread after
//!   the CQE, not at cancellation time.
//! * **The slab owns a descriptor reference too.** Every fd argument is an
//!   `&Arc<OwnedFd>`, and the executor clones that `Arc` into the slab entry.
//!   A descriptor therefore cannot close while an operation naming it sits in
//!   the backlog or in flight, even if every caller has dropped its future and
//!   its own handle. Without this the ring thread could submit a
//!   `write`/`unlinkat`/`renameat` against a descriptor number the kernel has
//!   already recycled onto an unrelated file.
//!
//! Buffer lengths are the third leg: [`PooledBuf`] storage is recycled without
//! zeroing, so the ring is never given a length beyond what the caller asked
//! for, and `set_len` on completion uses the CQE result.
//!
//! # Cancellation
//!
//! Dropping a returned future does **not** cancel the operation. It kills only
//! the [`oneshot::Receiver`]; the SQE still runs and its CQE is still reaped.
//! Everything the operation owns - payload buffers, path strings, descriptor
//! references, and the `OwnedFd` an `openat2` produced - is released on the
//! ring thread once that CQE lands. This is what makes cancellation safe rather
//! than merely tolerated, and it is why callers owe the executor nothing beyond
//! passing a live `Arc`.

use std::collections::VecDeque;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;

use io_uring::{opcode, squeue, types, IoUring};
use rustix::event::{eventfd, EventfdFlags};
use tokio::sync::oneshot;

use super::buffers::PooledBuf;

/// `user_data` reserved for the eventfd wakeup read. Real operations use
/// `slab_key + 1`, so zero can never collide with one.
const EVENTFD_USER_DATA: u64 = 0;

const _: () = {
    const fn require_send_sync_clone<T: Send + Sync + Clone>() {}
    require_send_sync_clone::<UringExecutor>();
};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Handle onto a pool of io_uring lanes. Cloning is a refcount bump; all
/// clones share the same lanes and the same round-robin cursor.
///
/// Dropping the last clone shuts the lanes down: the submission channels close,
/// each ring thread is woken through its eventfd, drains the completions it
/// still has in flight, and exits before `drop` returns.
///
/// # Descriptor and cancellation contract
///
/// Every fd argument is an `&Arc<OwnedFd>` and the executor clones it into the
/// operation, so it holds its own reference until the CQE arrives. Two
/// consequences:
///
/// * **Callers owe no liveness beyond passing a live `Arc`.** They may drop
///   their own handle at any point; the descriptor stays open for as long as
///   the ring needs it.
/// * **Dropping a returned future does not cancel the operation.** The write
///   still happens, the directory is still created. Cancellation only discards
///   the result: the payload, the descriptor reference, and any `OwnedFd` the
///   operation produced are all released on the ring thread when the CQE lands.
///
/// Callers that need an operation to *not* happen must not start it.
#[derive(Clone)]
pub struct UringExecutor(Arc<Inner>);

impl UringExecutor {
    /// Start `threads` ring lanes, each with a submission queue of `entries`.
    ///
    /// `entries` also caps how many operations one lane keeps in flight, which
    /// keeps the completion queue (twice `entries` by default) from
    /// overflowing. Work beyond that cap waits in a per-lane backlog.
    pub fn new(threads: usize, entries: u32) -> io::Result<UringExecutor> {
        let threads = threads.max(1);
        let entries = entries.max(1);
        let mut lanes = Vec::with_capacity(threads);
        for i in 0..threads {
            match Lane::spawn(i, entries) {
                Ok(lane) => lanes.push(lane),
                Err(e) => {
                    shutdown_lanes(&mut lanes);
                    return Err(e);
                }
            }
        }
        Ok(UringExecutor(Arc::new(Inner {
            lanes,
            next: AtomicUsize::new(0),
        })))
    }

    /// Hand one operation to a lane and await its completion result.
    async fn submit(&self, op: OpDesc, payload: OpPayload) -> (i32, OpPayload) {
        let (tx, rx) = oneshot::channel();
        let lanes = &self.0.lanes;
        let idx = self.0.next.fetch_add(1, Ordering::Relaxed) % lanes.len();
        let lane = &lanes[idx];
        lane.tx
            .as_ref()
            .expect("submission channel is only taken during shutdown")
            .send(Task {
                op,
                payload,
                done: tx,
            })
            .expect("ring thread alive");
        // Strictly after the send: the ring thread must never see the counter
        // rise without also being able to see the task.
        lane.wake();
        rx.await.expect("ring thread never drops tasks")
    }

    pub async fn read(
        &self,
        fd: &Arc<OwnedFd>,
        offset: u64,
        buf: PooledBuf,
        len: u32,
    ) -> (PooledBuf, io::Result<u32>) {
        assert!(
            len as usize <= buf.capacity(),
            "read length exceeds buffer capacity"
        );
        let (res, payload) = self
            .submit(
                OpDesc::Read {
                    fd: fd.clone(),
                    offset,
                    len,
                },
                OpPayload::Rw { buf },
            )
            .await;
        let OpPayload::Rw { buf } = payload else {
            unreachable!("read completed with a non-Rw payload")
        };
        finish_transfer(buf, res)
    }

    pub async fn write(
        &self,
        fd: &Arc<OwnedFd>,
        offset: u64,
        buf: PooledBuf,
        len: u32,
    ) -> (PooledBuf, io::Result<u32>) {
        assert!(
            len as usize <= buf.capacity(),
            "write length exceeds buffer capacity"
        );
        let (res, payload) = self
            .submit(
                OpDesc::Write {
                    fd: fd.clone(),
                    offset,
                    len,
                },
                OpPayload::Rw { buf },
            )
            .await;
        let OpPayload::Rw { buf } = payload else {
            unreachable!("write completed with a non-Rw payload")
        };
        // Unlike read, the caller's bytes are unchanged, so the logical length
        // stays whatever the caller set.
        (buf, unit_or_count(res))
    }

    pub async fn fsync(&self, fd: &Arc<OwnedFd>, datasync: bool) -> io::Result<()> {
        let (res, _) = self
            .submit(
                OpDesc::Fsync {
                    fd: fd.clone(),
                    datasync,
                },
                OpPayload::Bare,
            )
            .await;
        unit(res)
    }

    pub async fn fallocate(
        &self,
        fd: &Arc<OwnedFd>,
        mode: i32,
        offset: u64,
        len: u64,
    ) -> io::Result<()> {
        let (res, _) = self
            .submit(
                OpDesc::Fallocate {
                    fd: fd.clone(),
                    mode,
                    offset,
                    len,
                },
                OpPayload::Bare,
            )
            .await;
        unit(res)
    }

    /// The descriptor is wrapped on the ring thread, not here, so a cancelled
    /// caller cannot leak it: the failed oneshot send drops the payload, which
    /// drops the `OwnedFd`, which closes the file.
    pub async fn openat2(
        &self,
        dirfd: &Arc<OwnedFd>,
        name: CString,
        how: types::OpenHow,
    ) -> io::Result<OwnedFd> {
        let (res, payload) = self
            .submit(
                OpDesc::OpenAt2 {
                    dirfd: dirfd.clone(),
                },
                OpPayload::Open {
                    name,
                    how: Box::new(how),
                    opened: None,
                },
            )
            .await;
        let OpPayload::Open { opened, .. } = payload else {
            unreachable!("openat2 completed with a non-Open payload")
        };
        match opened {
            Some(fd) => Ok(fd),
            None => Err(io::Error::from_raw_os_error(-res)),
        }
    }

    pub async fn statx(
        &self,
        dirfd: &Arc<OwnedFd>,
        name: CString,
        flags: i32,
        mask: u32,
    ) -> io::Result<libc::statx> {
        // SAFETY: `libc::statx` is a `repr(C)` aggregate of integers and nested
        // integer structs with no padding invariants, niches, or pointers, so
        // the all-zero bit pattern is a valid inhabitant. The kernel overwrites
        // the fields selected by `mask` on success; on failure the caller gets
        // an `Err` and never observes the zeros.
        let out: Box<libc::statx> = Box::new(unsafe { std::mem::zeroed() });
        let (res, payload) = self
            .submit(
                OpDesc::Statx {
                    dirfd: dirfd.clone(),
                    flags,
                    mask,
                },
                OpPayload::Statx { name, out },
            )
            .await;
        let OpPayload::Statx { out, .. } = payload else {
            unreachable!("statx completed with a non-Statx payload")
        };
        unit(res).map(|()| *out)
    }

    pub async fn unlinkat(
        &self,
        dirfd: &Arc<OwnedFd>,
        name: CString,
        rmdir: bool,
    ) -> io::Result<()> {
        let (res, _) = self
            .submit(
                OpDesc::UnlinkAt {
                    dirfd: dirfd.clone(),
                    rmdir,
                },
                OpPayload::Path { name },
            )
            .await;
        unit(res)
    }

    pub async fn mkdirat(&self, dirfd: &Arc<OwnedFd>, name: CString, mode: u32) -> io::Result<()> {
        let (res, _) = self
            .submit(
                OpDesc::MkDirAt {
                    dirfd: dirfd.clone(),
                    mode,
                },
                OpPayload::Path { name },
            )
            .await;
        unit(res)
    }

    pub async fn renameat(
        &self,
        olddir: &Arc<OwnedFd>,
        old: CString,
        newdir: &Arc<OwnedFd>,
        new: CString,
        flags: u32,
    ) -> io::Result<()> {
        let (res, _) = self
            .submit(
                OpDesc::RenameAt {
                    olddir: olddir.clone(),
                    newdir: newdir.clone(),
                    flags,
                },
                OpPayload::TwoPath { a: old, b: new },
            )
            .await;
        unit(res)
    }

    pub async fn linkat(
        &self,
        olddir: &Arc<OwnedFd>,
        old: CString,
        newdir: &Arc<OwnedFd>,
        new: CString,
        flags: i32,
    ) -> io::Result<()> {
        let (res, _) = self
            .submit(
                OpDesc::LinkAt {
                    olddir: olddir.clone(),
                    newdir: newdir.clone(),
                    flags,
                },
                OpPayload::TwoPath { a: old, b: new },
            )
            .await;
        unit(res)
    }

    pub async fn symlinkat(
        &self,
        target: CString,
        newdir: &Arc<OwnedFd>,
        name: CString,
    ) -> io::Result<()> {
        let (res, _) = self
            .submit(
                OpDesc::SymlinkAt {
                    newdir: newdir.clone(),
                },
                OpPayload::TwoPath { a: target, b: name },
            )
            .await;
        unit(res)
    }

    pub async fn fgetxattr(
        &self,
        fd: &Arc<OwnedFd>,
        name: CString,
        buf: PooledBuf,
        len: u32,
    ) -> (PooledBuf, io::Result<u32>) {
        assert!(
            len as usize <= buf.capacity(),
            "fgetxattr length exceeds buffer capacity"
        );
        let (res, payload) = self
            .submit(
                OpDesc::FGetXattr {
                    fd: fd.clone(),
                    len,
                },
                OpPayload::XattrRw { buf, name },
            )
            .await;
        let OpPayload::XattrRw { buf, .. } = payload else {
            unreachable!("fgetxattr completed with a non-XattrRw payload")
        };
        finish_transfer(buf, res)
    }

    pub async fn fsetxattr(
        &self,
        fd: &Arc<OwnedFd>,
        name: CString,
        value: PooledBuf,
        len: u32,
        flags: i32,
    ) -> (PooledBuf, io::Result<()>) {
        assert!(
            len as usize <= value.capacity(),
            "fsetxattr length exceeds buffer capacity"
        );
        let (res, payload) = self
            .submit(
                OpDesc::FSetXattr {
                    fd: fd.clone(),
                    len,
                    flags,
                },
                OpPayload::XattrRw { buf: value, name },
            )
            .await;
        let OpPayload::XattrRw { buf, .. } = payload else {
            unreachable!("fsetxattr completed with a non-XattrRw payload")
        };
        (buf, unit(res))
    }
}

fn unit(res: i32) -> io::Result<()> {
    if res < 0 {
        Err(io::Error::from_raw_os_error(-res))
    } else {
        Ok(())
    }
}

fn unit_or_count(res: i32) -> io::Result<u32> {
    if res < 0 {
        Err(io::Error::from_raw_os_error(-res))
    } else {
        Ok(res as u32)
    }
}

/// Common tail for the two ops that fill a buffer: publish the transferred
/// byte count as the buffer's logical length so the recycled tail of a
/// [`PooledBuf`] is never readable as data.
fn finish_transfer(mut buf: PooledBuf, res: i32) -> (PooledBuf, io::Result<u32>) {
    if res < 0 {
        return (buf, Err(io::Error::from_raw_os_error(-res)));
    }
    // Clamp once and report the clamped figure: the count the caller sees must
    // never exceed what `as_slice` will hand back.
    let n = (res as usize).min(buf.capacity());
    buf.set_len(n);
    (buf, Ok(n as u32))
}

// ---------------------------------------------------------------------------
// Lanes
// ---------------------------------------------------------------------------

struct Inner {
    lanes: Vec<Lane>,
    next: AtomicUsize,
}

impl Drop for Inner {
    fn drop(&mut self) {
        shutdown_lanes(&mut self.lanes);
    }
}

struct Lane {
    /// `None` only while shutting down, so `Drop` can close the channel before
    /// the thread is joined.
    tx: Option<Sender<Task>>,
    wake: OwnedFd,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Lane {
    fn spawn(index: usize, entries: u32) -> io::Result<Lane> {
        // Build the fd and the ring on the caller's thread so setup failures
        // are reported by `UringExecutor::new` rather than lost in a thread.
        let wake =
            eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).map_err(io::Error::from)?;
        let ring: IoUring = IoUring::new(entries)?;
        let ring_wake = wake.try_clone()?;
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name(format!("lbfs-uring-{index}"))
            .spawn(move || ring_thread(ring, rx, ring_wake, entries as usize))?;
        Ok(Lane {
            tx: Some(tx),
            wake,
            thread: Some(thread),
        })
    }

    fn wake(&self) {
        poke(&self.wake);
    }
}

/// Bump a lane's eventfd counter.
///
/// The only failure a non-blocking eventfd write can report is `EAGAIN` at
/// counter saturation, which already means the counter is nonzero and the
/// reader will wake. Anything else would require the fd to be invalid, which
/// cannot happen while we own it.
fn poke(fd: &OwnedFd) {
    let _ = rustix::io::write(fd, &1u64.to_ne_bytes());
}

/// Close every lane's channel, wake its thread so it notices, and join.
fn shutdown_lanes(lanes: &mut Vec<Lane>) {
    for lane in lanes.iter_mut() {
        // Dropping the sender alone is not enough: the thread is asleep inside
        // `submit_and_wait` and only the eventfd can bring it back.
        lane.tx = None;
        lane.wake();
    }
    for lane in lanes.iter_mut() {
        if let Some(handle) = lane.thread.take() {
            let _ = handle.join();
        }
    }
    lanes.clear();
}

// ---------------------------------------------------------------------------
// Work items
// ---------------------------------------------------------------------------

/// Everything the kernel will dereference, owned by one value that the slab
/// holds for the lifetime of the operation.
enum OpPayload {
    Rw {
        buf: PooledBuf,
    },
    XattrRw {
        buf: PooledBuf,
        name: CString,
    },
    Path {
        name: CString,
    },
    TwoPath {
        a: CString,
        b: CString,
    },
    Open {
        name: CString,
        how: Box<types::OpenHow>,
        /// Filled in by `reap` from a successful CQE. Living in the payload is
        /// what makes cancellation close the file: a failed oneshot send drops
        /// the payload, and the `OwnedFd` with it.
        opened: Option<OwnedFd>,
    },
    Statx {
        name: CString,
        out: Box<libc::statx>,
    },
    Bare,
}

/// Non-pointer arguments: descriptors, offsets, lengths, flags.
///
/// The descriptors are `Arc<OwnedFd>` rather than `RawFd` on purpose. The slab
/// holds this value for as long as the operation lives, so cloning the `Arc`
/// here is what keeps the file open across a caller's cancellation.
enum OpDesc {
    Read {
        fd: Arc<OwnedFd>,
        offset: u64,
        len: u32,
    },
    Write {
        fd: Arc<OwnedFd>,
        offset: u64,
        len: u32,
    },
    Fsync {
        fd: Arc<OwnedFd>,
        datasync: bool,
    },
    Fallocate {
        fd: Arc<OwnedFd>,
        mode: i32,
        offset: u64,
        len: u64,
    },
    OpenAt2 {
        dirfd: Arc<OwnedFd>,
    },
    Statx {
        dirfd: Arc<OwnedFd>,
        flags: i32,
        mask: u32,
    },
    UnlinkAt {
        dirfd: Arc<OwnedFd>,
        rmdir: bool,
    },
    MkDirAt {
        dirfd: Arc<OwnedFd>,
        mode: u32,
    },
    RenameAt {
        olddir: Arc<OwnedFd>,
        newdir: Arc<OwnedFd>,
        flags: u32,
    },
    LinkAt {
        olddir: Arc<OwnedFd>,
        newdir: Arc<OwnedFd>,
        flags: i32,
    },
    SymlinkAt {
        newdir: Arc<OwnedFd>,
    },
    FGetXattr {
        fd: Arc<OwnedFd>,
        len: u32,
    },
    FSetXattr {
        fd: Arc<OwnedFd>,
        len: u32,
        flags: i32,
    },
}

/// One unit of work: on the channel before submission, in the slab after it.
/// `op` stays alive alongside `payload` because it holds the descriptor
/// references the in-flight SQE names.
struct Task {
    op: OpDesc,
    payload: OpPayload,
    done: oneshot::Sender<(i32, OpPayload)>,
}

/// Build the SQE for one operation.
///
/// Every pointer produced here is taken from `payload`, which the caller has
/// already moved into the slab. Raw pointers into heap allocations survive the
/// slab's `Vec` reallocating, and the slab entry outlives the CQE, so each
/// pointer is valid for the kernel's entire use of it.
fn build_sqe(task: &mut Task) -> squeue::Entry {
    let Task { op, payload, .. } = task;
    match (&*op, payload) {
        (OpDesc::Read { fd, offset, len }, OpPayload::Rw { buf }) => {
            let ptr = buf.as_mut_slice().as_mut_ptr();
            opcode::Read::new(types::Fd(fd.as_raw_fd()), ptr, *len)
                .offset(*offset)
                .build()
        }
        (OpDesc::Write { fd, offset, len }, OpPayload::Rw { buf }) => {
            let ptr = buf.as_mut_slice().as_ptr();
            opcode::Write::new(types::Fd(fd.as_raw_fd()), ptr, *len)
                .offset(*offset)
                .build()
        }
        (OpDesc::Fsync { fd, datasync }, OpPayload::Bare) => {
            let flags = if *datasync {
                types::FsyncFlags::DATASYNC
            } else {
                types::FsyncFlags::empty()
            };
            opcode::Fsync::new(types::Fd(fd.as_raw_fd()))
                .flags(flags)
                .build()
        }
        (
            OpDesc::Fallocate {
                fd,
                mode,
                offset,
                len,
            },
            OpPayload::Bare,
        ) => opcode::Fallocate::new(types::Fd(fd.as_raw_fd()), *len)
            .offset(*offset)
            .mode(*mode)
            .build(),
        (OpDesc::OpenAt2 { dirfd }, OpPayload::Open { name, how, .. }) => {
            let how_ptr: *const types::OpenHow = &**how;
            opcode::OpenAt2::new(types::Fd(dirfd.as_raw_fd()), name.as_ptr(), how_ptr).build()
        }
        (OpDesc::Statx { dirfd, flags, mask }, OpPayload::Statx { name, out }) => {
            let out_ptr: *mut types::statx = std::ptr::from_mut::<libc::statx>(&mut **out).cast();
            opcode::Statx::new(types::Fd(dirfd.as_raw_fd()), name.as_ptr(), out_ptr)
                .flags(*flags)
                .mask(*mask)
                .build()
        }
        (OpDesc::UnlinkAt { dirfd, rmdir }, OpPayload::Path { name }) => {
            let flags = if *rmdir { libc::AT_REMOVEDIR } else { 0 };
            opcode::UnlinkAt::new(types::Fd(dirfd.as_raw_fd()), name.as_ptr())
                .flags(flags)
                .build()
        }
        (OpDesc::MkDirAt { dirfd, mode }, OpPayload::Path { name }) => {
            opcode::MkDirAt::new(types::Fd(dirfd.as_raw_fd()), name.as_ptr())
                .mode(*mode)
                .build()
        }
        (
            OpDesc::RenameAt {
                olddir,
                newdir,
                flags,
            },
            OpPayload::TwoPath { a, b },
        ) => opcode::RenameAt::new(
            types::Fd(olddir.as_raw_fd()),
            a.as_ptr(),
            types::Fd(newdir.as_raw_fd()),
            b.as_ptr(),
        )
        .flags(*flags)
        .build(),
        (
            OpDesc::LinkAt {
                olddir,
                newdir,
                flags,
            },
            OpPayload::TwoPath { a, b },
        ) => opcode::LinkAt::new(
            types::Fd(olddir.as_raw_fd()),
            a.as_ptr(),
            types::Fd(newdir.as_raw_fd()),
            b.as_ptr(),
        )
        .flags(*flags)
        .build(),
        (OpDesc::SymlinkAt { newdir }, OpPayload::TwoPath { a, b }) => {
            opcode::SymlinkAt::new(types::Fd(newdir.as_raw_fd()), a.as_ptr(), b.as_ptr()).build()
        }
        (OpDesc::FGetXattr { fd, len }, OpPayload::XattrRw { buf, name }) => {
            let value = buf.as_mut_slice().as_mut_ptr().cast::<libc::c_void>();
            opcode::FGetXattr::new(types::Fd(fd.as_raw_fd()), name.as_ptr(), value, *len).build()
        }
        (OpDesc::FSetXattr { fd, len, flags }, OpPayload::XattrRw { buf, name }) => {
            let value = buf.as_mut_slice().as_ptr().cast::<libc::c_void>();
            opcode::FSetXattr::new(types::Fd(fd.as_raw_fd()), name.as_ptr(), value, *len)
                .flags(*flags)
                .build()
        }
        _ => unreachable!("operation descriptor paired with the wrong payload"),
    }
}

// ---------------------------------------------------------------------------
// Slab
// ---------------------------------------------------------------------------

/// Stable-key store for in-flight operations.
///
/// A key is only reused after its CQE has been reaped, which is what lets the
/// key double as `user_data`.
struct Slab {
    entries: Vec<Option<Task>>,
    free: Vec<usize>,
    live: usize,
    cap: usize,
}

impl Slab {
    fn new(cap: usize) -> Slab {
        Slab {
            entries: Vec::with_capacity(cap),
            free: Vec::new(),
            live: 0,
            cap,
        }
    }

    fn is_empty(&self) -> bool {
        self.live == 0
    }

    fn is_full(&self) -> bool {
        self.live >= self.cap
    }

    fn insert(&mut self, value: Task) -> usize {
        self.live += 1;
        match self.free.pop() {
            Some(key) => {
                self.entries[key] = Some(value);
                key
            }
            None => {
                self.entries.push(Some(value));
                self.entries.len() - 1
            }
        }
    }

    fn get_mut(&mut self, key: usize) -> &mut Task {
        self.entries[key]
            .as_mut()
            .expect("slab key is live until its CQE arrives")
    }

    fn remove(&mut self, key: usize) -> Option<Task> {
        let taken = self.entries.get_mut(key).and_then(Option::take);
        if taken.is_some() {
            self.live -= 1;
            self.free.push(key);
        }
        taken
    }

    fn drain(&mut self) -> impl Iterator<Item = Task> + '_ {
        self.live = 0;
        self.free.clear();
        self.entries.drain(..).flatten()
    }
}

// ---------------------------------------------------------------------------
// Ring thread
// ---------------------------------------------------------------------------

struct RingState {
    ring: IoUring,
    wake: OwnedFd,
    /// Destination for the eventfd read. Boxed so its address is independent
    /// of where `RingState` lives.
    ev_buf: Box<[u8; 8]>,
    ev_armed: bool,
    slab: Slab,
    backlog: VecDeque<Task>,
}

fn ring_thread(ring: IoUring, rx: Receiver<Task>, wake: OwnedFd, max_inflight: usize) {
    let mut state = RingState {
        ring,
        wake,
        ev_buf: Box::new([0u8; 8]),
        ev_armed: false,
        slab: Slab::new(max_inflight),
        backlog: VecDeque::new(),
    };
    // A panic must not unwind through `RingState`'s drop glue. Field order puts
    // `ring` first, so an ordinary drop would close the ring before freeing the
    // payloads the kernel may still be writing to. `catch_unwind` turns that
    // into the same controlled leak the error path takes. `AssertUnwindSafe` is
    // honest here: the only thing we do with `state` afterwards is `abandon`,
    // which never reads a value that a torn invariant could corrupt.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| state.run(&rx)));
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::error!(error = %e, "io_uring lane aborted");
            state.abandon();
        }
        Err(_) => {
            tracing::error!("io_uring lane panicked");
            state.abandon();
        }
    }
}

impl RingState {
    fn run(&mut self, rx: &Receiver<Task>) -> io::Result<()> {
        let mut shutdown = false;
        loop {
            // 1. Drain everything queued since the last pass.
            if !shutdown {
                loop {
                    match rx.try_recv() {
                        Ok(task) => self.backlog.push_back(task),
                        Err(TryRecvError::Empty) => break,
                        // `Disconnected` is only reported once the queue is
                        // empty, so nothing is lost here.
                        Err(TryRecvError::Disconnected) => {
                            shutdown = true;
                            break;
                        }
                    }
                }
            }

            // 2. Submit as much of the backlog as the in-flight cap allows.
            self.fill()?;

            // 3. Keep exactly one eventfd read armed while still taking work.
            if !shutdown && !self.ev_armed {
                self.arm_eventfd()?;
            }

            // 4. Shutting down with nothing left to do. The armed eventfd read
            //    still points at `ev_buf`, so retire it before returning rather
            //    than dropping a buffer the kernel may write to.
            if shutdown && self.backlog.is_empty() && self.slab.is_empty() {
                if !self.ev_armed {
                    return Ok(());
                }
                poke(&self.wake);
            }

            // 5. Sleep until something finishes.
            self.submit_and_wait(1)?;

            // 6. Hand results back.
            self.reap();
        }
    }

    fn fill(&mut self) -> io::Result<()> {
        while !self.backlog.is_empty() && !self.slab.is_full() {
            let task = self
                .backlog
                .pop_front()
                .expect("backlog was checked non-empty");
            let key = self.slab.insert(task);
            let entry = build_sqe(self.slab.get_mut(key)).user_data(key as u64 + 1);
            if let Err(e) = push(&mut self.ring, &entry) {
                // The SQE never reached the kernel, so the payload is ours to
                // return; nothing is in flight against it.
                if let Some(f) = self.slab.remove(key) {
                    let _ = f.done.send((-io_errno(&e), f.payload));
                }
                return Err(e);
            }
        }
        Ok(())
    }

    fn arm_eventfd(&mut self) -> io::Result<()> {
        let ptr = self.ev_buf.as_mut_ptr();
        let entry = opcode::Read::new(types::Fd(self.wake.as_raw_fd()), ptr, 8)
            .build()
            .user_data(EVENTFD_USER_DATA);
        push(&mut self.ring, &entry)?;
        self.ev_armed = true;
        Ok(())
    }

    fn submit_and_wait(&mut self, want: usize) -> io::Result<()> {
        loop {
            match self.ring.submit_and_wait(want) {
                Ok(_) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // `EBUSY` means the completion queue is full and the kernel
                // refused new submissions; reaping frees space, so this makes
                // progress rather than spinning.
                //
                // This branch is unreachable in practice, and that matters:
                // `reap` can clear `ev_armed`, and the retry below does not
                // re-arm, so an empty slab here would block forever. The
                // in-flight cap (<= `entries`, against a CQ of `2 * entries`)
                // is the only reason the CQ never fills. Anything that raises
                // the cap must also make this branch return to `run` instead of
                // retrying in place.
                Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                    debug_assert!(
                        !self.slab.is_empty(),
                        "EBUSY with an empty slab: the in-flight cap no longer bounds the CQ"
                    );
                    self.reap();
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn reap(&mut self) {
        let RingState {
            ring,
            ev_armed,
            slab,
            ..
        } = self;
        for cqe in ring.completion() {
            let user_data = cqe.user_data();
            if user_data == EVENTFD_USER_DATA {
                // The counter has been consumed and reset; `run` re-arms.
                *ev_armed = false;
                continue;
            }
            let Some(task) = slab.remove((user_data - 1) as usize) else {
                continue;
            };
            let Task {
                op,
                mut payload,
                done,
            } = task;
            let res = cqe.result();
            if res >= 0 {
                if let OpPayload::Open { opened, .. } = &mut payload {
                    // SAFETY: `OpPayload::Open` belongs to `openat2` alone, so a
                    // non-negative result here is the descriptor the kernel just
                    // created for this SQE. The ring thread has not stored it
                    // anywhere and no other owner exists, so this is the single
                    // transfer of ownership and `OwnedFd` closes it exactly once
                    // - whether it reaches the caller or dies with a cancelled
                    // payload two lines below.
                    *opened = Some(unsafe { OwnedFd::from_raw_fd(res) });
                }
            }
            // A dropped receiver means the caller cancelled. Everything the
            // operation owns - buffers, path strings, the descriptor references
            // in `op`, and any freshly opened fd - is released here, after the
            // CQE, which is exactly when the kernel is finished with it.
            let _ = done.send((res, payload));
            drop(op);
        }
    }

    /// Fatal-error path: never free memory the kernel might still be writing
    /// to, but do release the callers waiting on us.
    ///
    /// Leaking is the conservative choice - closing the ring fd does not
    /// synchronously guarantee that every in-flight request has stopped
    /// touching its buffers, so the payloads, the descriptor references, and
    /// the ring itself are forgotten rather than dropped. Dropping the oneshot
    /// senders makes each waiting caller's `await` panic instead of hang.
    ///
    /// Backlogged tasks were never submitted, so those drop normally.
    fn abandon(mut self) {
        for task in self.slab.drain() {
            let Task { op, payload, done } = task;
            // The kernel may still write into the payload, and may still name
            // the descriptors in `op`; neither may be released.
            std::mem::forget(op);
            std::mem::forget(payload);
            drop(done);
        }
        // Order matters. Backlogged tasks were never submitted, so releasing
        // them is correct - but releasing them can itself panic (`PooledBuf`
        // returns to a pool behind a `Mutex`). Leak the ring and `ev_buf`
        // first, so an unwind out of this handler can no longer free the
        // eventfd read's destination while that read is still armed.
        let backlog = std::mem::take(&mut self.backlog);
        std::mem::forget(self);
        drop(backlog);
    }
}

/// Push one SQE, submitting first if the submission queue has filled up.
fn push(ring: &mut IoUring, entry: &squeue::Entry) -> io::Result<()> {
    {
        let mut sq = ring.submission();
        // SAFETY: every pointer inside `entry` was produced by `build_sqe` (or
        // `arm_eventfd`) from an allocation owned by the matching slab entry
        // (or by `RingState::ev_buf`). Those allocations are kept alive until
        // the CQE for this `user_data` is reaped, so they outlive the kernel's
        // use of them. The `SubmissionQueue` guard flushes the tail on drop.
        if unsafe { sq.push(entry) }.is_ok() {
            return Ok(());
        }
    }
    // Full: hand the queued entries to the kernel, which empties the SQ.
    ring.submit()?;
    let mut sq = ring.submission();
    // SAFETY: as above; `entry` and its pointer targets are unchanged.
    unsafe { sq.push(entry) }.map_err(|_| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            "io_uring submission queue still full after submit",
        )
    })
}

fn io_errno(e: &io::Error) -> i32 {
    e.raw_os_error().unwrap_or(libc::EIO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn cstr(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    /// Every fd argument is an `Arc<OwnedFd>`; the executor keeps its own
    /// reference for as long as an operation names the descriptor.
    fn owned(file: std::fs::File) -> Arc<OwnedFd> {
        Arc::new(OwnedFd::from(file))
    }

    fn rw_file(path: std::path::PathBuf) -> std::fs::File {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .unwrap()
    }

    fn create_how() -> types::OpenHow {
        types::OpenHow::new()
            .flags((libc::O_RDWR | libc::O_CREAT | libc::O_EXCL) as u64)
            .mode(0o644)
    }

    /// Descriptors this process holds onto anything under `root`.
    ///
    /// Scoping to one tempdir rather than counting all of `/proc/self/fd` is
    /// what makes this deterministic: `cargo test` runs these tests in parallel
    /// threads of one process, so a whole-process count drifts with whatever
    /// the neighbours have open.
    fn fds_under(root: &std::path::Path) -> usize {
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
            .filter(|target| target.starts_with(root))
            .count()
    }

    #[tokio::test]
    async fn write_then_read_round_trips_through_ring() {
        let exec = UringExecutor::new(1, 64).unwrap();
        let dir = tempfile::tempdir().unwrap();
        // Read+write, not `File::create`: an O_WRONLY descriptor makes the
        // read below fail with EBADF before it ever reaches the ring.
        let fd = owned(rw_file(dir.path().join("f")));

        let pool = super::super::buffers::BufferPool::new(4096, 4);
        let mut buf = pool.get();
        buf.as_mut_slice()[..5].copy_from_slice(b"hello");
        let (_buf, res) = exec.write(&fd, 0, buf, 5).await;
        assert_eq!(res.unwrap(), 5);

        let rbuf = pool.get();
        let (rbuf, res) = exec.read(&fd, 0, rbuf, 5).await;
        assert_eq!(res.unwrap(), 5);
        assert_eq!(&rbuf.as_mut_ref_for_test()[..5], b"hello");
        exec.fsync(&fd, false).await.unwrap();
    }

    #[tokio::test]
    async fn mkdir_statx_unlink_via_ring() {
        let exec = UringExecutor::new(1, 64).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let dfd = owned(std::fs::File::open(dir.path()).unwrap());

        exec.mkdirat(&dfd, cstr("sub"), 0o755).await.unwrap();
        let st = exec
            .statx(&dfd, cstr("sub"), 0, libc::STATX_BASIC_STATS)
            .await
            .unwrap();
        assert_eq!(st.stx_mode as u32 & libc::S_IFMT, libc::S_IFDIR);
        exec.unlinkat(&dfd, cstr("sub"), true).await.unwrap();
        let err = exec
            .statx(&dfd, cstr("sub"), 0, libc::STATX_BASIC_STATS)
            .await
            .unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
    }

    #[tokio::test]
    async fn many_concurrent_ops_complete() {
        let exec = UringExecutor::new(1, 8).unwrap(); // entries smaller than op count
        let dir = tempfile::tempdir().unwrap();
        let dfd = owned(std::fs::File::open(dir.path()).unwrap());
        let futs: Vec<_> = (0..64)
            .map(|i| exec.mkdirat(&dfd, cstr(&format!("d{i}")), 0o755))
            .collect();
        for f in futures::future::join_all(futs).await {
            f.unwrap();
        }
    }

    #[tokio::test]
    async fn open_fallocate_link_rename_symlink_via_ring() {
        let exec = UringExecutor::new(2, 32).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let dfd = owned(std::fs::File::open(dir.path()).unwrap());

        let file = Arc::new(exec.openat2(&dfd, cstr("a"), create_how()).await.unwrap());
        exec.fallocate(&file, 0, 0, 4096).await.unwrap();

        let st = exec
            .statx(&dfd, cstr("a"), 0, libc::STATX_BASIC_STATS)
            .await
            .unwrap();
        assert_eq!(st.stx_size, 4096);

        exec.linkat(&dfd, cstr("a"), &dfd, cstr("b"), 0)
            .await
            .unwrap();
        exec.renameat(&dfd, cstr("b"), &dfd, cstr("c"), 0)
            .await
            .unwrap();
        exec.symlinkat(cstr("c"), &dfd, cstr("l")).await.unwrap();

        let link = exec
            .statx(
                &dfd,
                cstr("l"),
                libc::AT_SYMLINK_NOFOLLOW,
                libc::STATX_BASIC_STATS,
            )
            .await
            .unwrap();
        assert_eq!(link.stx_mode as u32 & libc::S_IFMT, libc::S_IFLNK);

        for name in ["a", "c", "l"] {
            exec.unlinkat(&dfd, cstr(name), false).await.unwrap();
        }
    }

    #[tokio::test]
    async fn xattr_round_trips_through_ring() {
        let exec = UringExecutor::new(1, 8).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let fd = owned(rw_file(dir.path().join("x")));
        let pool = super::super::buffers::BufferPool::new(64, 2);

        let mut value = pool.get();
        value.as_mut_slice()[..3].copy_from_slice(b"bar");
        let (_value, res) = exec.fsetxattr(&fd, cstr("user.lbfs"), value, 3, 0).await;
        match res {
            Ok(()) => {}
            // Filesystems without user-namespace xattrs have nothing to prove
            // here; the ring plumbing is the same either way.
            Err(e) if e.raw_os_error() == Some(libc::EOPNOTSUPP) => return,
            Err(e) => panic!("fsetxattr through the ring failed: {e}"),
        }

        let out = pool.get();
        let (out, res) = exec.fgetxattr(&fd, cstr("user.lbfs"), out, 64).await;
        assert_eq!(res.unwrap(), 3);
        assert_eq!(out.as_slice(), b"bar");
    }

    #[tokio::test]
    async fn short_read_hides_recycled_buffer_tail() {
        let exec = UringExecutor::new(1, 8).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let fd = owned(rw_file(dir.path().join("f")));
        let pool = super::super::buffers::BufferPool::new(32, 1);

        // Dirty the pooled storage, then hand it back. The pool does not zero.
        let mut dirty = pool.get();
        dirty.as_mut_slice().fill(b'X');
        drop(dirty);

        let mut w = pool.get();
        w.as_mut_slice()[..3].copy_from_slice(b"abc");
        let (w, res) = exec.write(&fd, 0, w, 3).await;
        assert_eq!(res.unwrap(), 3);
        drop(w);

        let r = pool.get();
        let (r, res) = exec.read(&fd, 0, r, 32).await;
        assert_eq!(res.unwrap(), 3);
        assert_eq!(r.as_slice(), b"abc");
    }

    #[tokio::test]
    async fn dropping_the_executor_joins_every_lane() {
        // A lane that never woke and a lane that did both have to exit; if the
        // eventfd nudge in `shutdown_lanes` were missing, this would hang.
        for _ in 0..4 {
            let exec = UringExecutor::new(3, 4).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let dfd = owned(std::fs::File::open(dir.path()).unwrap());
            let clone = exec.clone();
            clone.mkdirat(&dfd, cstr("z"), 0o755).await.unwrap();
            drop(clone);
            drop(exec);
        }
    }

    #[tokio::test]
    async fn cancelled_futures_leave_the_lane_usable() {
        let exec = UringExecutor::new(1, 4).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let dfd = owned(std::fs::File::open(dir.path()).unwrap());

        // Poll each future once so the task reaches the ring, then drop it. The
        // executor still owns the payload and the directory fd, so the ring
        // thread completes and reaps every one of these on its own.
        for i in 0..16 {
            let mut fut = Box::pin(exec.mkdirat(&dfd, cstr(&format!("c{i}")), 0o755));
            let _ = futures::poll!(fut.as_mut());
            drop(fut);
        }
        // The caller's own handle goes away too: only the executor's clone of
        // the Arc keeps the directory descriptor open now.
        drop(dfd);

        // The lane must still serve later work rather than wedge on the
        // cancelled entries.
        let dfd = owned(std::fs::File::open(dir.path()).unwrap());
        for i in 0..8 {
            exec.mkdirat(&dfd, cstr(&format!("after{i}")), 0o755)
                .await
                .unwrap();
        }
        drop(exec); // drains and joins; a wedged lane would hang here
    }

    #[tokio::test]
    async fn cancelled_openat2_closes_the_descriptor_it_opened() {
        const OPENS: usize = 64;
        let dir = tempfile::tempdir().unwrap();
        let dfd = owned(std::fs::File::open(dir.path()).unwrap());

        assert_eq!(fds_under(dir.path()), 1, "only `dfd` should be open yet");
        {
            let exec = UringExecutor::new(1, 8).unwrap();
            let mut futs = Vec::new();
            for i in 0..OPENS {
                let mut fut = Box::pin(exec.openat2(&dfd, cstr(&format!("o{i}")), create_how()));
                let _ = futures::poll!(fut.as_mut());
                futs.push(fut);
            }
            drop(futs); // every caller cancels before its CQE
            drop(exec); // drains: the ring thread owns each opened fd and must close it
        }

        // `dfd` and nothing else. Wrapping the CQE result on the ring thread is
        // what makes this hold; decoding it in the caller's future would leak
        // all `OPENS` descriptors instead.
        assert_eq!(
            fds_under(dir.path()),
            1,
            "cancelled opens leaked descriptors into {:?}",
            dir.path()
        );
    }
}
