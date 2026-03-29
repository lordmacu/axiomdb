//! Integration tests for bulk and indexed DELETE apply paths.

mod common;

use axiomdb_core::error::DbError;
use axiomdb_sql::QueryResult;
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

// ── Bulk DELETE / TRUNCATE fast-path tests (Phase 5.16) ──────────────────────

fn setup_pk_table(storage: &mut MemoryStorage, txn: &mut TxnManager) {
    run(
        "CREATE TABLE items (id INT NOT NULL, label TEXT)",
        storage,
        txn,
    );
    for i in 1..=5 {
        run(
            &format!("INSERT INTO items VALUES ({i}, 'item{i}')"),
            storage,
            txn,
        );
    }
}

#[test]
fn test_bulk_delete_pk_table_returns_correct_count() {
    let (mut storage, mut txn) = setup();
    setup_pk_table(&mut storage, &mut txn);
    let result = run("DELETE FROM items", &mut storage, &mut txn);
    match result {
        QueryResult::Affected { count, .. } => assert_eq!(count, 5),
        other => panic!("expected Affected, got {other:?}"),
    }
}

#[test]
fn test_bulk_delete_leaves_table_empty() {
    let (mut storage, mut txn) = setup();
    setup_pk_table(&mut storage, &mut txn);
    run("DELETE FROM items", &mut storage, &mut txn);
    let rows = rows(run("SELECT * FROM items", &mut storage, &mut txn));
    assert!(rows.is_empty(), "table must be empty after bulk DELETE");
}

#[test]
fn test_bulk_delete_allows_reinsert_same_pk() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT NOT NULL)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1)", &mut storage, &mut txn);
    run("DELETE FROM t", &mut storage, &mut txn);
    // Reinserting the same PK must succeed — no stale index entry.
    let result = run_result("INSERT INTO t VALUES (1)", &mut storage, &mut txn);
    assert!(
        result.is_ok(),
        "reinsert after bulk DELETE must succeed: {result:?}"
    );
}

#[test]
fn test_truncate_indexed_table_allows_reinsert() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT NOT NULL, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO t VALUES (1, 'Alice')", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (2, 'Bob')", &mut storage, &mut txn);
    run("TRUNCATE TABLE t", &mut storage, &mut txn);
    let rows_after = rows(run("SELECT * FROM t", &mut storage, &mut txn));
    assert!(rows_after.is_empty(), "table must be empty after TRUNCATE");
    // Must be able to reinsert — old index roots are gone.
    let result = run_result("INSERT INTO t VALUES (1, 'Carol')", &mut storage, &mut txn);
    assert!(
        result.is_ok(),
        "reinsert after TRUNCATE must succeed: {result:?}"
    );
}

#[test]
fn test_bulk_delete_then_rollback_restores_data() {
    let (mut storage, mut txn) = setup();
    setup_pk_table(&mut storage, &mut txn);
    run("BEGIN", &mut storage, &mut txn);
    run("DELETE FROM items", &mut storage, &mut txn);
    let count_inside = rows(run("SELECT * FROM items", &mut storage, &mut txn)).len();
    assert_eq!(count_inside, 0, "inside txn: table must appear empty");
    run("ROLLBACK", &mut storage, &mut txn);
    let count_after = rows(run("SELECT * FROM items", &mut storage, &mut txn)).len();
    assert_eq!(
        count_after, 5,
        "after ROLLBACK: original data must be restored"
    );
}

