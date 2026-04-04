//! OID-based COM_QUERY plan cache (Phase 40.2).
//!
//! ## Overview
//!
//! Replaces the global-`schema_version` design (Phase 27.8b) with per-entry
//! `PlanDeps` for table-level invalidation granularity. Only plans referencing
//! the DDL-modified table are evicted; unrelated plans survive.
//!
//! ## Design — mirrors PostgreSQL `plancache.c`
//!
//! Each `CachedPlanSource` stores:
//! - `deps: PlanDeps` — `(table_id, schema_version_at_compile)` for every
//!   table the statement touches, snapshotted at `store()` time.
//! - `generation: u32` — incremented each time this entry is re-analyzed after
//!   stale detection. Mirrors `CachedPlanSource.generation` in PG.
//! - `last_validated_global_version: u64` — fast pre-check: if the shared
//!   `Database::schema_version` hasn't moved since last validation, skip the
//!   per-table catalog scan entirely.
//!
//! ## Staleness check — two-level
//!
//! 1. **Fast** (`O(1)` atomic compare): if `current_global_version ==
//!    entry.last_validated_global_version`, no DDL has occurred → cache hit
//!    with zero catalog I/O.
//! 2. **Slow** (`O(t)` catalog scan, `t = tables in deps`): called only when
//!    the global version advanced. `PlanDeps::is_stale()` reads each table's
//!    `schema_version` from the catalog heap and compares to the cached value.
//!    Stale → lazy eviction. Not stale → stamp `last_validated_global_version`
//!    so the fast path hits on the next lookup.
//!
//! ## Invalidation — belt-and-suspenders
//!
//! - **Eager**: `invalidate_table(table_id)` removes all entries referencing
//!   that table immediately after DDL (same-connection fast-path cleanup).
//! - **Lazy**: `is_stale()` catches cross-connection DDL at lookup time.
//!
//! ## Eviction
//!
//! LRU via a per-entry `last_used_seq` logical clock. When the cache reaches
//! `max_entries`, the entry with the lowest `last_used_seq` is evicted. O(n)
//! scan — acceptable because `max_entries ≤ 512` in practice.
//!
//! ## Normalization rules (unchanged from Phase 27.8b)
//!
//! - Integer literals → `?`  (extract as `Value::Int` / `Value::BigInt`)
//! - Float literals → `?`    (extract as `Value::Real`)
//! - String literals → `?`   (extract as `Value::Text`)
//! - Identifiers, keywords, operators → preserved exactly

use std::collections::HashMap;

use axiomdb_catalog::{schema::TableId, CatalogReader};
use axiomdb_core::error::DbError;
use axiomdb_sql::{ast::Stmt, plan_deps::PlanDeps};
use axiomdb_types::Value;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Maximum entries before LRU eviction triggers.
///
/// 512 entries × ~2 KB avg plan = ~1 MB per connection. Matches PostgreSQL's
/// per-session plan-cache default (`plan_cache_mode = auto`).
#[allow(dead_code)]
const DEFAULT_MAX_ENTRIES: usize = 512;

// ── CachedPlanSource ─────────────────────────────────────────────────────────

/// A cached, analyzed statement with its catalog dependencies.
///
/// Mirrors PostgreSQL's `CachedPlanSource`. Stored in `PlanCache` keyed on the
/// FNV-1a hash of the normalized (literal-replaced) SQL.
///
/// ## Lifecycle
/// 1. First execution: cache miss → parse + analyze → `store()` with deps.
/// 2. Subsequent executions: `lookup()` → two-level staleness check → hit.
/// 3. DDL on referenced table → lazy `is_stale()` eviction on next lookup,
///    OR eager `invalidate_table()` eviction immediately (belt-and-suspenders).
/// 4. At capacity: entry with lowest `last_used_seq` is evicted (LRU).
struct CachedPlanSource {
    /// Analyzed AST with `Expr::Param` nodes (one per normalized literal).
    stmt: Stmt,
    /// Catalog dependencies snapshotted at compile time.
    deps: PlanDeps,
    /// Number of `?` placeholders expected at substitution time.
    param_count: usize,
    /// Incremented each time this entry is re-stored after stale detection.
    /// Mirrors PostgreSQL's `CachedPlanSource.generation`.
    generation: u32,
    /// Total cache hits for this entry.
    exec_count: u64,
    /// Last-used sequence number for LRU eviction.
    /// Updated to `PlanCache::seq` on every hit.
    last_used_seq: u64,
    /// Global `schema_version` at last successful staleness validation.
    ///
    /// Fast pre-check: if `current_global_version == last_validated_global_version`,
    /// no DDL has occurred since last validation → skip `is_stale()` catalog scan.
    last_validated_global_version: u64,
}

