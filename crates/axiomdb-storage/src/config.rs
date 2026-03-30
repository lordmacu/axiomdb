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

// ── WalDurabilityPolicy ─────────────────────────────────────────────────────

/// WAL durability contract for committed DML transactions.
///
/// Orthogonal to `WalSyncMethod` (which syscall to use when syncing).
/// This enum controls **whether** a commit waits for durable sync at all.
///
/// Modelled after PostgreSQL `synchronous_commit` and InnoDB
/// `innodb_flush_log_at_trx_commit`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WalDurabilityPolicy {
    /// Default. No `OK` before durable WAL sync.
    /// Equivalent to `innodb_flush_log_at_trx_commit = 1` / `synchronous_commit = on`.
    #[default]
    Strict,
    /// WAL bytes are flushed to the OS page cache before `OK`, but no durable
    /// sync on every commit. Acknowledged commits may be lost after crash or
    /// power loss.
    /// Equivalent to `innodb_flush_log_at_trx_commit = 2` / `synchronous_commit = off`.
    Normal,
    /// No per-commit durability barrier. Benchmark/dev only.
    /// Equivalent to `innodb_flush_log_at_trx_commit = 0`.
    Off,
}

impl<'de> Deserialize<'de> for WalDurabilityPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.to_lowercase().as_str() {
            "strict" => Ok(Self::Strict),
            "normal" => Ok(Self::Normal),
            "off" => Ok(Self::Off),
            other => Err(serde::de::Error::custom(format!(
                "invalid wal_durability value '{other}': expected 'strict', 'normal', or 'off'"
            ))),
        }
    }
}

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

    /// Legacy toggle — superseded by `wal_durability`.
    ///
    /// When `wal_durability` is absent and `fsync = false`, the resolved
    /// durability policy is `Off`. When both are present, `wal_durability`
    /// takes precedence and `fsync` is ignored.
    #[serde(default = "default_fsync")]
    pub fsync: bool,

    /// Explicit WAL durability policy for committed DML.
    ///
    /// - `"strict"` (default) — no OK before durable WAL sync.
    /// - `"normal"` — flush to OS page cache, no durable sync per commit.
    /// - `"off"` — no per-commit barrier; benchmark/dev only.
    ///
    /// When absent, falls back to legacy `fsync` field:
    /// `fsync = true` → `Strict`, `fsync = false` → `Off`.
    #[serde(default)]
    pub wal_durability: Option<WalDurabilityPolicy>,

    /// Minimum log level passed to `tracing_subscriber`.
    /// Accepted values: `"error"`, `"warn"`, `"info"`, `"debug"`, `"trace"`.
    /// Default: `"info"`.
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Maximum number of prepared statements cached per connection.
    ///
    /// When the limit is reached, the least-recently-used prepared statement is
    /// evicted to make room for the new one. The evicted statement's `stmt_id`
    /// returns error 1243 on subsequent `COM_STMT_EXECUTE` calls.
    /// Default: `1024`.
    #[serde(default = "default_max_prepared_stmts")]
    pub max_prepared_stmts_per_connection: usize,
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

fn default_max_prepared_stmts() -> usize {
    1024
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            data_dir: None,
            max_wal_size_mb: default_max_wal_size_mb(),
            fsync: default_fsync(),
            wal_durability: None,
            log_level: default_log_level(),
            max_prepared_stmts_per_connection: default_max_prepared_stmts(),
        }
    }
}

impl DbConfig {
    /// Resolves the effective WAL durability policy.
    ///
    /// Precedence:
    /// 1. Explicit `wal_durability` field if present.
    /// 2. Legacy `fsync` field: `true` → `Strict`, `false` → `Off`.
    pub fn resolved_wal_durability(&self) -> WalDurabilityPolicy {
        match self.wal_durability {
            Some(policy) => policy,
            None => {
                if self.fsync {
                    WalDurabilityPolicy::Strict
                } else {
                    WalDurabilityPolicy::Off
                }
            }
        }
    }

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
            position: None,
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
        assert_eq!(cfg.max_prepared_stmts_per_connection, 1024);
        assert!(cfg.wal_durability.is_none());
        assert_eq!(cfg.resolved_wal_durability(), WalDurabilityPolicy::Strict);
    }

    #[test]
    fn test_load_none_returns_defaults() {
        let cfg = DbConfig::load(None).unwrap();
        assert_eq!(cfg.max_wal_size_mb, 256);
        assert!(cfg.fsync);
        assert_eq!(cfg.max_prepared_stmts_per_connection, 1024);
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

    // ── WalDurabilityPolicy tests ───────────────────────────────────────────

    #[test]
    fn test_wal_durability_strict_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("axiomdb.toml");
        std::fs::write(&path, "wal_durability = \"strict\"\n").unwrap();

        let cfg = DbConfig::load(Some(&path)).unwrap();
        assert_eq!(cfg.resolved_wal_durability(), WalDurabilityPolicy::Strict);
    }

    #[test]
    fn test_wal_durability_normal_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("axiomdb.toml");
        std::fs::write(&path, "wal_durability = \"normal\"\n").unwrap();

        let cfg = DbConfig::load(Some(&path)).unwrap();
        assert_eq!(cfg.resolved_wal_durability(), WalDurabilityPolicy::Normal);
    }

    #[test]
    fn test_wal_durability_off_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("axiomdb.toml");
        std::fs::write(&path, "wal_durability = \"off\"\n").unwrap();

        let cfg = DbConfig::load(Some(&path)).unwrap();
        assert_eq!(cfg.resolved_wal_durability(), WalDurabilityPolicy::Off);
    }

    #[test]
    fn test_wal_durability_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("axiomdb.toml");
        std::fs::write(&path, "wal_durability = \"NORMAL\"\n").unwrap();

        let cfg = DbConfig::load(Some(&path)).unwrap();
        assert_eq!(cfg.resolved_wal_durability(), WalDurabilityPolicy::Normal);
    }

    #[test]
    fn test_wal_durability_invalid_value_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("axiomdb.toml");
        std::fs::write(&path, "wal_durability = \"turbo\"\n").unwrap();

        let err = DbConfig::load(Some(&path)).unwrap_err();
        assert!(
            matches!(err, DbError::ParseError { .. }),
            "expected ParseError, got: {err}"
        );
    }

    #[test]
    fn test_legacy_fsync_false_maps_to_off() {
        let cfg = DbConfig {
            fsync: false,
            wal_durability: None,
            ..DbConfig::default()
        };
        assert_eq!(cfg.resolved_wal_durability(), WalDurabilityPolicy::Off);
    }

    #[test]
    fn test_explicit_durability_overrides_legacy_fsync() {
        let cfg = DbConfig {
            fsync: false,
            wal_durability: Some(WalDurabilityPolicy::Strict),
            ..DbConfig::default()
        };
        assert_eq!(cfg.resolved_wal_durability(), WalDurabilityPolicy::Strict);
    }

    #[test]
    fn test_wal_durability_overrides_fsync_in_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("axiomdb.toml");
        std::fs::write(&path, "fsync = false\nwal_durability = \"strict\"\n").unwrap();

        let cfg = DbConfig::load(Some(&path)).unwrap();
        assert_eq!(cfg.resolved_wal_durability(), WalDurabilityPolicy::Strict);
    }
}
