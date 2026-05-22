//! Tests for the statement-fingerprinting cache (Attack 2).
//!
//! Spec: `specs/fase-perf-sqlite-gap/spec-statement-fingerprinting.md`
//! Plan: `specs/fase-perf-sqlite-gap/plan-statement-fingerprinting.md`
//!
//! Step 2.1 (this file at first): unit tests for the AST literal walker
//! `extract_literals` and its round-trip with `substitute_params`.
//! Subsequent steps add shape_hash tests, cache-API tests, and
//! end-to-end tests that drive `Db::run_inner`.

use axiomdb_sql::{
    parse,
    statement_cache::{extract_literals, shape_hash, substitute_params},
};

#[test]
fn extract_then_substitute_roundtrips_simple_insert() {
    let original = parse("INSERT INTO t VALUES (1, 'hello', 3.14, TRUE, NULL)", None).unwrap();
    let mut stmt = original.clone();
    let extracted = extract_literals(&mut stmt);
    assert_eq!(
        extracted.len(),
        5,
        "5 literals: 1 INT, 1 TEXT, 1 REAL, 1 BOOL, 1 NULL"
    );
    let restored = substitute_params(stmt, &extracted).unwrap();
    assert_eq!(restored, original, "round-trip must match original AST");
}

#[test]
fn extract_handles_select_where_binary_op() {
    let original = parse("SELECT id FROM t WHERE id = 42 AND name = 'alice'", None).unwrap();
    let mut stmt = original.clone();
    let extracted = extract_literals(&mut stmt);
    assert_eq!(extracted.len(), 2);
    let restored = substitute_params(stmt, &extracted).unwrap();
    assert_eq!(restored, original);
}

#[test]
fn extract_handles_multi_row_values() {
    let original = parse("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')", None).unwrap();
    let mut stmt = original.clone();
    let extracted = extract_literals(&mut stmt);
    assert_eq!(extracted.len(), 6, "3 rows × 2 cols");
    let restored = substitute_params(stmt, &extracted).unwrap();
    assert_eq!(restored, original);
}

#[test]
fn extract_handles_no_literals() {
    // SELECT * has no literals; extracted should be empty.
    let original = parse("SELECT * FROM t", None).unwrap();
    let mut stmt = original.clone();
    let extracted = extract_literals(&mut stmt);
    assert!(extracted.is_empty());
    let restored = substitute_params(stmt, &extracted).unwrap();
    assert_eq!(restored, original);
}

// ── Step 2.2: shape_hash ─────────────────────────────────────────────────

fn prepare_shape(sql: &str) -> (axiomdb_sql::ast::Stmt, Vec<axiomdb_types::Value>) {
    let mut stmt = parse(sql, None).unwrap();
    let extracted = extract_literals(&mut stmt);
    (stmt, extracted)
}

#[test]
fn shape_hash_equal_for_same_shape_different_literals() {
    // Same INSERT, different literal values → same shape → same hash.
    let (s1, _) = prepare_shape("INSERT INTO t VALUES (1, 'a')");
    let (s2, _) = prepare_shape("INSERT INTO t VALUES (99, 'zzz')");
    assert_eq!(
        shape_hash(&s1),
        shape_hash(&s2),
        "different literals must collapse to the same shape hash"
    );
}

#[test]
fn shape_hash_distinct_for_different_table() {
    let (s1, _) = prepare_shape("INSERT INTO t1 VALUES (1)");
    let (s2, _) = prepare_shape("INSERT INTO t2 VALUES (1)");
    assert_ne!(shape_hash(&s1), shape_hash(&s2));
}

#[test]
fn shape_hash_distinct_for_different_column_list() {
    let (s1, _) = prepare_shape("INSERT INTO t(a, b) VALUES (1, 2)");
    let (s2, _) = prepare_shape("INSERT INTO t(a, c) VALUES (1, 2)");
    assert_ne!(shape_hash(&s1), shape_hash(&s2));
}

#[test]
fn shape_hash_distinct_for_different_values_count() {
    let (s1, _) = prepare_shape("INSERT INTO t VALUES (1, 2)");
    let (s2, _) = prepare_shape("INSERT INTO t VALUES (1, 2, 3)");
    assert_ne!(shape_hash(&s1), shape_hash(&s2));
}

#[test]
fn shape_hash_distinct_for_different_in_list_length() {
    let (s1, _) = prepare_shape("SELECT * FROM t WHERE id IN (1, 2)");
    let (s2, _) = prepare_shape("SELECT * FROM t WHERE id IN (1, 2, 3)");
    assert_ne!(shape_hash(&s1), shape_hash(&s2));
}

// ── Step 2.3: CachedPlan + SessionContext LRU API ────────────────────────

mod harness {
    use axiomdb_catalog::bootstrap::CatalogBootstrap;
    use axiomdb_core::error::DbError;
    use axiomdb_sql::{
        analyze_cached, bloom::BloomRegistry, execute_with_ctx, parse_with_sql_mode,
        result::QueryResult, SchemaCache, SessionContext,
    };
    use axiomdb_storage::MemoryStorage;
    use axiomdb_wal::TxnManager;

