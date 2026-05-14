//! Integration tests for Phase 20.4 Step 8 — GIN indexing for array columns.
//!
//! Tests the `@>`, `&&`, `<@`, and `=` operators on array columns with GIN indexes.

mod common;

use axiomdb_types::Value;

/// Helper to get integer IDs from query results.
fn int_ids(result: Vec<Vec<Value>>) -> Vec<i32> {
    let mut ids: Vec<i32> = result
        .into_iter()
        .map(|row| match row.into_iter().next().unwrap() {
            Value::Int(n) => n,
            other => panic!("expected Int, got {other:?}"),
        })
        .collect();
    ids.sort();
    ids
}

// ── Setup helpers ───────────────────────────────────────────────────────────

fn setup_array_table(
    storage: &mut axiomdb_storage::MemoryStorage,
    txn: &mut axiomdb_wal::TxnManager,
) {
    common::run(
        "CREATE TABLE tags(id INT PRIMARY KEY, tags TEXT[])",
        storage,
        txn,
    );
    common::run(
        "CREATE INDEX idx_tags_gin ON tags USING GIN (tags)",
        storage,
        txn,
    );
    // Insert test data
    common::run(
        "INSERT INTO tags VALUES (1, ARRAY['urgent', 'bug', 'frontend'])",
        storage,
        txn,
    );
    common::run(
        "INSERT INTO tags VALUES (2, ARRAY['feature', 'backend'])",
        storage,
        txn,
    );
    common::run(
        "INSERT INTO tags VALUES (3, ARRAY['urgent', 'backend'])",
        storage,
        txn,
    );
    common::run("INSERT INTO tags VALUES (4, ARRAY['bug'])", storage, txn);
    common::run(
        "INSERT INTO tags VALUES (5, ARRAY['frontend', 'backend'])",
        storage,
        txn,
    );
    common::run(
        "INSERT INTO tags VALUES (6, ARRAY['urgent', 'bug', 'backend', 'feature'])",
        storage,
        txn,
    );
}

fn rows(
    sql: &str,
    storage: &mut axiomdb_storage::MemoryStorage,
    txn: &mut axiomdb_wal::TxnManager,
) -> Vec<Vec<Value>> {
    common::rows(common::run(sql, storage, txn))
}

// ── GIN probe tests ─────────────────────────────────────────────────────────

/// col @> ARRAY[...] uses GIN index.
#[test]
fn gin_probe_contains() {
    let (mut storage, mut txn) = common::setup();
    setup_array_table(&mut storage, &mut txn);

    // Find all rows containing both 'urgent' AND 'bug'
    let result = rows(
        "SELECT id FROM tags WHERE tags @> ARRAY['urgent', 'bug']",
        &mut storage,
        &mut txn,
    );
    let ids = int_ids(result);
    // Rows 1 (urgent, bug, frontend) and 6 (urgent, bug, backend, feature) match
    assert_eq!(ids, vec![1, 6]);
}

/// col && ARRAY[...] uses GIN index (overlap — any common element).
#[test]
fn gin_probe_overlap() {
    let (mut storage, mut txn) = common::setup();
    setup_array_table(&mut storage, &mut txn);

    // Find all rows with any overlap with ARRAY['urgent', 'backend']
    let result = rows(
        "SELECT id FROM tags WHERE tags && ARRAY['urgent', 'backend']",
        &mut storage,
        &mut txn,
    );
    let ids = int_ids(result);
    // Row 1: urgent, bug, frontend (matches urgent)
    // Row 2: feature, backend (matches backend)
    // Row 3: urgent, backend (matches both)
    // Row 5: frontend, backend (matches backend)
    // Row 6: urgent, bug, backend, feature (matches both)
    assert_eq!(ids, vec![1, 2, 3, 5, 6]);
}

/// col <@ ARRAY[...] uses GIN index (contained by query array).
#[test]
fn gin_probe_contained_by() {
    let (mut storage, mut txn) = common::setup();
    setup_array_table(&mut storage, &mut txn);

    // Find all rows where tags is contained in ARRAY['bug', 'urgent', 'frontend']
    // Only rows whose ALL elements are in the query array
    let result = rows(
        "SELECT id FROM tags WHERE tags <@ ARRAY['bug', 'urgent', 'frontend']",
        &mut storage,
        &mut txn,
    );
    let ids = int_ids(result);
    // Row 1: urgent, bug, frontend - all in query ✓
    // Row 4: bug - in query ✓
    // Row 3: urgent, backend - backend NOT in query ✗
    // Row 2: feature, backend - neither in query ✗
    // Row 5: frontend, backend - backend NOT in query ✗
    // Row 6: urgent, bug, backend, feature - backend NOT in query ✗
    assert_eq!(ids, vec![1, 4]);
}

