//! Raising the mount's readahead window, best effort.
//!
//! `docs/benchmarks/2026-08-28-readahead.md` measured why this module exists:
//! the kernel clamps the readahead the client asks for in `INIT` to the
//! backing device's `read_ahead_kb`, whose default of 128 costs about half the
//! throughput of a buffered sequential read. The knob that fixes it is a sysfs
//! file, `/sys/class/bdi/<major:minor>/read_ahead_kb`, and the mount's own
//! `major:minor` sits in `/proc/self/mountinfo` field 3 — so the client can
//! find and write it with no `stat()` on the mountpoint, and so no FUSE round
//! trip into itself. That claim holds because the caller resolves the
//! mountpoint *before* the mount exists and hands [`apply`] the resolved path;
//! nothing in this module touches the mounted tree.
//!
//! Everything here is advisory. The knob is `root:root` mode 644, so an
//! unprivileged client gets `EACCES`; that earns one WARN naming the exact
//! command an operator needs, and the mount carries on. Throughput is not
//! correctness, and this must never turn a working mount into a failed one.
//! The kernel recreates the bdi on every mount, so the setting does not
//! survive a remount either way.

use std::path::{Path, PathBuf};

/// The readahead to ask for, or `None` for "do not try".
///
/// The default ties to the negotiated `max_io_size` rather than to a literal,
/// because that is where the measured curve flattens: the win arrives when the
/// readahead window reaches the FUSE request ceiling, and it stops there
/// (`docs/benchmarks/2026-08-28-readahead.md`). The derivation never goes
/// below the kernel's own default of 128, though — a tiny negotiated I/O size
/// must not make a privileged client *lower* the knob and log success. An
/// operator's explicit value passes through untouched, and an explicit `0`
/// turns the attempt off entirely — there is nothing useful to write, and a
/// deployment that manages the knob itself should not see a WARN about it.
pub fn effective_readahead_kb(flag: Option<u32>, max_io_size: u32) -> Option<u32> {
    match flag {
        Some(0) => None,
        Some(kb) => Some(kb),
        None => Some((max_io_size / 1024).max(128)),
    }
}

/// The mount's bdi name — `major:minor`, e.g. `0:46` — from the text of
/// `/proc/self/mountinfo`.
///
/// Field 3 of the matching line is the name of the `/sys/class/bdi/` directory
/// holding `read_ahead_kb`; field 5 is the mount point, with spaces, tabs,
/// newlines and backslashes octal-escaped (`\040` and friends — see
/// `proc_pid_mountinfo(5)`). Lines that do not parse are skipped rather than
/// fatal: this whole path is best effort, and one unreadable line must not
/// cost the readahead a different line would have provided.
pub fn bdi_for_mount(mountinfo: &str, mountpoint: &Path) -> Option<String> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ');
            let dev = fields.nth(2)?;
            let point = fields.nth(1)?;
            if !is_dev_name(dev) || unescape(point) != mountpoint {
                return None;
            }
            Some(dev.to_string())
        })
        // From the back: mountinfo lists mounts oldest first, so when two
        // entries share a mount point the later one is on top, and the top one
        // is the mount whose readahead the kernel consults.
        .next_back()
}

/// Whether a field looks like the `major:minor` mountinfo promises — because a
/// value that does not is a line this parser misread, and writing into
/// `/sys/class/bdi/<garbage>/` on the strength of it helps nobody.
fn is_dev_name(field: &str) -> bool {
    field.split_once(':').is_some_and(|(major, minor)| {
        !major.is_empty()
            && !minor.is_empty()
            && major.bytes().all(|b| b.is_ascii_digit())
            && minor.bytes().all(|b| b.is_ascii_digit())
    })
}