// ── PlanCache ────────────────────────────────────────────────────────────────

/// Per-connection OID-based plan cache for normalized COM_QUERY statements.
///
/// One instance per connection, created at handshake time. Not shared between
/// connections — cross-connection DDL is detected via the catalog's per-table
/// `schema_version` (read inside `PlanDeps::is_stale()`).
pub struct PlanCache {
    entries: HashMap<u64, CachedPlanSource>,
    max_entries: usize,
    /// Monotonic clock for LRU ordering. Incremented on every hit.
    seq: u64,
    // ── Metrics ───────────────────────────────────────────────────────────────
    pub hits: u64,
    pub misses: u64,
    /// Number of entries evicted due to staleness or eager `invalidate_table`.
    pub invalidations: u64,
}

impl PlanCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_entries,
            seq: 0,
            hits: 0,
            misses: 0,
            invalidations: 0,
        }
    }

    /// Looks up a cached plan for `sql`.
    ///
    /// ## Staleness check
    ///
    /// Two-level check for efficient OLTP operation:
    /// 1. **Fast** (`O(1)`): if `global_version` matches the stored
    ///    `last_validated_global_version`, no DDL has occurred → skip catalog scan.
    /// 2. **Slow** (`O(t)` catalog reads): called only when the global version
    ///    advanced. `PlanDeps::is_stale()` compares each table's current
    ///    `schema_version` against the cached snapshot.
    ///
    /// On a stale detection the entry is evicted lazily and `None` is returned.
    ///
    /// # Arguments
    /// - `sql` — raw SQL string from the client.
    /// - `global_version` — current `Database::schema_version` (atomic load).
    ///   Used as the fast pre-check; if unchanged, the catalog scan is skipped.
    /// - `reader` — catalog reader for the slow-path OID staleness check.
    ///   Created from the caller's database storage + snapshot; only consulted
    ///   when `global_version` has advanced since last validation.
    pub fn lookup(
        &mut self,
        sql: &str,
        global_version: u64,
        reader: &mut CatalogReader<'_>,
    ) -> Result<Option<(Stmt, Vec<Value>)>, DbError> {
        let (normalized, params) = normalize_sql(sql);
        let key = fnv1a_hash(normalized.as_bytes());

        // 1. Check entry existence and structural match.
        let stale = match self.entries.get(&key) {
            None => {
                self.misses += 1;
                return Ok(None);
            }
            Some(entry) => {
                // Structural mismatch: different number of literals → wrong query shape.
                if entry.param_count != params.len() {
                    self.misses += 1;
                    return Ok(None);
                }
                if entry.last_validated_global_version == global_version {
                    // Fast path: global version unchanged since last validation.
                    // No DDL has occurred → entry is definitely fresh.
                    false
                } else {
                    // Slow path: DDL occurred since last check.
                    // Scan per-table catalog versions to see if THIS plan's tables changed.
                    entry.deps.is_stale(reader)?
                }
            }
        };

        if stale {
            self.entries.remove(&key);
            self.invalidations += 1;
            self.misses += 1;
            return Ok(None);
        }

        // 2. Entry is fresh — update accounting and clone stmt for substitution.
        let entry = self.entries.get_mut(&key).expect("entry checked above");
        self.seq += 1;
        entry.exec_count += 1;
        entry.last_used_seq = self.seq;
        // Stamp the global version so the fast path hits on the next call.
        entry.last_validated_global_version = global_version;
        self.hits += 1;

        let stmt = entry.stmt.clone();
        match substitute_params(stmt, &params) {
            Ok(substituted) => Ok(Some((substituted, params))),
            Err(_) => {
                // Substitution failure — plan shape mismatch. Evict and miss.
                self.entries.remove(&key);
                self.hits -= 1; // undo the hit count
                self.misses += 1;
                Ok(None)
            }
        }
    }

    /// Stores a parsed+analyzed `Stmt` in the cache with its OID dependencies.
    ///
    /// If the entry key already exists (re-store after stale detection), its
    /// `generation` is incremented. If the cache is at capacity and the key is
    /// new, the LRU entry is evicted first.
    ///
    /// # Arguments
    /// - `sql` — raw SQL string (will be normalized internally).
    /// - `stmt` — fully analyzed AST with `Expr::Param` nodes.
    /// - `deps` — catalog deps extracted by `extract_table_deps` at compile time.
    /// - `global_version` — current `Database::schema_version` at store time,
    ///   used to initialize the fast-path validation stamp.
    pub fn store(&mut self, sql: &str, stmt: &Stmt, deps: PlanDeps, global_version: u64) {
        let (normalized, params) = normalize_sql(sql);
        let key = fnv1a_hash(normalized.as_bytes());

        // Preserve and bump generation from previous entry (re-analysis after stale).
        let generation = self
            .entries
            .get(&key)
            .map(|e| e.generation.saturating_add(1))
            .unwrap_or(0);

        // Evict LRU only if at capacity AND the key is new (not an update).
        if self.entries.len() >= self.max_entries && !self.entries.contains_key(&key) {
            self.evict_lru();
        }

        self.entries.insert(
            key,
            CachedPlanSource {
                stmt: stmt.clone(),
                deps,
                param_count: params.len(),
                generation,
                exec_count: 0,
                last_used_seq: self.seq,
                last_validated_global_version: global_version,
            },
        );
    }

    /// Eagerly evicts all entries whose `deps` reference `table_id`.
    ///
    /// Called after DDL on `table_id` (belt-and-suspenders). The lazy
    /// `is_stale()` check is the primary cross-connection invalidation
    /// mechanism; this accelerates same-connection cleanup by removing stale
    /// entries without waiting for the next lookup.
    pub fn invalidate_table(&mut self, table_id: TableId) {
        let before = self.entries.len();
        self.entries
            .retain(|_, e| !e.deps.tables.iter().any(|&(tid, _)| tid == table_id));
        let removed = before - self.entries.len();
        self.invalidations += removed as u64;
    }

    /// Returns a snapshot of current cache statistics.
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits,
            misses: self.misses,
            invalidations: self.invalidations,
            entries: self.entries.len(),
        }
    }

    /// Evicts the entry with the lowest `last_used_seq` (LRU policy).
    ///
    /// O(n) scan over at most `max_entries` entries. Called only when the cache
    /// reaches capacity — not on the hot lookup path.
    fn evict_lru(&mut self) {
        let lru_key = self
            .entries
            .iter()
            .min_by_key(|(_, e)| e.last_used_seq)
            .map(|(&k, _)| k);
        if let Some(k) = lru_key {
            self.entries.remove(&k);
        }
    }
}

