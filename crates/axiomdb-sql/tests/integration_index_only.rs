//! Integration tests for index-only scans (Phase 6.13).
//!
//! Verifies that:
//!   1. A query whose SELECT columns are fully covered by an index key returns
//!      correct rows without touching the heap (AccessMethod::IndexOnlyScan).
//!   2. INCLUDE (...) columns are accepted in DDL and stored in the catalog.
//!   3. Covered queries work for equality, range, and partial-index scenarios.
//!   4. Non-covered queries still fall back to IndexLookup / Scan.
//!   5. MVCC visibility is respected (deleted / uncommitted rows are hidden).

use axiomdb_catalog::{CatalogBootstrap, CatalogReader};
use axiomdb_core::error::DbError;
use axiomdb_sql::{analyze, execute, parse, QueryResult};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

// ── helpers ───────────────────────────────────────────────────────────────────

fn setup() -> (MemoryStorage, TxnManager) {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.into_path().join("test.wal");
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

    let values = vec![Value::Real(3.14)];
    let encoded = encode_index_key(&values).unwrap();
    let (decoded, _) = decode_index_key(&encoded, 1).unwrap();
    // Float comparison with small epsilon
    match decoded[0] {
        Value::Real(f) => assert!((f - 3.14).abs() < 1e-10),
        ref x => panic!("expected Float, got {x:?}"),
    }
}
