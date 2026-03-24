//! Engine configuration loaded from a TOML file (`axiomdb.toml`).
//!
//! [`DbConfig::load`] reads the file at the given path. If the path is `None`
//! or the file does not exist, compiled-in defaults are returned. Partial TOML
//! files are accepted — missing fields fall back to defaults via `#[serde(default)]`.
//!
//! ## Example `axiomdb.toml`
//!
//! ```toml
//! max_wal_size_mb = 512
//! fsync           = true
//! log_level       = "debug"
//! data_dir        = "/var/lib/axiomdb"
//! ```

use std::path::{Path, PathBuf};

use axiomdb_core::error::DbError;
use serde::Deserialize;

// ── DbConfig ──────────────────────────────────────────────────────────────────

/// Engine-wide configuration.
///
/// All fields are optional in the TOML file; missing fields use the values from
/// [`DbConfig::default()`].
#[derive(Debug, Clone, Deserialize)]
pub struct DbConfig {
    /// Directory where `.db` and `.wal` files are stored.
    /// `None` means the caller supplies the path at open time.
    #[serde(default)]
    pub data_dir: Option<PathBuf>,

    /// Maximum WAL file size in megabytes before a rotation is triggered.
    /// Default: 256 MB.
    #[serde(default = "default_max_wal_size_mb")]
    pub max_wal_size_mb: u64,

    /// Whether to `fsync` on WAL commit. Set `false` only in tests or
    /// when durability is not required (e.g., in-memory workloads).
    /// Default: `true`.
    #[serde(default = "default_fsync")]
    pub fsync: bool,

    /// Minimum log level passed to `tracing_subscriber`.
    /// Accepted values: `"error"`, `"warn"`, `"info"`, `"debug"`, `"trace"`.
    /// Default: `"info"`.
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// WAL Group Commit interval in milliseconds.
    ///
    /// When > 0, DML commits are batched: instead of one `fsync` per transaction,
    /// up to `group_commit_max_batch` concurrent transactions share a single `fsync`,
    /// improving throughput under concurrent write load.
    ///
    /// `0` (default) disables group commit — every DML commit fsyncs immediately,
    /// identical to pre-3.19 behavior. Recommended value for production: `1`.
    #[serde(default)]
    pub group_commit_interval_ms: u64,

    /// Maximum number of transactions in a single group commit batch.
    ///
    /// When `group_commit_interval_ms > 0` and this many transactions are waiting
    /// for fsync confirmation, a flush+fsync is triggered immediately without
    /// waiting for the timer. Default: `64`.
    #[serde(default = "default_group_commit_max_batch")]
    pub group_commit_max_batch: usize,
}

fn default_max_wal_size_mb() -> u64 {
    256
}

fn default_fsync() -> bool {
    true
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_group_commit_max_batch() -> usize {
    64
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            data_dir: None,
            max_wal_size_mb: default_max_wal_size_mb(),
            fsync: default_fsync(),
            log_level: default_log_level(),
            group_commit_interval_ms: 0,
            group_commit_max_batch: default_group_commit_max_batch(),
        }
    }
}

impl DbConfig {
    /// Loads configuration from `path`.
    ///
    /// - `None` → returns [`DbConfig::default()`] immediately.
    /// - Path does not exist → returns [`DbConfig::default()`] without error.
    /// - File exists but is not valid TOML → returns `Err(DbError::ParseError)`.
    /// - File exists with valid TOML → merges with defaults (missing fields use defaults).
    pub fn load(path: Option<&Path>) -> Result<Self, DbError> {
        let path = match path {
            Some(p) => p,
            None => return Ok(Self::default()),
        };

        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(e) => return Err(DbError::Io(e)),
        };

        toml::from_str(&text).map_err(|e| DbError::ParseError {
            message: format!("invalid axiomdb.toml: {e}"),
        })
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_values() {
        let cfg = DbConfig::default();
        assert_eq!(cfg.max_wal_size_mb, 256);
        assert!(cfg.fsync);
        assert_eq!(cfg.log_level, "info");
        assert!(cfg.data_dir.is_none());
    }

    #[test]
    fn test_load_none_returns_defaults() {
        let cfg = DbConfig::load(None).unwrap();
        assert_eq!(cfg.max_wal_size_mb, 256);
        assert!(cfg.fsync);
    }

    #[test]
    fn test_load_nonexistent_path_returns_defaults() {
        let cfg = DbConfig::load(Some(Path::new("/tmp/no_such_axiomdb_config.toml"))).unwrap();
        assert_eq!(cfg.max_wal_size_mb, 256);
    }

    #[test]
    fn test_load_full_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("axiomdb.toml");
        std::fs::write(
            &path,
            r#"
max_wal_size_mb = 512
fsync           = false
log_level       = "debug"
data_dir        = "/var/lib/axiomdb"
"#,
        )
        .unwrap();

        let cfg = DbConfig::load(Some(&path)).unwrap();
        assert_eq!(cfg.max_wal_size_mb, 512);
        assert!(!cfg.fsync);
        assert_eq!(cfg.log_level, "debug");
        assert_eq!(cfg.data_dir, Some(PathBuf::from("/var/lib/axiomdb")));
    }

    #[test]
    fn test_load_partial_config_uses_defaults_for_missing_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("axiomdb.toml");
        std::fs::write(&path, "fsync = false\n").unwrap();

        let cfg = DbConfig::load(Some(&path)).unwrap();
        assert!(!cfg.fsync);
        // Other fields must be defaults.
        assert_eq!(cfg.max_wal_size_mb, 256);
        assert_eq!(cfg.log_level, "info");
        assert!(cfg.data_dir.is_none());
    }

    #[test]
    fn test_load_invalid_toml_returns_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("axiomdb.toml");
        std::fs::write(&path, "not valid toml [[[").unwrap();

        let err = DbConfig::load(Some(&path)).unwrap_err();
        assert!(
            matches!(err, DbError::ParseError { .. }),
            "expected ParseError, got: {err}"
        );
    }

    #[test]
    fn test_load_empty_file_uses_all_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("axiomdb.toml");
        std::fs::write(&path, "").unwrap();

        let cfg = DbConfig::load(Some(&path)).unwrap();
        assert_eq!(cfg.max_wal_size_mb, 256);
        assert!(cfg.fsync);
        assert_eq!(cfg.log_level, "info");
    }
}
