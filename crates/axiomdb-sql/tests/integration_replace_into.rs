//! Integration tests for `REPLACE INTO` (MySQL upsert).
//!
//! Covers every acceptance criterion in
//! `specs/fase-gap-audit/spec-replace-into.md`.

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
    let r = run_ctx(sql, storage, txn, bloom, ctx).unwrap();
    match r {
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

// ── Acceptance #2 / #1: basic parsing + single-statement forms ──────────────

#[test]
fn replace_parses_all_source_forms() {
    for sql in [
        "REPLACE INTO t VALUES (1)",
        "REPLACE INTO t (id) VALUES (1)",
        "REPLACE INTO t SET id = 1",
        "REPLACE INTO t DEFAULT VALUES",
        "REPLACE LOW_PRIORITY INTO t VALUES (1)",
        "REPLACE DELAYED INTO t (id) VALUES (1)",
    ] {
        let stmt = axiomdb_sql::parse(sql, None)
            .unwrap_or_else(|e| panic!("parse failed for {sql:?}: {e}"));
        match stmt {
            axiomdb_sql::ast::Stmt::Insert(s) => {
                assert!(s.replace, "expected replace=true for {sql:?}, got {s:?}",)
            }
            other => panic!("expected Insert for {sql:?}, got {other:?}"),
        }
    }
}

// Acceptance #3: REPLACE IGNORE must be rejected.
#[test]
fn replace_ignore_is_a_parse_error() {
    let err = axiomdb_sql::parse("REPLACE IGNORE INTO t VALUES (1)", None)
        .expect_err("REPLACE IGNORE must fail to parse");
    let msg = format!("{err}");
    assert!(
        msg.to_ascii_lowercase().contains("replace ignore"),
        "expected explicit 'REPLACE IGNORE' message, got: {msg}",
    );
}

// The `REPLACE(...)` scalar function must keep working in expression context.
#[test]
fn replace_as_scalar_function_still_works_in_expr() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT REPLACE('abc', 'a', 'X')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!();
    };
    assert_eq!(rows.len(), 1);
    match &rows[0][0] {
        Value::Text(s) => assert_eq!(s, "Xbc"),
        other => panic!("expected Text, got {other:?}"),
    }
}

// ── Acceptance #4: no conflict → behaves as INSERT ──────────────────────────

#[test]
fn replace_no_conflict_behaves_as_insert() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE t (id INT, name TEXT)",
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
        "REPLACE INTO t VALUES (1, 'a')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(n, 1, "REPLACE of a non-conflicting row counts as INSERT");
    assert_eq!(
        rows_of(
            "SELECT id FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .len(),
        1,
    );
}

// ── Acceptance #5: PK conflict → affected == 2 ──────────────────────────────

#[test]
fn replace_on_primary_key_conflict_reports_two_affected() {
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
        "CREATE UNIQUE INDEX idx_pk ON t (id)",
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
        "REPLACE INTO t VALUES (1, 99)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(
        n, 2,
        "PK conflict replaces 1 row + inserts 1 row → affected = 2",
    );
    let rows = rows_of(
        "SELECT id, v FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][1], Value::Int(99) | Value::BigInt(99)));
}

// ── Acceptance #6: non-PK UNIQUE conflict ───────────────────────────────────

#[test]
fn replace_on_unique_index_conflict() {
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
        "REPLACE INTO t VALUES (2, 'a@x', 20)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(n, 2);
    // Only one row remains — the replacement.
    assert_eq!(
        rows_of(
            "SELECT id FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx
        )
        .len(),
        1,
    );
}

// ── Acceptance #7: composite UNIQUE conflict ────────────────────────────────

#[test]
fn replace_on_composite_unique_conflict() {
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
        "REPLACE INTO t VALUES (1, 10, 999)",
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
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], Value::Int(999) | Value::BigInt(999)));
}

// ── Acceptance #8: NULL in unique column → no conflict ──────────────────────

