//! Who clears set-user-ID and set-group-ID, and what "clear" means exactly.
//!
//! Once the client negotiates `FUSE_HANDLE_KILLPRIV_V2`, its kernel stops
//! probing `security.capability` before every write — one round trip per write
//! saved — and stops performing the strip through its own `SETATTR`. The
//! promise moves here.
//!
//! Most of the time this module does nothing per operation, and that is the
//! design rather than an oversight. `vm/lbfs-server.service` runs the daemon as
//! an ordinary user, and an unprivileged process cannot skip the kernel's own
//! strip: `setattr_should_drop_suidgid` returns a kill mask exactly when
//! `capable(CAP_FSETID)` is false (`fs/attr.c:75`), and the server's `write(2)`
//! and `ftruncate(2)` both run through it. Chown needs nothing from anybody —
//! `chown_common` sets `ATTR_KILL_SUID | ATTR_KILL_PRIV` for every
//! non-directory with no capability check at all (`fs/open.c:769-771`).
//!
//! A server holding `CAP_FSETID` is the case that needs code, because there the
//! kernel steps aside and the promise would go unkept.
//!
//! # Why not virtiofsd's toggle
//!
//! virtiofsd drops `CAP_FSETID` from its effective set around the write and
//! puts it back after. That works because virtiofsd calls `pwrite` on the
//! thread that dropped it. lbfs submits an SQE instead, and the kernel runs it
//! either inline or on an `io-wq` worker — a separate task whose credentials
//! were copied when the worker started, not when the SQE landed. A per-request
//! toggle would therefore apply to some writes and skip others depending on
//! whether that write happened to block, which is a worse failure than not
//! trying.

use rustix::thread::{capabilities, CapabilitySet};

/// Which side of the syscall boundary clears the privileged mode bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillPrivPolicy {
    /// The server holds no `CAP_FSETID`, so the backing kernel clears the bits
    /// inside the server's own syscalls. Nothing to do per operation, and the
    /// write path stays at one syscall.
    Kernel,
    /// The server holds `CAP_FSETID`, so the backing kernel skips the strip and
    /// the server performs it.
    Explicit,
}

impl KillPrivPolicy {
    /// Reads the effective capability set once, at startup.
    ///
    /// A capability set can only shrink without `CAP_SETPCAP`, and lbfs never
    /// raises one, so reading it once is honest for the life of the process.
    /// A failing `capget` picks the strict branch: doing redundant work is a
    /// cost, and skipping a promised strip is a hole.
    pub fn detect() -> KillPrivPolicy {
        match capabilities(None) {
            Ok(sets) if !sets.effective.contains(CapabilitySet::FSETID) => KillPrivPolicy::Kernel,
            _ => KillPrivPolicy::Explicit,
        }
    }
}

/// The mode a strip would leave behind, or `None` when nothing needs clearing.
///
/// Mirrors `setattr_should_drop_suidgid` (`fs/attr.c:63-79`) with one
/// deliberate narrowing. The kernel's `setattr_should_drop_sgid`
/// (`fs/attr.c:33-45`) also clears set-group-ID from a file whose group the
/// *caller* does not belong to, even with no group-execute bit. v1 carries no
/// caller credentials on the wire, so the server cannot evaluate that branch
/// and follows the rule the FUSE uapi actually states
/// (`include/uapi/linux/fuse.h:429-433`): group execute, or nothing.
///
/// `None` for anything that is not a regular file, matching the `S_ISREG`
/// guard at `fs/attr.c:75`. Set-group-ID on a directory means inheritance, not
/// privilege.
pub fn stripped_mode(mode: u32) -> Option<u32> {
    if mode & libc::S_IFMT != libc::S_IFREG {
        return None;
    }
    let mut out = mode;
    if mode & libc::S_ISUID != 0 {
        out &= !libc::S_ISUID;
    }
    if mode & libc::S_ISGID != 0 && mode & libc::S_IXGRP != 0 {
        out &= !libc::S_ISGID;
    }
    (out != mode).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_modes_need_no_strip() {
        assert_eq!(stripped_mode(libc::S_IFREG | 0o644), None);
        assert_eq!(stripped_mode(libc::S_IFREG | 0o777), None);
        // Mode 0o000, spelled without the `| 0o000` clippy reads as a no-op.
        assert_eq!(stripped_mode(libc::S_IFREG), None);
    }

    #[test]
    fn setuid_always_goes() {
        assert_eq!(
            stripped_mode(libc::S_IFREG | 0o4755),
            Some(libc::S_IFREG | 0o0755)
        );
        // No execute bits anywhere, and set-user-ID still goes.
        assert_eq!(
            stripped_mode(libc::S_IFREG | 0o4644),
            Some(libc::S_IFREG | 0o0644)
        );
    }

    /// The uapi rule (`include/uapi/linux/fuse.h:429-433`): set-group-ID dies
    /// only when the file carries group execute. Without it the bit is a
    /// mandatory-locking marker, and clearing it would change unrelated
    /// semantics.
    #[test]
    fn setgid_goes_only_with_group_execute() {
        assert_eq!(
            stripped_mode(libc::S_IFREG | 0o2755),
            Some(libc::S_IFREG | 0o0755)
        );
        assert_eq!(stripped_mode(libc::S_IFREG | 0o2644), None);
    }

    #[test]
    fn both_bits_go_together() {
        assert_eq!(
            stripped_mode(libc::S_IFREG | 0o6755),
            Some(libc::S_IFREG | 0o0755)
        );
        // Set-user-ID goes, set-group-ID stays: no group execute.
        assert_eq!(
            stripped_mode(libc::S_IFREG | 0o6745),
            Some(libc::S_IFREG | 0o2745)
        );
    }

    /// `setattr_should_drop_suidgid` guards its whole result on
    /// `S_ISREG(mode)` (`fs/attr.c:75`). Directories keep set-group-ID because
    /// that bit means inheritance there, not privilege.
    #[test]
    fn only_regular_files_lose_bits() {
        assert_eq!(stripped_mode(libc::S_IFDIR | 0o2775), None);
        assert_eq!(stripped_mode(libc::S_IFDIR | 0o4755), None);
        assert_eq!(stripped_mode(libc::S_IFLNK | 0o6777), None);
    }

    /// The suite runs unprivileged, so the backing kernel is the actor and the
    /// server does nothing per write. A run that reports `Explicit` here means
    /// the tests run as root, and the write-path assertions below would then
    /// measure the other branch.
    #[test]
    fn an_unprivileged_process_leaves_the_work_to_the_kernel() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        assert_eq!(KillPrivPolicy::detect(), KillPrivPolicy::Kernel);
    }
}
