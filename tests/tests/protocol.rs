//! The wire contract, pinned against a real server over a real socket.
//!
//! Spec §10 layer 2: raw frames over TCP against an in-process server
//! exporting a tempdir. No FUSE, no VM, no mount — the client here is a socket
//! and a postcard encoder, which is what lets it send the frames a real client
//! never would.
//!
//! # What this suite is for, and what it is not
//!
//! `crates/lbfs-server/src/fs/local` already unit-tests the backend, and
//! `crates/lbfs-server/tests/session.rs` already smoke-tests the frame
//! plumbing. Repeating either would be waste. What only this layer can pin is
//! the *join* between them: that every opcode reaches the trait method it
//! names, that its request and reply structs survive the round trip in both
//! directions, that an errno the backend produced arrives as the frame's
//! status, and that the connection lives or dies exactly where the protocol
//! says it should.
//!
//! So the depth here is the per-opcode matrix, the errno paths, directory
//! paging and its `FORGET` ledger, the two durability policies, and
//! pipelining. Negotiation, attach statuses and the oversize-frame rules get
//! only what `session.rs` does not already cover.
//!
//! Every case builds its own tempdir and its own server on `127.0.0.1:0`.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use lbfs_proto::frame::{
    FrameHeader, FLAG_NO_REPLY, MAGIC, MAX_BODY_SIZE, PROTOCOL_VERSION, STATUS_ATTACH_DENIED,
    STATUS_NOT_EXPORTED, WINDOW_CLAMP,
};
use lbfs_proto::ops::*;
use lbfs_proto::types::*;
use lbfs_server::config::FsyncPolicy;
use lbfs_tests::{resolved, serve, TestClient, TestServer};

/// The default server ceilings the suite negotiates against.
const SERVER_WINDOW: u32 = 128;
const SERVER_IO: u32 = 1 << 20;

fn hello_request(max_inflight: u32, max_io_size: u32) -> HelloRequest {
    HelloRequest {
        magic: MAGIC,
        version: PROTOCOL_VERSION,
        max_inflight,
        max_io_size,
        writeback: false,
    }
}

