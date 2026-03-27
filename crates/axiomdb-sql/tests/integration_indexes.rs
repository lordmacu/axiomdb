//! Integration tests for secondary indexes (Phase 6.1–6.3).
//!
//! Tests cover:
//! - CREATE INDEX populates B-Tree from existing data
//! - Planner uses index for WHERE col = literal
//! - Planner uses index for range scan
//! - Index maintenance on INSERT
//! - Index maintenance on DELETE
//! - Index maintenance on UPDATE
//! - UNIQUE index violation
//! - NULL in UNIQUE index (allowed)
//! - DROP INDEX frees pages

use axiomdb_catalog::CatalogBootstrap;
use axiomdb_core::error::DbError;
use axiomdb_sql::{analyze, execute, parse, QueryResult};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

// ── Helpers ───────────────────────────────────────────────────────────────────

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

fn setup() -> (MemoryStorage, TxnManager) {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.keep().join("test.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let txn = TxnManager::create(&wal_path).unwrap();
    (storage, txn)
}

fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[test]
fn test_create_index_on_existing_data() {
    let (mut storage, mut txn) = setup();

    // Create table and insert rows before creating index.
    run("BEGIN", &mut storage, &mut txn);
    run(
        "CREATE TABLE users (id INT, email TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (1, 'alice@example.com')",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (2, 'bob@example.com')",
        &mut storage,
        &mut txn,
    );
    run("COMMIT", &mut storage, &mut txn);

    // Create index AFTER data is already there.
    run("BEGIN", &mut storage, &mut txn);
    run(
        "CREATE INDEX users_id_idx ON users (id)",
        &mut storage,
        &mut txn,
    );
    run("COMMIT", &mut storage, &mut txn);

    // Query should return both rows via full scan (planner uses index only for equality).
    run("BEGIN", &mut storage, &mut txn);
    let result = rows(run("SELECT id, email FROM users", &mut storage, &mut txn));
    assert_eq!(result.len(), 2);
    run("COMMIT", &mut storage, &mut txn);
}

#[test]
fn test_planner_uses_index_for_equality() {
    let (mut storage, mut txn) = setup();

    run("BEGIN", &mut storage, &mut txn);
    run(
        "CREATE TABLE products (id INT, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO products VALUES (1, 'apple')",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO products VALUES (2, 'banana')",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO products VALUES (3, 'cherry')",
        &mut storage,
        &mut txn,
    );
    run(
        "CREATE INDEX products_id_idx ON products (id)",
        &mut storage,
        &mut txn,
    );
    run("COMMIT", &mut storage, &mut txn);

    run("BEGIN", &mut storage, &mut txn);
    let result = rows(run(
        "SELECT id, name FROM products WHERE id = 2",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Int(2));
    assert_eq!(result[0][1], Value::Text("banana".into()));
    run("COMMIT", &mut storage, &mut txn);
}

#[test]
fn test_planner_index_lookup_no_match() {
    let (mut storage, mut txn) = setup();

    run("BEGIN", &mut storage, &mut txn);
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1)", &mut storage, &mut txn);
    run("CREATE INDEX t_id_idx ON t (id)", &mut storage, &mut txn);
    run("COMMIT", &mut storage, &mut txn);

    run("BEGIN", &mut storage, &mut txn);
    let result = rows(run(
        "SELECT id FROM t WHERE id = 99",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(result.len(), 0);
    run("COMMIT", &mut storage, &mut txn);
}

#[test]
fn test_primary_key_lookup_without_secondary_index() {
    let (mut storage, mut txn) = setup();

    run("BEGIN", &mut storage, &mut txn);
    run(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (1, 'alice'), (2, 'bob'), (3, 'carol')",
        &mut storage,
        &mut txn,
    );
    run("COMMIT", &mut storage, &mut txn);

    run("BEGIN", &mut storage, &mut txn);
    let result = rows(run(
        "SELECT id, name FROM users WHERE id = 2",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(result, vec![vec![Value::Int(2), Value::Text("bob".into())]]);
    run("COMMIT", &mut storage, &mut txn);
}

#[test]
fn test_index_maintained_on_insert() {
    let (mut storage, mut txn) = setup();

    run("BEGIN", &mut storage, &mut txn);
    run(
        "CREATE TABLE scores (id INT, score INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "CREATE INDEX scores_id_idx ON scores (id)",
        &mut storage,
        &mut txn,
    );
    run("COMMIT", &mut storage, &mut txn);

    // Insert rows after index is created.
    run("BEGIN", &mut storage, &mut txn);
    run(
        "INSERT INTO scores VALUES (10, 100)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO scores VALUES (20, 200)",
        &mut storage,
        &mut txn,
    );
    run("COMMIT", &mut storage, &mut txn);

    // Index lookup should find row inserted after index creation.
    run("BEGIN", &mut storage, &mut txn);
    let result = rows(run(
        "SELECT score FROM scores WHERE id = 10",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Int(100));
    run("COMMIT", &mut storage, &mut txn);
}

#[test]
fn test_multi_row_insert_with_primary_key_only_table() {
    let (mut storage, mut txn) = setup();

    run("BEGIN", &mut storage, &mut txn);
    run(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (1, 'alice'), (2, 'bob'), (3, 'carol')",
        &mut storage,
        &mut txn,
    );
    run("COMMIT", &mut storage, &mut txn);

    run("BEGIN", &mut storage, &mut txn);
    let result = rows(run(
        "SELECT id, name FROM users ORDER BY id",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(
        result,
        vec![
            vec![Value::Int(1), Value::Text("alice".into())],
            vec![Value::Int(2), Value::Text("bob".into())],
            vec![Value::Int(3), Value::Text("carol".into())],
        ]
    );
    run("COMMIT", &mut storage, &mut txn);
}

#[test]
fn test_multi_row_insert_unique_secondary_duplicate_in_same_statement_errors() {
    let (mut storage, mut txn) = setup();

    run("BEGIN", &mut storage, &mut txn);
    run(
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "CREATE UNIQUE INDEX users_email_idx ON users (email)",
        &mut storage,
        &mut txn,
    );
    run("COMMIT", &mut storage, &mut txn);

    run("BEGIN", &mut storage, &mut txn);
    let err = run_result(
        "INSERT INTO users VALUES (1, 'alice@x.com'), (2, 'alice@x.com')",
        &mut storage,
        &mut txn,
    );
    assert!(
        matches!(err, Err(DbError::UniqueViolation { .. })),
        "expected UniqueViolation, got: {err:?}"
    );
    run("ROLLBACK", &mut storage, &mut txn);

    run("BEGIN", &mut storage, &mut txn);
    let result = rows(run("SELECT id FROM users", &mut storage, &mut txn));
    assert!(
        result.is_empty(),
        "failing multi-row INSERT must not leak rows"
    );
    run("COMMIT", &mut storage, &mut txn);
}

#[test]
fn test_index_maintained_on_delete() {
    let (mut storage, mut txn) = setup();

    run("BEGIN", &mut storage, &mut txn);
    run("CREATE TABLE items (id INT)", &mut storage, &mut txn);
    run("INSERT INTO items VALUES (1)", &mut storage, &mut txn);
    run("INSERT INTO items VALUES (2)", &mut storage, &mut txn);
    run(
        "CREATE INDEX items_id_idx ON items (id)",
        &mut storage,
        &mut txn,
    );
    run("COMMIT", &mut storage, &mut txn);

    // Delete row 1.
    run("BEGIN", &mut storage, &mut txn);
    run("DELETE FROM items WHERE id = 1", &mut storage, &mut txn);
    run("COMMIT", &mut storage, &mut txn);

    // Index lookup for deleted row should return empty.
    run("BEGIN", &mut storage, &mut txn);
    let result = rows(run(
        "SELECT id FROM items WHERE id = 1",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(result.len(), 0, "deleted row should not be found via index");

    // Row 2 should still be findable.
    let result = rows(run(
        "SELECT id FROM items WHERE id = 2",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(result.len(), 1);
    run("COMMIT", &mut storage, &mut txn);
}

#[test]
fn test_index_maintained_on_update() {
    let (mut storage, mut txn) = setup();

    run("BEGIN", &mut storage, &mut txn);
    run(
        "CREATE TABLE kv (key INT, val TEXT)",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO kv VALUES (1, 'old')", &mut storage, &mut txn);
    run(
        "CREATE INDEX kv_key_idx ON kv (key)",
        &mut storage,
        &mut txn,
    );
    run("COMMIT", &mut storage, &mut txn);

    // Update: key 1 → 99.
    run("BEGIN", &mut storage, &mut txn);
    run(
        "UPDATE kv SET key = 99 WHERE val = 'old'",
        &mut storage,
        &mut txn,
    );
    run("COMMIT", &mut storage, &mut txn);

    // Old key should be gone.
    run("BEGIN", &mut storage, &mut txn);
    let result = rows(run(
        "SELECT val FROM kv WHERE key = 1",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(
        result.len(),
        0,
        "old key should be removed from index after update"
    );

    // New key should be findable.
    let result = rows(run(
        "SELECT val FROM kv WHERE key = 99",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Text("old".into()));
    run("COMMIT", &mut storage, &mut txn);
}

#[test]
fn test_update_primary_key_range_path_updates_only_matching_rows() {
    let (mut storage, mut txn) = setup();

    run("BEGIN", &mut storage, &mut txn);
    run(
        "CREATE TABLE scores (id INT PRIMARY KEY, score INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO scores VALUES (1, 10), (2, 20), (3, 30), (4, 40), (5, 50), (6, 60)",
        &mut storage,
        &mut txn,
    );
    run("COMMIT", &mut storage, &mut txn);

    run("BEGIN", &mut storage, &mut txn);
    run(
        "UPDATE scores SET score = score + 5 WHERE id >= 3 AND id < 6",
        &mut storage,
        &mut txn,
    );
    run("COMMIT", &mut storage, &mut txn);

    run("BEGIN", &mut storage, &mut txn);
    let result = rows(run(
        "SELECT id, score FROM scores ORDER BY id",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(
        result,
        vec![
            vec![Value::Int(1), Value::Int(10)],
            vec![Value::Int(2), Value::Int(20)],
            vec![Value::Int(3), Value::Int(35)],
            vec![Value::Int(4), Value::Int(45)],
            vec![Value::Int(5), Value::Int(55)],
            vec![Value::Int(6), Value::Int(60)],
        ]
    );
    run("COMMIT", &mut storage, &mut txn);
}

#[test]
fn test_update_secondary_index_candidate_path_updates_matching_rows() {
    let (mut storage, mut txn) = setup();

    run("BEGIN", &mut storage, &mut txn);
    run(
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT, score INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "CREATE UNIQUE INDEX users_email_idx ON users (email)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (1, 'alice@x.com', 10), (2, 'bob@x.com', 20)",
        &mut storage,
        &mut txn,
    );
    run("COMMIT", &mut storage, &mut txn);

    run("BEGIN", &mut storage, &mut txn);
    run(
        "UPDATE users SET score = score + 7 WHERE email = 'alice@x.com'",
        &mut storage,
        &mut txn,
    );
    run("COMMIT", &mut storage, &mut txn);

    run("BEGIN", &mut storage, &mut txn);
    let result = rows(run(
        "SELECT id, score FROM users ORDER BY id",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(
        result,
        vec![
            vec![Value::Int(1), Value::Int(17)],
            vec![Value::Int(2), Value::Int(20)],
        ]
    );
    run("COMMIT", &mut storage, &mut txn);
}

#[test]
fn test_unique_index_violation() {
    let (mut storage, mut txn) = setup();

    run("BEGIN", &mut storage, &mut txn);
    run("CREATE TABLE members (email TEXT)", &mut storage, &mut txn);
    run(
        "CREATE UNIQUE INDEX members_email_uniq ON members (email)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO members VALUES ('alice@example.com')",
        &mut storage,
        &mut txn,
    );
    run("COMMIT", &mut storage, &mut txn);

    // Second insert with same email should fail.
    run("BEGIN", &mut storage, &mut txn);
    let err = run_result(
        "INSERT INTO members VALUES ('alice@example.com')",
        &mut storage,
        &mut txn,
    );
    assert!(
        matches!(err, Err(DbError::UniqueViolation { .. })),
        "expected UniqueViolation, got: {err:?}"
    );
    run("ROLLBACK", &mut storage, &mut txn);
}

#[test]
fn test_null_in_unique_index_allowed() {
    let (mut storage, mut txn) = setup();

    run("BEGIN", &mut storage, &mut txn);
    run("CREATE TABLE nullable_key (k INT)", &mut storage, &mut txn);
    run(
        "CREATE UNIQUE INDEX nullable_key_idx ON nullable_key (k)",
        &mut storage,
        &mut txn,
    );
    // Two NULLs in a UNIQUE column are allowed (NULL ≠ NULL in SQL).
    run(
        "INSERT INTO nullable_key VALUES (NULL)",
        &mut storage,
        &mut txn,
    );
    run("COMMIT", &mut storage, &mut txn);

    run("BEGIN", &mut storage, &mut txn);
    run(
        "INSERT INTO nullable_key VALUES (NULL)",
        &mut storage,
        &mut txn,
    );
    run("COMMIT", &mut storage, &mut txn);

    run("BEGIN", &mut storage, &mut txn);
    let result = rows(run("SELECT k FROM nullable_key", &mut storage, &mut txn));
    assert_eq!(result.len(), 2);
    run("COMMIT", &mut storage, &mut txn);
}

#[test]
fn test_drop_index() {
    let (mut storage, mut txn) = setup();

    run("BEGIN", &mut storage, &mut txn);
    run("CREATE TABLE foo (x INT)", &mut storage, &mut txn);
    run("INSERT INTO foo VALUES (1)", &mut storage, &mut txn);
    run("CREATE INDEX foo_x_idx ON foo (x)", &mut storage, &mut txn);
    run("COMMIT", &mut storage, &mut txn);

    // Drop index.
    run("BEGIN", &mut storage, &mut txn);
    run("DROP INDEX foo_x_idx ON foo", &mut storage, &mut txn);
    run("COMMIT", &mut storage, &mut txn);

    // Query should still work (falls back to full scan).
    run("BEGIN", &mut storage, &mut txn);
    let result = rows(run("SELECT x FROM foo WHERE x = 1", &mut storage, &mut txn));
    assert_eq!(result.len(), 1);
    run("COMMIT", &mut storage, &mut txn);
}

#[test]
fn test_create_index_duplicate_name_error() {
    let (mut storage, mut txn) = setup();

    run("BEGIN", &mut storage, &mut txn);
    run("CREATE TABLE bar (x INT)", &mut storage, &mut txn);
    run("CREATE INDEX bar_x_idx ON bar (x)", &mut storage, &mut txn);
    run("COMMIT", &mut storage, &mut txn);

    run("BEGIN", &mut storage, &mut txn);
    let err = run_result("CREATE INDEX bar_x_idx ON bar (x)", &mut storage, &mut txn);
    assert!(
        matches!(err, Err(DbError::IndexAlreadyExists { .. })),
        "expected IndexAlreadyExists, got: {err:?}"
    );
    run("ROLLBACK", &mut storage, &mut txn);
}

#[test]
fn test_text_index_lookup() {
    let (mut storage, mut txn) = setup();

    run("BEGIN", &mut storage, &mut txn);
    run(
        "CREATE TABLE names (id INT, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO names VALUES (1, 'alice')",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO names VALUES (2, 'bob')",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO names VALUES (3, 'charlie')",
        &mut storage,
        &mut txn,
    );
    run(
        "CREATE INDEX names_name_idx ON names (name)",
        &mut storage,
        &mut txn,
    );
    run("COMMIT", &mut storage, &mut txn);

    run("BEGIN", &mut storage, &mut txn);
    let result = rows(run(
        "SELECT id FROM names WHERE name = 'bob'",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Int(2));
    run("COMMIT", &mut storage, &mut txn);
}

// ── Phase 5.21 — staged flush preserves index correctness ─────────────────────

use axiomdb_sql::{bloom::BloomRegistry, execute_with_ctx, SessionContext};

fn run_ctx_idx(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> QueryResult {
    let stmt = parse(sql, None).unwrap();
    let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    let analyzed = analyze(stmt, storage, snap).unwrap();
    execute_with_ctx(analyzed, storage, txn, bloom, ctx)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
}

fn setup_ctx_idx() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.keep().join("test.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let txn = TxnManager::create(&wal_path).unwrap();
    (storage, txn, BloomRegistry::new(), SessionContext::new())
}

/// Rows staged via the 5.21 path are findable through a secondary index after commit.
#[test]
fn test_5_21_staged_flush_secondary_index_correct() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx_idx();

    run_ctx_idx(
        "CREATE TABLE sidxt (id INT, name VARCHAR(64))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    run_ctx_idx(
        "CREATE INDEX idx_name ON sidxt (name)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    run_ctx_idx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    for i in 1..=50 {
        run_ctx_idx(
            &format!("INSERT INTO sidxt VALUES ({i}, 'name_{i}')"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
    }
    run_ctx_idx("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx);

    // Index lookup must find the row.
    let r = run_ctx_idx(
        "SELECT id FROM sidxt WHERE name = 'name_25'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    if let axiomdb_sql::QueryResult::Rows { rows, .. } = r {
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Int(25));
    } else {
        panic!("expected Rows");
    }
}

/// PK (UNIQUE) index stays correct after a staged batch flush.
#[test]
fn test_5_21_staged_flush_pk_index_correct() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx_idx();

    run_ctx_idx(
        "CREATE TABLE pkt (id INT UNIQUE, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    run_ctx_idx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    for i in 1..=30 {
        run_ctx_idx(
            &format!("INSERT INTO pkt VALUES ({i}, {i})"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
    }
    run_ctx_idx("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx);

    // All 30 rows accessible; no phantom rows.
    let r = run_ctx_idx(
        "SELECT COUNT(*) FROM pkt",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    if let QueryResult::Rows { rows, .. } = r {
        assert_eq!(rows[0][0], Value::BigInt(30));
    }

    // Inserting a duplicate after commit correctly fails.
    run_ctx_idx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    let e = {
        let stmt = parse("INSERT INTO pkt VALUES (1, 999)", None).unwrap();
        let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
        let analyzed = analyze(stmt, &storage, snap).unwrap();
        execute_with_ctx(analyzed, &mut storage, &mut txn, &mut bloom, &mut ctx)
            .expect_err("expected UniqueViolation")
    };
    assert!(
        matches!(e, DbError::UniqueViolation { .. }),
        "wrong error: {e:?}"
    );
    run_ctx_idx("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut ctx);
}
