# Spec: 3.16 — Basic Configuration (dbyo.toml)

## What to build

A `DbConfig` struct that loads engine configuration from a TOML file with safe
defaults when the file is missing or a field is omitted. Used by the server
entry point and the embedded API.

## DbConfig

```rust
// nexusdb-storage/src/config.rs
#[derive(Debug, Clone, serde::Deserialize)]
pub struct DbConfig {
    pub data_dir: Option<PathBuf>,      // default: None (caller chooses)
    pub max_wal_size_mb: u64,           // default: 256  (MB)
    pub fsync: bool,                    // default: true
    pub log_level: String,             // default: "info"
}

impl DbConfig {
    /// Returns compiled-in defaults.
    pub fn default() -> Self

    /// Loads config from `path`. If path is None or the file does not exist,
    /// returns defaults. Field-level defaults apply for any missing key.
    pub fn load(path: Option<&Path>) -> Result<Self, DbError>
}
```

The `page_size` is a compile-time constant (`PAGE_SIZE = 16384`) — not runtime configurable in Phase 3.

## Acceptance criteria

- [ ] `DbConfig::default()` returns valid defaults (fsync=true, max_wal_size_mb=256, log_level="info")
- [ ] `DbConfig::load(None)` returns defaults without error
- [ ] `DbConfig::load(Some(nonexistent_path))` returns defaults without error
- [ ] Valid TOML file is parsed correctly
- [ ] Partial TOML (only some fields) fills missing fields with defaults
- [ ] Invalid TOML returns `Err(DbError::ParseError)`
- [ ] No `unwrap()` in `src/`

## Dependencies

- `toml` + `serde` crates (toml = "1" added to workspace)