#[test]
fn test_bulk_delete_savepoint_rollback_restores_data() {
    // Tests TxnManager savepoint API directly (SQL SAVEPOINT is Phase 7.12).
    let (mut storage, mut txn) = setup();
    setup_pk_table(&mut storage, &mut txn);
    txn.begin().unwrap(); // explicit BEGIN
                          // Insert item6 before the savepoint.
    run(
        "INSERT INTO items VALUES (6, 'item6')",
        &mut storage,
        &mut txn,
    );
    let sp = txn.savepoint();
    // Bulk delete inside the transaction.
    run("DELETE FROM items", &mut storage, &mut txn);
    assert_eq!(
        rows(run("SELECT * FROM items", &mut storage, &mut txn)).len(),
        0,
        "inside txn after DELETE: must be empty"
    );
    // Rollback to savepoint — restores pre-delete state (items 1-5 + item6).
    txn.rollback_to_savepoint(sp, &mut storage).unwrap();
    let count = rows(run("SELECT * FROM items", &mut storage, &mut txn)).len();
    assert_eq!(
        count, 6,
        "after savepoint rollback: must see items 1–5 + item6"
    );
    txn.commit().unwrap();
}

#[test]
fn test_truncate_resets_auto_increment_bulk_path() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT AUTO_INCREMENT NOT NULL, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO t (name) VALUES ('a')", &mut storage, &mut txn);
    run("INSERT INTO t (name) VALUES ('b')", &mut storage, &mut txn);
    run("TRUNCATE TABLE t", &mut storage, &mut txn);
    run("INSERT INTO t (name) VALUES ('c')", &mut storage, &mut txn);
    let rows_result = rows(run("SELECT id FROM t", &mut storage, &mut txn));
    assert_eq!(rows_result.len(), 1);
    // After TRUNCATE + bulk path, auto-increment must reset — id should be 1.
    assert_eq!(
        rows_result[0][0],
        axiomdb_types::Value::Int(1),
        "AUTO_INCREMENT must reset to 1 after TRUNCATE (bulk path)"
    );
}

