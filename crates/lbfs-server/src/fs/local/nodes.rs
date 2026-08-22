//! The node table: FUSE node ids to `O_PATH` descriptors.
//!
//! Every filesystem operation the server executes resolves its target through
//! this table. A node owns an `O_PATH` descriptor, which on local Linux
//! filesystems pins the inode for as long as the kernel holds a lookup count on
//! the id, so `(st_dev, st_ino)` cannot come to name a different file
//! underneath a live node. That is what the dedup in `register` rests on.
//! Exporting a network or FUSE filesystem voids the argument: `st_ino` there is
//! server-assigned and may be recycled or synthesized by hashing, so two
//! distinct live files can collide on one key and `register` would merge them.
//!
//! Separately, `forget` drops a reverse-map entry without checking which node
//! it names. A structural invariant, not the inode pin, is what makes that
//! safe: for every live `(id, node)` in `nodes`, `by_key[node.key] == id`, and
//! `register` can never create a second live node for one key, because the
//! dedup hit returns first and every mutation runs under a single lock. The
//! invariant needs ids to stay unrecycled; a `debug_assert!` in `forget` trips
//! if that ever changes.

use lbfs_proto::types::{NodeId, ROOT_NODE};
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};

/// `(st_dev, st_ino)` — identifies one inode, so hard links to the same file
/// collapse onto a single node.
pub type FileKey = (u64, u64);

struct Node {
    fd: Arc<OwnedFd>,
    key: FileKey,
    generation: u64,
    /// `st_mode & S_IFMT`, captured when the node was first registered.
    ///
    /// A live inode never changes type — no syscall exists that would do it —
    /// and the `O_PATH` descriptor above pins that inode for as long as the
    /// node lives, so this value cannot go stale. Permission bits are *not*
    /// stored, because `SETATTR` changes those and nothing here would notice.
    file_type: u32,
    /// The kernel's lookup count for this id, decremented by `FUSE_FORGET`.
    nlookup: u64,
}

struct Inner {
    nodes: HashMap<NodeId, Node>,
    by_key: HashMap<FileKey, NodeId>,
    next_id: NodeId,
    next_generation: u64,
}

pub struct NodeTable(Mutex<Inner>);

impl NodeTable {
    /// Installs `root_fd` as `ROOT_NODE` with an immortal lookup count.
    pub fn new(root_fd: OwnedFd, root_key: FileKey, file_type: u32) -> NodeTable {
        let mut nodes = HashMap::new();
        let mut by_key = HashMap::new();
        nodes.insert(
            ROOT_NODE,
            Node {
                fd: Arc::new(root_fd),
                key: root_key,
                generation: 0,
                file_type,
                nlookup: u64::MAX,
            },
        );
        by_key.insert(root_key, ROOT_NODE);
        NodeTable(Mutex::new(Inner {
            nodes,
            by_key,
            next_id: ROOT_NODE + 1,
            next_generation: 1,
        }))
    }

    /// Returns the node for `key`, creating it if this is the first lookup.
    ///
    /// When `key` is already present the existing node is returned with its
    /// lookup count bumped and `fd` closed — the table already owns a
    /// descriptor for that inode.
    pub fn register(
        &self,
        fd: OwnedFd,
        key: FileKey,
        file_type: u32,
    ) -> (NodeId, u64, Arc<OwnedFd>) {
        let mut g = self.0.lock().unwrap();
        if let Some(&id) = g.by_key.get(&key) {
            let node = g.nodes.get_mut(&id).expect("by_key points at a live node");
            node.nlookup = node.nlookup.saturating_add(1);
            return (id, node.generation, Arc::clone(&node.fd));
            // `fd` drops here: the table already owns an fd for this file.
        }
        let id = g.next_id;
        g.next_id += 1;
        let generation = g.next_generation;
        g.next_generation += 1;
        let fd = Arc::new(fd);
        g.nodes.insert(
            id,
            Node {
                fd: Arc::clone(&fd),
                key,
                generation,
                file_type,
                nlookup: 1,
            },
        );
        g.by_key.insert(key, id);
        (id, generation, fd)
    }

    /// Resolves a node id to its descriptor and generation. `None` means the
    /// id is unknown, which the caller reports as `ESTALE`.
    pub fn get(&self, node: NodeId) -> Option<(Arc<OwnedFd>, u64)> {
        let g = self.0.lock().unwrap();
        g.nodes
            .get(&node)
            .map(|n| (Arc::clone(&n.fd), n.generation))
    }

    /// The stored `S_IFMT` bits, or `None` for an id this table never issued
    /// or has already forgotten.
    pub fn file_type(&self, node: NodeId) -> Option<u32> {
        let g = self.0.lock().unwrap();
        g.nodes.get(&node).map(|n| n.file_type)
    }

