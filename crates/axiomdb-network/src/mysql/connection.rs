//! Per-connection SQL execution state helpers.
//!
//! Phase 40.10 splits the shared engine state from the per-connection SQL
//! state. The MySQL wire layer still owns protocol/session variables in
//! [`crate::mysql::session::ConnectionState`], while the executor consumes
//! `SessionContext` + `SchemaCache`. This module keeps the synchronization
//! between both views in one place so the handler does not duplicate it.
//!
//! ## `ConnectionEngineCtx`
//!
//! Bundles the three per-connection executor-side objects — `SessionContext`,
//! `SchemaCache`, and `PlanCache` — into a single struct so the handler does
//! not juggle three separate locals.

use axiomdb_sql::{SchemaCache, SessionContext};

use super::plan_cache::PlanCache;
use super::session::ConnectionState;

// ── ConnectionEngineCtx ──────────────────────────────────────────────────────

/// Per-connection engine execution context.
///
/// Owns the SQL-layer state that lives for the duration of a connection:
/// - `session` — autocommit, current database, transaction state, warnings
/// - `schema_cache` — avoids repeated catalog heap scans for the same table
/// - `plan_cache` — normalised COM_QUERY plan reuse (OID-based invalidation)
///
/// Created once after authentication and passed through the command loop.
/// Reset on `COM_RESET_CONNECTION` / `COM_CHANGE_USER`.
pub(crate) struct ConnectionEngineCtx {
    pub session: SessionContext,
    pub schema_cache: SchemaCache,
    pub plan_cache: PlanCache,
}

impl ConnectionEngineCtx {
    /// Default plan cache capacity per connection.
    const DEFAULT_PLAN_CACHE_CAP: usize = 256;

    /// Creates a fresh engine context for a new connection.
    pub fn new() -> Self {
        Self {
            session: SessionContext::new(),
            schema_cache: SchemaCache::new(),
            plan_cache: PlanCache::new(Self::DEFAULT_PLAN_CACHE_CAP),
        }
    }

    /// Resets to a clean state (COM_RESET_CONNECTION / COM_CHANGE_USER).
    pub fn reset(&mut self) {
        self.session = SessionContext::new();
        self.schema_cache = SchemaCache::new();
        self.plan_cache = PlanCache::new(Self::DEFAULT_PLAN_CACHE_CAP);
    }

    /// Synchronizes protocol/session variables from the wire connection state
    /// into the executor-facing SQL session state.
    pub fn sync_from_wire(&mut self, conn_state: &ConnectionState) {
        self.session.autocommit = conn_state.autocommit;
        self.session.strict_mode = axiomdb_sql::session::sql_mode_is_strict(
            conn_state
                .get_variable("sql_mode")
                .as_deref()
                .unwrap_or("STRICT_TRANS_TABLES"),
        );
        self.session.ansi_quotes = conn_state.ansi_quotes();
        self.session.on_error = conn_state.on_error();
        self.session.compat_mode = conn_state.compat_mode();
        self.session.explicit_collation = conn_state.explicit_collation();
    }

    /// Updates the selected database in both protocol and executor session state.
    pub fn set_current_database(&mut self, conn_state: &mut ConnectionState, db_name: String) {
        conn_state.current_database = db_name.clone();
        self.session.set_current_database(db_name);
    }

    /// Copies the selected database from the executor session back into the
    /// protocol-visible connection state after statement execution.
    pub fn sync_database_to_wire(&self, conn_state: &mut ConnectionState) {
        conn_state.current_database = self.session.selected_database().unwrap_or("").to_string();
    }
}
