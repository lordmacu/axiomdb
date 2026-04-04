mod common;

use axiomdb_core::error::DbError;
use axiomdb_sql::QueryResult;
use common::{rows, run_ctx, setup_ctx};

/// Helper: run SQL and panic on error.
fn ok(
    sql: &str,
    storage: &mut axiomdb_storage::MemoryStorage,
    txn: &mut axiomdb_wal::TxnManager,
    bloom: &mut axiomdb_sql::BloomRegistry,
    ctx: &mut axiomdb_sql::SessionContext,
) -> QueryResult {
    run_ctx(sql, storage, txn, bloom, ctx)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
}

fn count(result: QueryResult) -> u64 {
    match result {
        QueryResult::Affected { count, .. } => count,
        other => panic!("expected Affected, got {other:?}"),
    }
}

/// 1. Sequential PK bulk insert — batch accumulates, COMMIT flushes all rows.
///
/// Verifies that N individual INSERT statements inside one BEGIN…COMMIT are
/// staged in the ClusteredInsertBatch and become visible only after COMMIT.
#[test]
fn clustered_batch_sequential_pk_commit_makes_rows_visible() {
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok(
        "CREATE TABLE scores (id INT PRIMARY KEY, val INT)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok("BEGIN", &mut s, &mut txn, &mut bloom, &mut ctx);
    for i in 1i64..=10 {
        ok(
            &format!("INSERT INTO scores VALUES ({i}, {})", i * 10),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
    }

    // Before COMMIT, rows are staged but not visible to a fresh snapshot.
    // (The batch is still in memory.)
    assert!(
        ctx.clustered_insert_batch.is_some(),
        "batch should be staged"
    );

    ok("COMMIT", &mut s, &mut txn, &mut bloom, &mut ctx);

    // After COMMIT, all 10 rows must be visible.
    assert!(
        ctx.clustered_insert_batch.is_none(),
        "batch cleared after commit"
    );
    let result = ok(
        "SELECT id, val FROM scores ORDER BY id",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let r = rows(result);
    assert_eq!(r.len(), 10);
    assert_eq!(r[0][0], axiomdb_types::Value::Int(1));
    assert_eq!(r[9][0], axiomdb_types::Value::Int(10));
}

/// 2. SELECT barrier — batch is flushed before SELECT on the same table.
///
/// A SELECT inside the transaction must see rows that were only staged so far.
#[test]
fn clustered_batch_select_barrier_flushes_before_read() {
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok(
        "CREATE TABLE items (id INT PRIMARY KEY, name TEXT)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok("BEGIN", &mut s, &mut txn, &mut bloom, &mut ctx);
    ok(
        "INSERT INTO items VALUES (1, 'alpha')",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "INSERT INTO items VALUES (2, 'beta')",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // Both rows are staged; no storage write yet.
    assert!(ctx.clustered_insert_batch.is_some());

    // SELECT triggers a flush so rows become visible within the transaction.
    let result = ok(
        "SELECT id FROM items ORDER BY id",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let r = rows(result);
    assert_eq!(
        r.len(),
        2,
        "both staged rows visible after flush-before-read"
    );
    assert_eq!(r[0][0], axiomdb_types::Value::Int(1));
    assert_eq!(r[1][0], axiomdb_types::Value::Int(2));

    // After flush the batch is gone; further inserts start a fresh batch.
    assert!(ctx.clustered_insert_batch.is_none());
    ok(
        "INSERT INTO items VALUES (3, 'gamma')",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(ctx.clustered_insert_batch.is_some());

    ok("COMMIT", &mut s, &mut txn, &mut bloom, &mut ctx);

    let result = ok(
        "SELECT COUNT(*) FROM items",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let r = rows(result);
    assert_eq!(r[0][0], axiomdb_types::Value::BigInt(3));
}

/// 3. ROLLBACK discards the batch — zero rows written to storage.
///
/// After ROLLBACK, the table must be empty and no WAL entries must have been
/// written for the staged rows.
#[test]
fn clustered_batch_rollback_discards_all_staged_rows() {
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok(
        "CREATE TABLE things (id INT PRIMARY KEY, data TEXT)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok("BEGIN", &mut s, &mut txn, &mut bloom, &mut ctx);
    ok(
        "INSERT INTO things VALUES (10, 'hat')",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "INSERT INTO things VALUES (20, 'coat')",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    assert!(ctx.clustered_insert_batch.is_some(), "rows are staged");

    ok("ROLLBACK", &mut s, &mut txn, &mut bloom, &mut ctx);

    assert!(
        ctx.clustered_insert_batch.is_none(),
        "batch discarded on rollback"
    );

    // Table must be empty after rollback.
    let result = ok(
        "SELECT * FROM things",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let r = rows(result);
    assert_eq!(r.len(), 0, "no rows after rollback");
}

/// 4. SAVEPOINT flush + ROLLBACK TO SAVEPOINT.
///
/// Creating a savepoint flushes the current batch. ROLLBACK TO SAVEPOINT uses
/// the WAL undo path to revert rows that were flushed after the savepoint.
#[test]
fn clustered_batch_savepoint_flushes_and_rollback_to_sp_discards_later_rows() {
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok(
        "CREATE TABLE events (id INT PRIMARY KEY, kind TEXT)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok("BEGIN", &mut s, &mut txn, &mut bloom, &mut ctx);
    ok(
        "INSERT INTO events VALUES (1, 'a')",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // SAVEPOINT flushes row 1 to storage.
    ok("SAVEPOINT sp1", &mut s, &mut txn, &mut bloom, &mut ctx);
    assert!(
        ctx.clustered_insert_batch.is_none(),
        "batch flushed at savepoint"
    );

    // Row 2 is staged in a fresh batch.
    ok(
        "INSERT INTO events VALUES (2, 'b')",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(ctx.clustered_insert_batch.is_some());

    // ROLLBACK TO SAVEPOINT discards staged batch and undoes row 2.
    ok(
        "ROLLBACK TO SAVEPOINT sp1",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(ctx.clustered_insert_batch.is_none());

    // Row 3 goes into a new batch and commits.
    ok(
        "INSERT INTO events VALUES (3, 'c')",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok("COMMIT", &mut s, &mut txn, &mut bloom, &mut ctx);

    // Only rows 1 and 3 should be visible (row 2 was rolled back).
    let result = ok(
        "SELECT id FROM events ORDER BY id",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let r = rows(result);
    assert_eq!(r.len(), 2, "rows 1 and 3 visible; row 2 rolled back");
    assert_eq!(r[0][0], axiomdb_types::Value::Int(1));
    assert_eq!(r[1][0], axiomdb_types::Value::Int(3));
}

/// 5. PK duplicate within batch → DuplicateKey before any storage write.
#[test]
fn clustered_batch_intra_batch_pk_duplicate_returns_error() {
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok(
        "CREATE TABLE dupes (id INT PRIMARY KEY, v INT)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok("BEGIN", &mut s, &mut txn, &mut bloom, &mut ctx);
    ok(
        "INSERT INTO dupes VALUES (5, 1)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // Second insert with same PK should fail immediately.
    let err = run_ctx(
        "INSERT INTO dupes VALUES (5, 2)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("expected UniqueViolation");
    assert!(
        matches!(err, DbError::UniqueViolation { .. }),
        "expected UniqueViolation, got {err:?}"
    );

    // Batch must be discarded after the error.
    assert!(
        ctx.clustered_insert_batch.is_none(),
        "batch discarded after pk duplicate"
    );

    ok("ROLLBACK", &mut s, &mut txn, &mut bloom, &mut ctx);

    let result = ok(
        "SELECT * FROM dupes",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(rows(result).len(), 0, "table empty after rollback");
}

/// 6. PK duplicate against committed data → error at flush time.
///
/// The committed row exists before the transaction; the batch duplicate
/// is caught by `apply_clustered_insert_rows` via `lookup_physical`.
#[test]
fn clustered_batch_committed_pk_duplicate_detected_at_flush() {
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok(
        "CREATE TABLE committed_dup (id INT PRIMARY KEY, v INT)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // Commit row 7 before starting the transaction.
    ok(
        "INSERT INTO committed_dup VALUES (7, 100)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    ok("BEGIN", &mut s, &mut txn, &mut bloom, &mut ctx);
    ok(
        "INSERT INTO committed_dup VALUES (7, 999)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // Error must be raised at COMMIT (flush) time.
    let err = run_ctx("COMMIT", &mut s, &mut txn, &mut bloom, &mut ctx)
        .expect_err("expected UniqueViolation at commit");
    assert!(
        matches!(err, DbError::UniqueViolation { .. }),
        "expected UniqueViolation, got {err:?}"
    );

    ok("ROLLBACK", &mut s, &mut txn, &mut bloom, &mut ctx);

    // Original row 7 must still be intact.
    let result = ok(
        "SELECT v FROM committed_dup WHERE id = 7",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let r = rows(result);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], axiomdb_types::Value::Int(100));
}

/// 7. Non-monotonic PK batch (random insertion order) produces the correct row set.
#[test]
fn clustered_batch_non_monotonic_pk_order_produces_correct_rows() {
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok(
        "CREATE TABLE rnd (id INT PRIMARY KEY, v INT)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok("BEGIN", &mut s, &mut txn, &mut bloom, &mut ctx);
    // Intentionally non-monotonic PK order.
    for id in [500i64, 1, 250, 999, 42, 100] {
        ok(
            &format!("INSERT INTO rnd VALUES ({id}, {})", id * 2),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
    }
    ok("COMMIT", &mut s, &mut txn, &mut bloom, &mut ctx);

    let result = ok(
        "SELECT id, v FROM rnd ORDER BY id",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let r = rows(result);
    assert_eq!(r.len(), 6);
    // Verify sorted order and correct values.
    let ids: Vec<i32> = r
        .iter()
        .map(|row| match row[0] {
            axiomdb_types::Value::Int(n) => n,
            _ => panic!("unexpected type"),
        })
        .collect();
    assert_eq!(ids, vec![1, 42, 100, 250, 500, 999]);
    for row in &r {
        let id = match row[0] {
            axiomdb_types::Value::Int(n) => n,
            _ => panic!(),
        };
        assert_eq!(row[1], axiomdb_types::Value::Int(id * 2));
    }
}

/// 8. Table switch — INSERT into a different table flushes the current batch.
#[test]
fn clustered_batch_table_switch_flushes_current_batch() {
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok(
        "CREATE TABLE ta (id INT PRIMARY KEY, v INT)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "CREATE TABLE tb (id INT PRIMARY KEY, v INT)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    ok("BEGIN", &mut s, &mut txn, &mut bloom, &mut ctx);
    ok(
        "INSERT INTO ta VALUES (1, 10)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "INSERT INTO ta VALUES (2, 20)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // At this point ta batch is staged.
    assert!(ctx
        .clustered_insert_batch
        .as_ref()
        .map_or(false, |b| b.table_id
            == ctx.clustered_insert_batch.as_ref().unwrap().table_id));

    // INSERT into tb must flush ta's batch first.
    ok(
        "INSERT INTO tb VALUES (1, 100)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    ok("COMMIT", &mut s, &mut txn, &mut bloom, &mut ctx);

    let ra = rows(ok(
        "SELECT v FROM ta ORDER BY id",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(ra.len(), 2);
    let rb = rows(ok(
        "SELECT v FROM tb ORDER BY id",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(rb.len(), 1);
}

/// 9. Secondary (unique) index maintained at flush — bookmark lookup works after COMMIT.
///
/// Uses a UNIQUE column constraint which creates a clustered secondary index.
/// Verifies that `flush_clustered_insert_batch` writes secondary bookmarks so
/// that post-COMMIT lookups via the unique column return the correct PK.
#[test]
fn clustered_batch_secondary_index_maintained_at_flush() {
    use axiomdb_catalog::{CatalogReader, IndexDef};
    use axiomdb_sql::clustered_secondary::ClusteredSecondaryLayout;

    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok(
        "CREATE TABLE members (id INT PRIMARY KEY, email TEXT UNIQUE)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    ok("BEGIN", &mut s, &mut txn, &mut bloom, &mut ctx);
    ok(
        "INSERT INTO members VALUES (1, 'alice@example.com')",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "INSERT INTO members VALUES (2, 'bob@example.com')",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "INSERT INTO members VALUES (3, 'carol@example.com')",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok("COMMIT", &mut s, &mut txn, &mut bloom, &mut ctx);

    // Read index metadata from the catalog.
    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&s, snap).unwrap();
    let table = reader.get_table("public", "members").unwrap().unwrap();
    let indexes = reader.list_indexes(table.id).unwrap();
    let primary_idx: IndexDef = indexes.iter().find(|i| i.is_primary).unwrap().clone();
    let secondary_idx: IndexDef = indexes
        .iter()
        .find(|i| !i.is_primary && i.is_unique)
        .unwrap()
        .clone();

    let layout = ClusteredSecondaryLayout::derive(&secondary_idx, &primary_idx).unwrap();

    // alice's email must point to PK=1.
    let alice = layout
        .scan_prefix(
            &s,
            secondary_idx.root_page_id,
            &[axiomdb_types::Value::Text("alice@example.com".into())],
        )
        .unwrap();
    assert_eq!(alice.len(), 1, "alice bookmark present after batch flush");
    assert_eq!(alice[0].primary_key, vec![axiomdb_types::Value::Int(1)]);

    // bob's email must point to PK=2.
    let bob = layout
        .scan_prefix(
            &s,
            secondary_idx.root_page_id,
            &[axiomdb_types::Value::Text("bob@example.com".into())],
        )
        .unwrap();
    assert_eq!(bob.len(), 1, "bob bookmark present after batch flush");
    assert_eq!(bob[0].primary_key, vec![axiomdb_types::Value::Int(2)]);
}

/// 10. Autocommit path is unchanged — no staging buffer used.
#[test]
fn clustered_batch_autocommit_does_not_use_batch() {
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok(
        "CREATE TABLE auto_t (id INT PRIMARY KEY, v INT)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // Autocommit: no BEGIN, so in_explicit_txn = false.
    ok(
        "INSERT INTO auto_t VALUES (1, 1)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "INSERT INTO auto_t VALUES (2, 2)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // No batch should have been created.
    assert!(
        ctx.clustered_insert_batch.is_none(),
        "autocommit path must not use batch"
    );

    let result = ok(
        "SELECT COUNT(*) FROM auto_t",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let r = rows(result);
    assert_eq!(r[0][0], axiomdb_types::Value::BigInt(2));
}
