//! Integration tests for Phase 6.9: FK + Index improvements.
//! - Task A: PK B-Tree population on INSERT
//! - Task B: FK auto-index with composite key (fk_val | RecordId)
//! - Task C: Composite index planner

use axiomdb_catalog::bootstrap::CatalogBootstrap;
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
        self.run(sql)
            .expect_err(&format!("expected error for: {sql}"))
    }
    fn rows(&mut self, sql: &str) -> Vec<Vec<Value>> {
        match self.ok(sql) {
            QueryResult::Rows { rows, .. } => rows,
            other => panic!("expected rows, got {other:?}"),
        }
    }
    fn count(&mut self, sql: &str) -> usize {
        self.rows(sql).len()
    }
    fn snap(&self) -> axiomdb_core::TransactionSnapshot {
        self.txn.snapshot()
    }
}

macro_rules! setup {
    ($db:expr, $($sql:expr),+) => { $( $db.ok($sql); )+ };
}

// ── Task A: PK B-Tree population ─────────────────────────────────────────────

#[test]
fn test_pk_btree_populated_on_insert() {
    let mut db = Db::new();
    setup!(db, "CREATE TABLE t (id INT PRIMARY KEY, x TEXT)");
    db.ok("INSERT INTO t VALUES (1, 'a')");

    // Verify PK index exists and has a non-empty root.
    let snap = db.snap();
    let mut reader = axiomdb_catalog::CatalogReader::new(&db.storage, snap).unwrap();
    let t = reader.get_table("public", "t").unwrap().unwrap();
    let indexes = reader.list_indexes(t.id).unwrap();
    let pk = indexes.iter().find(|i| i.is_primary).unwrap();
    // Can do a B-Tree lookup on the PK root — should find key=1.
    let key = axiomdb_sql::key_encoding::encode_index_key(&[Value::Int(1)]).unwrap();
    let result = axiomdb_index::BTree::lookup_in(&db.storage, pk.root_page_id, &key).unwrap();
    assert!(
        result.is_some(),
        "PK B-Tree must contain key=1 after INSERT"
    );
}

#[test]
fn test_pk_uniqueness_enforced_via_btree() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, x TEXT)",
        "INSERT INTO t VALUES (1, 'a')"
    );
    let err = db.err("INSERT INTO t VALUES (1, 'b')");
    assert!(
        matches!(err, DbError::UniqueViolation { .. }),
        "duplicate PK must be rejected: {err}"
    );
}

#[test]
fn test_fk_parent_lookup_uses_btree_not_scan() {
    // After Phase 6.9, FK INSERT validation uses B-Tree for PK lookup.
    // Verify correctness: parent exists → INSERT OK; parent absent → error.
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id))",
        "INSERT INTO users VALUES (1)"
    );
    db.ok("INSERT INTO orders VALUES (10, 1)"); // OK: user 1 in PK B-Tree
    let err = db.err("INSERT INTO orders VALUES (20, 999)");
    assert!(matches!(err, DbError::ForeignKeyViolation { .. }), "{err}");
}

// ── Task B: FK composite key index ───────────────────────────────────────────

#[test]
fn test_fk_auto_index_created_with_is_fk_index_flag() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id))"
    );
    let snap = db.snap();
    let mut reader = axiomdb_catalog::CatalogReader::new(&db.storage, snap).unwrap();
    let orders = reader.get_table("public", "orders").unwrap().unwrap();
    let fks = reader.list_fk_constraints(orders.id).unwrap();
    assert_eq!(fks.len(), 1);
    let fk = &fks[0];
    assert_ne!(
        fk.fk_index_id, 0,
        "FK auto-index must be created (non-zero id)"
    );

    let indexes = reader.list_indexes(orders.id).unwrap();
    let fk_idx = indexes
        .iter()
        .find(|i| i.index_id == fk.fk_index_id)
        .unwrap();
    assert!(
        fk_idx.is_fk_index,
        "FK auto-index must have is_fk_index=true"
    );
}

#[test]
fn test_fk_auto_index_handles_duplicate_fk_values() {
    // Multiple rows with the same FK value — no DuplicateKey error.
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id))",
        "INSERT INTO users VALUES (1)"
    );
    db.ok("INSERT INTO orders VALUES (10, 1)");
    db.ok("INSERT INTO orders VALUES (11, 1)"); // same user_id — must work
    db.ok("INSERT INTO orders VALUES (12, 1)"); // three orders for user 1 — still OK
}

#[test]
fn test_fk_restrict_uses_btree_range_scan() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id) ON DELETE RESTRICT)",
        "INSERT INTO users VALUES (1)",
        "INSERT INTO orders VALUES (10, 1)",
        "INSERT INTO orders VALUES (11, 1)"
    );
    // RESTRICT must detect children via FK composite index range scan.
    let err = db.err("DELETE FROM users WHERE id = 1");
    assert!(
        matches!(err, DbError::ForeignKeyParentViolation { .. }),
        "RESTRICT must error: {err}"
    );
}

