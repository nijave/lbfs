use lbfs_proto::frame::{DEFAULT_MAX_INFLIGHT, DEFAULT_MAX_IO_SIZE, WINDOW_CLAMP};
use serde::Deserialize;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("toml: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("bad size literal: {0}")]
    BadSize(String),
    #[error("bad glob: {0}")]
    BadGlob(#[from] globset::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsyncPolicy {
    Honor,
    Ignore,
}

/// Every key this file may hold, and nothing else.
///
/// `deny_unknown_fields` because the alternative is silence. `listen` and
/// `allowed_paths` are required, so a typo in either already stops the server.
/// The other three have defaults, and a typo in one of those changes nothing:
/// the server starts and runs on the default the operator was trying to
/// override. `fsync_policy = "ignore"` for `fsync = "ignore"` leaves durability
/// where the operator did not want it, and says so nowhere. A refusal at
/// startup naming the key is the only outcome an operator can act on.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    listen: String,
    allowed_paths: Vec<String>,
    max_inflight: Option<u32>,
    max_io_size: Option<String>,
    fsync: Option<FsyncPolicy>,
}

#[derive(Debug)]
pub struct Config {
    pub listen: String,
    pub allowed_paths: Vec<String>,
    pub max_inflight: u32,
    pub max_io_size: u32,
    pub fsync: FsyncPolicy,
}

impl Config {
    pub fn from_toml(s: &str) -> Result<Config, ConfigError> {
        let raw: RawConfig = toml::from_str(s)?;
        let max_io_size = match raw.max_io_size {
            Some(ref lit) => parse_size(lit)?,
            None => DEFAULT_MAX_IO_SIZE,
        };
        Ok(Config {
            listen: raw.listen,
            allowed_paths: raw.allowed_paths,
            max_inflight: raw
                .max_inflight
                .unwrap_or(DEFAULT_MAX_INFLIGHT)
                .clamp(WINDOW_CLAMP.0, WINDOW_CLAMP.1),
            max_io_size,
            fsync: raw.fsync.unwrap_or(FsyncPolicy::Honor),
        })
    }
}

/// Widen to `u64` before scaling: a `u32` shift discards the high bits it
/// pushes off the top instead of trapping, so `"4096MiB"` would otherwise
/// parse as a silent `0`.
pub fn parse_size(s: &str) -> Result<u32, ConfigError> {
    let bad = || ConfigError::BadSize(s.to_string());
    let (digits, mult) = if let Some(n) = s.strip_suffix("MiB") {
        (n, 1u64 << 20)
    } else if let Some(n) = s.strip_suffix("KiB") {
        (n, 1u64 << 10)
    } else {
        (s, 1u64)
    };
    let v: u64 = digits.trim().parse().map_err(|_| bad())?;
    u32::try_from(v.checked_mul(mult).ok_or_else(bad)?).map_err(|_| bad())
}

#[derive(Debug, thiserror::Error)]
pub enum AttachError {
    #[error("path cannot be opened as a directory")]
    NotExported,
    #[error("resolved path is not on the allowlist")]
    Denied,
}

pub struct Allowlist {
    set: globset::GlobSet,
}

impl Allowlist {
    pub fn new(patterns: &[String]) -> Result<Allowlist, ConfigError> {
        let mut b = globset::GlobSetBuilder::new();
        for p in patterns {
            b.add(
                globset::GlobBuilder::new(p)
                    .literal_separator(true)
                    .build()?,
            );
        }
        Ok(Allowlist { set: b.build()? })
    }