#[test]
fn replace_with_null_in_unique_column_does_not_displace() {
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
        "REPLACE INTO t VALUES (2, NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(n, 1, "NULL vs NULL doesn't conflict under MATCH SIMPLE");
    assert_eq!(
        rows_of(
            "SELECT id FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .len(),
        2,
    );
}

// ── Acceptance #9: multi-index conflict → all matches deleted ───────────────

#[test]
fn replace_multi_index_conflict_displaces_every_matching_row() {
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
        "CREATE UNIQUE INDEX idx_id ON t (id)",
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
    // Two *different* rows — one will be displaced on id, the other on email.
    run_ctx(
        "INSERT INTO t VALUES (1, 'a@x', 10), (2, 'b@x', 20)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // New row (id=1, email='b@x') conflicts with both existing rows.
    let n = affected(
        "REPLACE INTO t VALUES (1, 'b@x', 99)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(n, 3, "1 insert + 2 displaced rows = affected 3");
    let rows = rows_of(
        "SELECT id, email, v FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][2], Value::Int(99) | Value::BigInt(99)));
}

// ── Acceptance #10: FK cascade on displaced parent ─────────────────────────-

#[test]
fn replace_fk_cascade_on_displaced_parent() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE parent (id INT, label TEXT)",
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
        "CREATE TABLE child (\
            id INT, parent_id INT, \
            FOREIGN KEY (parent_id) REFERENCES parent(id) ON DELETE CASCADE\
         )",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO parent VALUES (1, 'old')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO child VALUES (100, 1), (101, 1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // REPLACE displaces the parent with id=1 → CASCADE should delete children.
    let n = affected(
        "REPLACE INTO parent VALUES (1, 'new')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(n, 2, "1 parent replaced → affected 2");
    let remaining = rows_of(
        "SELECT id FROM child",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(
        remaining.len(),
        0,
        "CASCADE should have deleted every child of the displaced parent",
    );
}

// ── Acceptance #11: FK RESTRICT aborts ──────────────────────────────────────

#[test]
fn replace_fk_restrict_aborts_and_preserves_state() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE parent (id INT, label TEXT)",
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
        "CREATE TABLE child (\
            id INT, parent_id INT, \
            FOREIGN KEY (parent_id) REFERENCES parent(id) ON DELETE RESTRICT\
         )",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO parent VALUES (1, 'old')",
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
    let err = run_ctx(
        "REPLACE INTO parent VALUES (1, 'new')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("RESTRICT must block REPLACE when children exist");
    assert!(
        matches!(err, DbError::ForeignKeyParentViolation { .. }),
        "unexpected error: {err:?}",
    );
    // Parent row still has the original label — no partial state.
    let rows = rows_of(
        "SELECT label FROM parent WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    match &rows[0][0] {
        Value::Text(s) => assert_eq!(s, "old"),
        other => panic!("unexpected label: {other:?}"),
    }
}

// ── Acceptance #13: clustered rejection for MVP ────────────────────────────

#[test]
fn replace_on_clustered_table_returns_not_implemented_in_mvp() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 'a')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let err = run_ctx(
        "REPLACE INTO t VALUES (1, 'b')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("clustered REPLACE must error cleanly in MVP");
    assert!(
        matches!(err, DbError::NotImplemented { .. }),
        "unexpected error: {err:?}",
    );
}

// ── Acceptance #16: batch VALUES with mixed conflict / no-conflict ─────────-

#[test]
fn replace_batch_values_mixed_conflict_and_insert() {
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
        "INSERT INTO t VALUES (1, 10), (2, 20)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // Row 1 → conflict on id=1 (affected += 2)
    // Row 2 → no conflict on id=3 (affected += 1)
    // Row 3 → conflict on id=2 (affected += 2)
    // Total affected = 5.
    let n = affected(
        "REPLACE INTO t VALUES (1, 100), (3, 30), (2, 200)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(n, 5);
    let rows = rows_of(
        "SELECT id, v FROM t ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(rows.len(), 3);
}

// ── Acceptance #14: REPLACE ... DEFAULT VALUES (no conflict path) ──────────

#[test]
fn replace_default_values_no_conflict() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE t (id INT AUTO_INCREMENT, name TEXT DEFAULT 'anon')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let n = affected(
        "REPLACE INTO t DEFAULT VALUES",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(n, 1);
    let rows = rows_of(
        "SELECT name FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(rows.len(), 1);
}

// ── Acceptance #15: REPLACE ... SET ─────────────────────────────────────────

#[test]
fn replace_set_syntax_displaces_existing() {
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
        "REPLACE INTO t SET id = 1, v = 99",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(n, 2);
}

// ── Acceptance #12: REPLACE SELECT with self-reference ─────────────────────-

#[test]
fn replace_select_self_reference_materializes_source_first() {
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
        "INSERT INTO t VALUES (1, 10), (2, 20)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // SELECT must be materialized before the displace loop starts, otherwise
    // we'd be reading and writing concurrently.
    let n = affected(
        "REPLACE INTO t SELECT id, v + 100 FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // Every row conflicts with itself → 2 inserts + 2 deletes = 4.
    assert_eq!(n, 4);
    let rows = rows_of(
        "SELECT v FROM t ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(rows.len(), 2);
    assert!(matches!(rows[0][0], Value::Int(110) | Value::BigInt(110)));
    assert!(matches!(rows[1][0], Value::Int(120) | Value::BigInt(120)));
}