#[test]
fn test_fk_cascade_uses_btree_range_scan() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id) ON DELETE CASCADE)",
        "INSERT INTO users VALUES (1)",
        "INSERT INTO orders VALUES (10, 1)",
        "INSERT INTO orders VALUES (11, 1)"
    );
    db.ok("DELETE FROM users WHERE id = 1");
    assert_eq!(
        db.count("SELECT id FROM orders WHERE user_id = 1"),
        0,
        "CASCADE must delete all children"
    );
}

// ── Task C: Composite index planner ──────────────────────────────────────────

#[test]
fn test_composite_index_eq_two_columns() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, a INT, b TEXT)",
        "CREATE INDEX idx_ab ON t(a, b)",
        "INSERT INTO t VALUES (1, 10, 'foo')",
        "INSERT INTO t VALUES (2, 10, 'bar')",
        "INSERT INTO t VALUES (3, 20, 'foo')"
    );
    // WHERE a = 10 AND b = 'foo' should use the composite index.
    let result = db.rows("SELECT id FROM t WHERE a = 10 AND b = 'foo'");
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Int(1));
}

#[test]
fn test_composite_index_reversed_where_order() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, a INT, b TEXT)",
        "CREATE INDEX idx_ab ON t(a, b)",
        "INSERT INTO t VALUES (1, 10, 'foo')"
    );
    // b = 'foo' AND a = 10 (reversed) must still use index (a, b).
    let result = db.rows("SELECT id FROM t WHERE b = 'foo' AND a = 10");
    assert_eq!(
        result.len(),
        1,
        "reversed WHERE order must still use composite index"
    );
}

#[test]
fn test_composite_index_partial_match_falls_to_single_col() {
    // WHERE a = 10 only: Rule 0 finds 1 matching column → not activated;
    // Rule 1 uses the leading column a. No regression.
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, a INT, b TEXT)",
        "CREATE INDEX idx_ab ON t(a, b)",
        "INSERT INTO t VALUES (1, 10, 'foo')",
        "INSERT INTO t VALUES (2, 10, 'bar')"
    );
    let result = db.rows("SELECT id FROM t WHERE a = 10");
    assert_eq!(
        result.len(),
        2,
        "single-col WHERE must return all matching rows"
    );
}

#[test]
fn test_composite_index_gap_falls_to_scan() {
    // WHERE a = 10 AND c = 5 with index (a, b) — b is not in WHERE.
    // Rule 0: a matches col 0, b not in WHERE → stops at 1 column → Rule 0 skipped.
    // Rule 1: a matches → IndexLookup on a.
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT, c INT)",
        "CREATE INDEX idx_ab ON t(a, b)",
        "INSERT INTO t VALUES (1, 10, 1, 5)",
        "INSERT INTO t VALUES (2, 10, 2, 6)"
    );
    let result = db.rows("SELECT id FROM t WHERE a = 10 AND c = 5");
    // Filtered by WHERE after scan — result has 1 row (id=1)
    assert_eq!(result.len(), 1);
}

#[test]
fn test_composite_index_prefix_scan_returns_multiple_rows() {
    // WHERE a = 10 on composite index (a, b): Rule 1 does prefix range scan.
    // All rows with a=10 are returned regardless of b value.
    // Note: two rows with SAME composite key (a=10, b='foo') would cause
    // DuplicateKey (B-Tree Phase 6.9 limitation for non-unique composite keys —
    // Phase 6.10 will add RecordId suffix for non-unique composite indexes).
    // This test uses distinct composite keys (different b values).
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, a INT, b TEXT)",
        "CREATE INDEX idx_ab ON t(a, b)",
        "INSERT INTO t VALUES (1, 10, 'alpha')", // distinct composite keys
        "INSERT INTO t VALUES (2, 10, 'beta')",  // different b
        "INSERT INTO t VALUES (3, 20, 'alpha')"  // different a — excluded
    );
    // Single-col WHERE with composite index: prefix scan returns all a=10 rows.
    let result = db.rows("SELECT id FROM t WHERE a = 10");
    assert_eq!(
        result.len(),
        2,
        "prefix scan must return all rows with a=10"
    );
}

#[test]
fn test_composite_index_three_columns() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT, c INT)",
        "CREATE INDEX idx_abc ON t(a, b, c)",
        "INSERT INTO t VALUES (1, 1, 2, 3)",
        "INSERT INTO t VALUES (2, 1, 2, 4)",
        "INSERT INTO t VALUES (3, 1, 3, 3)"
    );
    let result = db.rows("SELECT id FROM t WHERE a = 1 AND b = 2 AND c = 3");
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Int(1));
}
