//! Phase 24.4 — CITEXT case-insensitive text type.
//!
//! CITEXT stores text verbatim (original case preserved on read) but
//! all equality / index lookups go through the `utf8mb4_unicode_ci`
//! collation, so `WHERE col = 'value'` matches case-insensitively.

mod common;

use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    setup_ctx()
}

fn run_ok(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> QueryResult {
    run_ctx(sql, storage, txn, bloom, ctx)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
}

fn rows(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Vec<Vec<Value>> {
    match run_ok(sql, storage, txn, bloom, ctx) {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn citext_equality_is_case_insensitive() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ok(
        "CREATE TABLE u (id INT, email CITEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    run_ok(
        "INSERT INTO u VALUES (1, 'Alice@Example.com')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    let r = rows(
        "SELECT id FROM u WHERE email = 'alice@example.com'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(r.len(), 1, "lowercase query should match");
    assert_eq!(r[0][0], Value::Int(1));

    let r = rows(
        "SELECT id FROM u WHERE email = 'ALICE@EXAMPLE.COM'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(r.len(), 1, "uppercase query should match");
}

#[test]
fn citext_preserves_original_case_on_read() {
    // CITEXT compares case-insensitively but stores the original case
    // verbatim — PostgreSQL semantics.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ok(
        "CREATE TABLE u (id INT, email CITEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    run_ok(
        "INSERT INTO u VALUES (1, 'Alice@Example.com')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let r = rows(
        "SELECT email FROM u",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(r[0][0], Value::Text("Alice@Example.com".into()));
}

#[test]
fn citext_not_equal_is_case_insensitive() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ok(
        "CREATE TABLE u (id INT, email CITEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    run_ok(
        "INSERT INTO u VALUES (1, 'Alice@example.com'), (2, 'bob@example.com')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let r = rows(
        "SELECT id FROM u WHERE email <> 'ALICE@EXAMPLE.COM' ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(r.len(), 1, "<> should exclude case-equal match");
    assert_eq!(r[0][0], Value::Int(2));
}

#[test]
fn citext_explicit_collate_wins() {
    // If the user explicitly specifies COLLATE binary, that overrides
    // the CITEXT default and makes equality case-sensitive again.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ok(
        "CREATE TABLE u (id INT, email CITEXT COLLATE utf8mb4_bin)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    run_ok(
        "INSERT INTO u VALUES (1, 'Alice@Example.com')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let r = rows(
        "SELECT id FROM u WHERE email = 'alice@example.com'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(r.len(), 0, "binary collation should not match");
}

#[test]
fn plain_text_with_explicit_ci_collate_also_works() {
    // The fix in eval/ops.rs (text_eq for Eq/NotEq) means plain TEXT
    // with explicit COLLATE = utf8mb4_unicode_ci now also gets
    // case-insensitive equality — CITEXT is just sugar for this.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ok(
        "CREATE TABLE u (id INT, email TEXT COLLATE utf8mb4_unicode_ci)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    run_ok(
        "INSERT INTO u VALUES (1, 'Alice@Example.com')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let r = rows(
        "SELECT id FROM u WHERE email = 'alice@example.com'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(r.len(), 1);
}
