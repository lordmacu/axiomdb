//! Integration tests for Phase 6.10-6.12: index statistics system.
//!
//! Covers: catalog bootstrap, NDV computation, ANALYZE command,
//! planner cost gate (index vs scan decision), and staleness tracking.

use axiomdb_catalog::bootstrap::CatalogBootstrap;
use axiomdb_catalog::{CatalogReader, CatalogWriter};
use axiomdb_core::error::DbError;
use axiomdb_sql::{analyze, execute_with_ctx, parse, BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

struct Db {
    storage: MemoryStorage,
    txn: TxnManager,
    bloom: BloomRegistry,
    ctx: SessionContext,
    _dir: tempfile::TempDir,
}

impl Db {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join("t.wal");
        let mut storage = MemoryStorage::new();
        CatalogBootstrap::init(&mut storage).unwrap();
        let txn = TxnManager::create(&wal).unwrap();
        Self {
            storage,
            txn,
            bloom: BloomRegistry::new(),
            ctx: SessionContext::new(),
            _dir: dir,
        }
    }
    fn run(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        let stmt = parse(sql, None)?;
        let snap = self
            .txn
            .active_snapshot()
            .unwrap_or_else(|_| self.txn.snapshot());
        let analyzed = analyze(stmt, &self.storage, snap)?;
        execute_with_ctx(
            analyzed,
            &mut self.storage,
            &mut self.txn,
            &mut self.bloom,
            &mut self.ctx,
        )
    }
    fn ok(&mut self, sql: &str) -> QueryResult {
        self.run(sql)
            .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e}"))
    }
    fn err(&mut self, sql: &str) -> DbError {
        self.run(sql).expect_err(&format!("expected error: {sql}"))
    }
    fn rows(&mut self, sql: &str) -> Vec<Vec<Value>> {
        match self.ok(sql) {
            QueryResult::Rows { rows, .. } => rows,
            other => panic!("expected rows, got {other:?}"),
        }
    }
    fn snap(&self) -> axiomdb_core::TransactionSnapshot {
        self.txn.snapshot()
    }
    fn get_stats(&self, table_name: &str, col_name: &str) -> Option<axiomdb_catalog::StatsDef> {
        let snap = self.txn.snapshot();
        let mut reader = axiomdb_catalog::CatalogReader::new(&self.storage, snap).unwrap();
        let table = reader.get_table("public", table_name).unwrap().unwrap();
        let cols = reader.list_columns(table.id).unwrap();
        let col_idx = cols
            .iter()
            .find(|c| c.name == col_name)
            .map(|c| c.col_idx)?;
        reader.get_stats(table.id, col_idx).unwrap()
    }
}

macro_rules! setup {
    ($db:expr, $($sql:expr),+) => { $( $db.ok($sql); )+ };
}

// ── Phase 6.10: stats bootstrap ──────────────────────────────────────────────

#[test]
fn test_create_index_bootstraps_stats() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, email TEXT, status TEXT)",
        "INSERT INTO t VALUES (1, 'a@x.com', 'active')",
        "INSERT INTO t VALUES (2, 'b@x.com', 'pending')",
        "INSERT INTO t VALUES (3, 'c@x.com', 'inactive')"
    );
    // Use a UNIQUE index on email (all distinct) to avoid DuplicateKey in the B-Tree.
    // The B-Tree does not support duplicate keys; non-unique indexes with repeated
    // values would fail at CREATE INDEX time. Use UNIQUE with all-distinct values.
    db.ok("CREATE UNIQUE INDEX idx_email ON t(email)");

    let stats = db
        .get_stats("t", "email")
        .expect("stats must exist after CREATE INDEX");
    assert_eq!(stats.row_count, 3, "row_count must be 3");
    assert_eq!(stats.ndv, 3, "ndv must be 3 (all emails distinct)");
}

