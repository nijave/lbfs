//! Opcode to [`FileSystem`] call, and back to a reply.
//!
//! Deliberately the dullest file in the server. Every arm is the same three
//! steps — decode the postcard body, call the one trait method that opcode
//! names, encode whatever came back — and the value of writing them out rather
//! than generating them is that a reader can check any single opcode against
//! the protocol table without holding a macro in their head.
//!
//! Two things this layer does *not* do, both on purpose:
//!
//! * **It does not enforce frame limits.** Sizes are checked in the session's
//!   read loop, before anything is allocated for them, because a length past
//!   the negotiated maximum is connection-fatal rather than an error reply
//!   (spec §3.1). By the time a request reaches here it is within bounds.
//! * **It does not decide what an error means.** A backend errno travels to
//!   the client's FUSE reply unchanged; the only errno invented here is
//!   `EINVAL` for a body that will not decode.

use std::sync::Arc;

use lbfs_proto::frame::{MAX_BODY_SIZE, STATUS_OK};
use lbfs_proto::ops::*;
use lbfs_proto::types::XattrReply;
use lbfs_proto::Errno;
use serde::Serialize;

use crate::fs::local::buffers::PooledBuf;
use crate::fs::FileSystem;

/// Bulk bytes travelling in a frame's data segment, in either direction.
///
/// Two variants because the two sources of bulk data own their memory
/// differently and neither should be copied into the other's shape: a `READ`
/// or a `WRITE` moves through the pool that the io_uring path already uses,
/// while an xattr value is a plain allocation the size of the value.
pub enum DataPayload {
    Pooled(PooledBuf),
    Owned(Vec<u8>),
}

impl DataPayload {
    pub fn as_slice(&self) -> &[u8] {
        match self {
            DataPayload::Pooled(buf) => buf.as_slice(),
            DataPayload::Owned(bytes) => bytes,
        }
    }
}

/// `(status, body, data)` — exactly what the writer task needs to build a
/// frame. `status` is `STATUS_OK` or a raw errno.
pub type Reply = (u16, Vec<u8>, Option<DataPayload>);

/// Decode the request body or answer `EINVAL`.
///
/// `return`s out of the enclosing function, which is why it is a macro: each
/// arm decodes a different type and the early exit has to happen in the arm.
macro_rules! decode {
    ($t:ty, $body:expr) => {
        match postcard::from_bytes::<$t>($body) {
            Ok(req) => req,
            Err(_) => return err(Errno::EINVAL),
        }
    };
}

fn ok<T: Serialize>(v: &T) -> Reply {
    match postcard::to_allocvec(v) {
        Ok(body) => (STATUS_OK, body, None),
        // Unreachable for every type in `ops`: postcard's allocating writer
        // has no failure mode for plain data. `EIO` rather than an `unwrap`
        // because a panic here would kill the request task and strand the
        // client waiting for a reply that can no longer come.
        Err(_) => err(Errno::EIO),
    }
}

/// A reply that carries nothing but its status.
fn done() -> Reply {
    (STATUS_OK, Vec::new(), None)
}

fn err(e: Errno) -> Reply {
    (e.0, Vec::new(), None)
}

/// Turn `FsResult<()>` into a reply, for the many ops that answer only success
/// or failure.
fn unit(r: Result<(), Errno>) -> Reply {
    match r {
        Ok(()) => done(),
        Err(e) => err(e),
    }
}

/// An xattr get or list: the length in the body, the bytes in the data segment.
fn xattr(r: Result<(u32, Vec<u8>), Errno>) -> Reply {
    match r {
        Ok((size, bytes)) => match postcard::to_allocvec(&XattrReply { size }) {
            Ok(body) => (STATUS_OK, body, Some(DataPayload::Owned(bytes))),
            Err(_) => err(Errno::EIO),
        },
        Err(e) => err(e),
    }
}

