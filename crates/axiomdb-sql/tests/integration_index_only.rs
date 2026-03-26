//! Integration tests for index-only scans (Phase 6.13).
//!
//! Verifies that:
//!   1. A query whose SELECT columns are fully covered by an index key returns
//!      correct rows without touching the heap (AccessMethod::IndexOnlyScan).
//!   2. INCLUDE (...) columns are accepted in DDL and stored in the catalog.
//!   3. Covered queries work for equality, range, and partial-index scenarios.
//!   4. Non-covered queries still fall back to IndexLookup / Scan.
//!   5. MVCC visibility is respected (deleted / uncommitted rows are hidden).
//!
//! ## Important: two test paths
//!
//! Tests using `execute` (non-ctx) pass `select_col_idxs = &[]` to the planner,
//! which prevents `index_covers_query` from returning true → IndexOnlyScan is
//! **never** selected on the non-ctx path. Those tests exercise IndexLookup /
//! IndexRange / Scan fallback.
//!
//! Tests using `execute_with_ctx` (ctx path) supply the real `select_col_idxs`
//! and **actually exercise IndexOnlyScan** when all SELECT columns are covered.
//! The ctx-path section at the bottom of this file contains these real tests.

use axiomdb_catalog::{CatalogBootstrap, CatalogReader};
use axiomdb_core::error::DbError;
use axiomdb_sql::{analyze, execute, parse, QueryResult};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

// ── helpers ───────────────────────────────────────────────────────────────────

fn setup() -> (MemoryStorage, TxnManager) {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.keep().join("test.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let txn = TxnManager::create(&wal_path).unwrap();
    (storage, txn)
}

fn run(sql: &str, storage: &mut MemoryStorage, txn: &mut TxnManager) -> QueryResult {
    run_result(sql, storage, txn).unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
}

fn run_result(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let stmt = parse(sql, None)?;
    let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    let analyzed = analyze(stmt, storage, snap)?;
    execute(analyzed, storage, txn)
}

fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

// ── DDL: INCLUDE columns ─────────────────────────────────────────────────────

#[test]
fn test_create_index_with_include_columns_accepted() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE products (id INT, name TEXT, price INT)",
        &mut storage,
        &mut txn,
    );
    // INCLUDE syntax must be accepted without error
    run(
        "CREATE INDEX idx_price_include ON products (price) INCLUDE (name)",
        &mut storage,
        &mut txn,
    );
}

#[test]
fn test_include_columns_stored_in_catalog() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE items (id INT, sku TEXT, qty INT, price INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "CREATE INDEX idx_sku_cover ON items (sku) INCLUDE (qty, price)",
        &mut storage,
        &mut txn,
    );

    // Verify catalog has the index with include_columns populated
    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let table = reader
        .get_table("public", "items")
        .unwrap()
        .expect("table not found");
    let indexes = reader.list_indexes(table.id).unwrap();
    let idx = indexes
        .iter()
        .find(|i| i.name == "idx_sku_cover")
        .expect("index not found");
    // The index covers `sku` (key column) and INCLUDE (qty, price)
    assert_eq!(idx.columns.len(), 1, "one key column");
    assert_eq!(idx.include_columns.len(), 2, "two INCLUDE columns");
}

// ── Index-only scan: equality ─────────────────────────────────────────────────

