use lbfs_proto::frame::{DEFAULT_MAX_INFLIGHT, DEFAULT_MAX_IO_SIZE, WINDOW_CLAMP};
use serde::Deserialize;
use std::path::{Path, PathBuf};

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

#[derive(Debug, Deserialize)]
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

#[derive(Debug)]
pub enum AttachError {
    NotExported,
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

    /// Canonicalize FIRST, then match — symlinks cannot smuggle a path
    /// past the allowlist (spec §4).
    pub fn check(&self, requested: &Path) -> Result<PathBuf, AttachError> {
        let canon = std::fs::canonicalize(requested).map_err(|_| AttachError::NotExported)?;
        if self.set.is_match(&canon) {
            Ok(canon)
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

    #[test]
    fn allowlist_matches_canonical_path_not_requested_path() {
        let tmp = tempfile::tempdir().unwrap();
        let exports = tmp.path().join("exports");
        let outside = tmp.path().join("secret");
        std::fs::create_dir_all(exports.join("data")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        // Symlink inside the exported tree pointing outside it.
        std::os::unix::fs::symlink(&outside, exports.join("leak")).unwrap();

        let pattern = format!("{}/exports/*", tmp.path().canonicalize().unwrap().display());
        let allow = Allowlist::new(&[pattern]).unwrap();

        assert!(allow.check(&exports.join("data")).is_ok());
        // Canonicalizes to .../secret which no glob matches.
        assert!(matches!(
            allow.check(&exports.join("leak")),
            Err(AttachError::Denied)
        ));
        assert!(matches!(
            allow.check(&exports.join("missing")),
            Err(AttachError::NotExported)
        ));
    }
}