/// One request, start to finish.
///
/// `data` is the frame's data segment: a pooled buffer for `WRITE`, an owned
/// vector for `SETXATTR`, and `None` for everything else — the read loop
/// refuses a data segment on any other opcode.
pub(crate) async fn dispatch(
    op: Opcode,
    body: &[u8],
    data: Option<DataPayload>,
    fs: &Arc<dyn FileSystem>,
) -> Reply {
    match op {
        Opcode::Lookup => {
            let req = decode!(LookupRequest, body);
            match fs.lookup(req.parent, &req.name).await {
                Ok(entry) => ok(&entry),
                Err(e) => err(e),
            }
        }
        Opcode::Getattr => {
            let req = decode!(GetattrRequest, body);
            match fs.getattr(req.node, req.fh).await {
                Ok(attr) => ok(&attr),
                Err(e) => err(e),
            }
        }
        Opcode::Setattr => {
            let req = decode!(SetattrRequest, body);
            match fs.setattr(req.node, req.args).await {
                Ok(attr) => ok(&attr),
                Err(e) => err(e),
            }
        }
        Opcode::Readlink => {
            let req = decode!(ReadlinkRequest, body);
            match fs.readlink(req.node).await {
                Ok(target) => ok(&ReadlinkReply { target }),
                Err(e) => err(e),
            }
        }
        Opcode::Symlink => {
            let req = decode!(SymlinkRequest, body);
            match fs.symlink(req.parent, &req.name, &req.target).await {
                Ok(entry) => ok(&entry),
                Err(e) => err(e),
            }
        }
        Opcode::Mkdir => {
            let req = decode!(MkdirRequest, body);
            match fs.mkdir(req.parent, &req.name, req.mode).await {
                Ok(entry) => ok(&entry),
                Err(e) => err(e),
            }
        }
        Opcode::Unlink => {
            let req = decode!(UnlinkRequest, body);
            unit(fs.unlink(req.parent, &req.name).await)
        }
        Opcode::Rmdir => {
            let req = decode!(RmdirRequest, body);
            unit(fs.rmdir(req.parent, &req.name).await)
        }
        Opcode::Rename => {
            let req = decode!(RenameRequest, body);
            unit(
                fs.rename(
                    req.parent,
                    &req.name,
                    req.newparent,
                    &req.newname,
                    req.flags,
                )
                .await,
            )
        }
        Opcode::Link => {
            let req = decode!(LinkRequest, body);
            match fs.link(req.node, req.newparent, &req.newname).await {
                Ok(entry) => ok(&entry),
                Err(e) => err(e),
            }
        }
        Opcode::Open => {
            let req = decode!(OpenRequest, body);
            match fs.open(req.node, req.flags).await {
                Ok(fh) => ok(&OpenReply { fh }),
                Err(e) => err(e),
            }
        }
        Opcode::Create => {
            let req = decode!(CreateRequest, body);
            match fs.create(req.parent, &req.name, req.mode, req.flags).await {
                Ok((entry, fh)) => ok(&CreateReply { entry, fh }),
                Err(e) => err(e),
            }
        }
        Opcode::Read => {
            let req = decode!(ReadRequest, body);
            // The read loop has already refused a `size` past the negotiated
            // maximum, so the pooled buffer this fills is within its capacity.
            match fs.read(req.node, req.fh, req.offset, req.size).await {
                Ok(buf) => (STATUS_OK, Vec::new(), Some(DataPayload::Pooled(buf))),
                Err(e) => err(e),
            }
        }
        Opcode::Write => {
            let req = decode!(WriteRequest, body);
            let Some(DataPayload::Pooled(buf)) = data else {
                // Structurally unreachable: the read loop builds a pooled
                // buffer for every WRITE, even a zero-length one.
                return err(Errno::EINVAL);
            };
            // The length is the buffer's initialized prefix - the exact count
            // read off the socket - never the header's claim and never the
            // buffer's capacity. Pooled storage is recycled without zeroing,
            // so a larger number would write a previous request's bytes into
            // the client's file.
            let len = u32::try_from(buf.as_slice().len()).unwrap_or(u32::MAX);
            match fs.write(req.node, req.fh, req.offset, buf, len).await {
                Ok(written) => ok(&WriteReply { written }),
                Err(e) => err(e),
            }
        }
        Opcode::Flush => {
            let req = decode!(FlushRequest, body);
            unit(fs.flush(req.node, req.fh).await)
        }
        Opcode::Release => {
            let req = decode!(ReleaseRequest, body);
            unit(fs.release(req.node, req.fh).await)
        }
        Opcode::Fsync => {
            let req = decode!(FsyncRequest, body);
            unit(fs.fsync(req.node, req.fh, req.datasync).await)
        }
        Opcode::Fallocate => {
            let req = decode!(FallocateRequest, body);
            unit(
                fs.fallocate(req.node, req.fh, req.offset, req.length, req.mode)
                    .await,
            )
        }
        Opcode::Lseek => {
            let req = decode!(LseekRequest, body);
            match fs.lseek(req.node, req.fh, req.offset, req.whence).await {
                Ok(offset) => ok(&LseekReply { offset }),
                Err(e) => err(e),
            }
        }
        Opcode::CopyFileRange => {
            let req = decode!(CopyFileRangeRequest, body);
            match fs
                .copy_file_range(
                    req.node_in,
                    req.fh_in,
                    req.off_in,
                    req.node_out,
                    req.fh_out,
                    req.off_out,
                    req.len,
                )
                .await
            {
                Ok(copied) => ok(&CopyFileRangeReply { copied }),
                Err(e) => err(e),
            }
        }
        Opcode::Opendir => {
            let req = decode!(OpendirRequest, body);
            match fs.opendir(req.node).await {
                Ok(dh) => ok(&OpendirReply { dh }),
                Err(e) => err(e),
            }
        }
        Opcode::Readdir => {
            let req = decode!(ReaddirRequest, body);
            match fs
                .readdir(req.node, req.dh, req.offset, page_budget(req.max_bytes))
                .await
            {
                Ok(reply) => ok(&reply),
                Err(e) => err(e),
            }
        }
        Opcode::Readdirplus => {
            let req = decode!(ReaddirRequest, body);
            match fs
                .readdirplus(req.node, req.dh, req.offset, page_budget(req.max_bytes))
                .await
            {
                Ok(reply) => ok(&reply),
                Err(e) => err(e),
            }
        }
        Opcode::Releasedir => {
            let req = decode!(ReleasedirRequest, body);
            unit(fs.releasedir(req.node, req.dh).await)
        }
        Opcode::Fsyncdir => {
            let req = decode!(FsyncdirRequest, body);
            unit(fs.fsyncdir(req.node, req.dh, req.datasync).await)
        }
        Opcode::Statfs => {
            let req = decode!(StatfsRequest, body);
            match fs.statfs(req.node).await {
                Ok(reply) => ok(&reply),
                Err(e) => err(e),
            }
        }
        Opcode::Getxattr => {
            let req = decode!(GetxattrRequest, body);
            xattr(fs.getxattr(req.node, &req.name, req.size).await)
        }
        Opcode::Setxattr => {
            let req = decode!(SetxattrRequest, body);
            let Some(value) = data else {
                // As with WRITE: the read loop always supplies one, empty or
                // not.
                return err(Errno::EINVAL);
            };
            unit(
                fs.setxattr(req.node, &req.name, value.as_slice(), req.flags)
                    .await,
            )
        }
        Opcode::Listxattr => {
            let req = decode!(ListxattrRequest, body);
            xattr(fs.listxattr(req.node, req.size).await)
        }
        Opcode::Removexattr => {
            let req = decode!(RemovexattrRequest, body);
            unit(fs.removexattr(req.node, &req.name).await)
        }
        // The session runs these itself: the two handshake opcodes are refused
        // after `ATTACH`, and `FORGET` carries `NO_REPLY` and never reaches a
        // reply-producing path. `EINVAL` rather than `unreachable!` so a future
        // refactor that lets one slip through answers the client instead of
        // panicking a request task and stranding it.
        Opcode::Hello | Opcode::Attach | Opcode::Forget => err(Errno::EINVAL),
    }
}

/// How many bytes of directory entries the backend may produce.
///
/// The client's ask, capped at the largest body a frame can carry. An honest
/// answer to a bigger ask would be a frame the client is obliged to treat as
/// fatal (spec §3.1), so the ask is trimmed rather than served or refused.
/// `LocalFs` clamps again internally; both are cheap and neither is the other's
/// excuse.
fn page_budget(max_bytes: u32) -> u32 {
    max_bytes.min(MAX_BODY_SIZE)
}