    /// Open the client's requested path and hand back the descriptor, but only
    /// if the allowlist matches the path that descriptor actually names.
    ///
    /// Open first, verify second, and export the very descriptor that was
    /// verified (spec §3.2 step 3, §4). The other order — canonicalize, match,
    /// then open — leaves a window: between the `canonicalize` and the `open`
    /// a component can be swapped for a symlink, and the server then exports a
    /// tree the allowlist never approved. Reading the resolved name back from
    /// `/proc/self/fd/N` closes that window because the kernel is reporting
    /// where the *already pinned* inode lives, not re-walking a path.
    ///
    /// For the same reason the resolved path is matched as-is and never
    /// canonicalized again: a second walk is a second chance to be raced.
    ///
    /// `O_PATH | O_DIRECTORY` pins the inode without read access and without
    /// running any device's `open`; the final component is deliberately
    /// followed, since a client naming a symlinked export directory is asking
    /// for the target and the target is what gets matched.
    pub fn open_export(&self, requested: &Path) -> Result<OwnedFd, AttachError> {
        let fd = rustix::fs::open(
            requested,
            rustix::fs::OFlags::PATH | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| AttachError::NotExported)?;
        // A `/proc` that is missing or unreadable means the resolved path
        // cannot be established, and an unverifiable path is refused rather
        // than trusted. `Server::new` probes for this at startup so it
        // surfaces as a refusal to serve instead of a puzzling attach denial.
        let resolved = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))
            .map_err(|_| AttachError::Denied)?;
        self.check_resolved(&resolved)?;
        Ok(fd)
    }

    /// Match a path that is already resolved, with no filesystem access.
    ///
    /// Split out from [`Allowlist::open_export`] so the matching rule can be
    /// tested on its own, and so a caller that resolved a path some other way
    /// does not have to re-derive it. Callers holding a *client-supplied* path
    /// want `open_export` instead: this function trusts what it is given.
    pub fn check_resolved(&self, resolved: &Path) -> Result<(), AttachError> {
        if self.set.is_match(resolved) {
            Ok(())
        } else {
            Err(AttachError::Denied)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config_and_applies_defaults() {
        let cfg = Config::from_toml(
            r#"
            listen = "127.0.0.1:9423"
            allowed_paths = ["/srv/exports/*"]
        "#,
        )
        .unwrap();
        assert_eq!(cfg.max_inflight, 128);
        assert_eq!(cfg.max_io_size, 1 << 20);
        assert!(matches!(cfg.fsync, FsyncPolicy::Honor));
    }

    #[test]
    fn parses_sizes_and_fsync_ignore() {
        assert_eq!(parse_size("1MiB").unwrap(), 1 << 20);
        assert_eq!(parse_size("64KiB").unwrap(), 64 << 10);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert!(parse_size("1GiBoop").is_err());
        // Overflowing u32 must error, never wrap to a plausible-looking size.
        assert!(parse_size("4096MiB").is_err());
        assert!(parse_size("4097MiB").is_err());
        assert!(parse_size("5120MiB").is_err());
        assert!(parse_size("4194304KiB").is_err());
        assert!(parse_size("4294967296").is_err());
        // The largest in-range literal still parses.
        assert_eq!(parse_size("4095MiB").unwrap(), 4095 << 20);
        let cfg = Config::from_toml(
            r#"
            listen = "0.0.0.0:9423"
            allowed_paths = ["/a/*"]
            fsync = "ignore"
            max_inflight = 5000
        "#,
        )
        .unwrap();
        assert!(matches!(cfg.fsync, FsyncPolicy::Ignore));
        assert_eq!(cfg.max_inflight, 1024); // clamped
    }

    /// A misspelt key is refused, and the message names it.
    ///
    /// `fsync_policy` is the one that matters most: it looks right, it parses
    /// as TOML, and accepting it would run the export at `Honor` while the
    /// operator believed they had asked for `Ignore` — or the reverse, which
    /// is a durability decision made by a typo.
    #[test]
    fn an_unknown_key_is_refused_and_named() {
        let err = Config::from_toml(
            r#"
            listen = "0.0.0.0:9423"
            allowed_paths = ["/a/*"]
            fsync_policy = "ignore"
        "#,
        )
        .expect_err("a key the server does not know is a refusal, not a default");
        let msg = err.to_string();
        assert!(msg.contains("fsync_policy"), "{msg}");
        // The real key still parses, so the rule rejects the typo and not the
        // option it was aiming at.
        assert!(Config::from_toml(
            r#"
            listen = "0.0.0.0:9423"
            allowed_paths = ["/a/*"]
            fsync = "ignore"
        "#,
        )
        .is_ok());
    }

    #[test]
    fn allowlist_matches_resolved_descriptor_not_requested_path() {
        let tmp = tempfile::tempdir().unwrap();
        let exports = tmp.path().join("exports");
        let outside = tmp.path().join("secret");
        std::fs::create_dir_all(exports.join("data")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        // Symlink inside the exported tree pointing outside it.
        std::os::unix::fs::symlink(&outside, exports.join("leak")).unwrap();

        let pattern = format!("{}/exports/*", tmp.path().canonicalize().unwrap().display());
        let allow = Allowlist::new(&[pattern]).unwrap();

        assert!(allow.open_export(&exports.join("data")).is_ok());
        // The descriptor names .../secret, which no glob matches - the
        // requested path being inside the exported tree buys nothing.
        assert!(matches!(
            allow.open_export(&exports.join("leak")),
            Err(AttachError::Denied)
        ));
        assert!(matches!(
            allow.open_export(&exports.join("missing")),
            Err(AttachError::NotExported)
        ));
        // A non-directory is refused by the open, not by the match.
        std::fs::write(exports.join("file"), b"x").unwrap();
        assert!(matches!(
            allow.open_export(&exports.join("file")),
            Err(AttachError::NotExported)
        ));
    }

    #[test]
    fn resolved_check_does_not_touch_the_filesystem() {
        let allow = Allowlist::new(&["/srv/exports/*".to_string()]).unwrap();
        assert!(allow.check_resolved(Path::new("/srv/exports/a")).is_ok());
        // `literal_separator` keeps a single `*` from spanning components.
        assert!(matches!(
            allow.check_resolved(Path::new("/srv/exports/a/b")),
            Err(AttachError::Denied)
        ));
        assert!(matches!(
            allow.check_resolved(Path::new("/etc")),
            Err(AttachError::Denied)
        ));
    }
}