    /// Drops `nlookup` references, releasing the node (and its descriptor) at
    /// zero. The root is immortal; unknown ids are ignored.
    pub fn forget(&self, node: NodeId, nlookup: u64) {
        if node == ROOT_NODE {
            return;
        }
        let mut g = self.0.lock().unwrap();
        let exhausted = match g.nodes.get_mut(&node) {
            Some(n) => {
                n.nlookup = n.nlookup.saturating_sub(nlookup);
                n.nlookup == 0
            }
            None => false,
        };
        if !exhausted {
            return;
        }
        // Constraint: the final `close(2)` must not run under the table lock.
        // Dropping the last `Arc<OwnedFd>` for an unlinked file frees the inode
        // and its blocks inside `close(2)` (journal work on ext4, tens of
        // milliseconds for a large file), and FORGETs arrive batched, so every
        // other operation on the connection would queue behind the whole batch.
        // Bind the removed node so it outlives the guard, unlock, then drop it.
        let dead = g.nodes.remove(&node);
        if let Some(n) = &dead {
            debug_assert_eq!(g.by_key.get(&n.key), Some(&node));
            g.by_key.remove(&n.key);
        }
        drop(g);
        drop(dead);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::OwnedFd;

    fn open_path(p: &std::path::Path) -> OwnedFd {
        rustix::fs::open(p, rustix::fs::OFlags::PATH, rustix::fs::Mode::empty()).unwrap()
    }

    fn key_of(fd: &OwnedFd) -> FileKey {
        let st = rustix::fs::fstat(fd).unwrap();
        (st.st_dev as u64, st.st_ino as u64)
    }

    fn table_over_tempdir() -> (tempfile::TempDir, NodeTable) {
        let dir = tempfile::tempdir().unwrap();
        let root = open_path(dir.path());
        let key = key_of(&root);
        (dir, NodeTable::new(root, key, libc::S_IFDIR))
    }

    /// `xattr_fd` needs the file type and nothing else about the mode, and a
    /// live inode never changes type. Storing it at registration means the
    /// xattr path stops paying for an `fstat` it can answer from memory.
    #[test]
    fn a_node_remembers_its_file_type() {
        let (dir, table) = table_over_tempdir();
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();

        let ffd = open_path(&dir.path().join("f"));
        let (fid, _, _) = table.register(
            ffd,
            key_of(&open_path(&dir.path().join("f"))),
            libc::S_IFREG,
        );
        let dfd = open_path(&dir.path().join("d"));
        let (did, _, _) = table.register(
            dfd,
            key_of(&open_path(&dir.path().join("d"))),
            libc::S_IFDIR,
        );

        assert_eq!(table.file_type(fid), Some(libc::S_IFREG));
        assert_eq!(table.file_type(did), Some(libc::S_IFDIR));
        assert_eq!(table.file_type(ROOT_NODE), Some(libc::S_IFDIR));
        assert_eq!(table.file_type(9999), None);
    }

    #[test]
    fn register_get_forget_lifecycle() {
        let (dir, table) = table_over_tempdir();
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        let fd = open_path(&dir.path().join("f"));
        let key = key_of(&fd);

        let (id, generation, _) = table.register(fd, key, libc::S_IFREG);
        assert!(id > 1);
        assert!(table.get(id).is_some());

        table.forget(id, 1);
        assert!(table.get(id).is_none()); // => ESTALE upstream
        let _ = generation;
    }

    #[test]
    fn hardlinks_dedup_to_one_node_with_bumped_refcount() {
        let (dir, table) = table_over_tempdir();
        std::fs::write(dir.path().join("a"), b"x").unwrap();
        std::fs::hard_link(dir.path().join("a"), dir.path().join("b")).unwrap();

        let fd_a = open_path(&dir.path().join("a"));
        let key_a = key_of(&fd_a);
        let fd_b = open_path(&dir.path().join("b"));
        let key_b = key_of(&fd_b);
        assert_eq!(key_a, key_b);

        let (id_a, _, _) = table.register(fd_a, key_a, libc::S_IFREG);
        let (id_b, _, _) = table.register(fd_b, key_b, libc::S_IFREG);
        assert_eq!(id_a, id_b);

        table.forget(id_a, 1);
        assert!(table.get(id_a).is_some()); // second ref still holds it
        table.forget(id_a, 1);
        assert!(table.get(id_a).is_none());
    }

    #[test]
    fn generations_differ_when_id_slot_recycles_a_key() {
        let (dir, table) = table_over_tempdir();
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        let fd1 = open_path(&dir.path().join("f"));
        let key1 = key_of(&fd1);
        let (_, gen1, _) = table.register(fd1, key1, libc::S_IFREG);
        table.forget(ROOT_NODE + 1, 1);

        std::fs::remove_file(dir.path().join("f")).unwrap();
        std::fs::write(dir.path().join("f"), b"y").unwrap();
        let fd2 = open_path(&dir.path().join("f"));
        let key2 = key_of(&fd2);
        let (_, gen2, _) = table.register(fd2, key2, libc::S_IFREG);
        assert_ne!(gen1, gen2);
    }

    /// Deterministic counterpart to the test above, which cannot fail on tmpfs:
    /// tmpfs draws inode numbers from a monotonic counter, so a real
    /// create/delete/create cycle never hands the same `FileKey` back and the
    /// recycle path goes unexercised. `register` takes the key from its caller,
    /// so fabricate one and present it twice.
    #[test]
    fn a_recycled_key_after_full_forget_gets_a_fresh_id_and_generation() {
        let (dir, table) = table_over_tempdir();
        let recycled: FileKey = (0, 12345);

        std::fs::write(dir.path().join("first"), b"x").unwrap();
        let fd1 = open_path(&dir.path().join("first"));
        let (id1, gen1, _) = table.register(fd1, recycled, libc::S_IFREG);
        table.forget(id1, 1);
        assert!(table.get(id1).is_none());

        std::fs::write(dir.path().join("second"), b"y").unwrap();
        let fd2 = open_path(&dir.path().join("second"));
        let (id2, gen2, _) = table.register(fd2, recycled, libc::S_IFREG);

        assert_ne!(id1, id2, "a forgotten id must never be re-issued");
        assert_ne!(gen1, gen2, "a recycled key must get a fresh generation");
    }

    #[test]
    fn root_survives_forget() {
        let (_dir, table) = table_over_tempdir();
        table.forget(ROOT_NODE, u64::MAX);
        assert!(table.get(ROOT_NODE).is_some());
    }
}