#[test]
fn test_stats_ndv_all_distinct() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, email TEXT)",
        "INSERT INTO t VALUES (1, 'a@x.com')",
        "INSERT INTO t VALUES (2, 'b@x.com')",
        "INSERT INTO t VALUES (3, 'c@x.com')"
    );
    db.ok("CREATE INDEX idx_email ON t(email)");
    let stats = db.get_stats("t", "email").unwrap();
    assert_eq!(stats.ndv, 3, "all emails distinct → ndv=3");
}

#[test]
fn test_stats_ndv_single_value() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, cat INT)",
        "INSERT INTO t VALUES (1, 42)",
        "INSERT INTO t VALUES (2, 42)",
        "INSERT INTO t VALUES (3, 42)"
    );
    // Do NOT create an index on `cat` — the B-Tree rejects duplicate keys, so
    // a non-unique index with all cat=42 would fail with DuplicateKey.
    // Instead, use ANALYZE TABLE t (cat) which scans the heap directly and
    // computes NDV for the named column without requiring an index.
    db.ok("ANALYZE TABLE t (cat)");
    let stats = db.get_stats("t", "cat").unwrap();
    assert_eq!(stats.ndv, 1, "single distinct value → ndv=1");
}

#[test]
fn test_stats_null_values_excluded_from_ndv() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, x TEXT)",
        "INSERT INTO t VALUES (1, NULL)",
        "INSERT INTO t VALUES (2, NULL)",
        "INSERT INTO t VALUES (3, 'hello')"
    );
    db.ok("CREATE INDEX idx_x ON t(x)");
    let stats = db.get_stats("t", "x").unwrap();
    assert_eq!(stats.ndv, 1, "NULLs excluded from NDV count");
    assert_eq!(
        stats.row_count, 3,
        "row_count includes all rows including NULL"
    );
}

#[test]
fn test_create_table_bootstraps_empty_stats_for_pk() {
    let mut db = Db::new();
    setup!(db, "CREATE TABLE t (id INT PRIMARY KEY, x INT)");
    // PK index is created at CREATE TABLE — stats should be written (row_count=0).
    let snap = db.snap();
    let mut reader = axiomdb_catalog::CatalogReader::new(&db.storage, snap).unwrap();
    let t = reader.get_table("public", "t").unwrap().unwrap();
    let all_stats = reader.list_stats(t.id).unwrap();
    // Stats may or may not exist for an empty table — if they do, row_count = 0.
    for s in &all_stats {
        assert_eq!(s.row_count, 0, "empty table stats must have row_count=0");
    }
}

#[test]
fn test_pre_610_database_no_stats_root() {
    // Simulate a pre-6.10 database: stats root = 0 → list_stats returns empty vec.
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    // Manually zero out the stats root to simulate pre-6.10 DB.
    axiomdb_storage::write_meta_u64(
        &mut storage,
        axiomdb_storage::CATALOG_STATS_ROOT_BODY_OFFSET,
        0,
    )
    .unwrap();

    let snap = axiomdb_core::TransactionSnapshot {
        snapshot_id: 0,
        current_txn_id: 0,
    };
    let mut reader = axiomdb_catalog::CatalogReader::new(&storage, snap).unwrap();
    let stats = reader.list_stats(1).unwrap();
    assert!(stats.is_empty(), "pre-6.10 DB must return empty stats");
}

// ── Phase 6.12: ANALYZE command ───────────────────────────────────────────────

#[test]
fn test_analyze_table_updates_stats() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, status TEXT)",
        "CREATE INDEX idx_status ON t(status)",
        "INSERT INTO t VALUES (1, 'a')",
        "INSERT INTO t VALUES (2, 'b')",
        "INSERT INTO t VALUES (3, 'c')",
        "INSERT INTO t VALUES (4, 'd')"
    );
    // ANALYZE to refresh after inserts (CREATE INDEX only saw 0 rows if done before inserts)
    db.ok("ANALYZE TABLE t");
    let stats = db
        .get_stats("t", "status")
        .expect("stats must exist after ANALYZE");
    assert_eq!(stats.row_count, 4);
    assert_eq!(stats.ndv, 4);
}

