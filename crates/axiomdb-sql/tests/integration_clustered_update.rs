mod common;

use axiomdb_catalog::{CatalogReader, IndexDef};
use axiomdb_sql::clustered_secondary::ClusteredSecondaryLayout;
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;
use common::{affected_count, rows, run, run_ctx, setup, setup_ctx};

fn setup_users(
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut axiomdb_sql::bloom::BloomRegistry,
    ctx: &mut axiomdb_sql::session::SessionContext,
) {
    run_ctx(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)",
        storage,
        txn,
        bloom,
        ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (1, 'Alice', 30)",
        storage,
        txn,
        bloom,
        ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (2, 'Bob', 25)",
        storage,
        txn,
        bloom,
        ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (3, 'Charlie', 35)",
        storage,
        txn,
        bloom,
        ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (4, 'Diana', 28)",
        storage,
        txn,
        bloom,
        ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (5, 'Eve', 22)",
        storage,
        txn,
        bloom,
        ctx,
    )
    .unwrap();
}

fn table_indexes(
    storage: &impl axiomdb_storage::StorageEngine,
    txn: &TxnManager,
    table_name: &str,
) -> Vec<IndexDef> {
    let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    let mut reader = CatalogReader::new(storage, snap).unwrap();
    let table = reader.get_table("public", table_name).unwrap().unwrap();
    reader.list_indexes(table.id).unwrap()
}

fn primary_index(indexes: &[IndexDef]) -> IndexDef {
    indexes.iter().find(|idx| idx.is_primary).unwrap().clone()
}

fn only_unique_secondary(indexes: &[IndexDef]) -> IndexDef {
    indexes
        .iter()
        .find(|idx| !idx.is_primary && idx.is_unique)
        .unwrap()
        .clone()
}

fn scan_secondary_prefix(
    storage: &impl axiomdb_storage::StorageEngine,
    secondary_idx: &IndexDef,
    primary_idx: &IndexDef,
    logical_prefix: &[Value],
) -> Vec<Vec<Value>> {
    let layout = ClusteredSecondaryLayout::derive(secondary_idx, primary_idx).unwrap();
    layout
        .scan_prefix(storage, secondary_idx.root_page_id, logical_prefix)
        .unwrap()
        .into_iter()
        .map(|entry| entry.primary_key)
        .collect()
}