/// Write `kb` to the mount's `read_ahead_kb`, and say what happened.
///
/// `mountpoint` must be the resolved path the mount sits on — the caller
/// canonicalizes it before the mount exists, because resolving a path once a
/// FUSE root lives there would `lstat` it, a `GETATTR` round trip back into
/// this very client. mountinfo prints mount points post-resolution, so the
/// resolved path is also the one field 5 matches.
///
/// One INFO on success; on any failure exactly one WARN carrying the command
/// an operator needs, because `EACCES` is the expected case — the knob is
/// `root:root` mode 644 and the client ordinarily runs unprivileged. Never an
/// error: a mount that works at half throughput beats no mount.
pub fn apply(mountpoint: &Path, kb: u32) {
    let mountinfo = match std::fs::read_to_string("/proc/self/mountinfo") {
        Ok(text) => text,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "cannot read /proc/self/mountinfo, so the mount's read_ahead_kb \
                 stays at the kernel's default; see \
                 docs/benchmarks/2026-08-28-readahead.md for what that costs"
            );
            return;
        }
    };
    let Some(bdi) = bdi_for_mount(&mountinfo, mountpoint) else {
        tracing::warn!(
            mountpoint = %mountpoint.display(),
            "no mountinfo entry for the mountpoint, so the mount's \
             read_ahead_kb stays at the kernel's default; see \
             docs/benchmarks/2026-08-28-readahead.md for what that costs"
        );
        return;
    };
    let knob = format!("/sys/class/bdi/{bdi}/read_ahead_kb");
    match std::fs::write(&knob, kb.to_string()) {
        Ok(()) => tracing::info!(kb, knob = %knob, "set the mount's readahead"),
        Err(e) => tracing::warn!(
            error = %e,
            "cannot set the mount's readahead, which costs about half of \
             buffered sequential read throughput \
             (docs/benchmarks/2026-08-28-readahead.md); an operator can run: \
             echo {kb} | sudo tee {knob}"
        ),
    }
}