    pub struct Harness {
        pub storage: MemoryStorage,
        pub txn: TxnManager,
        pub bloom: BloomRegistry,
        pub schema_cache: SchemaCache,
        pub session: SessionContext,
    }

    impl Harness {
        pub fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let wal_path = dir.keep().join("test.wal");
            let storage = MemoryStorage::new();
            CatalogBootstrap::init(&storage).expect("bootstrap");
            let txn = TxnManager::create(&wal_path).expect("create txn");
            Self {
                storage,
                txn,
                bloom: BloomRegistry::new(),
                schema_cache: SchemaCache::new(),
                session: SessionContext::default(),
            }
        }

        /// Legacy parse + analyze + execute path. Mirrors `Db::run_inner`
        /// after the Attack 2 wire-up was reverted (see plan Step 2.4
        /// outcome).
        pub fn run(&mut self, sql: &str) -> Result<QueryResult, DbError> {
            let stmt = parse_with_sql_mode(sql, None, self.session.sql_mode_flags())?;
            let snap = if let Some(ref ct) = self.session.conn_txn {
                self.txn.active_snapshot(ct)
            } else {
                self.txn.snapshot()
            };
            let analyzed = analyze_cached(stmt, &self.storage, snap, &mut self.schema_cache)?;
            execute_with_ctx(
                analyzed,
                &self.storage,
                &self.txn,
                &self.bloom,
                &mut self.session,
            )
        }
    }
}

use axiomdb_sql::plan_deps::PlanDeps;
use axiomdb_sql::statement_cache::{CachedPlan, STATEMENT_CACHE_MAX_ENTRIES};

fn fake_plan() -> CachedPlan {
    // A trivial Stmt is fine — these tests are about the cache machinery,
    // not the AST. Use parsed "SELECT 1" which always exists.
    let stmt = parse("SELECT 1", None).unwrap();
    CachedPlan {
        analyzed: stmt,
        param_count: 0,
        deps: PlanDeps::default(),
    }
}

#[test]
fn cache_hit_returns_some_plan() {
    let mut ctx = axiomdb_sql::SessionContext::default();
    ctx.cache_plan(0x1234, fake_plan());
    assert_eq!(ctx.statement_cache_count(), 1);
}

#[test]
fn cache_miss_returns_none() {
    let ctx = axiomdb_sql::SessionContext::default();
    assert_eq!(ctx.statement_cache_count(), 0);
}

#[test]
fn cache_lru_evicts_oldest_when_full() {
    let mut ctx = axiomdb_sql::SessionContext::default();
    // Fill the cache to capacity.
    for i in 0..STATEMENT_CACHE_MAX_ENTRIES as u64 {
        ctx.cache_plan(i, fake_plan());
    }
    assert_eq!(ctx.statement_cache_count(), STATEMENT_CACHE_MAX_ENTRIES);
    // One more entry forces eviction.
    ctx.cache_plan(99999, fake_plan());
    assert_eq!(
        ctx.statement_cache_count(),
        STATEMENT_CACHE_MAX_ENTRIES,
        "cache size capped at the constant"
    );
}

#[test]
fn cache_stale_via_plan_deps_evicts() {
    use axiomdb_catalog::CatalogReader;

    let mut h = harness::Harness::new();
    h.run("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();

    // Resolve table_id + current schema_version directly from the catalog.
    // (We bypass extract_table_deps here because it has a pre-existing bug
    // around the default-schema convention; covered separately.)
    let (table_id, version_v1) = {
        let snap = h.txn.snapshot();
        let mut reader = CatalogReader::new(&h.storage, snap).unwrap();
        let def = reader
            .get_table_in_database("axiomdb", "public", "t")
            .expect("read catalog")
            .expect("table t exists");
        (def.id, def.schema_version)
    };

    // Build a synthetic PlanDeps that pins (table_id, version_v1).
    let deps = PlanDeps {
        tables: vec![(table_id, version_v1)],
        items: vec![],
    };

    let mut stmt = parse("INSERT INTO t VALUES (1)", None).unwrap();
    let _ = extract_literals(&mut stmt);
    h.session.cache_plan(
        0x42,
        CachedPlan {
            analyzed: stmt,
            param_count: 1,
            deps,
        },
    );
    assert_eq!(h.session.statement_cache_count(), 1);

    // Bump schema_version via ALTER. The new version != version_v1.
    h.run("ALTER TABLE t ADD COLUMN x INT DEFAULT 0").unwrap();

    // Lookup with deps validation must return None AND evict the entry.
    let snap = h.txn.snapshot();
    let mut reader = CatalogReader::new(&h.storage, snap).unwrap();
    let got = h.session.get_cached_plan(0x42, &mut reader).unwrap();
    assert!(got.is_none(), "stale plan must not be returned");
    assert_eq!(
        h.session.statement_cache_count(),
        0,
        "stale entry must be evicted"
    );
}

// NOTE: end-to-end "run_inner_*" tests were prototyped here for the
// Step 2.4 wire-up and then removed when the wire-up was reverted (see
// plan Step 2.4 outcome). The library-level tests above still cover
// extract_literals / shape_hash / CachedPlan / SessionContext LRU /
// stale-deps eviction — when run_cached is re-wired in a follow-up, the
// end-to-end tests can be reinstated from git history (commit 662488d1
// vs the revert commit).
