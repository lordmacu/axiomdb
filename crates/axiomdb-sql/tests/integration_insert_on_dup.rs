//! Integration tests for `INSERT ... ON DUPLICATE KEY UPDATE` (MySQL ODKU).
//!
//! Every acceptance criterion in
//! `specs/fase-gap-audit/spec-insert-on-duplicate-key-update.md`.

mod common;

use axiomdb_core::error::DbError;
use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    setup_ctx()
}

fn affected(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> u64 {
    match run_ctx(sql, storage, txn, bloom, ctx).unwrap() {
        QueryResult::Affected { count, .. } => count,
        other => panic!("expected Affected, got {other:?}"),
    }
}

fn rows_of(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Vec<Vec<Value>> {
    let QueryResult::Rows { rows, .. } = run_ctx(sql, storage, txn, bloom, ctx).unwrap() else {
        panic!("expected Rows");
    };
    rows
}

// ── Parsing ────────────────────────────────────────────────────────────────-

#[test]
fn odku_parses_after_all_source_forms() {
    for sql in [
        "INSERT INTO t VALUES (1) ON DUPLICATE KEY UPDATE v = 1",
        "INSERT INTO t (id) VALUES (1) ON DUPLICATE KEY UPDATE v = 2",
        "INSERT INTO t SET id = 1 ON DUPLICATE KEY UPDATE v = 3",
        "INSERT INTO t DEFAULT VALUES ON DUPLICATE KEY UPDATE v = 4",
        "INSERT INTO t SELECT 1 ON DUPLICATE KEY UPDATE v = 5",
        "INSERT IGNORE INTO t VALUES (1) ON DUPLICATE KEY UPDATE v = 6",
    ] {
        let stmt = axiomdb_sql::parse(sql, None)
            .unwrap_or_else(|e| panic!("parse failed for {sql:?}: {e}"));
        match stmt {
            axiomdb_sql::ast::Stmt::Insert(s) => assert!(
                s.on_duplicate_update.is_some(),
                "expected ODKU for {sql:?}, got {s:?}",
            ),
            other => panic!("expected Insert for {sql:?}, got {other:?}"),
        }
    }
}

#[test]
fn odku_on_replace_is_rejected() {
    let err = axiomdb_sql::parse(
        "REPLACE INTO t VALUES (1) ON DUPLICATE KEY UPDATE v = 1",
        None,
    )
    .expect_err("REPLACE + ODKU must fail to parse");
    assert!(
        format!("{err}").contains("mutually exclusive"),
        "unexpected error: {err}",
    );
}

// ── No-conflict path ───────────────────────────────────────────────────────-

#[test]
fn odku_no_conflict_behaves_as_insert() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE t (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE UNIQUE INDEX idx_id ON t (id)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let n = affected(
        "INSERT INTO t VALUES (1, 10) ON DUPLICATE KEY UPDATE v = 99",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(n, 1);
    let rows = rows_of(
        "SELECT id, v FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][1], Value::Int(10) | Value::BigInt(10)));
}

// ── Conflict on PK → UPDATE branch ─────────────────────────────────────────-

#[test]
fn odku_on_pk_conflict_updates_in_place() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE t (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE UNIQUE INDEX idx_id ON t (id)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let n = affected(
        "INSERT INTO t VALUES (1, 99) ON DUPLICATE KEY UPDATE v = 777",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(n, 2, "PK conflict + UPDATE branch: MySQL reports 2");
    let rows = rows_of(
        "SELECT id, v FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][1], Value::Int(777) | Value::BigInt(777)));
}

// ── VALUES(col) pseudo-function ───────────────────────────────────────────-

#[test]
fn odku_values_pseudo_fn_resolves_to_proposed_row() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE t (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE UNIQUE INDEX idx_id ON t (id)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // VALUES(v) = 50 (proposed), existing v = 10, new should be 50 + 1 = 51.
    let n = affected(
        "INSERT INTO t VALUES (1, 50) ON DUPLICATE KEY UPDATE v = VALUES(v) + 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(n, 2);
    let rows = rows_of(
        "SELECT v FROM t WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(matches!(rows[0][0], Value::Int(51) | Value::BigInt(51)));
}

// VALUES(col) outside ODKU must not be recognized as the pseudo-function.
#[test]
fn odku_values_outside_odku_is_not_recognized() {
    // `SELECT VALUES(col)` — the parser doesn't accept VALUES as an
    // expression; it's a statement-level keyword. Just confirm the
    // statement fails to parse.
    let err = axiomdb_sql::parse("SELECT VALUES(col)", None)
        .expect_err("VALUES(...) outside ODKU must not parse as the ODKU pseudo-function");
    let _ = err;
}

// ── Update leaves row unchanged → affected = 0 ─────────────────────────────-

#[test]
fn odku_update_unchanged_is_zero_affected() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE t (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE UNIQUE INDEX idx_id ON t (id)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // UPDATE sets v = 10 (same as existing).
    let n = affected(
        "INSERT INTO t VALUES (1, 99) ON DUPLICATE KEY UPDATE v = 10",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(n, 0, "MySQL: 0 when UPDATE leaves values unchanged");
}

// ── Non-PK UNIQUE + composite ──────────────────────────────────────────────-

#[test]
fn odku_on_non_pk_unique_conflict() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE t (id INT, email TEXT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE UNIQUE INDEX idx_email ON t (email)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 'a@x', 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let n = affected(
        "INSERT INTO t VALUES (2, 'a@x', 20) ON DUPLICATE KEY UPDATE v = 99",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(n, 2);
}

#[test]
fn odku_on_composite_unique_conflict() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE t (a INT, b INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE UNIQUE INDEX idx_ab ON t (a, b)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 10, 100)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let n = affected(
        "INSERT INTO t VALUES (1, 10, 200) ON DUPLICATE KEY UPDATE v = VALUES(v)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(n, 2);
    let rows = rows_of(
        "SELECT v FROM t WHERE a = 1 AND b = 10",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(matches!(rows[0][0], Value::Int(200) | Value::BigInt(200)));
}

// ── NULL in unique — no conflict ───────────────────────────────────────────-

#[test]
fn odku_null_in_unique_is_insert() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE t (id INT, email TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE UNIQUE INDEX idx_email ON t (email)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let n = affected(
        "INSERT INTO t VALUES (2, NULL) ON DUPLICATE KEY UPDATE id = 99",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(n, 1, "NULL never conflicts — behaves as INSERT");
}

// ── Counter increment pattern (the canonical ODKU use case) ─────────────────

#[test]
fn odku_counter_increment_pattern() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE hits (page TEXT, n INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE UNIQUE INDEX idx_page ON hits (page)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // First hit → insert (affected=1).
    let n1 = affected(
        "INSERT INTO hits VALUES ('/a', 1) ON DUPLICATE KEY UPDATE n = n + 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(n1, 1);

    // Second hit → update (affected=2). Counter goes 1 → 2.
    let n2 = affected(
        "INSERT INTO hits VALUES ('/a', 1) ON DUPLICATE KEY UPDATE n = n + 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(n2, 2);

    let rows = rows_of(
        "SELECT n FROM hits WHERE page = '/a'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(matches!(rows[0][0], Value::Int(2) | Value::BigInt(2)));
}

// ── Batch VALUES mixed conflict/insert ─────────────────────────────────────-

#[test]
fn odku_batch_mixed_insert_and_update() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE t (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE UNIQUE INDEX idx_id ON t (id)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Row 1 → conflict, UPDATE, changed → +2
    // Row 2 → no conflict, INSERT → +1
    let n = affected(
        "INSERT INTO t VALUES (1, 100), (2, 20) ON DUPLICATE KEY UPDATE v = VALUES(v)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(n, 3);
    let rows = rows_of(
        "SELECT id, v FROM t ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(rows.len(), 2);
}

// ── Clustered rejection (MVP scope) ────────────────────────────────────────-

#[test]
fn odku_on_clustered_returns_not_implemented() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let err = run_ctx(
        "INSERT INTO t VALUES (1, 99) ON DUPLICATE KEY UPDATE v = 99",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("clustered ODKU must error cleanly in MVP");
    assert!(matches!(err, DbError::NotImplemented { .. }));
}

// ── INSERT IGNORE + ODKU coexist ────────────────────────────────────────────

#[test]
fn odku_with_ignore_prefix_works() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE t (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE UNIQUE INDEX idx_id ON t (id)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let n = affected(
        "INSERT IGNORE INTO t VALUES (1, 99) ON DUPLICATE KEY UPDATE v = 42",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(n, 2);
    let rows = rows_of(
        "SELECT v FROM t WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(matches!(rows[0][0], Value::Int(42) | Value::BigInt(42)));
}

// ── SELECT source ───────────────────────────────────────────────────────────

#[test]
fn odku_from_select_source() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE t (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE UNIQUE INDEX idx_id ON t (id)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE src (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO src VALUES (1, 500), (2, 600)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // Row id=1 conflicts → UPDATE (v → 500, +2); row id=2 is INSERT (+1).
    let n = affected(
        "INSERT INTO t SELECT id, v FROM src ON DUPLICATE KEY UPDATE v = VALUES(v)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(n, 3);
}

// ── FK child validation on UPDATE branch ───────────────────────────────────-

#[test]
fn odku_fk_child_validation_on_updated_row() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE parent (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE UNIQUE INDEX pk_parent ON parent (id)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO parent VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE child (id INT, pid INT, FOREIGN KEY (pid) REFERENCES parent(id))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE UNIQUE INDEX pk_child ON child (id)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO child VALUES (100, 1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Updating child.pid to 999 via ODKU must violate FK.
    let err = run_ctx(
        "INSERT INTO child VALUES (100, 1) ON DUPLICATE KEY UPDATE pid = 999",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("FK violation on update branch must fail");
    assert!(
        matches!(err, DbError::ForeignKeyViolation { .. }),
        "unexpected error: {err:?}",
    );
}