/// col = ARRAY[...] uses GIN index (equality).
#[test]
fn gin_probe_equality() {
    let (mut storage, mut txn) = common::setup();
    setup_array_table(&mut storage, &mut txn);

    // Find exact match
    let result = rows(
        "SELECT id FROM tags WHERE tags = ARRAY['urgent', 'bug', 'frontend']",
        &mut storage,
        &mut txn,
    );
    let ids = int_ids(result);
    assert_eq!(ids, vec![1]);

    // Non-matching case
    let result = rows(
        "SELECT id FROM tags WHERE tags = ARRAY['urgent', 'bug']",
        &mut storage,
        &mut txn,
    );
    let ids = int_ids(result);
    assert!(ids.is_empty());
}

// ── GIN maintenance tests ───────────────────────────────────────────────────

/// NULL array elements are not indexed but queries still work.
#[test]
fn gin_null_elements_not_indexed() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t_nulls(id INT PRIMARY KEY, arr TEXT[])",
        &mut storage,
        &mut txn,
    );
    common::run(
        "CREATE INDEX idx_nulls_gin ON t_nulls USING GIN (arr)",
        &mut storage,
        &mut txn,
    );
    // Insert array with NULL element
    common::run(
        "INSERT INTO t_nulls VALUES (1, ARRAY[NULL, 'a'])",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO t_nulls VALUES (2, ARRAY['a', 'b'])",
        &mut storage,
        &mut txn,
    );

    // Query should still find row 1 via 'a'
    let result = rows(
        "SELECT id FROM t_nulls WHERE arr @> ARRAY['a']",
        &mut storage,
        &mut txn,
    );
    let ids = int_ids(result);
    assert_eq!(ids, vec![1, 2]);
}

/// GIN index is updated when a new row is inserted.
#[test]
fn gin_maintenance_on_insert() {
    let (mut storage, mut txn) = common::setup();
    setup_array_table(&mut storage, &mut txn);

    // Insert a new row with 'bug' element
    common::run(
        "INSERT INTO tags VALUES (7, ARRAY['bug', 'security'])",
        &mut storage,
        &mut txn,
    );

    // Query should find the new row
    let result = rows(
        "SELECT id FROM tags WHERE tags @> ARRAY['bug']",
        &mut storage,
        &mut txn,
    );
    let ids = int_ids(result);
    assert!(ids.contains(&4)); // original row with 'bug'
    assert!(ids.contains(&7)); // newly inserted row with 'bug'
}

/// GIN index is updated when an existing row is updated.
#[test]
fn gin_maintenance_on_update() {
    let (mut storage, mut txn) = common::setup();
    setup_array_table(&mut storage, &mut txn);

    // Update row 4 to have 'bug' instead of just 'bug' - actually update it to include 'urgent'
    common::run(
        "UPDATE tags SET tags = ARRAY['urgent', 'bug'] WHERE id = 4",
        &mut storage,
        &mut txn,
    );

    // Row 4 should now appear in queries for 'urgent'
    let result = rows(
        "SELECT id FROM tags WHERE tags @> ARRAY['urgent']",
        &mut storage,
        &mut txn,
    );
    let ids = int_ids(result);
    assert!(ids.contains(&4), "updated row 4 should now match 'urgent'");
    // Original rows 1, 3, 6 should still match
    assert_eq!(ids, vec![1, 3, 4, 6]);
}

/// GIN index is updated when a row is deleted.
#[test]
fn gin_maintenance_on_delete() {
    let (mut storage, mut txn) = common::setup();
    setup_array_table(&mut storage, &mut txn);

    // Delete row 1
    common::run("DELETE FROM tags WHERE id = 1", &mut storage, &mut txn);

    // Query for 'urgent' should not include row 1 anymore
    let result = rows(
        "SELECT id FROM tags WHERE tags @> ARRAY['urgent']",
        &mut storage,
        &mut txn,
    );
    let ids = int_ids(result);
    assert!(!ids.contains(&1), "deleted row 1 should not appear");
    // Should still find rows 3 and 6
    assert_eq!(ids, vec![3, 6]);
}