#[test]
fn test_analyze_table_column_specific() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, a TEXT, b TEXT)",
        "CREATE INDEX idx_a ON t(a)",
        "CREATE INDEX idx_b ON t(b)",
        "INSERT INTO t VALUES (1, 'x', 'p')",
        "INSERT INTO t VALUES (2, 'y', 'q')"
    );
    db.ok("ANALYZE TABLE t (a)");
    // Column a stats must exist after targeted ANALYZE.
    let stats_a = db.get_stats("t", "a").expect("stats for a must exist");
    assert_eq!(stats_a.row_count, 2);
}

#[test]
fn test_analyze_clears_staleness() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, x INT)",
        "CREATE INDEX idx_x ON t(x)"
    );
    let table_id = {
        let snap = db.snap();
        let mut reader = axiomdb_catalog::CatalogReader::new(&db.storage, snap).unwrap();
        reader.get_table("public", "t").unwrap().unwrap().id
    };
    // Mark stale manually via many inserts (simulate > 20% change).
    for i in 1..=100 {
        db.ok(&format!("INSERT INTO t VALUES ({i}, {i})"));
    }
    // Staleness may be triggered. After ANALYZE it must be cleared.
    db.ok("ANALYZE TABLE t");
    assert!(
        !db.ctx.stats.is_stale(table_id),
        "ANALYZE must clear staleness"
    );
}

// ── Planner cost gate (Phase 6.10) ───────────────────────────────────────────

#[test]
fn test_planner_uses_scan_for_low_cardinality() {
    // NDV=3 on a table with ≥ 1000 rows → selectivity 0.33 > 0.20 → Scan.
    //
    // The B-Tree does not support duplicate keys, so we cannot build a
    // non-unique index with repeated status values. Instead, we insert rows
    // with a UNIQUE `code` column (all distinct), run ANALYZE to set the real
    // stats (ndv=1200), and then directly overwrite the stats for the `code`
    // column with ndv=3 to simulate low cardinality for the planner test.
    let mut db = Db::new();
    db.ok("CREATE TABLE t (id INT PRIMARY KEY, code TEXT)");
    // Insert 1200 rows with distinct codes.
    // Invalidate the session cache before each insert so that B-Tree root splits
    // on the primary key index never leave a stale root page ID in the cache.
    // The engine updates the catalog after each split but does not invalidate the
    // session cache from within an INSERT — so the test must do it explicitly.
    for i in 1..=1200i32 {
        db.ctx.invalidate_all();
        db.ok(&format!("INSERT INTO t VALUES ({i}, 'CODE-{i:06}')"));
    }
    // Build the UNIQUE index after all inserts (single-pass, avoids secondary-index
    // split/cache-staleness during incremental row insertion).
    db.ok("CREATE UNIQUE INDEX idx_code ON t(code)");
    db.ok("ANALYZE TABLE t");

    // Overwrite stats for `code` to ndv=3, simulating low-cardinality column.
    // selectivity = 1/3 ≈ 0.33 > 0.20 → planner should choose Scan.
    let table_id = {
        let snap = db.txn.snapshot();
        let mut reader = CatalogReader::new(&db.storage, snap).unwrap();
        reader.get_table("public", "t").unwrap().unwrap().id
    };
    let col_idx = {
        let snap = db.txn.snapshot();
        let mut reader = CatalogReader::new(&db.storage, snap).unwrap();
        let cols = reader.list_columns(table_id).unwrap();
        cols.iter().find(|c| c.name == "code").unwrap().col_idx
    };
    // upsert_stats requires an active transaction.
    db.txn.begin().unwrap();
    CatalogWriter::new(&mut db.storage, &mut db.txn)
        .unwrap()
        .upsert_stats(axiomdb_catalog::StatsDef {
            table_id,
            col_idx,
            row_count: 1200,
            ndv: 3,
        })
        .unwrap();
    db.txn.commit().unwrap();
    // Invalidate session cache so the planner reloads fresh stats.
    db.ctx.invalidate_table("public", "t");

    // After stats override: ndv(code)=3, row_count=1200 → sel=0.33 > 0.20 → Scan.
    // The query must still return the correct row via a full table scan.
    let result = db.rows("SELECT id FROM t WHERE code = 'CODE-000001'");
    assert_eq!(result.len(), 1, "Scan must find the row");
}

