mod common;

use axiomdb_catalog::{CatalogReader, IndexDef};
use axiomdb_sql::clustered_secondary::ClusteredSecondaryLayout;
use axiomdb_storage::{MemoryStorage, StorageEngine};
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;
use common::{rows, run_ctx, setup_ctx};

fn setup_and_populate(
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
    for i in 1..=10 {
        let age = 20 + i;
        run_ctx(
            &format!("INSERT INTO users VALUES ({i}, 'user_{i}', {age})"),
            storage,
            txn,
            bloom,
            ctx,
        )
        .unwrap();
    }
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
fn vacuum_removes_dead_cells_after_delete() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_and_populate(&mut storage, &mut txn, &mut bloom, &mut ctx);

    // Delete 5 rows.
    run_ctx(
        "DELETE FROM users WHERE id <= 5",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Verify 5 remaining before VACUUM.
    let r = rows(
        run_ctx(
            "SELECT COUNT(*) FROM users",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::BigInt(5));

    // Run VACUUM.
    let result = run_ctx("VACUUM users", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    let r = rows(result);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("users".into()));
    // dead_rows_removed should be 5.
    assert_eq!(r[0][1], Value::Int(5));

    // Data still correct after VACUUM.
    let r = rows(
        run_ctx(
            "SELECT COUNT(*) FROM users",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::BigInt(5));

    // Verify remaining rows are the right ones.
    let r = rows(
        run_ctx(
            "SELECT id FROM users ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 5);
    assert_eq!(r[0][0], Value::Int(6));
    assert_eq!(r[4][0], Value::Int(10));
}

#[test]
fn vacuum_on_empty_table_succeeds() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE empty_t (id INT PRIMARY KEY, val TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "VACUUM empty_t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let r = rows(result);
    assert_eq!(r[0][1], Value::Int(0)); // 0 dead rows
}

#[test]
fn vacuum_with_no_dead_cells_removes_nothing() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_and_populate(&mut storage, &mut txn, &mut bloom, &mut ctx);

    // No deletes — all alive.
    let result = run_ctx("VACUUM users", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    let r = rows(result);
    assert_eq!(r[0][1], Value::Int(0)); // 0 dead rows

    // All rows still present.
    let r = rows(
        run_ctx(
            "SELECT COUNT(*) FROM users",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::BigInt(10));
}

#[test]
fn vacuum_after_delete_all_rows() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_and_populate(&mut storage, &mut txn, &mut bloom, &mut ctx);

    run_ctx(
        "DELETE FROM users",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx("VACUUM users", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    let r = rows(result);
    assert_eq!(r[0][1], Value::Int(10)); // 10 dead rows removed

    // Table empty.
    let r = rows(
        run_ctx(
            "SELECT COUNT(*) FROM users",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::BigInt(0));

    // Can re-insert after VACUUM.
    run_ctx(
        "INSERT INTO users VALUES (1, 'new', 99)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let r = rows(
        run_ctx(
            "SELECT * FROM users",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1));
}

#[test]
fn vacuum_reclaims_space_for_new_inserts() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_and_populate(&mut storage, &mut txn, &mut bloom, &mut ctx);

    // Delete half.
    run_ctx(
        "DELETE FROM users WHERE id <= 5",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // VACUUM.
    run_ctx("VACUUM users", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    // Insert new rows — should reuse freed space.
    for i in 11..=15 {
        let age = 40 + i;
        run_ctx(
            &format!("INSERT INTO users VALUES ({i}, 'new_{i}', {age})"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
    }

    let r = rows(
        run_ctx(
            "SELECT COUNT(*) FROM users",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::BigInt(10)); // 5 remaining + 5 new
}

#[test]
fn vacuum_clustered_secondary_cleanup_removes_dead_bookmarks() {
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
    run_ctx(
        "INSERT INTO users VALUES (2, 'bob@example.com', 'Bob')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "DELETE FROM users WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx("VACUUM users", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    let r = rows(result);
    assert_eq!(r[0][1], Value::Int(1));
    assert_eq!(r[0][2], Value::Int(1));

    let indexes = table_indexes(&storage, &txn, "users");
    let primary_idx = primary_index(&indexes);
    let secondary_idx = only_unique_secondary(&indexes);
    let bookmark = scan_secondary_prefix(
        &storage,
        &secondary_idx,
        &primary_idx,
        &[Value::Text("alice@example.com".into())],
    );
    assert!(
        bookmark.is_empty(),
        "dead secondary bookmark must be removed after clustered purge"
    );
}

#[test]
fn vacuum_keeps_secondary_bookmark_for_uncommitted_clustered_delete() {
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
        "DELETE FROM users WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx("VACUUM users", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    let r = rows(result);
    assert_eq!(r[0][1], Value::Int(0));
    assert_eq!(r[0][2], Value::Int(0));

    let indexes = table_indexes(&storage, &txn, "users");
    let primary_idx = primary_index(&indexes);
    let secondary_idx = only_unique_secondary(&indexes);
    let bookmark = scan_secondary_prefix(
        &storage,
        &secondary_idx,
        &primary_idx,
        &[Value::Text("alice@example.com".into())],
    );
    assert_eq!(
        bookmark,
        vec![vec![Value::Int(1)]],
        "secondary bookmark must survive while clustered row still exists physically"
    );

    run_ctx("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    let r = rows(
        run_ctx(
            "SELECT name FROM users WHERE email = 'alice@example.com'",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r, vec![vec![Value::Text("Alice".into())]]);
}

#[test]
fn vacuum_reuses_overflow_pages_after_clustered_purge() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE docs (id INT PRIMARY KEY, body TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let first_body = "x".repeat(5_000);
    let second_body = "y".repeat(5_000);

    run_ctx(
        &format!("INSERT INTO docs VALUES (1, '{first_body}')"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let pages_after_first_insert = storage.page_count();

    run_ctx(
        "DELETE FROM docs WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("VACUUM docs", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    run_ctx(
        &format!("INSERT INTO docs VALUES (2, '{second_body}')"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    assert_eq!(
        storage.page_count(),
        pages_after_first_insert,
        "overflow pages should be freed and then reused by the next large clustered row"
    );
}

#[test]
fn vacuum_keeps_secondary_queries_working_after_bulk_cleanup() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT UNIQUE, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    for id in 1..=512 {
        let email = format!("user_{id:04}_very_long_secondary_key@example.com");
        run_ctx(
            &format!("INSERT INTO users VALUES ({id}, '{email}', 'User{id}')"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
    }

    run_ctx(
        "DELETE FROM users WHERE id < 512",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("VACUUM users", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    let after_indexes = table_indexes(&storage, &txn, "users");
    let after_primary = primary_index(&after_indexes);
    let after_secondary = only_unique_secondary(&after_indexes);

    let bookmark = scan_secondary_prefix(
        &storage,
        &after_secondary,
        &after_primary,
        &[Value::Text(
            "user_0512_very_long_secondary_key@example.com".into(),
        )],
    );
    assert_eq!(bookmark, vec![vec![Value::Int(512)]]);

    let r = rows(
        run_ctx(
            "SELECT id FROM users WHERE email = 'user_0512_very_long_secondary_key@example.com'",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r, vec![vec![Value::Int(512)]]);
}
