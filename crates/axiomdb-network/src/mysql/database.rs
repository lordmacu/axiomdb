//! Backward-compatible `Database` facade for the shared engine handle.
//!
//! Phase 40.10 moves the real implementation to [`SharedDatabase`] in
//! `shared_db.rs`. This module remains as a thin compatibility surface so
//! existing imports (`mysql::Database`) keep resolving while the rest of the
//! codebase migrates.

pub use super::shared_db::{
    is_read_only_sql, CommitRx, SharedDatabase, RUNTIME_MODE_DEGRADED, RUNTIME_MODE_READ_WRITE,
};

pub type Database = SharedDatabase;
