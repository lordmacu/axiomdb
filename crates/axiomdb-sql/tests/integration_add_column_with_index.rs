//! Regression for 4.22e follow-up: `ALTER TABLE ADD COLUMN` on a heap table
//! that already has secondary indexes must preserve index entries against
//! the post-rewrite RIDs, so index-driven lookups keep returning every row.

mod common;

use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    setup_ctx()
}

fn run_rows(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Vec<Vec<Value>> {
    let QueryResult::Rows { rows, .. } = run_ctx(sql, storage, txn, bloom, ctx).unwrap() else {
        panic!("expected rows");
    };
    rows
}

#[test]
fn add_column_on_heap_preserves_secondary_index_entries() {
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
        "CREATE INDEX idx_name ON t (name)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 'alice'), (2, 'bob'), (3, 'carol')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Add a new column — internal rewrite deletes + re-inserts every row,
    // which generates new RIDs. The index on `name` must follow.
    run_ctx(
        "ALTER TABLE t ADD COLUMN age INT DEFAULT 0",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Index-driven point lookup must still find every row.
    for name in ["alice", "bob", "carol"] {
        let sql = format!("SELECT id FROM t WHERE name = '{name}'");
        let rows = run_rows(&sql, &mut storage, &mut txn, &mut bloom, &mut ctx);
        assert_eq!(
            rows.len(),
            1,
            "index lookup for name='{name}' lost the row after ADD COLUMN",
        );
    }

    // Full scan must still return every row with the new column populated.
    let all = run_rows(
        "SELECT id, name, age FROM t ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(all.len(), 3);
    for row in &all {
        assert!(
            matches!(row[2], Value::Int(0) | Value::BigInt(0)),
            "age column must default to 0, got {:?}",
            row[2],
        );
    }
}