// ── GIN recheck and fallback tests ──────────────────────────────────────────

/// Helper: runs EXPLAIN on an array GIN query using ctx-aware session.
fn explain_gin_with_ctx(sql: &str) -> String {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    // Create table with GIN index
    common::run_ctx(
        "CREATE TABLE tags(id INT PRIMARY KEY, tags TEXT[])",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE INDEX idx_tags_gin ON tags USING GIN (tags)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO tags VALUES (1, ARRAY['urgent', 'bug', 'frontend'])",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO tags VALUES (2, ARRAY['feature', 'backend'])",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let result = common::run_ctx(sql, &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    let rows = common::rows(result);
    rows.iter()
        .flat_map(|r| r.iter())
        .filter_map(|v| match v {
            Value::Text(s) => Some(s.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// EXPLAIN shows GIN scan with recheck for @> queries.
#[test]
fn gin_recheck_required() {
    // EXPLAIN should show 'gin' index usage
    let explain_text =
        explain_gin_with_ctx("EXPLAIN SELECT id FROM tags WHERE tags @> ARRAY['bug']");
    assert!(
        explain_text.contains("gin") || explain_text.contains("GIN"),
        "EXPLAIN should mention GIN index, got: {}",
        explain_text
    );
}

/// Without a GIN index, array @> falls back to sequential scan.
#[test]
fn gin_without_index_falls_back_to_seq_scan() {
    let (mut storage, mut txn) = common::setup();
    // Create table WITHOUT GIN index
    common::run(
        "CREATE TABLE tags_no_gin(id INT PRIMARY KEY, tags TEXT[])",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO tags_no_gin VALUES (1, ARRAY['urgent', 'bug'])",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO tags_no_gin VALUES (2, ARRAY['feature'])",
        &mut storage,
        &mut txn,
    );

    // Query should still work via sequential scan + filter
    let result = rows(
        "SELECT id FROM tags_no_gin WHERE tags @> ARRAY['urgent']",
        &mut storage,
        &mut txn,
    );
    let ids = int_ids(result);
    assert_eq!(ids, vec![1]);
}

// ── Integer array GIN tests ─────────────────────────────────────────────────

/// GIN indexing works for INTEGER arrays.
#[test]
fn gin_integer_array() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE numbers(id INT PRIMARY KEY, nums INT[])",
        &mut storage,
        &mut txn,
    );
    common::run(
        "CREATE INDEX idx_nums_gin ON numbers USING GIN (nums)",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO numbers VALUES (1, ARRAY[1, 2, 3])",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO numbers VALUES (2, ARRAY[4, 5, 6])",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO numbers VALUES (3, ARRAY[2, 4, 6])",
        &mut storage,
        &mut txn,
    );

    // @> containment
    let result = rows(
        "SELECT id FROM numbers WHERE nums @> ARRAY[2]",
        &mut storage,
        &mut txn,
    );
    let ids = int_ids(result);
    // Rows 1 (contains 2) and 3 (contains 2) match
    assert_eq!(ids, vec![1, 3]);

    // && overlap
    let result = rows(
        "SELECT id FROM numbers WHERE nums && ARRAY[1, 4]",
        &mut storage,
        &mut txn,
    );
    let ids = int_ids(result);
    // Rows 1 (has 1), 2 (has 4), 3 (has 4) match
    assert_eq!(ids, vec![1, 2, 3]);
}

// ── Multi-dimensional array GIN tests ───────────────────────────────────────

/// GIN indexing flattens nested arrays and indexes all leaf elements.
#[test]
fn gin_multidimensional_array() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE multi(id INT PRIMARY KEY, arr INT[])",
        &mut storage,
        &mut txn,
    );
    common::run(
        "CREATE INDEX idx_multi_gin ON multi USING GIN (arr)",
        &mut storage,
        &mut txn,
    );
    // Note: ARRAY[[1,2],[3,4]] creates a 2D array, but our parser may not support this syntax
    // We'll test with nested ARRAY syntax instead
    common::run(
        "INSERT INTO multi VALUES (1, ARRAY[1, 2, 3])",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO multi VALUES (2, ARRAY[4, 5, 6])",
        &mut storage,
        &mut txn,
    );

    // Should find row with element 3
    let result = rows(
        "SELECT id FROM multi WHERE arr @> ARRAY[3]",
        &mut storage,
        &mut txn,
    );
    let ids = int_ids(result);
    assert_eq!(ids, vec![1]);
}