#[test]
fn test_truncate_parent_fk_table_fails() {
    let (mut storage, mut txn) = setup();
    // Parent must have PRIMARY KEY so FK enforcement can find the index.
    run(
        "CREATE TABLE parent (id INT NOT NULL, PRIMARY KEY (id))",
        &mut storage,
        &mut txn,
    );
    run(
        "CREATE TABLE child (id INT NOT NULL, parent_id INT NOT NULL REFERENCES parent(id))",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO parent VALUES (1)", &mut storage, &mut txn);
    run("INSERT INTO child VALUES (1, 1)", &mut storage, &mut txn);
    let result = run_result("TRUNCATE TABLE parent", &mut storage, &mut txn);
    assert!(
        matches!(result, Err(DbError::ForeignKeyParentViolation { .. })),
        "TRUNCATE of parent FK table must fail: {result:?}"
    );
    // Table still has data.
    assert_eq!(
        rows(run("SELECT * FROM parent", &mut storage, &mut txn)).len(),
        1
    );
}

#[test]
fn test_delete_parent_fk_table_uses_slow_path() {
    // DELETE on a parent-FK table must still use the row-by-row path (which
    // enforces RESTRICT semantics), not the bulk-empty fast path.
    // Uses execute_with_ctx since FK enforcement is most reliable through the ctx path.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE parent (id INT NOT NULL, PRIMARY KEY (id))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE child (id INT NOT NULL, parent_id INT NOT NULL REFERENCES parent(id) ON DELETE RESTRICT)",
        &mut storage, &mut txn, &mut bloom, &mut ctx,
    ).unwrap();
    run_ctx(
        "INSERT INTO parent VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO child VALUES (1, 1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // DELETE on parent while child references it must fail with FK violation.
    let result = run_ctx(
        "DELETE FROM parent",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        matches!(result, Err(DbError::ForeignKeyParentViolation { .. })),
        "DELETE on parent with referencing child must fail: {result:?}"
    );
}

// ── Indexed DELETE WHERE fast path tests (Phase 6.3b) ────────────────────────

#[test]
fn test_delete_where_pk_equality_uses_indexed_path() {
    // DELETE FROM t WHERE id = 5 on a table with PRIMARY KEY (id).
    // After deletion, id=5 must be gone and all others intact.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE pk_del (id INT NOT NULL, name TEXT, PRIMARY KEY (id))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    for i in 1..=10i32 {
        run_ctx(
            &format!("INSERT INTO pk_del VALUES ({i}, 'row{i}')"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
    }

    let result = run_ctx(
        "DELETE FROM pk_del WHERE id = 5",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(
        result,
        QueryResult::Affected {
            count: 1,
            last_insert_id: None
        }
    );

    // id=5 must be gone.
    let r5 = run_ctx(
        "SELECT id FROM pk_del WHERE id = 5",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    if let QueryResult::Rows { rows, .. } = r5 {
        assert!(rows.is_empty(), "id=5 should be deleted");
    } else {
        panic!("expected Rows");
    }
    // Other rows intact.
    for i in [1, 2, 3, 4, 6, 7, 8, 9, 10i32] {
        let r = run_ctx(
            &format!("SELECT id FROM pk_del WHERE id = {i}"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
        if let QueryResult::Rows { rows, .. } = r {
            assert_eq!(rows.len(), 1, "row {i} should still exist");
        } else {
            panic!("expected Rows");
        }
    }
}

#[test]
fn test_delete_where_pk_range_deletes_correct_rows() {
    // DELETE FROM t WHERE id > 7 — should delete ids 8,9,10 only.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE pk_range_del (id INT NOT NULL, PRIMARY KEY (id))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    for i in 1..=10i32 {
        run_ctx(
            &format!("INSERT INTO pk_range_del VALUES ({i})"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
    }

    let result = run_ctx(
        "DELETE FROM pk_range_del WHERE id > 7",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(
        result,
        QueryResult::Affected {
            count: 3,
            last_insert_id: None
        }
    );

    // Rows 1-7 intact.
    for i in 1..=7i32 {
        let r = run_ctx(
            &format!("SELECT id FROM pk_range_del WHERE id = {i}"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
        if let QueryResult::Rows { rows, .. } = r {
            assert_eq!(rows.len(), 1, "row {i} should exist");
        } else {
            panic!("expected Rows");
        }
    }
    // Rows 8-10 gone.
    for i in 8..=10i32 {
        let r = run_ctx(
            &format!("SELECT id FROM pk_range_del WHERE id = {i}"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
        if let QueryResult::Rows { rows, .. } = r {
            assert!(rows.is_empty(), "row {i} should be deleted");
        } else {
            panic!("expected Rows");
        }
    }
}

#[test]
fn test_delete_where_secondary_index_maintains_all_indexes() {
    // DELETE using a secondary index must still maintain all indexes correctly.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE sec_del (id INT NOT NULL, score INT, PRIMARY KEY (id))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE INDEX idx_score ON sec_del (score)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    for i in 1..=5i32 {
        run_ctx(
            &format!("INSERT INTO sec_del VALUES ({i}, {})", i * 10),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
    }

    run_ctx(
        "DELETE FROM sec_del WHERE score = 30",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // id=3 (score=30) must be gone; others intact.
    let r = run_ctx(
        "SELECT id FROM sec_del",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    if let QueryResult::Rows { rows, .. } = r {
        let ids: Vec<_> = rows.iter().map(|r| r[0].clone()).collect();
        assert!(
            !ids.contains(&axiomdb_types::Value::Int(3)),
            "id=3 must be deleted"
        );
        assert_eq!(ids.len(), 4, "4 rows should remain");
    }

    // Reinserting the deleted key must succeed (no stale index entry).
    run_ctx(
        "INSERT INTO sec_del VALUES (3, 30)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
}

#[test]
fn test_update_batch_deletes_old_pk_and_secondary_keys() {
    // Phase 5.19: UPDATE must batch-delete old keys from both the PRIMARY KEY
    // and secondary indexes before reinserting new keys.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE upd_batch (id INT NOT NULL, score INT, PRIMARY KEY (id))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE INDEX idx_upd_score ON upd_batch (score)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    for i in 1..=10i32 {
        run_ctx(
            &format!("INSERT INTO upd_batch VALUES ({i}, {})", i * 10),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
    }

    let result = run_ctx(
        "UPDATE upd_batch SET id = id + 100, score = score + 1 WHERE id <= 5",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 5);

    for i in 1..=5i32 {
        let old_pk = rows(
            run_ctx(
                &format!("SELECT id FROM upd_batch WHERE id = {i}"),
                &mut storage,
                &mut txn,
                &mut bloom,
                &mut ctx,
            )
            .unwrap(),
        );
        assert!(old_pk.is_empty(), "old PK value {i} should be gone");

        let new_id = i + 100;
        let new_pk = rows(
            run_ctx(
                &format!("SELECT id, score FROM upd_batch WHERE id = {new_id}"),
                &mut storage,
                &mut txn,
                &mut bloom,
                &mut ctx,
            )
            .unwrap(),
        );
        assert_eq!(
            new_pk,
            vec![vec![Value::Int(new_id), Value::Int(i * 10 + 1)]],
            "new PK value {new_id} should exist with the updated score",
        );

        let old_score = i * 10;
        let old_score_rows = rows(
            run_ctx(
                &format!("SELECT id FROM upd_batch WHERE score = {old_score}"),
                &mut storage,
                &mut txn,
                &mut bloom,
                &mut ctx,
            )
            .unwrap(),
        );
        assert!(
            old_score_rows.is_empty(),
            "old secondary-index key {old_score} should be gone",
        );

        let new_score = old_score + 1;
        let new_score_rows = rows(
            run_ctx(
                &format!("SELECT id FROM upd_batch WHERE score = {new_score}"),
                &mut storage,
                &mut txn,
                &mut bloom,
                &mut ctx,
            )
            .unwrap(),
        );
        assert_eq!(
            new_score_rows,
            vec![vec![Value::Int(new_id)]],
            "new secondary-index key {new_score} should resolve to the new PK",
        );
    }

    let untouched = rows(
        run_ctx(
            "SELECT id, score FROM upd_batch WHERE id = 6",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(untouched, vec![vec![Value::Int(6), Value::Int(60)]]);
}

#[test]
fn test_delete_where_non_sargable_falls_back_to_scan() {
    // DELETE with a non-indexable predicate must still delete the right rows.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE fallback_del (id INT NOT NULL, label TEXT, PRIMARY KEY (id))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    for i in 1..=6i32 {
        run_ctx(
            &format!("INSERT INTO fallback_del VALUES ({i}, 'x{i}')"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
    }
    // LIKE is not sargable → must fall back to scan but still be correct.
    let result = run_ctx(
        "DELETE FROM fallback_del WHERE label LIKE 'x%3%' OR id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    if let QueryResult::Affected { count, .. } = result {
        assert!(count >= 1, "at least 1 row deleted");
    }
}

#[test]
fn test_delete_where_indexed_then_reinsert_same_pk() {
    // After indexed DELETE, reinserting the same PK must succeed.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE reins_del (id INT NOT NULL, val INT, PRIMARY KEY (id))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    for i in 1..=5i32 {
        run_ctx(
            &format!("INSERT INTO reins_del VALUES ({i}, {i})"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
    }
    run_ctx(
        "DELETE FROM reins_del WHERE id = 3",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Must succeed — no stale unique constraint.
    run_ctx(
        "INSERT INTO reins_del VALUES (3, 99)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let r = run_ctx(
        "SELECT val FROM reins_del WHERE id = 3",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    if let QueryResult::Rows { rows, .. } = r {
        assert_eq!(rows[0][0], axiomdb_types::Value::Int(99));
    }
}

#[test]
fn test_update_noop_counts_matched_rows_but_leaves_data_unchanged() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE noop_upd (id INT NOT NULL, score INT, PRIMARY KEY (id))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO noop_upd VALUES (1, 10), (2, 20), (3, 30)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "UPDATE noop_upd SET score = score WHERE id <= 2",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 2);

    let rows = rows(
        run_ctx(
            "SELECT id, score FROM noop_upd ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(1), Value::Int(10)],
            vec![Value::Int(2), Value::Int(20)],
            vec![Value::Int(3), Value::Int(30)],
        ]
    );
}