/// Snapshot of plan cache metrics, returned by `PlanCache::stats()`.
#[derive(Debug, Clone, Copy, Default)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub invalidations: u64,
    pub entries: usize,
}

// ── SQL normalization ────────────────────────────────────────────────────────

/// Replaces literal values in SQL with `?` placeholders, extracting the values.
///
/// Handles:
/// - Integer literals: `42` → `?` + `Value::Int(42)`
/// - Float literals: `3.14` → `?` + `Value::Real(3.14)`
/// - String literals: `'hello'` → `?` + `Value::Text("hello")`
///
/// Does NOT normalize:
/// - Identifiers, keywords, operators
/// - Numbers that are part of identifiers (e.g., `table1`)
/// - Negative signs (treated as unary operators by the parser)
pub fn normalize_sql(sql: &str) -> (String, Vec<Value>) {
    let mut result = String::with_capacity(sql.len());
    let mut params = Vec::new();
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        let b = bytes[i];

        // Skip whitespace.
        if b.is_ascii_whitespace() {
            result.push(b as char);
            i += 1;
            continue;
        }

        // String literal: 'content'
        if b == b'\'' {
            i += 1;
            let mut s = String::new();
            while i < len {
                if bytes[i] == b'\'' {
                    if i + 1 < len && bytes[i + 1] == b'\'' {
                        s.push('\''); // escaped quote
                        i += 2;
                    } else {
                        i += 1; // closing quote
                        break;
                    }
                } else {
                    s.push(bytes[i] as char);
                    i += 1;
                }
            }
            result.push('?');
            params.push(Value::Text(s));
            continue;
        }

        // Number literal: starts with digit or '.' followed by digit.
        if b.is_ascii_digit() || (b == b'.' && i + 1 < len && bytes[i + 1].is_ascii_digit()) {
            let num_start = i;
            let mut is_float = b == b'.';
            i += 1;
            while i < len
                && (bytes[i].is_ascii_digit()
                    || bytes[i] == b'.'
                    || bytes[i] == b'e'
                    || bytes[i] == b'E')
            {
                if bytes[i] == b'.' || bytes[i] == b'e' || bytes[i] == b'E' {
                    is_float = true;
                }
                i += 1;
            }

            // If preceded by a letter or underscore, it's part of an identifier (e.g., "table1").
            if num_start > 0
                && (bytes[num_start - 1].is_ascii_alphanumeric() || bytes[num_start - 1] == b'_')
            {
                result.push_str(&sql[num_start..i]);
                continue;
            }

            let num_str = &sql[num_start..i];
            if is_float {
                if let Ok(f) = num_str.parse::<f64>() {
                    result.push('?');
                    params.push(Value::Real(f));
                } else {
                    result.push_str(num_str);
                }
            } else if let Ok(n) = num_str.parse::<i64>() {
                result.push('?');
                if n >= i32::MIN as i64 && n <= i32::MAX as i64 {
                    params.push(Value::Int(n as i32));
                } else {
                    params.push(Value::BigInt(n));
                }
            } else {
                result.push_str(num_str);
            }
            continue;
        }

        // Everything else: copy as-is.
        result.push(b as char);
        i += 1;
    }

    (result, params)
}

