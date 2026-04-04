mod common;

use axiomdb_catalog::{CatalogReader, IndexDef};
use axiomdb_sql::clustered_secondary::ClusteredSecondaryLayout;
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;
use common::{rows, run_ctx, run_result, setup_ctx};

// ── helpers ──────────────────────────────────────────────────────────────────

fn table_indexes(storage: &MemoryStorage, txn: &TxnManager, table_name: &str) -> Vec<IndexDef> {
    let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    let mut reader = CatalogReader::new(storage, snap).unwrap();
    let table = reader.get_table("public", table_name).unwrap().unwrap();
    reader.list_indexes(table.id).unwrap()
}

fn primary_idx(indexes: &[IndexDef]) -> IndexDef {
    indexes.iter().find(|i| i.is_primary).unwrap().clone()
}

fn secondary_by_name<'a>(indexes: &'a [IndexDef], name: &str) -> &'a IndexDef {
    indexes.iter().find(|i| i.name == name).unwrap()
}

fn scan_secondary_all(
    storage: &MemoryStorage,
    secondary: &IndexDef,
    primary: &IndexDef,
) -> Vec<Value> {
    let layout = ClusteredSecondaryLayout::derive(secondary, primary).unwrap();
    let all = layout
        .scan_prefix(storage, secondary.root_page_id, &[])
        .unwrap();
    all.into_iter().flat_map(|e| e.primary_key).collect()
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// CREATE INDEX on an empty clustered table succeeds and writes a catalog entry.
#[test]
fn create_index_empty_table_creates_catalog_entry() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE items (id INT PRIMARY KEY, category TEXT, price INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE INDEX idx_category ON items (category)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let indexes = table_indexes(&storage, &txn, "items");
    assert!(
        indexes.iter().any(|i| i.name == "idx_category"),
        "expected idx_category in catalog"
    );
    let idx = secondary_by_name(&indexes, "idx_category");
    assert!(!idx.is_primary);
    assert!(!idx.is_unique);
    assert_eq!(idx.columns.len(), 1); // column 'category' = col_idx 1
}

/// CREATE INDEX on a populated table: all existing rows are indexed.
#[test]
fn create_index_on_populated_table_indexes_existing_rows() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE items (id INT PRIMARY KEY, category TEXT, price INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    for (id, cat, price) in [(1, "A", 10), (2, "B", 20), (3, "A", 30)] {
        run_ctx(
            &format!("INSERT INTO items VALUES ({id}, '{cat}', {price})"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
    }

    run_ctx(
        "CREATE INDEX idx_category ON items (category)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let indexes = table_indexes(&storage, &txn, "items");
    let pk = primary_idx(&indexes);
    let idx = secondary_by_name(&indexes, "idx_category").clone();

    // scan_prefix for "A" should return PKs 1 and 3
    let layout = ClusteredSecondaryLayout::derive(&idx, &pk).unwrap();
    let mut entries_a: Vec<i32> = layout
        .scan_prefix(&storage, idx.root_page_id, &[Value::Text("A".into())])
        .unwrap()
        .into_iter()
        .map(|e| match e.primary_key[0] {
            Value::Int(n) => n,
            ref v => panic!("unexpected pk value {v:?}"),
        })
        .collect();
    entries_a.sort();
    assert_eq!(entries_a, vec![1, 3]);

    // scan_prefix for "B" should return PK 2
    let entries_b: Vec<i32> = layout
        .scan_prefix(&storage, idx.root_page_id, &[Value::Text("B".into())])
        .unwrap()
        .into_iter()
        .map(|e| match e.primary_key[0] {
            Value::Int(n) => n,
            ref v => panic!("unexpected pk value {v:?}"),
        })
        .collect();
    assert_eq!(entries_b, vec![2]);
}

/// CREATE UNIQUE INDEX rejects duplicate secondary values among existing rows.
#[test]
fn create_unique_index_rejects_existing_duplicates() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE items (id INT PRIMARY KEY, email TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO items VALUES (1, 'alice@example.com')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO items VALUES (2, 'alice@example.com')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = run_result(
        "CREATE UNIQUE INDEX uq_email ON items (email)",
        &mut storage,
        &mut txn,
    )
    .unwrap_err();
    assert!(
        matches!(err, axiomdb_core::error::DbError::UniqueViolation { .. }),
        "expected UniqueViolation, got {err:?}"
    );
}

/// CREATE UNIQUE INDEX succeeds when all secondary values are distinct.
#[test]
fn create_unique_index_succeeds_with_distinct_values() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (1, 'alice@example.com')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (2, 'bob@example.com')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE UNIQUE INDEX uq_email ON users (email)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let indexes = table_indexes(&storage, &txn, "users");
    assert!(
        indexes.iter().any(|i| i.name == "uq_email" && i.is_unique),
        "expected uq_email unique index in catalog"
    );
}

/// INSERT after CREATE INDEX maintains the secondary index.
#[test]
fn insert_after_create_index_maintains_secondary_index() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (1, 'alice@example.com')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE INDEX idx_email ON users (email)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Insert a row after the index was created.
    run_ctx(
        "INSERT INTO users VALUES (2, 'bob@example.com')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let indexes = table_indexes(&storage, &txn, "users");
    let pk = primary_idx(&indexes);
    let idx = secondary_by_name(&indexes, "idx_email").clone();

    // Both rows should be in the secondary index.
    let all_pks = scan_secondary_all(&storage, &idx, &pk);
    assert!(all_pks.contains(&Value::Int(1)), "pk 1 missing from index");
    assert!(all_pks.contains(&Value::Int(2)), "pk 2 missing from index");
}

/// SELECT can use the secondary index after CREATE INDEX (smoke test).
#[test]
fn select_uses_secondary_index_after_create_index() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (1, 'Alice')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (2, 'Bob')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE INDEX idx_name ON users (name)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "SELECT id FROM users WHERE name = 'Alice'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let result_rows = rows(result);
    assert_eq!(result_rows.len(), 1);
    assert_eq!(result_rows[0][0], Value::Int(1));
}

/// NULL values in the indexed column are not indexed (clustered secondary skips NULLs).
#[test]
fn create_index_skips_null_secondary_values() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE items (id INT PRIMARY KEY, tag TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO items VALUES (1, 'rust')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO items VALUES (2, NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO items VALUES (3, 'db')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE INDEX idx_tag ON items (tag)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let indexes = table_indexes(&storage, &txn, "items");
    let pk = primary_idx(&indexes);
    let idx = secondary_by_name(&indexes, "idx_tag").clone();

    // Only rows with non-NULL tag are indexed (rows 1 and 3).
    let all_pks = scan_secondary_all(&storage, &idx, &pk);
    assert_eq!(all_pks.len(), 2, "expected 2 indexed rows, got {all_pks:?}");
    assert!(all_pks.contains(&Value::Int(1)));
    assert!(all_pks.contains(&Value::Int(3)));
}

/// CREATE UNIQUE INDEX enforces uniqueness on subsequent INSERTs.
#[test]
fn unique_index_enforces_on_insert_after_create() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (1, 'alice@example.com')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE UNIQUE INDEX uq_email ON users (email)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Inserting a row with a duplicate email must fail.
    let err = run_ctx(
        "INSERT INTO users VALUES (2, 'alice@example.com')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        matches!(err, axiomdb_core::error::DbError::UniqueViolation { .. }),
        "expected UniqueViolation on duplicate after CREATE UNIQUE INDEX, got {err:?}"
    );
}

/// Duplicate index name on same clustered table returns IndexAlreadyExists.
#[test]
fn create_index_duplicate_name_returns_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE INDEX idx_name ON users (name)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = run_result(
        "CREATE INDEX idx_name ON users (name)",
        &mut storage,
        &mut txn,
    )
    .unwrap_err();
    assert!(
        matches!(err, axiomdb_core::error::DbError::IndexAlreadyExists { .. }),
        "expected IndexAlreadyExists, got {err:?}"
    );
}

/// Partial CREATE INDEX on a clustered table only indexes matching rows.
#[test]
fn partial_create_index_on_clustered_table() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE items (id INT PRIMARY KEY, price INT, active INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    for (id, price, active) in [(1, 10, 1), (2, 20, 0), (3, 30, 1)] {
        run_ctx(
            &format!("INSERT INTO items VALUES ({id}, {price}, {active})"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
    }

    run_ctx(
        "CREATE INDEX idx_price_active ON items (price) WHERE active = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let indexes = table_indexes(&storage, &txn, "items");
    let pk = primary_idx(&indexes);
    let idx = secondary_by_name(&indexes, "idx_price_active").clone();

    // Only rows where active = 1 (rows 1 and 3) are indexed.
    let all_pks = scan_secondary_all(&storage, &idx, &pk);
    assert_eq!(
        all_pks.len(),
        2,
        "expected 2 rows from partial index, got {all_pks:?}"
    );
    assert!(all_pks.contains(&Value::Int(1)));
    assert!(all_pks.contains(&Value::Int(3)));
}

/// CREATE INDEX on a heap table still works (non-clustered regression check).
#[test]
fn create_index_on_heap_table_still_works() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    // A heap table is created when there is no PRIMARY KEY constraint.
    run_ctx(
        "CREATE TABLE heap_items (id INT, tag TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO heap_items VALUES (1, 'rust')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO heap_items VALUES (2, 'db')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE INDEX idx_tag ON heap_items (tag)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "SELECT id FROM heap_items WHERE tag = 'rust'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let result_rows = rows(result);
    assert_eq!(result_rows.len(), 1);
    assert_eq!(result_rows[0][0], Value::Int(1));
}