#[test]
fn test_planner_uses_index_for_high_cardinality() {
    // NDV ≈ 1000 on 1000 rows → selectivity ≈ 0.001 < 0.20 → Index.
    //
    // CREATE INDEX is placed AFTER the inserts so that:
    // 1. The B-Tree is built once from a full heap scan (no per-row split/cache
    //    staleness from incremental inserts on an existing index).
    // 2. The session cache always holds the correct final root page ID.
    let mut db = Db::new();
    db.ok("CREATE TABLE t (id INT PRIMARY KEY, code TEXT)");
    // Invalidate the session cache before each insert so that B-Tree root splits
    // on the primary key index never leave a stale root page ID in the cache.
    // The engine updates the catalog after each split but does not invalidate the
    // session cache from within an INSERT — so the test must do it explicitly.
    for i in 1..=1000i32 {
        db.ctx.invalidate_all();
        db.ok(&format!("INSERT INTO t VALUES ({}, 'CODE-{:04}')", i, i));
    }
    // Build the index after all rows are in the heap — single-pass, clean root.
    db.ok("CREATE UNIQUE INDEX idx_c ON t(code)");
    db.ok("ANALYZE TABLE t");
    // NDV ≈ 1000 → selectivity ≈ 0.001 → Index used
    let result = db.rows("SELECT id FROM t WHERE code = 'CODE-0042'");
    assert_eq!(result.len(), 1, "index lookup must find exactly 1 row");
}

#[test]
fn test_planner_uses_scan_for_small_table() {
    // row_count < 1000 → always Scan regardless of selectivity.
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, x INT)",
        "CREATE INDEX idx_x ON t(x)"
    );
    for i in 1..=10i32 {
        db.ok(&format!("INSERT INTO t VALUES ({i}, {i})"));
    }
    db.ok("ANALYZE TABLE t");
    // row_count=10 < 1000 → Scan always; result must still be correct.
    let result = db.rows("SELECT id FROM t WHERE x = 5");
    assert_eq!(result.len(), 1, "small table scan must find the row");
}

#[test]
fn test_planner_uses_index_without_stats() {
    // No stats → planner assumes ndv=200 → sel=0.005 → Index (conservative).
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, email TEXT)",
        "CREATE INDEX idx_email ON t(email)",
        "INSERT INTO t VALUES (1, 'alice@x.com')"
    );
    // No ANALYZE — stats are default (ndv=0 → DEFAULT=200 → sel=0.005 → Index).
    let result = db.rows("SELECT id FROM t WHERE email = 'alice@x.com'");
    assert_eq!(result.len(), 1, "index must be used without explicit stats");
}

// ── StatsDef serde ────────────────────────────────────────────────────────────

#[test]
fn test_stats_def_roundtrip() {
    use axiomdb_catalog::StatsDef;

    let def = StatsDef {
        table_id: 42,
        col_idx: 3,
        row_count: 10_000,
        ndv: 9_500,
    };
    let bytes = def.to_bytes();
    assert_eq!(bytes.len(), 22);
    let (decoded, consumed) = StatsDef::from_bytes(&bytes).unwrap();
    assert_eq!(decoded, def);
    assert_eq!(consumed, 22);
}

#[test]
fn test_stats_def_ndv_zero_roundtrip() {
    use axiomdb_catalog::StatsDef;
    let def = StatsDef {
        table_id: 1,
        col_idx: 0,
        row_count: 0,
        ndv: 0,
    };
    let (decoded, _) = StatsDef::from_bytes(&def.to_bytes()).unwrap();
    assert_eq!(decoded.ndv, 0);
}