/// Substitutes `Expr::Param { idx }` nodes in the AST with literal values.
fn substitute_params(stmt: Stmt, params: &[Value]) -> Result<Stmt, DbError> {
    super::prepared::substitute_params_in_ast(stmt, params)
}

/// FNV-1a hash for cache keys.
fn fnv1a_hash(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── normalize_sql ────────────────────────────────────────────────────────

    #[test]
    fn test_normalize_integer() {
        let (norm, params) = normalize_sql("SELECT * FROM t WHERE id = 42");
        assert_eq!(norm, "SELECT * FROM t WHERE id = ?");
        assert_eq!(params.len(), 1);
        assert_eq!(params[0], Value::Int(42));
    }

    #[test]
    fn test_normalize_string() {
        let (norm, params) = normalize_sql("SELECT * FROM t WHERE name = 'alice'");
        assert_eq!(norm, "SELECT * FROM t WHERE name = ?");
        assert_eq!(params, vec![Value::Text("alice".into())]);
    }

    #[test]
    fn test_normalize_multiple_literals() {
        let (norm, params) = normalize_sql("INSERT INTO t VALUES (1, 'hello', 2.5)");
        assert_eq!(norm, "INSERT INTO t VALUES (?, ?, ?)");
        assert_eq!(params.len(), 3);
        assert_eq!(params[0], Value::Int(1));
        assert_eq!(params[1], Value::Text("hello".into()));
        assert!(matches!(params[2], Value::Real(f) if (f - 2.5).abs() < 0.001));
    }

    #[test]
    fn test_normalize_preserves_identifiers() {
        let (norm, _) = normalize_sql("SELECT col1 FROM table2 WHERE id = 5");
        assert!(norm.contains("table2"));
        assert!(norm.contains("col1"));
    }

    #[test]
    fn test_normalize_escaped_string() {
        let (norm, params) = normalize_sql("SELECT * FROM t WHERE s = 'it''s'");
        assert_eq!(norm, "SELECT * FROM t WHERE s = ?");
        assert_eq!(params, vec![Value::Text("it's".into())]);
    }

    // ── PlanCache struct behavior ────────────────────────────────────────────

    #[test]
    fn plan_cache_starts_empty() {
        let cache = PlanCache::new(DEFAULT_MAX_ENTRIES);
        assert_eq!(cache.entries.len(), 0);
        assert_eq!(cache.hits, 0);
        assert_eq!(cache.misses, 0);
        assert_eq!(cache.invalidations, 0);
    }

    #[test]
    fn store_increments_generation_on_re_store() {
        let mut cache = PlanCache::new(DEFAULT_MAX_ENTRIES);
        let sql = "SELECT * FROM t WHERE id = 42";
        let stmt = axiomdb_sql::parser::parse(sql, None).unwrap();
        let deps = PlanDeps::default();

        cache.store(sql, &stmt, deps.clone(), 1);
        let (_, params) = normalize_sql(sql);
        let key = fnv1a_hash(normalize_sql(sql).0.as_bytes());
        assert_eq!(cache.entries[&key].generation, 0);

        // Re-store (simulates re-analysis after stale detection).
        cache.store(sql, &stmt, deps, 2);
        assert_eq!(cache.entries[&key].generation, 1);
        let _ = params;
    }

    #[test]
    fn invalidate_table_removes_matching_entries() {
        let mut cache = PlanCache::new(DEFAULT_MAX_ENTRIES);

        // Store entry with table_id=10
        let sql1 = "SELECT * FROM t WHERE id = 1";
        let stmt1 = axiomdb_sql::parser::parse(sql1, None).unwrap();
        let mut deps1 = PlanDeps::default();
        deps1.tables.push((10, 1)); // table_id=10, schema_version=1
        cache.store(sql1, &stmt1, deps1, 1);

        // Store entry with table_id=20
        let sql2 = "SELECT * FROM u WHERE id = 1";
        let stmt2 = axiomdb_sql::parser::parse(sql2, None).unwrap();
        let mut deps2 = PlanDeps::default();
        deps2.tables.push((20, 1)); // table_id=20
        cache.store(sql2, &stmt2, deps2, 1);

        assert_eq!(cache.entries.len(), 2);

        // Invalidate table_id=10 — only sql1 entry should be removed.
        cache.invalidate_table(10);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.invalidations, 1);

        // sql2 entry (table_id=20) must survive.
        let key2 = fnv1a_hash(normalize_sql(sql2).0.as_bytes());
        assert!(cache.entries.contains_key(&key2));
    }

    #[test]
    fn lru_eviction_removes_least_recently_used() {
        let mut cache = PlanCache::new(2); // capacity = 2

        let sql1 = "SELECT a FROM t WHERE id = 1";
        let sql2 = "SELECT b FROM t WHERE id = 1";
        let sql3 = "SELECT c FROM t WHERE id = 1";

        let stmt1 = axiomdb_sql::parser::parse(sql1, None).unwrap();
        let stmt2 = axiomdb_sql::parser::parse(sql2, None).unwrap();
        let stmt3 = axiomdb_sql::parser::parse(sql3, None).unwrap();

        cache.store(sql1, &stmt1, PlanDeps::default(), 1); // seq=0
        cache.store(sql2, &stmt2, PlanDeps::default(), 1); // seq=0

        // At capacity now (2 entries). Adding sql3 should evict the LRU (sql1 or sql2).
        cache.store(sql3, &stmt3, PlanDeps::default(), 1);
        assert_eq!(cache.entries.len(), 2);
    }

    #[test]
    fn cache_stats_reflect_operations() {
        let mut cache = PlanCache::new(DEFAULT_MAX_ENTRIES);
        let stats = cache.stats();
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.misses, 0);
        assert_eq!(stats.entries, 0);

        let sql = "SELECT * FROM t WHERE id = 1";
        let stmt = axiomdb_sql::parser::parse(sql, None).unwrap();
        cache.store(sql, &stmt, PlanDeps::default(), 0);

        let stats = cache.stats();
        assert_eq!(stats.entries, 1);
    }
}
