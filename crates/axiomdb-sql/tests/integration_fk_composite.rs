//! Integration tests for composite (multi-column) foreign keys (GAP-C.2).
//!
//! Scoped to CREATE TABLE + INSERT child validation. UPDATE child and
//! parent-side cascade paths are known follow-up work.

mod common;

use axiomdb_core::error::DbError;
use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    // Parent: composite PK (a, b).
    run_ctx(
        "CREATE TABLE parent (a INT, b INT, label TEXT, PRIMARY KEY (a, b))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO parent VALUES (1, 10, 'p1'), (2, 20, 'p2')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    (storage, txn, bloom, ctx)
}

#[test]
fn composite_fk_insert_child_success() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    // Child needs a pre-declared index on (pa, pb) — auto-creation of
    // composite FK indexes is a future follow-up.
    run_ctx(
        "CREATE TABLE child (\
            id INT PRIMARY KEY, pa INT, pb INT, \
            FOREIGN KEY (pa, pb) REFERENCES parent (a, b)\
        )",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("composite FK must require a child index");

    // Declare the child composite index first, then add FK via ALTER TABLE
    // (parser already supports this).
    run_ctx(
        "CREATE TABLE child (id INT PRIMARY KEY, pa INT, pb INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE INDEX idx_child_pa_pb ON child (pa, pb)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "ALTER TABLE child ADD CONSTRAINT fk_pair FOREIGN KEY (pa, pb) REFERENCES parent (a, b)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Matching tuple → accepted.
    run_ctx(
        "INSERT INTO child VALUES (100, 1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT id FROM child",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows");
    };
    assert_eq!(rows.len(), 1);
}

#[test]
fn composite_fk_insert_child_mismatched_tuple_is_rejected() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE child (id INT PRIMARY KEY, pa INT, pb INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE INDEX idx_child_pa_pb ON child (pa, pb)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "ALTER TABLE child ADD CONSTRAINT fk_pair FOREIGN KEY (pa, pb) REFERENCES parent (a, b)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // (1, 20): pa=1 exists in parent, pb=20 exists in parent, but (1,20) is
    // NOT a parent tuple. Single-column FK check would incorrectly pass;
    // composite tuple check must reject.
    let err = run_ctx(
        "INSERT INTO child VALUES (200, 1, 20)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("mismatched composite tuple must violate FK");
    assert!(
        matches!(err, DbError::ForeignKeyViolation { .. }),
        "expected FK violation, got {err:?}",
    );
}

#[test]
fn composite_fk_insert_with_null_passes_match_simple() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE child (id INT PRIMARY KEY, pa INT, pb INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE INDEX idx_child_pa_pb ON child (pa, pb)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "ALTER TABLE child ADD CONSTRAINT fk_pair FOREIGN KEY (pa, pb) REFERENCES parent (a, b)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // MATCH SIMPLE: any NULL column skips the FK check.
    run_ctx(
        "INSERT INTO child VALUES (300, NULL, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT id FROM child WHERE id = 300",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!();
    };
    assert_eq!(rows.len(), 1);
    let _ = Value::Null; // silence import if unused
}