#[test]
fn clustered_update_non_key_in_place() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_users(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let result = run_ctx(
        "UPDATE users SET name = 'Alicia' WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 1);

    // Verify updated value.
    let r = rows(
        run_ctx(
            "SELECT * FROM users WHERE id = 1",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][1], Value::Text("Alicia".into()));
    assert_eq!(r[0][2], Value::Int(30)); // age unchanged
}

#[test]
fn clustered_update_all_rows() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_users(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let result = run_ctx(
        "UPDATE users SET age = age + 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 5);

    let r = rows(
        run_ctx(
            "SELECT age FROM users ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::Int(31));
    assert_eq!(r[1][0], Value::Int(26));
    assert_eq!(r[2][0], Value::Int(36));
    assert_eq!(r[3][0], Value::Int(29));
    assert_eq!(r[4][0], Value::Int(23));
}

#[test]
fn clustered_update_with_pk_range_where() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_users(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let result = run_ctx(
        "UPDATE users SET name = 'Updated' WHERE id >= 2 AND id < 5",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 3);

    let r = rows(
        run_ctx(
            "SELECT name FROM users ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::Text("Alice".into())); // id=1 unchanged
    assert_eq!(r[1][0], Value::Text("Updated".into())); // id=2
    assert_eq!(r[2][0], Value::Text("Updated".into())); // id=3
    assert_eq!(r[3][0], Value::Text("Updated".into())); // id=4
    assert_eq!(r[4][0], Value::Text("Eve".into())); // id=5 unchanged
}

#[test]
fn clustered_update_noop_when_values_unchanged() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_users(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let result = run_ctx(
        "UPDATE users SET name = name WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // MySQL semantics: matched_count = 1, but no physical change.
    assert_eq!(affected_count(result), 1);

    let r = rows(
        run_ctx(
            "SELECT * FROM users WHERE id = 1",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][1], Value::Text("Alice".into()));
}

#[test]
fn clustered_update_rollback_restores_original() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_users(&mut storage, &mut txn, &mut bloom, &mut ctx);

    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    run_ctx(
        "UPDATE users SET name = 'CHANGED' WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    let r = rows(
        run_ctx(
            "SELECT name FROM users WHERE id = 1",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::Text("Alice".into()));
}

#[test]
fn clustered_update_pk_change_rewrites_primary_and_secondary_bookmarks() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT UNIQUE, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (1, 'alice@example.com', 'Alice')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "UPDATE users SET id = 7 WHERE email = 'alice@example.com'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 1);

    let old_pk = rows(
        run_ctx(
            "SELECT name FROM users WHERE id = 1",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert!(old_pk.is_empty());
    let new_pk = rows(
        run_ctx(
            "SELECT name FROM users WHERE id = 7",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(new_pk[0][0], Value::Text("Alice".into()));

    let indexes = table_indexes(&storage, &txn, "users");
    let primary_idx = primary_index(&indexes);
    let secondary_idx = only_unique_secondary(&indexes);
    assert_eq!(
        scan_secondary_prefix(
            &storage,
            &secondary_idx,
            &primary_idx,
            &[Value::Text("alice@example.com".into())],
        ),
        vec![vec![Value::Int(7)]]
    );
}

#[test]
fn clustered_update_secondary_key_change_updates_bookmark_and_rolls_back_cleanly() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT UNIQUE, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (1, 'alice@example.com', 'Alice')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    run_ctx(
        "UPDATE users SET email = 'alice+new@example.com' WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let indexes = table_indexes(&storage, &txn, "users");
    let primary_idx = primary_index(&indexes);
    let secondary_idx = only_unique_secondary(&indexes);
    assert_eq!(
        scan_secondary_prefix(
            &storage,
            &secondary_idx,
            &primary_idx,
            &[Value::Text("alice+new@example.com".into())],
        ),
        vec![vec![Value::Int(1)]]
    );
    assert!(scan_secondary_prefix(
        &storage,
        &secondary_idx,
        &primary_idx,
        &[Value::Text("alice@example.com".into())],
    )
    .is_empty());

    run_ctx("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    let restored = rows(
        run_ctx(
            "SELECT email FROM users WHERE id = 1",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(restored[0][0], Value::Text("alice@example.com".into()));

    let indexes = table_indexes(&storage, &txn, "users");
    let primary_idx = primary_index(&indexes);
    let secondary_idx = only_unique_secondary(&indexes);
    assert_eq!(
        scan_secondary_prefix(
            &storage,
            &secondary_idx,
            &primary_idx,
            &[Value::Text("alice@example.com".into())],
        ),
        vec![vec![Value::Int(1)]]
    );
    assert!(scan_secondary_prefix(
        &storage,
        &secondary_idx,
        &primary_idx,
        &[Value::Text("alice+new@example.com".into())],
    )
    .is_empty());
}

#[test]
fn clustered_update_empty_result_set() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_users(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let result = run_ctx(
        "UPDATE users SET name = 'Nobody' WHERE id = 999",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 0);
}

#[test]
fn clustered_update_after_splits() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE docs (id INT PRIMARY KEY, body TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    for id in 0..200 {
        let body = "x".repeat(200 + (id % 5) * 40);
        run_ctx(
            &format!("INSERT INTO docs VALUES ({id}, '{body}')"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
    }

    // Update a row in the middle of the tree.
    let result = run_ctx(
        "UPDATE docs SET body = 'UPDATED' WHERE id = 100",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 1);

    let r = rows(
        run_ctx(
            "SELECT body FROM docs WHERE id = 100",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::Text("UPDATED".into()));

    // Verify other rows unaffected.
    let r = rows(
        run_ctx(
            "SELECT COUNT(*) FROM docs",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::BigInt(200));
}

#[test]
fn clustered_update_relocation_preserves_row_visibility() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE docs (id INT PRIMARY KEY, body TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    for id in 0..96 {
        run_ctx(
            &format!("INSERT INTO docs VALUES ({id}, '{}')", "x".repeat(120)),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
    }

    let result = run_ctx(
        &format!(
            "UPDATE docs SET body = '{}' WHERE id = 42",
            "y".repeat(4000)
        ),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 1);

    let r = rows(
        run_ctx(
            "SELECT body FROM docs WHERE id = 42",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::Text("y".repeat(4000)));
}

#[test]
fn clustered_update_non_ctx_path_is_supported() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT UNIQUE, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (1, 'alice@example.com', 'Alice')",
        &mut storage,
        &mut txn,
    );

    let result = run(
        "UPDATE users SET email = 'alice+nonctx@example.com' WHERE id = 1",
        &mut storage,
        &mut txn,
    );
    assert_eq!(affected_count(result), 1);

    let rows = rows(run(
        "SELECT email FROM users WHERE id = 1",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(rows[0][0], Value::Text("alice+nonctx@example.com".into()));

    let indexes = table_indexes(&storage, &txn, "users");
    let primary_idx = primary_index(&indexes);
    let secondary_idx = only_unique_secondary(&indexes);
    assert_eq!(
        scan_secondary_prefix(
            &storage,
            &secondary_idx,
            &primary_idx,
            &[Value::Text("alice+nonctx@example.com".into())],
        ),
        vec![vec![Value::Int(1)]]
    );
}

// ── 39.22 zero-alloc in-place patch tests ─────────────────────────────────────

/// Rollback of a fixed-size (INT) in-place UPDATE must restore the original
/// value. This exercises the WAL FieldDelta undo path with [u8;8] inline bytes.
#[test]
fn clustered_update_inplace_fixed_rollback() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    // Schema: (id INT, name TEXT, age INT) — age is fixed-size, TEXT before it
    // forces the runtime offset scan path in compute_field_location_runtime.
    setup_users(&mut storage, &mut txn, &mut bloom, &mut ctx);

    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    let res = run_ctx(
        "UPDATE users SET age = age + 100",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(res), 5);

    // Verify age was updated inside the transaction.
    let r = rows(
        run_ctx(
            "SELECT age FROM users WHERE id = 1",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::Int(130)); // 30 + 100

    // Rollback — the in-place patch must be undone via FieldDelta.old_bytes.
    run_ctx("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    let r2 = rows(
        run_ctx(
            "SELECT age FROM users ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r2[0][0], Value::Int(30)); // Alice
    assert_eq!(r2[1][0], Value::Int(25)); // Bob
    assert_eq!(r2[2][0], Value::Int(35)); // Charlie
}

/// MAYBE_NOP: updating a column to the same value (byte-identical result)
/// must not produce a WAL entry and must not change the row_version.
#[test]
fn clustered_update_inplace_maybe_nop_same_bytes() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_users(&mut storage, &mut txn, &mut bloom, &mut ctx);

    // age + 0 evaluates to the same Int value — expression equality catches it
    // at Phase 1 (Value-level NOP), so no patches are applied. The affected
    // count is 5 (rows matched by WHERE), not rows changed — consistent with
    // the rest of the executor which returns matched rows.
    let res = run_ctx(
        "UPDATE users SET age = age + 0",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(res), 5);

    // Values unchanged.
    let r = rows(
        run_ctx(
            "SELECT age FROM users ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::Int(30));
    assert_eq!(r[1][0], Value::Int(25));
}

/// Mixed schema with TEXT before the target INT column exercises the runtime
/// offset scan in compute_field_location_runtime — verifies the offset is
/// computed correctly when variable-length columns precede the target.
#[test]
fn clustered_update_inplace_mixed_schema_text_before_target() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    // Same schema as setup_users: (id INT PK, name TEXT, age INT)
    // age is column 2, TEXT is column 1 — runtime scan must skip the TEXT field.
    setup_users(&mut storage, &mut txn, &mut bloom, &mut ctx);

    run_ctx(
        "UPDATE users SET age = 99 WHERE id = 3",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let r = rows(
        run_ctx(
            "SELECT id, name, age FROM users WHERE id = 3",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::Int(3));
    assert_eq!(r[0][1], Value::Text("Charlie".into())); // name unchanged
    assert_eq!(r[0][2], Value::Int(99)); // age patched
}

/// Two fixed-size columns patched in a single UPDATE statement — verifies
/// multi-field_writes collection and WAL encoding with N > 1 FieldDelta.
#[test]
fn clustered_update_inplace_multiple_fixed_columns() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE scores (id INT NOT NULL, level INT NOT NULL, points INT NOT NULL, PRIMARY KEY (id))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO scores VALUES (1, 5, 100), (2, 3, 50), (3, 8, 200)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Update two INT columns simultaneously — both go through patch_field_in_place.
    let res = run_ctx(
        "UPDATE scores SET level = level + 1, points = points * 2",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(res), 3);

    let r = rows(
        run_ctx(
            "SELECT id, level, points FROM scores ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0], vec![Value::Int(1), Value::Int(6), Value::Int(200)]);
    assert_eq!(r[1], vec![Value::Int(2), Value::Int(4), Value::Int(100)]);
    assert_eq!(r[2], vec![Value::Int(3), Value::Int(9), Value::Int(400)]);

    // Rollback verifies both fields are restored via FieldDelta undo.
    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    run_ctx(
        "UPDATE scores SET level = level + 10, points = points + 1000",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    let r2 = rows(
        run_ctx(
            "SELECT level, points FROM scores WHERE id = 1",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r2[0][0], Value::Int(6)); // level: back to 6, not 16
    assert_eq!(r2[0][1], Value::Int(200)); // points: back to 200, not 1200
}