/// A FIFO in the export, built out of band.
///
/// It is the one file type whose `fsync(2)` fails, which makes it the only
/// witness to whether the durability policy ran the syscall at all.
fn make_fifo(path: &Path) {
    rustix::fs::mknodat(
        rustix::fs::CWD,
        path,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// Handshake and attach
// ---------------------------------------------------------------------------

/// Each side proposes, the smaller wins, and the result is bounded by what the
/// protocol allows regardless of what either side asked for. The reply echoes
/// the settled numbers, because they are the only ones both ends will use.
#[tokio::test]
async fn the_handshake_settles_and_echoes_the_limits() {
    let srv = TestServer::with(FsyncPolicy::Honor, SERVER_WINDOW, SERVER_IO).await;

    // Below both floors: the window clamps up to its minimum and the I/O size
    // to a page. A sub-page ceiling would make an ordinary read a violation.
    let c = srv.attached_with(1, 0, false).await;
    assert_eq!(
        *c.settled(),
        HelloReply {
            version: PROTOCOL_VERSION,
            max_inflight: WINDOW_CLAMP.0,
            max_io_size: 4096,
            max_body_size: MAX_BODY_SIZE,
        }
    );

    // Above what the server offers: the server's numbers win, and the window
    // never passes the protocol's own ceiling.
    let c = srv.attached_with(u32::MAX, u32::MAX, true).await;
    assert_eq!(
        *c.settled(),
        HelloReply {
            version: PROTOCOL_VERSION,
            max_inflight: SERVER_WINDOW,
            max_io_size: SERVER_IO,
            max_body_size: MAX_BODY_SIZE,
        }
    );
    // `writeback` is the client's alone and gets no echo, but it is a
    // positional field in the HELLO body: a session that reached ATTACH proves
    // the server read the five-field handshake this client sent.
    assert_eq!(c.root_attr().mode & libc::S_IFMT, libc::S_IFDIR);
}

/// The settled ceiling is inclusive. Its neighbour — a `size` one byte past it
/// — is connection-fatal, so an off-by-one here would kill legal mounts.
#[tokio::test]
async fn a_read_of_exactly_the_settled_maximum_is_legal() {
    const IO: u32 = 8192;
    let srv = TestServer::with(FsyncPolicy::Honor, SERVER_WINDOW, IO).await;
    std::fs::write(srv.join("f"), vec![b'z'; IO as usize]).unwrap();

    let mut c = srv.attached_with(SERVER_WINDOW, IO, false).await;
    assert_eq!(c.settled().max_io_size, IO);
    let f: Entry = c.lookup(ROOT_NODE, b"f").await.ok();
    let open: OpenReply = c.open(f.node, libc::O_RDONLY).await.ok();

    let reply = c.read(f.node, open.fh, 0, IO).await;
    reply.expect_ok();
    assert_eq!(reply.data.len(), IO as usize);
}

/// State is settled once, in order, before anything is served. An opcode that
/// arrives where a handshake frame belongs has no answer the protocol can
/// carry, so the connection ends.
#[tokio::test]
async fn opcodes_before_the_handshake_are_fatal() {
    let srv = TestServer::start().await;

    let mut c = srv.connect().await;
    c.begin(
        Opcode::Getattr,
        &GetattrRequest {
            node: ROOT_NODE,
            fh: None,
        },
    )
    .await;
    assert!(c.closed().await, "the first frame must be HELLO");

    let mut c = srv.connect().await;
    c.hello(&hello_request(SERVER_WINDOW, SERVER_IO))
        .await
        .expect_ok();
    c.begin(
        Opcode::Getattr,
        &GetattrRequest {
            node: ROOT_NODE,
            fh: None,
        },
    )
    .await;
    assert!(c.closed().await, "the second frame must be ATTACH");
}

/// A handshake body that will not decode is fatal, unlike a filesystem
/// request's: there is no session yet to answer for it, and postcard is not
/// self-describing, so a server that guessed would be guessing about which
/// protocol it is speaking.
#[tokio::test]
async fn malformed_handshake_bodies_are_fatal() {
    let srv = TestServer::start().await;

    // A HELLO that stops inside the magic.
    let mut c = srv.connect().await;
    c.begin_raw_body(Opcode::Hello, b"LB").await;
    assert!(c.closed().await, "a HELLO that will not decode is fatal");

    // An ATTACH whose path length prefix never finishes.
    let mut c = srv.connect().await;
    c.hello(&hello_request(SERVER_WINDOW, SERVER_IO))
        .await
        .expect_ok();
    c.begin_raw_body(Opcode::Attach, &[0xff]).await;
    assert!(c.closed().await, "an ATTACH that will not decode is fatal");
}

/// The two frames with no reply to carry an error. Dropping a malformed FORGET
/// would leak every node it named, and an unknown opcode has already
/// desynchronized whatever the client thinks it is speaking.
#[tokio::test]
async fn a_malformed_forget_and_an_unknown_opcode_are_fatal() {
    let srv = TestServer::start().await;

    let mut c = srv.attached().await;
    c.frame(90, Opcode::Forget as u16, FLAG_NO_REPLY, &[0xff], &[])
        .await;
    assert!(c.closed().await, "a FORGET that will not decode is fatal");

    let mut c = srv.attached().await;
    c.frame(91, 9999, 0, &[], &[]).await;
    assert!(c.closed().await, "an unknown opcode is fatal");
}

/// Attach is open-then-verify: the descriptor is opened first and the
/// allowlist is matched against the path that descriptor resolves to. The two
/// refusals are distinguishable because they mean different things — one is
/// "there is nothing here to open", the other "this is not yours".
#[tokio::test]
async fn attach_separates_not_exported_from_denied() {
    let dir = tempfile::tempdir().unwrap();
    let exports = dir.path().join("exports");
    std::fs::create_dir_all(exports.join("data")).unwrap();
    std::fs::create_dir_all(dir.path().join("secret")).unwrap();
    std::fs::write(exports.join("file"), b"x").unwrap();
    let addr = serve(
        vec![format!("{}/*", resolved(&exports))],
        FsyncPolicy::Honor,
        SERVER_WINDOW,
        SERVER_IO,
    )
    .await;

    // A directory the glob matches.
    let c = TestClient::connect_and_attach(addr, &exports.join("data")).await;
    assert_eq!(c.root_attr().mode & libc::S_IFMT, libc::S_IFDIR);
    assert_eq!(
        c.root_attr().ino,
        std::fs::metadata(exports.join("data")).unwrap().ino()
    );

    // Opens cleanly, resolves to a path no pattern matches.
    let mut c = TestClient::connect(addr).await;
    c.hello(&hello_request(SERVER_WINDOW, SERVER_IO))
        .await
        .expect_ok();
    assert_eq!(
        c.attach(&dir.path().join("secret")).await.status,
        STATUS_ATTACH_DENIED
    );
    assert!(c.closed().await, "the server closes after a refused attach");

    // Nothing to open at all.
    let mut c = TestClient::connect(addr).await;
    c.hello(&hello_request(SERVER_WINDOW, SERVER_IO))
        .await
        .expect_ok();
    assert_eq!(
        c.attach(&exports.join("missing")).await.status,
        STATUS_NOT_EXPORTED
    );

    // A real name inside the exported tree that is not a directory. The open
    // refuses it before the allowlist is ever consulted, so it reports
    // NOT_EXPORTED rather than DENIED.
    let mut c = TestClient::connect(addr).await;
    c.hello(&hello_request(SERVER_WINDOW, SERVER_IO))
        .await
        .expect_ok();
    assert_eq!(
        c.attach(&exports.join("file")).await.status,
        STATUS_NOT_EXPORTED
    );
}

// ---------------------------------------------------------------------------
// The opcode matrix
// ---------------------------------------------------------------------------

/// Every opcode in the v1 table, end to end, in one session.
///
/// The point is not that each operation works — the backend's own tests cover
/// that — but that each *opcode* is wired to it: the right trait method, the
/// right request struct decoded from the body, the right reply struct encoded
/// back, and bulk bytes in the data segment rather than the body. A
/// transposition in `dispatch` (READDIRPLUS decoding the wrong request,
/// COPY_FILE_RANGE swapping two of its seven fields) is invisible to a unit
/// test and fails here.
///
/// The coverage assertion at the end is what keeps it honest: the client
/// records every opcode it sends, and the test fails if the walk left one out.
#[tokio::test]
async fn every_opcode_happy_path() {
    let srv = TestServer::start().await;
    let mut c = srv.attached().await;
    let root = ROOT_NODE;

    // MKDIR. The server clears its umask at startup, so a mode arrives intact
    // rather than through whatever mask the server process inherited.
    let dir: Entry = c.mkdir(root, b"dir", 0o755).await.ok();
    assert_eq!(dir.attr.mode & libc::S_IFMT, libc::S_IFDIR);
    assert_eq!(dir.attr.mode & 0o7777, 0o755);
    assert!(dir.node > root);

    // CREATE hands back an entry and an open handle in one round trip.
    let created: CreateReply = c.create(dir.node, b"f", 0o644, libc::O_RDWR).await.ok();
    let (f, fh) = (created.entry, created.fh);
    assert_eq!(f.attr.mode & 0o7777, 0o644);
    assert_eq!(f.attr.size, 0);

    // WRITE carries its payload in the data segment.
    let written: WriteReply = c.write(f.node, fh, 0, b"hello world").await.ok();
    assert_eq!(written.written, 11);

    // READ answers in the data segment and leaves the body empty.
    let reply = c.read(f.node, fh, 0, 4096).await;
    reply.expect_ok();
    assert!(reply.body.is_empty(), "READ answers in the data segment");
    assert_eq!(reply.data, b"hello world");

    // GETATTR
    let attr: FileAttr = c.getattr(f.node).await.ok();
    assert_eq!((attr.size, attr.ino), (11, f.attr.ino));

    // SETATTR: chmod, truncate through the open handle, and an explicit mtime.
    let attr: FileAttr = c
        .call(
            Opcode::Setattr,
            &SetattrRequest {
                node: f.node,
                args: SetattrArgs {
                    mode: Some(0o600),
                    uid: None,
                    gid: None,
                    size: Some(5),
                    atime: TimeSet::Omit,
                    mtime: TimeSet::Set {
                        sec: 1_000,
                        nsec: 250,
                    },
                    fh: Some(fh),
                },
            },
        )
        .await
        .ok();
    assert_eq!(attr.mode & 0o7777, 0o600);
    assert_eq!(attr.size, 5);
    assert_eq!((attr.mtime_sec, attr.mtime_nsec), (1_000, 250));

    // SYMLINK and READLINK. The target is bytes the server never resolves.
    let link: Entry = c
        .call(
            Opcode::Symlink,
            &SymlinkRequest {
                parent: dir.node,
                name: b"l".to_vec(),
                target: b"f".to_vec(),
            },
        )
        .await
        .ok();
    assert_eq!(link.attr.mode & libc::S_IFMT, libc::S_IFLNK);
    let target: ReadlinkReply = c
        .call(Opcode::Readlink, &ReadlinkRequest { node: link.node })
        .await
        .ok();
    assert_eq!(target.target, b"f");

    // LINK. Nodes are keyed by (dev, ino), so a hardlink reports the source's
    // node — and the client owes a second FORGET for the same id.
    let hard: Entry = c
        .call(
            Opcode::Link,
            &LinkRequest {
                node: f.node,
                newparent: root,
                newname: b"hard".to_vec(),
            },
        )
        .await
        .ok();
    assert_eq!(hard.node, f.node, "a hardlink dedups onto its inode's node");
    assert_eq!(hard.attr.nlink, 2);

    // FALLOCATE, then LSEEK over what it produced.
    c.call(
        Opcode::Fallocate,
        &FallocateRequest {
            node: f.node,
            fh,
            offset: 0,
            length: 4096,
            mode: 0,
        },
    )
    .await
    .ok_unit();
    let attr: FileAttr = c.getattr(f.node).await.ok();
    assert_eq!(attr.size, 4096);
    let data_at: LseekReply = c
        .call(
            Opcode::Lseek,
            &LseekRequest {
                node: f.node,
                fh,
                offset: 0,
                whence: libc::SEEK_DATA as u32,
            },
        )
        .await
        .ok();
    assert_eq!(data_at.offset, 0, "the allocated range starts at zero");
    let end: LseekReply = c
        .call(
            Opcode::Lseek,
            &LseekRequest {
                node: f.node,
                fh,
                offset: 0,
                whence: libc::SEEK_END as u32,
            },
        )
        .await
        .ok();
    assert_eq!(end.offset, 4096);

    // COPY_FILE_RANGE: seven fields, two of them handles, and the bytes never
    // cross the wire in either direction.
    let g: CreateReply = c.create(dir.node, b"g", 0o644, libc::O_RDWR).await.ok();
    let copied: CopyFileRangeReply = c
        .call(
            Opcode::CopyFileRange,
            &CopyFileRangeRequest {
                node_in: f.node,
                fh_in: fh,
                off_in: 0,
                node_out: g.entry.node,
                fh_out: g.fh,
                off_out: 0,
                len: 5,
            },
        )
        .await
        .ok();
    assert_eq!(copied.copied, 5);
    let reply = c.read(g.entry.node, g.fh, 0, 64).await;
    reply.expect_ok();
    assert_eq!(reply.data, b"hello");

    // FLUSH, FSYNC, RELEASE.
    c.call(Opcode::Flush, &FlushRequest { node: f.node, fh })
        .await
        .ok_unit();
    c.call(
        Opcode::Fsync,
        &FsyncRequest {
            node: f.node,
            fh,
            datasync: true,
        },
    )
    .await
    .ok_unit();
    c.release(g.entry.node, g.fh).await.ok_unit();

    // RENAME with RENAME_NOREPLACE onto a free name, across directories.
    c.call(
        Opcode::Rename,
        &RenameRequest {
            parent: dir.node,
            name: b"g".to_vec(),
            newparent: root,
            newname: b"g2".to_vec(),
            flags: libc::RENAME_NOREPLACE,
        },
    )
    .await
    .ok_unit();

    // OPEN the renamed file under its new name. The node is unchanged, because
    // the inode is: a server-side rename cannot invalidate a node id.
    let g2: Entry = c.lookup(root, b"g2").await.ok();
    assert_eq!(g2.node, g.entry.node);
    let reopened: OpenReply = c.open(g2.node, libc::O_RDONLY).await.ok();
    let reply = c.read(g2.node, reopened.fh, 0, 64).await;
    reply.expect_ok();
    assert_eq!(reply.data, b"hello");
    c.release(g2.node, reopened.fh).await.ok_unit();

    // OPENDIR, READDIR, READDIRPLUS, FSYNCDIR, RELEASEDIR.
    let dh: OpendirReply = c.opendir(root).await.ok();
    let page: ReaddirReply = c.readdir(root, dh.dh, 0, MAX_BODY_SIZE).await.ok();
    assert!(page.end);
    let names: Vec<&[u8]> = page.entries.iter().map(|e| e.name.as_slice()).collect();
    for want in [b".".as_slice(), b"..", b"dir", b"hard", b"g2"] {
        assert!(
            names.contains(&want),
            "{want:?} is missing from the listing"
        );
    }
    assert!(
        page.entries.iter().all(|e| e.ino != 0),
        "glibc drops a dirent whose inode is zero"
    );
    let plus: ReaddirplusReply = c.readdirplus(root, dh.dh, 0, MAX_BODY_SIZE).await.ok();
    assert!(plus.end);
    let hard_plus = plus.entries.iter().find(|e| e.name == b"hard").unwrap();
    assert_eq!(hard_plus.entry.node, f.node);
    assert_eq!(hard_plus.entry.attr.size, 4096);
    c.call(
        Opcode::Fsyncdir,
        &FsyncdirRequest {
            node: root,
            dh: dh.dh,
            datasync: false,
        },
    )
    .await
    .ok_unit();
    c.releasedir(root, dh.dh).await.ok_unit();

    // STATFS
    let st: StatfsReply = c
        .call(Opcode::Statfs, &StatfsRequest { node: root })
        .await
        .ok();
    assert!(st.bsize > 0 && st.blocks > 0 && st.namelen > 0);

    // The xattr quartet. A backing filesystem without user xattrs answers
    // EOPNOTSUPP, which is a legitimate answer and not this test's subject;
    // the opcodes are still sent, so the coverage assertion holds either way.
    let set = c
        .call_data(
            Opcode::Setxattr,
            &SetxattrRequest {
                node: f.node,
                name: b"user.k".to_vec(),
                flags: 0,
            },
            b"v1",
        )
        .await;
    let supported = set.is_ok();
    assert!(
        supported || set.is_errno(libc::EOPNOTSUPP),
        "SETXATTR answered {}",
        set.status
    );
    let got = c
        .call(
            Opcode::Getxattr,
            &GetxattrRequest {
                node: f.node,
                name: b"user.k".to_vec(),
                size: 64,
            },
        )
        .await;
    let listed = c
        .call(
            Opcode::Listxattr,
            &ListxattrRequest {
                node: f.node,
                size: 256,
            },
        )
        .await;
    let removed = c
        .call(
            Opcode::Removexattr,
            &RemovexattrRequest {
                node: f.node,
                name: b"user.k".to_vec(),
            },
        )
        .await;
    if supported {
        let reply: XattrReply = got.ok();
        assert_eq!(reply.size, 2);
        assert_eq!(got.data, b"v1", "the value rides in the data segment");
        let reply: XattrReply = listed.ok();
        assert_eq!(reply.size as usize, listed.data.len());
        assert!(listed.data.windows(6).any(|w| w == b"user.k"));
        removed.ok_unit();
    } else {
        eprintln!("note: the export has no user xattrs; values were not checked");
    }

    // UNLINK and RMDIR put the export back the way it was found.
    c.unlink(root, b"hard").await.ok_unit();
    c.unlink(root, b"g2").await.ok_unit();
    c.unlink(dir.node, b"l").await.ok_unit();
    c.unlink(dir.node, b"f").await.ok_unit();
    c.rmdir(root, b"dir").await.ok_unit();
    assert_eq!(std::fs::read_dir(srv.path()).unwrap().count(), 0);

    // FORGET retires every lookup count the walk took. It carries NO_REPLY, so
    // the GETATTR below is the next frame back.
    c.forget(vec![
        (f.node, u64::MAX),
        (dir.node, u64::MAX),
        (link.node, u64::MAX),
        (g2.node, u64::MAX),
    ])
    .await;
    c.getattr(root).await.expect_ok();

    let seen = c.opcodes_seen().clone();
    let all: BTreeSet<u16> = (1u16..=33).collect();
    let missing: Vec<u16> = all.difference(&seen).copied().collect();
    assert!(missing.is_empty(), "opcodes never exercised: {missing:?}");
    assert_eq!(seen, all, "an opcode outside the v1 table was sent");
}

// ---------------------------------------------------------------------------
// Errno paths
// ---------------------------------------------------------------------------

/// The namespace refusals, each one the errno the syscall underneath would
/// have produced. A backend errno travels to the client's FUSE reply
/// unchanged, so the status *is* the contract (spec §8).
#[tokio::test]
async fn namespace_errno_paths() {
    let srv = TestServer::start().await;
    std::fs::create_dir(srv.join("full")).unwrap();
    std::fs::write(srv.join("full/inside"), b"x").unwrap();
    std::fs::write(srv.join("taken"), b"x").unwrap();
    std::fs::write(srv.join("plain"), b"x").unwrap();
    let mut c = srv.attached().await;

    c.lookup(ROOT_NODE, b"missing")
        .await
        .expect_errno(libc::ENOENT);
    c.rmdir(ROOT_NODE, b"full")
        .await
        .expect_errno(libc::ENOTEMPTY);
    c.rmdir(ROOT_NODE, b"plain")
        .await
        .expect_errno(libc::ENOTDIR);
    c.unlink(ROOT_NODE, b"full")
        .await
        .expect_errno(libc::EISDIR);
    c.mkdir(ROOT_NODE, b"taken", 0o755)
        .await
        .expect_errno(libc::EEXIST);

    // RENAME_NOREPLACE onto a name that exists. The flag reaches the kernel
    // untouched, which is the only reason this is EEXIST and not a clobber.
    let onto_taken = |flags| RenameRequest {
        parent: ROOT_NODE,
        name: b"plain".to_vec(),
        newparent: ROOT_NODE,
        newname: b"taken".to_vec(),
        flags,
    };
    c.call(Opcode::Rename, &onto_taken(libc::RENAME_NOREPLACE))
        .await
        .expect_errno(libc::EEXIST);
    // Without the flag the same rename replaces the target, so EEXIST above
    // was the flag's doing rather than a permission or a path problem.
    c.call(Opcode::Rename, &onto_taken(0)).await.ok_unit();

    // READLINK on something that is not a link. ENOENT, not the EINVAL a
    // path-based `readlink(2)` would give: the backend reads the link it holds
    // rather than one it names, and the empty-path form of `readlinkat` answers
    // ENOENT when the descriptor is not a symlink. Pinned as it behaves — a
    // client only issues READLINK for an inode whose attributes already said
    // symlink, so the two errnos are equally unreachable in practice.
    let taken: Entry = c.lookup(ROOT_NODE, b"taken").await.ok();
    c.call(Opcode::Readlink, &ReadlinkRequest { node: taken.node })
        .await
        .expect_errno(libc::ENOENT);
    c.opendir(taken.node).await.expect_errno(libc::ENOTDIR);

    // Names a lookup may never resolve. A FUSE lookup is single-component by
    // construction, so `.` and `..` could only be traversal.
    for name in [b"..".as_slice(), b".", b"", b"a/b", b"a\0b"] {
        c.lookup(ROOT_NODE, name).await.expect_errno(libc::EINVAL);
        c.mkdir(ROOT_NODE, name, 0o755)
            .await
            .expect_errno(libc::EINVAL);
    }
}

/// Nothing stops a client pairing any handle with any node, and every
/// operation on the server is descriptor-relative — so a handle onto one file
/// would otherwise read, truncate, or copy into another. Every op that takes a
/// handle checks the pair.
#[tokio::test]
async fn a_handle_or_node_the_client_does_not_own_is_refused() {
    let srv = TestServer::start().await;
    let mut c = srv.attached().await;
    let a: CreateReply = c.create(ROOT_NODE, b"a", 0o644, libc::O_RDWR).await.ok();
    let b: CreateReply = c.create(ROOT_NODE, b"b", 0o644, libc::O_RDWR).await.ok();
    assert_ne!(a.entry.node, b.entry.node);
    let (an, bn, afh, bfh) = (a.entry.node, b.entry.node, a.fh, b.fh);

    c.read(bn, afh, 0, 1).await.expect_errno(libc::EBADF);
    c.write(bn, afh, 0, b"x").await.expect_errno(libc::EBADF);
    c.call(Opcode::Flush, &FlushRequest { node: bn, fh: afh })
        .await
        .expect_errno(libc::EBADF);
    c.call(
        Opcode::Fsync,
        &FsyncRequest {
            node: bn,
            fh: afh,
            datasync: false,
        },
    )
    .await
    .expect_errno(libc::EBADF);
    c.call(
        Opcode::Fallocate,
        &FallocateRequest {
            node: bn,
            fh: afh,
            offset: 0,
            length: 1,
            mode: 0,
        },
    )
    .await
    .expect_errno(libc::EBADF);
    c.call(
        Opcode::Lseek,
        &LseekRequest {
            node: bn,
            fh: afh,
            offset: 0,
            whence: libc::SEEK_SET as u32,
        },
    )
    .await
    .expect_errno(libc::EBADF);
    c.call(
        Opcode::CopyFileRange,
        &CopyFileRangeRequest {
            node_in: bn,
            fh_in: afh,
            off_in: 0,
            node_out: bn,
            fh_out: bfh,
            off_out: 0,
            len: 1,
        },
    )
    .await
    .expect_errno(libc::EBADF);
    // SETATTR takes a handle too, and a truncate through it writes to whatever
    // descriptor it names.
    c.call(
        Opcode::Setattr,
        &SetattrRequest {
            node: bn,
            args: SetattrArgs {
                mode: None,
                uid: None,
                gid: None,
                size: Some(0),
                atime: TimeSet::Omit,
                mtime: TimeSet::Omit,
                fh: Some(afh),
            },
        },
    )
    .await
    .expect_errno(libc::EBADF);
    c.release(bn, afh).await.expect_errno(libc::EBADF);

    // A whence the kernel has no name for.
    c.call(
        Opcode::Lseek,
        &LseekRequest {
            node: an,
            fh: afh,
            offset: 0,
            whence: 999,
        },
    )
    .await
    .expect_errno(libc::EINVAL);

    // A node id this session never issued: it is either already forgotten or
    // from a previous session, and both are ESTALE.
    const GHOST: NodeId = 9_999;
    c.getattr(GHOST).await.expect_errno(libc::ESTALE);
    c.lookup(GHOST, b"x").await.expect_errno(libc::ESTALE);
    c.mkdir(GHOST, b"x", 0o755).await.expect_errno(libc::ESTALE);
    c.opendir(GHOST).await.expect_errno(libc::ESTALE);
    c.call(Opcode::Statfs, &StatfsRequest { node: GHOST })
        .await
        .expect_errno(libc::ESTALE);
    c.call(Opcode::Readlink, &ReadlinkRequest { node: GHOST })
        .await
        .expect_errno(libc::ESTALE);
    c.call(
        Opcode::Getxattr,
        &GetxattrRequest {
            node: GHOST,
            name: b"user.k".to_vec(),
            size: 0,
        },
    )
    .await
    .expect_errno(libc::ESTALE);

    // RELEASE retires the handle it answers for, and the only way to see that
    // from the wire is to use the handle afterwards. Nothing else in the
    // workspace proves it: a refactor that answered OK without removing the
    // handle would leak one descriptor per open with every test still green.
    c.release(an, afh).await.ok_unit();
    c.read(an, afh, 0, 1).await.expect_errno(libc::EBADF);

    // RELEASE and FLUSH are the two that tolerate a handle they cannot find: a
    // RELEASE whose reply was lost gets retried, and refusing the retry would
    // surface EBADF from the application's close(2).
    c.release(an, afh).await.ok_unit();
    c.call(Opcode::Flush, &FlushRequest { node: an, fh: afh })
        .await
        .ok_unit();
}

/// The node table drops a mapping when its last lookup count goes, and every
/// id the client keeps past that answers ESTALE (spec §8).
#[tokio::test]
async fn a_forgotten_node_answers_estale() {
    let srv = TestServer::start().await;
    std::fs::write(srv.join("f"), b"x").unwrap();
    let mut c = srv.attached().await;

    let e: Entry = c.lookup(ROOT_NODE, b"f").await.ok();
    c.getattr(e.node).await.expect_ok();
    // FORGET runs inline in the read loop, so it has already happened by the
    // time the next frame is read — no ordering to arrange.
    c.forget(vec![(e.node, 1)]).await;
    c.getattr(e.node).await.expect_errno(libc::ESTALE);

    // Two lookups of one name are two counts on one node, and each owes its
    // own FORGET. Retiring the id on the first would strand the client holding
    // a node the server no longer has.
    let first: Entry = c.lookup(ROOT_NODE, b"f").await.ok();
    let again: Entry = c.lookup(ROOT_NODE, b"f").await.ok();
    assert_eq!(first.node, again.node, "one inode is one node");
    assert_ne!(first.node, e.node, "a forgotten id is never re-issued");
    c.forget(vec![(first.node, 1)]).await;
    c.getattr(first.node).await.expect_ok();
    c.forget(vec![(first.node, 1)]).await;
    c.getattr(first.node).await.expect_errno(libc::ESTALE);

    // The export root is immortal: a client that could forget it would have no
    // way to name anything again.
    c.forget(vec![(ROOT_NODE, u64::MAX)]).await;
    c.getattr(ROOT_NODE).await.expect_ok();

    // An id nobody ever held is ignored, not fatal.
    c.forget(vec![(4_242, 1)]).await;
    c.getattr(ROOT_NODE).await.expect_ok();
}

// ---------------------------------------------------------------------------
// Durability policy (spec §6)
// ---------------------------------------------------------------------------

/// Whether `honor` really reaches the kernel is invisible on a regular file: a
/// successful `fsync` and a skipped one look identical. A FIFO is the witness
/// — `fsync(2)` answers EINVAL on one — so the two policies give visibly
/// different answers to the same frame.
#[tokio::test]
async fn fsync_honor_runs_the_syscall() {
    let srv = TestServer::with(FsyncPolicy::Honor, SERVER_WINDOW, SERVER_IO).await;
    make_fifo(&srv.join("p"));
    std::fs::write(srv.join("f"), b"x").unwrap();
    let mut c = srv.attached().await;

    let p: Entry = c.lookup(ROOT_NODE, b"p").await.ok();
    // O_NONBLOCK, or opening a peerless FIFO's read end never returns.
    let pfh: OpenReply = c.open(p.node, libc::O_RDONLY | libc::O_NONBLOCK).await.ok();
    c.call(
        Opcode::Fsync,
        &FsyncRequest {
            node: p.node,
            fh: pfh.fh,
            datasync: false,
        },
    )
    .await
    .expect_errno(libc::EINVAL);

    // On a file that can be synced, both flavours succeed.
    let f: Entry = c.lookup(ROOT_NODE, b"f").await.ok();
    let fh: OpenReply = c.open(f.node, libc::O_RDWR).await.ok();
    for datasync in [false, true] {
        c.call(
            Opcode::Fsync,
            &FsyncRequest {
                node: f.node,
                fh: fh.fh,
                datasync,
            },
        )
        .await
        .ok_unit();
    }
    // FSYNCDIR reaching the policy at all, which is all this can show: there
    // is no directory analogue of the FIFO, so `ignore` answers OK here too.
    // What makes that acceptable is that both halves run through the same
    // `maybe_fsync`, and the file case above already discriminates the two
    // policies against it.
    let dh: OpendirReply = c.opendir(ROOT_NODE).await.ok();
    c.call(
        Opcode::Fsyncdir,
        &FsyncdirRequest {
            node: ROOT_NODE,
            dh: dh.dh,
            datasync: false,
        },
    )
    .await
    .ok_unit();
}

/// `ignore` acknowledges without touching the disk — the trade an NFS `async`
/// export makes, latency for crash durability. Acknowledging is not the same
/// as skipping the request: the handle is still checked, and an `O_SYNC` open
/// still yields a working handle rather than a refusal.
#[tokio::test]
async fn fsync_ignore_acknowledges_without_the_syscall() {
    let srv = TestServer::with(FsyncPolicy::Ignore, SERVER_WINDOW, SERVER_IO).await;
    make_fifo(&srv.join("p"));
    let mut c = srv.attached().await;

    let p: Entry = c.lookup(ROOT_NODE, b"p").await.ok();
    let pfh: OpenReply = c.open(p.node, libc::O_RDONLY | libc::O_NONBLOCK).await.ok();
    // The exact frame the honoring server answered EINVAL. The policy is the
    // only difference between the two, so the syscall plainly never ran.
    c.call(
        Opcode::Fsync,
        &FsyncRequest {
            node: p.node,
            fh: pfh.fh,
            datasync: false,
        },
    )
    .await
    .ok_unit();

    // An O_SYNC open yields a writable handle: the flag is masked, the request
    // is not refused.
    let s: CreateReply = c
        .create(ROOT_NODE, b"s", 0o644, libc::O_WRONLY | libc::O_SYNC)
        .await
        .ok();
    let written: WriteReply = c.write(s.entry.node, s.fh, 0, b"durable enough").await.ok();
    assert_eq!(written.written, 14);
    c.call(
        Opcode::Fsync,
        &FsyncRequest {
            node: s.entry.node,
            fh: s.fh,
            datasync: true,
        },
    )
    .await
    .ok_unit();
    // The server-side truth: the bytes are on the export whatever the policy
    // did about durability.
    assert_eq!(std::fs::read(srv.join("s")).unwrap(), b"durable enough");

    // Ignoring the sync is not ignoring the handle.
    c.call(
        Opcode::Fsync,
        &FsyncRequest {
            node: p.node,
            fh: s.fh,
            datasync: false,
        },
    )
    .await
    .expect_errno(libc::EBADF);
    c.call(
        Opcode::Fsyncdir,
        &FsyncdirRequest {
            node: ROOT_NODE,
            dh: 4_242,
            datasync: false,
        },
    )
    .await
    .expect_errno(libc::EBADF);
}

// ---------------------------------------------------------------------------
// Directories
// ---------------------------------------------------------------------------

/// A page's cursor is the `d_off` the kernel reported for its last entry, so a
/// client that stopped mid-listing resumes exactly where it stopped: no name
/// repeated, none dropped. Reassembling the paged walk into the single-shot
/// listing is the only assertion that catches an off-by-one in either
/// direction.
#[tokio::test]
async fn readdir_pages_resume_from_their_cookies() {
    const FILES: usize = 24;
    let srv = TestServer::start().await;
    for i in 0..FILES {
        std::fs::write(srv.join(&format!("f{i:02}")), b"").unwrap();
    }
    let mut c = srv.attached().await;
    let dh: OpendirReply = c.opendir(ROOT_NODE).await.ok();

    let whole: ReaddirReply = c.readdir(ROOT_NODE, dh.dh, 0, MAX_BODY_SIZE).await.ok();
    assert!(whole.end);
    assert_eq!(whole.entries.len(), FILES + 2, "the files, plus . and ..");
    assert!(
        whole.entries.iter().all(|e| e.ino != 0),
        "glibc drops a dirent whose inode is zero"
    );
    // `..` reports the true parent inode, which is the one number a
    // directory's own attributes cannot supply.
    let parent = srv.path().canonicalize().unwrap();
    let parent_ino = std::fs::metadata(parent.parent().unwrap()).unwrap().ino();
    let dotdot = whole.entries.iter().find(|e| e.name == b"..").unwrap();
    assert_eq!(dotdot.ino, parent_ino);

    let mut paged: Vec<DirEntry> = Vec::new();
    let mut offset = 0;
    let mut pages = 0;
    loop {
        // A budget far too small for the listing, so the cursor does the work.
        let page: ReaddirReply = c.readdir(ROOT_NODE, dh.dh, offset, 64).await.ok();
        pages += 1;
        assert!(
            !page.entries.is_empty() || page.end,
            "a non-final page with no entries leaves the client no cursor"
        );
        if let Some(last) = page.entries.last() {
            offset = last.offset;
        }
        let end = page.end;
        paged.extend(page.entries);
        assert!(pages < 100, "paging must terminate");
        if end {
            break;
        }
    }
    assert!(pages > 1, "the budget must have forced pagination");
    assert_eq!(paged, whole.entries, "no name repeated, none dropped");

    // A cursor the server never issued is a client bug, and EINVAL says so
    // rather than silently truncating the listing.
    c.readdir(ROOT_NODE, dh.dh, u64::MAX, 4096)
        .await
        .expect_errno(libc::EINVAL);
    c.readdirplus(ROOT_NODE, dh.dh, u64::MAX, 4096)
        .await
        .expect_errno(libc::EINVAL);

    c.releasedir(ROOT_NODE, dh.dh).await.ok_unit();
    // A retried RELEASEDIR must not fail the application's closedir(3), but
    // the handle is genuinely gone.
    c.releasedir(ROOT_NODE, dh.dh).await.ok_unit();
    c.readdir(ROOT_NODE, dh.dh, 0, 4096)
        .await
        .expect_errno(libc::EBADF);
}

/// Spec §3.1 makes an oversize body fatal *on receipt*, so a client that asks
/// for a megabyte of entries and gets an honest megabyte back would kill its
/// own mount. The ask is trimmed rather than served.
#[tokio::test]
async fn a_directory_page_never_exceeds_the_body_maximum() {
    let srv = TestServer::start().await;
    // Names long enough that 64 KiB cannot hold the directory.
    for i in 0..400 {
        std::fs::write(srv.join(&format!("{i:0>200}")), b"").unwrap();
    }
    let mut c = srv.attached().await;
    let dh: OpendirReply = c.opendir(ROOT_NODE).await.ok();
    let max = MAX_BODY_SIZE as usize;

    for ask in [u32::MAX, 1 << 20] {
        let reply = c.readdir(ROOT_NODE, dh.dh, 0, ask).await;
        let page: ReaddirReply = reply.ok();
        assert!(!page.end, "the directory must not fit in one page");
        assert!(
            reply.body.len() <= max,
            "readdir body {} exceeds the frame maximum",
            reply.body.len()
        );

        let reply = c.readdirplus(ROOT_NODE, dh.dh, 0, ask).await;
        let page: ReaddirplusReply = reply.ok();
        assert!(!page.end);
        assert!(
            reply.body.len() <= max,
            "readdirplus body {} exceeds the frame maximum",
            reply.body.len()
        );
    }
}

/// Every `Entry` a READDIRPLUS page carries is a lookup the client's kernel
/// counts, so each owes exactly one FORGET — and the names that take no count
/// must come back as node 0, or the ledger never balances.
#[tokio::test]
async fn readdirplus_owes_one_forget_per_registered_entry() {
    let srv = TestServer::start().await;
    std::fs::create_dir(srv.join("d")).unwrap();
    for name in ["a", "b", "c", "gone"] {
        std::fs::write(srv.join(&format!("d/{name}")), b"x").unwrap();
    }
    let mut c = srv.attached().await;

    let d: Entry = c.lookup(ROOT_NODE, b"d").await.ok();
    let dh: OpendirReply = c.opendir(d.node).await.ok();
    // The snapshot was taken at OPENDIR; the directory moves on without it.
    std::fs::remove_file(srv.join("d/gone")).unwrap();

    let first: ReaddirplusReply = c.readdirplus(d.node, dh.dh, 0, MAX_BODY_SIZE).await.ok();
    assert!(first.end);
    assert_eq!(first.entries.len(), 6, ". .. a b c gone");

    let node_of = |page: &ReaddirplusReply, name: &[u8]| {
        page.entries
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("{name:?} is missing from the page"))
            .entry
            .node
    };

    // `.` and `..` are attribute-only: the client's kernel instantiates no
    // dentry for them, so registering them would put two entries per directory
    // on a ledger nobody will ever retire.
    for dot in [b".".as_slice(), b".."] {
        assert_eq!(node_of(&first, dot), 0, "{dot:?} takes no lookup count");
    }
    // A name that vanished after the snapshot still travels — READDIR and
    // READDIRPLUS must agree on what the directory holds — but as node 0, for
    // the same reason: no node, no count, no FORGET.
    assert_eq!(node_of(&first, b"gone"), 0);

    let names: [&[u8]; 3] = [b"a", b"b", b"c"];
    let registered: Vec<NodeId> = names.iter().map(|n| node_of(&first, n)).collect();
    assert!(registered.iter().all(|&n| n > ROOT_NODE));

    // A second page over the same names is a second lookup count each.
    let second: ReaddirplusReply = c.readdirplus(d.node, dh.dh, 0, MAX_BODY_SIZE).await.ok();
    for (i, name) in names.iter().enumerate() {
        assert_eq!(node_of(&second, name), registered[i], "one inode, one node");
    }

    let batch: Vec<(NodeId, u64)> = registered.iter().map(|&n| (n, 1)).collect();
    c.forget(batch.clone()).await;
    for &n in &registered {
        c.getattr(n).await.expect_ok();
    }
    c.forget(batch).await;
    for &n in &registered {
        c.getattr(n).await.expect_errno(libc::ESTALE);
    }

    c.releasedir(d.node, dh.dh).await.ok_unit();
}

// ---------------------------------------------------------------------------
// Frame contract
// ---------------------------------------------------------------------------

/// Checked before a byte is allocated for the body it describes, which is why
/// nothing follows the header here (spec §3.1).
#[tokio::test]
async fn an_oversize_body_len_closes_the_connection() {
    let srv = TestServer::start().await;
    let mut c = srv.attached().await;
    c.header_only(FrameHeader {
        request_id: 7,
        op_or_status: Opcode::Getattr as u16,
        flags: 0,
        body_len: MAX_BODY_SIZE + 1,
        data_len: 0,
    })
    .await;
    assert!(
        c.closed().await,
        "a body past MAX_BODY_SIZE is connection-fatal"
    );
}

/// Only WRITE and SETXATTR carry bulk bytes inbound. A data segment anywhere
/// else means the two ends disagree about the frame, and the stream is already
/// unrecoverable.
#[tokio::test]
async fn a_data_segment_on_the_wrong_opcode_closes_the_connection() {
    let srv = TestServer::start().await;
    let mut c = srv.attached().await;
    c.begin_data(
        Opcode::Getattr,
        &GetattrRequest {
            node: ROOT_NODE,
            fh: None,
        },
        b"junk",
    )
    .await;
    assert!(c.closed().await, "GETATTR carries no data segment");
}

/// SETXATTR's data segment is bounded by the body maximum rather than the
/// negotiated I/O size — an xattr is not I/O and does not travel on the I/O
/// budget — but a larger cap is still a cap. The header alone settles it, which
/// is why nothing follows this one.
#[tokio::test]
async fn a_setxattr_data_segment_past_the_body_maximum_closes_the_connection() {
    let srv = TestServer::start().await;
    let mut c = srv.attached().await;
    c.header_only(FrameHeader {
        request_id: 5,
        op_or_status: Opcode::Setxattr as u16,
        flags: 0,
        body_len: 0,
        data_len: MAX_BODY_SIZE + 1,
    })
    .await;
    assert!(
        c.closed().await,
        "the xattr exemption is a larger bound, not the absence of one"
    );
}

/// A body the server cannot decode is that request's problem, not the
/// connection's: the frame's lengths were honored, so the stream is still in
/// sync and the next request is still answerable. Every filesystem opcode has
/// to agree on that — the handshake pair and FORGET are fatal instead, and are
/// covered above.
#[tokio::test]
async fn a_malformed_body_answers_einval_on_every_filesystem_opcode() {
    let srv = TestServer::start().await;
    let mut c = srv.attached().await;

    // One byte with the varint continuation bit set. Every request type in the
    // table opens with a u64, so postcard runs out of input in all of them.
    let mut checked = 0;
    for raw in 3u16..=33 {
        if raw == Opcode::Forget as u16 {
            continue;
        }
        let op = Opcode::try_from(raw).unwrap();
        let id = c.begin_raw_body(op, &[0xff]).await;
        let reply = c.recv().await;
        assert_eq!(reply.id, id, "{op:?} answered somebody else's request");
        assert_eq!(
            reply.status,
            libc::EINVAL as u16,
            "{op:?} should answer EINVAL to a truncated body, got {}",
            reply.status
        );
        assert!(reply.body.is_empty(), "{op:?} error reply carries a body");
        checked += 1;
    }
    assert_eq!(checked, 30, "every filesystem opcode must be covered");

    // Still serving, thirty violations later.
    c.getattr(ROOT_NODE).await.expect_ok();
}

// ---------------------------------------------------------------------------
// Pipelining
// ---------------------------------------------------------------------------

/// FORGET is exempt from the in-flight window on purpose. It carries NO_REPLY,
/// so the client has no answer to count against its own accounting and no way
/// to know when a permit would free; charging it one would let a legal batch of
/// forgets, sent while the window is full, look like an overrun. It costs the
/// server nothing to leave out — forgets run inline in the read loop, so no
/// number of them buys the client any concurrency.
#[tokio::test]
async fn forget_takes_no_window_permit() {
    const WINDOW: u32 = 8;
    let srv = TestServer::start().await;
    for i in 0..4 {
        std::fs::write(srv.join(&format!("f{i}")), b"x").unwrap();
    }
    let mut c = srv.attached_with(WINDOW, SERVER_IO, false).await;
    assert_eq!(c.settled().max_inflight, WINDOW);

    let mut nodes: Vec<NodeId> = Vec::new();
    for i in 0..4 {
        let e: Entry = c.lookup(ROOT_NODE, format!("f{i}").as_bytes()).await.ok();
        nodes.push(e.node);
    }

    // Four windows' worth, back to back, with nothing else in flight. A permit
    // charged here would never come back — FORGET queues no reply for the
    // writer to release one against — so the connection would be gone long
    // before the last of these.
    for _ in 0..4 * WINDOW {
        c.forget(Vec::new()).await;
    }
    c.getattr(ROOT_NODE).await.expect_ok();

    // Now the shape the exemption exists for: a client retiring entries it will
    // never hear about again while its own reads are still outstanding.
    let mut owed = BTreeSet::new();
    for _ in 0..WINDOW {
        owed.insert(
            c.begin(
                Opcode::Getattr,
                &GetattrRequest {
                    node: ROOT_NODE,
                    fh: None,
                },
            )
            .await,
        );
    }
    for &n in &nodes {
        c.forget(vec![(n, 1)]).await;
    }
    for _ in 0..WINDOW {
        let reply = c.recv().await;
        reply.expect_ok();
        assert!(owed.remove(&reply.id), "reply {} was not owed", reply.id);
    }

    // And they were not merely tolerated: the nodes they named are gone.
    for &n in &nodes {
        c.getattr(n).await.expect_errno(libc::ESTALE);
    }
}

/// The window bounds requests in flight; it does not make them answer in
/// order. A READ waiting on a disk must not hold up the GETATTR behind it, and
/// the client re-associates answers by request id (spec §4).
///
/// Two rounds, because the second can only be admitted if the first round's
/// permits came back — and they are released by the writer after the reply is
/// on the socket, not by the handler when it queues one.
#[tokio::test]
async fn a_full_window_of_requests_is_answered_and_the_permits_recycle() {
    const WINDOW: u32 = 8;
    let srv = TestServer::start().await;
    std::fs::write(srv.join("f"), b"hello world").unwrap();
    std::os::unix::fs::symlink("f", srv.join("l")).unwrap();

    let mut c = srv.attached_with(WINDOW, SERVER_IO, false).await;
    assert_eq!(c.settled().max_inflight, WINDOW);
    let f: Entry = c.lookup(ROOT_NODE, b"f").await.ok();
    let l: Entry = c.lookup(ROOT_NODE, b"l").await.ok();
    let open: OpenReply = c.open(f.node, libc::O_RDONLY).await.ok();

    for round in 0..2 {
        let mut pending: BTreeMap<u64, &str> = BTreeMap::new();
        let id = c
            .begin(
                Opcode::Read,
                &ReadRequest {
                    node: f.node,
                    fh: open.fh,
                    offset: 0,
                    size: SERVER_IO,
                },
            )
            .await;
        pending.insert(id, "read");
        let id = c
            .begin(
                Opcode::Getattr,
                &GetattrRequest {
                    node: ROOT_NODE,
                    fh: None,
                },
            )
            .await;
        pending.insert(id, "getattr");
        let id = c
            .begin(
                Opcode::Lookup,
                &LookupRequest {
                    parent: ROOT_NODE,
                    name: b"f".to_vec(),
                },
            )
            .await;
        pending.insert(id, "lookup");
        let id = c
            .begin(Opcode::Statfs, &StatfsRequest { node: ROOT_NODE })
            .await;
        pending.insert(id, "statfs");
        let id = c
            .begin(Opcode::Readlink, &ReadlinkRequest { node: l.node })
            .await;
        pending.insert(id, "readlink");
        let id = c
            .begin(
                Opcode::Lseek,
                &LseekRequest {
                    node: f.node,
                    fh: open.fh,
                    offset: 0,
                    whence: libc::SEEK_END as u32,
                },
            )
            .await;
        pending.insert(id, "lseek");
        let id = c
            .begin(
                Opcode::Listxattr,
                &ListxattrRequest {
                    node: ROOT_NODE,
                    size: 0,
                },
            )
            .await;
        pending.insert(id, "listxattr");
        let id = c
            .begin(Opcode::Opendir, &OpendirRequest { node: ROOT_NODE })
            .await;
        pending.insert(id, "opendir");
        assert_eq!(pending.len(), WINDOW as usize, "the window must be full");

        for _ in 0..WINDOW {
            let reply = c.recv().await;
            let which = pending
                .remove(&reply.id)
                .unwrap_or_else(|| panic!("round {round}: reply {} was not owed", reply.id));
            reply.expect_ok();
            match which {
                "read" => assert_eq!(reply.data, b"hello world"),
                "readlink" => {
                    let r: ReadlinkReply = reply.ok();
                    assert_eq!(r.target, b"f");
                }
                "lseek" => {
                    let r: LseekReply = reply.ok();
                    assert_eq!(r.offset, 11);
                }
                _ => {}
            }
        }
        assert!(pending.is_empty(), "every id is answered exactly once");
    }
}

// ---------------------------------------------------------------------------
// Xattrs
// ---------------------------------------------------------------------------

/// Xattr get and list use FUSE's two-phase shape: `size == 0` asks for the
/// length alone, and the reply carries the count with an empty data segment.
/// The emptiness matters — the server fetches into a pooled buffer that is
/// recycled without zeroing, so a probe that returned its bytes would return
/// somebody else's.
#[tokio::test]
async fn xattr_probes_short_buffers_and_absent_names() {
    let srv = TestServer::start().await;
    std::fs::write(srv.join("f"), b"").unwrap();
    std::os::unix::fs::symlink("f", srv.join("l")).unwrap();
    let mut c = srv.attached().await;
    let f: Entry = c.lookup(ROOT_NODE, b"f").await.ok();

    let set = c
        .call_data(
            Opcode::Setxattr,
            &SetxattrRequest {
                node: f.node,
                name: b"user.k".to_vec(),
                flags: 0,
            },
            b"value",
        )
        .await;
    if set.is_errno(libc::EOPNOTSUPP) {
        eprintln!("skipping: the export has no user xattrs");
        return;
    }
    set.ok_unit();

    let probe = c
        .call(
            Opcode::Getxattr,
            &GetxattrRequest {
                node: f.node,
                name: b"user.k".to_vec(),
                size: 0,
            },
        )
        .await;
    let reply: XattrReply = probe.ok();
    assert_eq!(reply.size, 5);
    assert!(
        probe.data.is_empty(),
        "a probe writes nothing, so it returns nothing"
    );

    // A caller buffer shorter than the value is ERANGE, exactly what the
    // client's own getxattr(2) expects to see.
    c.call(
        Opcode::Getxattr,
        &GetxattrRequest {
            node: f.node,
            name: b"user.k".to_vec(),
            size: 2,
        },
    )
    .await
    .expect_errno(libc::ERANGE);

    // LISTXATTR honors the same two-phase rule.
    let probe = c
        .call(
            Opcode::Listxattr,
            &ListxattrRequest {
                node: f.node,
                size: 0,
            },
        )
        .await;
    let reply: XattrReply = probe.ok();
    assert!(reply.size > 0);
    assert!(probe.data.is_empty());

    // A name that is not there.
    for op in [Opcode::Getxattr, Opcode::Removexattr] {
        let reply = match op {
            Opcode::Getxattr => {
                c.call(
                    Opcode::Getxattr,
                    &GetxattrRequest {
                        node: f.node,
                        name: b"user.absent".to_vec(),
                        size: 64,
                    },
                )
                .await
            }
            _ => {
                c.call(
                    Opcode::Removexattr,
                    &RemovexattrRequest {
                        node: f.node,
                        name: b"user.absent".to_vec(),
                    },
                )
                .await
            }
        };
        reply.expect_errno(libc::ENODATA);
    }

    // v1 scopes xattrs to regular files and directories: the reopen the ring
    // ops need cannot happen on a symlink without dereferencing it.
    let l: Entry = c.lookup(ROOT_NODE, b"l").await.ok();
    c.call(
        Opcode::Getxattr,
        &GetxattrRequest {
            node: l.node,
            name: b"user.k".to_vec(),
            size: 64,
        },
    )
    .await
    .expect_errno(libc::EOPNOTSUPP);
}

/// The server's xattr buffer is sized by its I/O ceiling, and `max_io_size`
/// has no lower clamp in the config — so a value larger than that buffer is
/// reachable by configuration rather than merely in theory. It can never
/// travel, and saying so once beats sending the client back for a fetch that
/// can only answer ERANGE and a probe that repeats the advice forever.
#[tokio::test]
async fn a_value_larger_than_the_fetch_buffer_is_e2big_at_both_ends() {
    const BUF: u32 = 4096;
    let srv = TestServer::with(FsyncPolicy::Honor, SERVER_WINDOW, BUF).await;
    let path = srv.join("f");
    std::fs::write(&path, b"").unwrap();
    let value = vec![b'v'; 2 * BUF as usize];

    let mut c = srv.attached().await;
    assert_eq!(c.settled().max_io_size, BUF);
    let f: Entry = c.lookup(ROOT_NODE, b"f").await.ok();

    // Inbound. The frame itself is legal — an xattr value is bounded by the
    // body maximum, not the negotiated I/O size — so the connection survives
    // and the backend answers for the value.
    c.call_data(
        Opcode::Setxattr,
        &SetxattrRequest {
            node: f.node,
            name: b"user.big".to_vec(),
            flags: 0,
        },
        &value,
    )
    .await
    .expect_errno(libc::E2BIG);

    // Outbound. Plant the same value out of band so the probe has one to find.
    if rustix::fs::setxattr(&path, "user.big", &value, rustix::fs::XattrFlags::empty()).is_err() {
        eprintln!("skipping: the export has no user xattrs");
        return;
    }
    c.call(
        Opcode::Getxattr,
        &GetxattrRequest {
            node: f.node,
            name: b"user.big".to_vec(),
            size: 0,
        },
    )
    .await
    .expect_errno(libc::E2BIG);
    // A real fetch is ERANGE straight from the syscall, which is what a client
    // that guessed a size expects.
    c.call(
        Opcode::Getxattr,
        &GetxattrRequest {
            node: f.node,
            name: b"user.big".to_vec(),
            size: BUF,
        },
    )
    .await
    .expect_errno(libc::ERANGE);
}

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

/// A Linux filename is any byte sequence without `/` or NUL, and a symlink
/// target is any byte sequence without NUL. Neither is text, and re-encoding
/// either through UTF-8 would corrupt perfectly legal names — so both travel
/// as bytes the whole way and come back identical.
#[tokio::test]
async fn non_utf8_names_round_trip() {
    let odd: &[u8] = &[0xff, 0xfe];
    let odder: &[u8] = &[0xfd, 0x80, b'.'];
    let srv = TestServer::start().await;
    let mut c = srv.attached().await;

    let created: CreateReply = c.create(ROOT_NODE, odd, 0o644, libc::O_RDWR).await.ok();
    let written: WriteReply = c
        .write(created.entry.node, created.fh, 0, b"bytes")
        .await
        .ok();
    assert_eq!(written.written, 5);
    c.release(created.entry.node, created.fh).await.ok_unit();

    let looked: Entry = c.lookup(ROOT_NODE, odd).await.ok();
    assert_eq!(looked.node, created.entry.node);

    let dh: OpendirReply = c.opendir(ROOT_NODE).await.ok();
    let page: ReaddirReply = c.readdir(ROOT_NODE, dh.dh, 0, MAX_BODY_SIZE).await.ok();
    assert!(
        page.entries.iter().any(|e| e.name == odd),
        "the name must survive the listing byte for byte"
    );
    let plus: ReaddirplusReply = c.readdirplus(ROOT_NODE, dh.dh, 0, MAX_BODY_SIZE).await.ok();
    assert!(plus
        .entries
        .iter()
        .any(|e| e.name == odd && e.entry.node == created.entry.node));
    c.releasedir(ROOT_NODE, dh.dh).await.ok_unit();

    let link: Entry = c
        .call(
            Opcode::Symlink,
            &SymlinkRequest {
                parent: ROOT_NODE,
                name: odder.to_vec(),
                target: odd.to_vec(),
            },
        )
        .await
        .ok();
    let target: ReadlinkReply = c
        .call(Opcode::Readlink, &ReadlinkRequest { node: link.node })
        .await
        .ok();
    assert_eq!(target.target, odd);

    c.call(
        Opcode::Rename,
        &RenameRequest {
            parent: ROOT_NODE,
            name: odd.to_vec(),
            newparent: ROOT_NODE,
            newname: b"plain".to_vec(),
            flags: 0,
        },
    )
    .await
    .ok_unit();
    c.unlink(ROOT_NODE, b"plain").await.ok_unit();
    c.unlink(ROOT_NODE, odder).await.ok_unit();
    // The host agrees the export is empty again.
    assert_eq!(std::fs::read_dir(srv.path()).unwrap().count(), 0);
}