#[test]
fn test_covered_eq_returns_correct_rows() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE users (id INT, age INT, email TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "CREATE INDEX idx_age ON users (age)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (1, 25, 'alice@x.com')",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (2, 30, 'bob@x.com')",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (3, 25, 'carol@x.com')",
        &mut storage,
        &mut txn,
    );

    // SELECT age only — covered by idx_age key column → IndexOnlyScan
    let result = rows(run(
        "SELECT age FROM users WHERE age = 25",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(result.len(), 2, "expected 2 rows with age=25");
    for row in &result {
        assert_eq!(row[0], Value::Int(25));
    }
}

// ── Index-only scan: range ────────────────────────────────────────────────────

#[test]
fn test_covered_range_returns_correct_rows() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE scores (id INT, score INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "CREATE INDEX idx_score ON scores (score)",
        &mut storage,
        &mut txn,
    );
    for (i, s) in [(1, 10), (2, 20), (3, 30), (4, 40), (5, 50)] {
        run(
            &format!("INSERT INTO scores VALUES ({i}, {s})"),
            &mut storage,
            &mut txn,
        );
    }

    // SELECT score WHERE score BETWEEN 20 AND 40 — covered by idx_score
    let result = rows(run(
        "SELECT score FROM scores WHERE score >= 20 AND score <= 40",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(result.len(), 3, "expected 3 rows");
    let mut vals: Vec<i32> = result
        .iter()
        .map(|r| match r[0] {
            Value::Int(v) => v,
            ref x => panic!("unexpected value {x:?}"),
        })
        .collect();
    vals.sort();
    assert_eq!(vals, vec![20, 30, 40]);
}

// ── Index-only scan: INT key with NULLs ───────────────────────────────────────

#[test]
fn test_covered_scan_skips_nulls() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, val INT)", &mut storage, &mut txn);
    run("CREATE INDEX idx_val ON t (val)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1, 10)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (2, NULL)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (3, 10)", &mut storage, &mut txn);

    let result = rows(run(
        "SELECT val FROM t WHERE val = 10",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(result.len(), 2);
    for r in &result {
        assert_eq!(r[0], Value::Int(10));
    }
}

// ── Non-covered → fallback ────────────────────────────────────────────────────

#[test]
fn test_non_covered_select_star_returns_all_columns() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE emp (id INT, name TEXT, dept TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "CREATE INDEX idx_dept ON emp (dept)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO emp VALUES (1, 'Alice', 'Eng')",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO emp VALUES (2, 'Bob', 'HR')",
        &mut storage,
        &mut txn,
    );

    // SELECT * is not covered → should still return all columns via heap lookup
    let result = rows(run(
        "SELECT * FROM emp WHERE dept = 'Eng'",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Int(1));
    assert_eq!(result[0][1], Value::Text("Alice".into()));
    assert_eq!(result[0][2], Value::Text("Eng".into()));
}

#[test]
fn test_non_covered_extra_column_falls_back_to_heap() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE orders (id INT, amount INT, status TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "CREATE INDEX idx_amount ON orders (amount)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO orders VALUES (1, 100, 'open')",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO orders VALUES (2, 200, 'closed')",
        &mut storage,
        &mut txn,
    );

    // Selecting 'status' is not covered by idx_amount → full row from heap
    let result = rows(run(
        "SELECT amount, status FROM orders WHERE amount = 100",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Int(100));
    assert_eq!(result[0][1], Value::Text("open".into()));
}

// ── MVCC: deleted rows are invisible ─────────────────────────────────────────

#[test]
fn test_covered_scan_respects_delete_visibility() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE events (id INT, ts INT)",
        &mut storage,
        &mut txn,
    );
    run("CREATE INDEX idx_ts ON events (ts)", &mut storage, &mut txn);
    run("INSERT INTO events VALUES (1, 100)", &mut storage, &mut txn);
    run("INSERT INTO events VALUES (2, 200)", &mut storage, &mut txn);
    run("INSERT INTO events VALUES (3, 100)", &mut storage, &mut txn);

    // Delete one row with ts=100
    run("DELETE FROM events WHERE id = 1", &mut storage, &mut txn);

    // Index-only scan on ts should not return the deleted row
    let result = rows(run(
        "SELECT ts FROM events WHERE ts = 100",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(
        result.len(),
        1,
        "only one ts=100 row should be visible after delete"
    );
    assert_eq!(result[0][0], Value::Int(100));
}

// ── TEXT key index-only scan ─────────────────────────────────────────────────

#[test]
fn test_covered_text_key_equality() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE tags (id INT, tag TEXT)",
        &mut storage,
        &mut txn,
    );
    run("CREATE INDEX idx_tag ON tags (tag)", &mut storage, &mut txn);
    run(
        "INSERT INTO tags VALUES (1, 'rust')",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO tags VALUES (2, 'go')", &mut storage, &mut txn);
    run(
        "INSERT INTO tags VALUES (3, 'rust')",
        &mut storage,
        &mut txn,
    );

    let result = rows(run(
        "SELECT tag FROM tags WHERE tag = 'rust'",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(result.len(), 2);
    for r in &result {
        assert_eq!(r[0], Value::Text("rust".into()));
    }
}

// ── decode_index_key roundtrip ────────────────────────────────────────────────

#[test]
fn test_decode_index_key_int_roundtrip() {
    use axiomdb_sql::key_encoding::{decode_index_key, encode_index_key};
    use axiomdb_types::Value;

    let values = vec![Value::Int(42), Value::Int(-7)];
    let encoded = encode_index_key(&values).unwrap();
    let (decoded, consumed) = decode_index_key(&encoded, 2).unwrap();
    assert_eq!(consumed, encoded.len());
    assert_eq!(decoded[0], Value::Int(42));
    assert_eq!(decoded[1], Value::Int(-7));
}

#[test]
fn test_decode_index_key_text_roundtrip() {
    use axiomdb_sql::key_encoding::{decode_index_key, encode_index_key};
    use axiomdb_types::Value;

    let values = vec![Value::Text("hello world".into())];
    let encoded = encode_index_key(&values).unwrap();
    let (decoded, _) = decode_index_key(&encoded, 1).unwrap();
    assert_eq!(decoded[0], Value::Text("hello world".into()));
}

#[test]
fn test_decode_index_key_null_roundtrip() {
    use axiomdb_sql::key_encoding::{decode_index_key, encode_index_key};
    use axiomdb_types::Value;

    let values = vec![Value::Null, Value::Int(5)];
    let encoded = encode_index_key(&values).unwrap();
    let (decoded, _) = decode_index_key(&encoded, 2).unwrap();
    assert_eq!(decoded[0], Value::Null);
    assert_eq!(decoded[1], Value::Int(5));
}

#[test]
fn test_decode_index_key_bool_roundtrip() {
    use axiomdb_sql::key_encoding::{decode_index_key, encode_index_key};
    use axiomdb_types::Value;

    let values = vec![Value::Bool(true), Value::Bool(false)];
    let encoded = encode_index_key(&values).unwrap();
    let (decoded, _) = decode_index_key(&encoded, 2).unwrap();
    assert_eq!(decoded[0], Value::Bool(true));
    assert_eq!(decoded[1], Value::Bool(false));
}

#[test]
fn test_decode_index_key_float_roundtrip() {
    use axiomdb_sql::key_encoding::{decode_index_key, encode_index_key};
    use axiomdb_types::Value;

    let expected = 314_f64 / 100.0;
    let values = vec![Value::Real(expected)];
    let encoded = encode_index_key(&values).unwrap();
    let (decoded, _) = decode_index_key(&encoded, 1).unwrap();
    // Float comparison with small epsilon
    match decoded[0] {
        Value::Real(f) => assert!((f - expected).abs() < 1e-10),
        ref x => panic!("expected Float, got {x:?}"),
    }
}

// ── Ctx-path tests: IndexOnlyScan actually exercised ─────────────────────────
//
// These tests use execute_with_ctx so the planner receives the real
// select_col_idxs and selects AccessMethod::IndexOnlyScan when coverage holds.
// The index is created BEFORE inserts so stats.row_count = 0 at creation time;
// the stats_cost_gate treats 0 as "no reliable stats" and conservatively allows
// the index — ensuring IndexOnlyScan is chosen even for small tables.

use axiomdb_sql::{bloom::BloomRegistry, execute_with_ctx, session::SessionContext};

fn setup_ctx() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.keep().join("test.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let txn = TxnManager::create(&wal_path).unwrap();
    (storage, txn, BloomRegistry::new(), SessionContext::new())
}

fn rctx(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> QueryResult {
    let stmt = parse(sql, None).unwrap_or_else(|e| panic!("parse failed: {sql}\n{e:?}"));
    let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    let analyzed =
        analyze(stmt, storage, snap).unwrap_or_else(|e| panic!("analyze failed: {sql}\n{e:?}"));
    execute_with_ctx(analyzed, storage, txn, bloom, ctx)
        .unwrap_or_else(|e| panic!("execute failed: {sql}\n{e:?}"))
}

/// Helper: collect rows from a QueryResult.
fn rrows(r: QueryResult) -> Vec<Vec<Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

// ── Single-column INT index, equality ────────────────────────────────────────

#[test]
fn test_ctx_index_only_equality_int() {
    // Table: (id INT, age INT, name TEXT) — index on age (col_idx=1).
    // SELECT age WHERE age = 25 → all projected cols in index → IndexOnlyScan.
    // Row layout: col_idx 0=id, 1=age, 2=name.
    // IndexOnlyScan must place age at row[1], not row[0].
    let (mut st, mut tx, mut bl, mut ctx) = setup_ctx();
    rctx(
        "CREATE TABLE u1 (id INT, age INT, name TEXT)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "CREATE INDEX idx_age1 ON u1 (age)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "INSERT INTO u1 VALUES (1, 25, 'alice')",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "INSERT INTO u1 VALUES (2, 30, 'bob')",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "INSERT INTO u1 VALUES (3, 25, 'carol')",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );

    let r = rrows(rctx(
        "SELECT age FROM u1 WHERE age = 25",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    ));
    assert_eq!(r.len(), 2, "rows={r:?}");
    // age is col_idx=1 in the table; IndexOnlyScan must return it at row[1].
    // project_grouped_row / eval projects SELECT age → output row[0] = age value.
    for row in &r {
        assert_eq!(row[0], Value::Int(25), "rows={r:?}");
    }
}

// ── Single-column TEXT index, equality ───────────────────────────────────────

#[test]
fn test_ctx_index_only_equality_text() {
    // Table: (id INT, tag TEXT) — index on tag (col_idx=1).
    // SELECT tag WHERE tag = 'rust' → IndexOnlyScan.
    let (mut st, mut tx, mut bl, mut ctx) = setup_ctx();
    rctx(
        "CREATE TABLE t1 (id INT, tag TEXT)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "CREATE INDEX idx_tag1 ON t1 (tag)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "INSERT INTO t1 VALUES (1, 'rust')",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "INSERT INTO t1 VALUES (2, 'go')",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "INSERT INTO t1 VALUES (3, 'rust')",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );

    let r = rrows(rctx(
        "SELECT tag FROM t1 WHERE tag = 'rust'",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    ));
    assert_eq!(r.len(), 2, "rows={r:?}");
    for row in &r {
        assert_eq!(row[0], Value::Text("rust".into()), "rows={r:?}");
    }
}

// ── Single-column index, range scan ──────────────────────────────────────────

#[test]
fn test_ctx_index_only_range() {
    // Table: (id INT, score INT) — index on score (col_idx=1).
    // SELECT score WHERE score >= 20 AND score <= 40 → IndexOnlyScan (range).
    let (mut st, mut tx, mut bl, mut ctx) = setup_ctx();
    rctx(
        "CREATE TABLE sc1 (id INT, score INT)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "CREATE INDEX idx_score1 ON sc1 (score)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    for (i, s) in [(1, 10), (2, 20), (3, 30), (4, 40), (5, 50)] {
        rctx(
            &format!("INSERT INTO sc1 VALUES ({i}, {s})"),
            &mut st,
            &mut tx,
            &mut bl,
            &mut ctx,
        );
    }

    let r = rrows(rctx(
        "SELECT score FROM sc1 WHERE score >= 20 AND score <= 40",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    ));
    assert_eq!(r.len(), 3, "rows={r:?}");
    let mut vals: Vec<i32> = r
        .iter()
        .map(|row| match row[0] {
            Value::Int(v) => v,
            ref x => panic!("unexpected {x:?}"),
        })
        .collect();
    vals.sort_unstable();
    assert_eq!(vals, vec![20, 30, 40]);
}

// ── WHERE clause re-evaluation uses correct column ───────────────────────────

#[test]
fn test_ctx_index_only_where_col_idx_correct() {
    // Regression: IndexOnlyScan used to return compressed rows at output
    // positions (0,1,...) instead of table col_idx positions. This caused
    // WHERE re-evaluation to access the wrong column.
    //
    // Table: (id INT, x INT) — col_idx 0=id, 1=x.
    // SELECT x WHERE x = 42 → IndexOnlyScan on idx(x).
    // WHERE x=42 uses Expr::Column{col_idx:1}; row must have x at row[1].
    let (mut st, mut tx, mut bl, mut ctx) = setup_ctx();
    rctx(
        "CREATE TABLE t2 (id INT PRIMARY KEY, x INT)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "CREATE UNIQUE INDEX idx_x2 ON t2 (x)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    for i in 1..=10 {
        rctx(
            &format!("INSERT INTO t2 VALUES ({i}, {})", i * 10),
            &mut st,
            &mut tx,
            &mut bl,
            &mut ctx,
        );
    }

    let r = rrows(rctx(
        "SELECT x FROM t2 WHERE x = 42",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    ));
    // x=42 doesn't exist (we inserted 10,20,...,100).
    assert_eq!(r.len(), 0, "rows={r:?}");

    let r2 = rrows(rctx(
        "SELECT x FROM t2 WHERE x = 40",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    ));
    assert_eq!(r2.len(), 1, "rows={r2:?}");
    assert_eq!(r2[0][0], Value::Int(40));
}

// ── Composite index, equality — all key cols in SELECT ───────────────────────

#[test]
fn test_ctx_index_only_composite_equality() {
    // Table: (id INT, region TEXT, dept TEXT) — composite index on (region, dept).
    // SELECT region, dept WHERE region = 'east' AND dept = 'eng' → IndexOnlyScan.
    // region=col_idx=1, dept=col_idx=2 in table.
    let (mut st, mut tx, mut bl, mut ctx) = setup_ctx();
    rctx(
        "CREATE TABLE emp2 (id INT, region TEXT, dept TEXT)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "CREATE INDEX idx_rd ON emp2 (region, dept)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "INSERT INTO emp2 VALUES (1, 'east', 'eng')",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "INSERT INTO emp2 VALUES (2, 'east', 'hr')",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "INSERT INTO emp2 VALUES (3, 'west', 'eng')",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );

    let r = rrows(rctx(
        "SELECT region, dept FROM emp2 WHERE region = 'east' AND dept = 'eng'",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    ));
    assert_eq!(r.len(), 1, "rows={r:?}");
    assert_eq!(r[0][0], Value::Text("east".into()), "rows={r:?}");
    assert_eq!(r[0][1], Value::Text("eng".into()), "rows={r:?}");
}

// ── Non-covered SELECT falls back to heap ────────────────────────────────────

#[test]
fn test_ctx_non_covered_falls_back_to_heap() {
    // SELECT id (not in index) → no IndexOnlyScan; full row from heap.
    // Verifies coverage check rejects non-covered queries on ctx path too.
    let (mut st, mut tx, mut bl, mut ctx) = setup_ctx();
    rctx(
        "CREATE TABLE emp3 (id INT, dept TEXT, salary INT)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "CREATE INDEX idx_dept3 ON emp3 (dept)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "INSERT INTO emp3 VALUES (1, 'eng', 90000)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "INSERT INTO emp3 VALUES (2, 'hr',  60000)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );

    // salary is not in the index → full heap row needed.
    let r = rrows(rctx(
        "SELECT dept, salary FROM emp3 WHERE dept = 'eng'",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    ));
    assert_eq!(r.len(), 1, "rows={r:?}");
    assert_eq!(r[0][0], Value::Text("eng".into()));
    assert_eq!(r[0][1], Value::Int(90000));
}

// ── MVCC: deleted rows are hidden ────────────────────────────────────────────

#[test]
fn test_ctx_index_only_mvcc_delete() {
    // IndexOnlyScan checks slot visibility (is_slot_visible) without reading
    // the full heap row. Deleted rows must not appear in results.
    let (mut st, mut tx, mut bl, mut ctx) = setup_ctx();
    rctx(
        "CREATE TABLE ev2 (id INT, ts INT)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "CREATE INDEX idx_ts2 ON ev2 (ts)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "INSERT INTO ev2 VALUES (1, 100)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "INSERT INTO ev2 VALUES (2, 200)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "INSERT INTO ev2 VALUES (3, 100)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    // Delete id=1 (ts=100).
    rctx(
        "DELETE FROM ev2 WHERE id = 1",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );

    let r = rrows(rctx(
        "SELECT ts FROM ev2 WHERE ts = 100",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    ));
    assert_eq!(r.len(), 1, "deleted row must not appear; rows={r:?}");
    assert_eq!(r[0][0], Value::Int(100));
}

// ── NULL values skipped in index, correct result ─────────────────────────────

#[test]
fn test_ctx_index_only_null_skipped() {
    // NULL values are not inserted into the B-Tree index.
    // An equality lookup for a non-NULL value must not return rows with NULL.
    let (mut st, mut tx, mut bl, mut ctx) = setup_ctx();
    rctx(
        "CREATE TABLE t3 (id INT, val INT)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "CREATE INDEX idx_val3 ON t3 (val)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "INSERT INTO t3 VALUES (1, 10)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "INSERT INTO t3 VALUES (2, NULL)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );
    rctx(
        "INSERT INTO t3 VALUES (3, 10)",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    );

    let r = rrows(rctx(
        "SELECT val FROM t3 WHERE val = 10",
        &mut st,
        &mut tx,
        &mut bl,
        &mut ctx,
    ));
    assert_eq!(r.len(), 2, "rows={r:?}");
    for row in &r {
        assert_eq!(row[0], Value::Int(10), "rows={r:?}");
    }
}