/// A mountinfo field, unescaped. Any three-octal-digit escape decodes — the
/// kernel emits `\040` space, `\011` tab, `\012` newline and `\134` backslash
/// — and a backslash not followed by three octal digits stays literal.
fn unescape(field: &str) -> PathBuf {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let raw = field.as_bytes();
    let mut bytes = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'\\' && i + 3 < raw.len() {
            let octal = &raw[i + 1..i + 4];
            if octal.iter().all(|b| (b'0'..=b'7').contains(b)) {
                bytes.push(octal.iter().fold(0u8, |n, b| (n << 3) | (b - b'0')));
                i += 4;
                continue;
            }
        }
        bytes.push(raw[i]);
        i += 1;
    }
    PathBuf::from(OsString::from_vec(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default is the negotiated I/O size in KiB — where the measured
    /// curve flattens — an explicit value is itself, and zero means "do not
    /// try" rather than "write a zero".
    #[test]
    fn the_default_derives_from_the_negotiated_io_size_and_zero_disables() {
        assert_eq!(effective_readahead_kb(None, 1 << 20), Some(1024));
        assert_eq!(effective_readahead_kb(None, 128 * 1024), Some(128));
        assert_eq!(effective_readahead_kb(Some(4096), 1 << 20), Some(4096));
        assert_eq!(effective_readahead_kb(Some(0), 1 << 20), None);
    }

    /// A small negotiated I/O size must not derive a default below the
    /// kernel's own 128 — a privileged client would lower the knob and log
    /// success. The clamp binds the derivation only: an operator's explicit
    /// value passes through untouched, however small.
    #[test]
    fn the_derived_default_never_goes_below_the_kernel_default() {
        assert_eq!(effective_readahead_kb(None, 4096), Some(128));
        assert_eq!(effective_readahead_kb(None, 64 * 1024), Some(128));
        assert_eq!(effective_readahead_kb(None, 128 * 1024), Some(128));
        assert_eq!(effective_readahead_kb(None, 256 * 1024), Some(256));
        assert_eq!(effective_readahead_kb(Some(4), 1 << 20), Some(4));
    }

    /// Real lines, lightly anonymised: an lbfs mount, the root filesystem, and
    /// a tmpfs. Only the entry whose mount point matches answers.
    const MOUNTINFO: &str = "\
29 1 253:0 / / rw,relatime shared:1 - ext4 /dev/mapper/root rw\n\
44 29 0:36 / /tmp rw,nosuid,nodev shared:19 - tmpfs tmpfs rw,size=8192k\n\
121 29 0:46 / /mnt/lbfs rw,nosuid,nodev,relatime shared:65 - fuse lbfs rw,user_id=1000,group_id=1000\n";

    #[test]
    fn the_matching_entry_names_the_bdi() {
        assert_eq!(
            bdi_for_mount(MOUNTINFO, Path::new("/mnt/lbfs")),
            Some("0:46".to_string())
        );
        assert_eq!(
            bdi_for_mount(MOUNTINFO, Path::new("/")),
            Some("253:0".to_string())
        );
    }

    /// Two entries at one mount point mean an overmount, and the later line is
    /// the visible one — the mount whose bdi the kernel consults.
    #[test]
    fn an_overmounted_path_answers_the_topmost_entry() {
        let stacked = "\
121 29 0:46 / /mnt/lbfs rw - fuse lbfs rw\n\
130 29 0:52 / /mnt/lbfs rw - fuse lbfs rw\n";
        assert_eq!(
            bdi_for_mount(stacked, Path::new("/mnt/lbfs")),
            Some("0:52".to_string())
        );
    }

    #[test]
    fn a_mountpoint_nothing_matches_answers_none() {
        assert_eq!(bdi_for_mount(MOUNTINFO, Path::new("/mnt/other")), None);
        assert_eq!(bdi_for_mount("", Path::new("/mnt/lbfs")), None);
    }

    /// The kernel octal-escapes spaces, tabs, newlines and backslashes in the
    /// mount point field, so a path with a space in it still occupies one
    /// field and still has to match the unescaped path the operator mounted.
    #[test]
    fn an_escaped_mount_point_matches_its_unescaped_path() {
        let line = "121 29 0:47 / /mnt/with\\040space\\134here rw - fuse lbfs rw\n";
        assert_eq!(
            bdi_for_mount(line, Path::new("/mnt/with space\\here")),
            Some("0:47".to_string())
        );
    }

    /// Malformed lines are skipped, never matched and never fatal: too few
    /// fields, or a third field that is not `major:minor`.
    #[test]
    fn malformed_lines_are_skipped() {
        let short = "121 29 0:46\n";
        assert_eq!(bdi_for_mount(short, Path::new("/mnt/lbfs")), None);

        let not_a_dev = "121 29 fuse / /mnt/lbfs rw - fuse lbfs rw\n";
        assert_eq!(bdi_for_mount(not_a_dev, Path::new("/mnt/lbfs")), None);

        // A good line after a bad one still answers.
        let mixed = format!("{not_a_dev}122 29 0:48 / /mnt/lbfs rw - fuse lbfs rw\n");
        assert_eq!(
            bdi_for_mount(&mixed, Path::new("/mnt/lbfs")),
            Some("0:48".to_string())
        );
    }

    #[test]
    fn unescape_handles_the_four_kernel_escapes_and_leaves_the_rest() {
        assert_eq!(unescape("/plain/path"), PathBuf::from("/plain/path"));
        assert_eq!(unescape("a\\040b"), PathBuf::from("a b"));
        assert_eq!(unescape("a\\011b"), PathBuf::from("a\tb"));
        assert_eq!(unescape("a\\012b"), PathBuf::from("a\nb"));
        assert_eq!(unescape("a\\134b"), PathBuf::from("a\\b"));
        // A trailing or non-octal backslash is literal, not an error.
        assert_eq!(unescape("a\\"), PathBuf::from("a\\"));
        assert_eq!(unescape("a\\04"), PathBuf::from("a\\04"));
        assert_eq!(unescape("a\\09xb"), PathBuf::from("a\\09xb"));
    }
}
