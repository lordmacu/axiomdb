mod common;

use std::sync::atomic::{AtomicU64, Ordering};

use axiomdb_catalog::{
    CatalogReader, CatalogWriter, IndexColumnDef, IndexDef, SortOrder as CatalogSortOrder,
    TableDef, TableStorageLayout,
};
use axiomdb_index::{
    page_layout::{cast_leaf_mut, NULL_PAGE},
    BTree,
};
use axiomdb_sql::{
    clustered_secondary::ClusteredSecondaryLayout, key_encoding::encode_index_key, TableEngine,
};
use axiomdb_storage::{MemoryStorage, Page, PageType, StorageEngine};
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;
use common::{affected_count, rows, run, run_ctx, run_result, setup, setup_ctx};

#[test]
fn rebuild_migrates_legacy_heap_table_to_clustered_and_preserves_secondary_bookmarks() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run(
        "CREATE TABLE users (id INT NOT NULL, email TEXT NOT NULL, name TEXT, age INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (3, 'charlie@x.com', 'Charlie', 35)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (1, 'alice@x.com', 'Alice', 30)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (2, 'bob@x.com', 'Bob', 25)",
        &mut storage,
        &mut txn,
    );
    run(
        "CREATE UNIQUE INDEX uq_users_email ON users(email)",
        &mut storage,
        &mut txn,
    );
    install_primary_index(&mut storage, &mut txn, "users", "PRIMARY", &["id"]);

    let (before_table, before_columns, before_indexes) = load_table_bundle(&storage, &txn, "users");
    assert_eq!(before_table.storage_layout, TableStorageLayout::Heap);
    let before_pk = before_indexes
        .iter()
        .find(|idx| idx.is_primary)
        .expect("legacy PK index present")
        .clone();
    let before_secondary = before_indexes
        .iter()
        .find(|idx| idx.name == "uq_users_email")
        .expect("legacy secondary index present")
        .clone();

    let affected = affected_count(
        run_ctx(
            "ALTER TABLE users REBUILD",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .expect("rebuild succeeds"),
    );
    assert_eq!(affected, 3);

    let (after_table, _after_columns, after_indexes) = load_table_bundle(&storage, &txn, "users");
    assert_eq!(after_table.storage_layout, TableStorageLayout::Clustered);
    assert_ne!(after_table.root_page_id, before_table.root_page_id);

    let after_pk = after_indexes
        .iter()
        .find(|idx| idx.is_primary)
        .expect("clustered PK index present")
        .clone();
    let after_secondary = after_indexes
        .iter()
        .find(|idx| idx.name == "uq_users_email")
        .expect("clustered secondary index present")
        .clone();

    assert_eq!(after_pk.root_page_id, after_table.root_page_id);
    assert_ne!(after_pk.root_page_id, before_pk.root_page_id);
    assert_ne!(after_secondary.root_page_id, before_secondary.root_page_id);

    assert!(
        storage.read_page(before_table.root_page_id).is_err(),
        "old heap root must be reclaimed after rebuild commit"
    );
    assert!(
        storage.read_page(before_pk.root_page_id).is_err(),
        "old PK root must be reclaimed after rebuild commit"
    );
    assert!(
        storage.read_page(before_secondary.root_page_id).is_err(),
        "old secondary root must be reclaimed after rebuild commit"
    );

    let ordered = rows(
        run_ctx(
            "SELECT id, name FROM users ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .expect("ordered select works after rebuild"),
    );
    assert_eq!(
        ordered,
        vec![
            vec![Value::Int(1), Value::Text("Alice".into())],
            vec![Value::Int(2), Value::Text("Bob".into())],
            vec![Value::Int(3), Value::Text("Charlie".into())],
        ]
    );

    let by_secondary = rows(
        run_ctx(
            "SELECT id FROM users WHERE email = 'bob@x.com'",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .expect("secondary lookup works after rebuild"),
    );
    assert_eq!(by_secondary, vec![vec![Value::Int(2)]]);

    let secondary_layout =
        ClusteredSecondaryLayout::derive(&after_secondary, &after_pk).expect("layout derives");
    let secondary_entries = secondary_layout
        .scan_prefix(
            &storage,
            after_secondary.root_page_id,
            &[Value::Text("bob@x.com".into())],
        )
        .expect("scan secondary bookmark");
    assert_eq!(secondary_entries.len(), 1);
    assert_eq!(secondary_entries[0].primary_key, vec![Value::Int(2)]);
    assert_ne!(
        secondary_entries[0].physical_key,
        encode_index_key(&[Value::Text("bob@x.com".into())]).expect("encode legacy logical key"),
        "clustered secondary key must include PK bookmark suffix, not only the logical key"
    );

    run_ctx(
        "UPDATE users SET age = 99 WHERE id = 2",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect("update works after rebuild");
    let updated = rows(
        run_ctx(
            "SELECT age FROM users WHERE id = 2",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .expect("read updated row"),
    );
    assert_eq!(updated, vec![vec![Value::Int(99)]]);

    run_ctx(
        "DELETE FROM users WHERE id = 3",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect("delete works after rebuild");
    run_ctx("VACUUM users", &mut storage, &mut txn, &mut bloom, &mut ctx)
        .expect("vacuum works after rebuild");
    let remaining = rows(
        run_ctx(
            "SELECT COUNT(*) FROM users",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .expect("count works after rebuild"),
    );
    assert_eq!(remaining, vec![vec![Value::BigInt(2)]]);

    assert_eq!(before_columns.len(), 4, "fixture sanity check");
}

#[test]
fn rebuild_empty_legacy_heap_table_allocates_clustered_root() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run(
        "CREATE TABLE empty_users (id INT NOT NULL, name TEXT)",
        &mut storage,
        &mut txn,
    );
    install_primary_index(&mut storage, &mut txn, "empty_users", "PRIMARY", &["id"]);

    let (before_table, _, _) = load_table_bundle(&storage, &txn, "empty_users");
    assert_eq!(before_table.storage_layout, TableStorageLayout::Heap);

    let affected = affected_count(
        run_ctx(
            "ALTER TABLE empty_users REBUILD",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .expect("empty rebuild succeeds"),
    );
    assert_eq!(affected, 0);

    let (after_table, _, after_indexes) = load_table_bundle(&storage, &txn, "empty_users");
    assert_eq!(after_table.storage_layout, TableStorageLayout::Clustered);
    let after_pk = after_indexes
        .iter()
        .find(|idx| idx.is_primary)
        .expect("primary index exists after rebuild");
    assert_eq!(after_pk.root_page_id, after_table.root_page_id);

    let count = rows(
        run_ctx(
            "SELECT COUNT(*) FROM empty_users",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .expect("empty table remains readable"),
    );
    assert_eq!(count, vec![vec![Value::BigInt(0)]]);
}

#[test]
fn rebuild_on_table_without_pk_fails() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE logs (ts INT, msg TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO logs VALUES (1, 'hello')",
        &mut storage,
        &mut txn,
    );

    let err = run_result("ALTER TABLE logs REBUILD", &mut storage, &mut txn).unwrap_err();
    assert!(
        format!("{err:?}").contains("PRIMARY KEY"),
        "expected 'requires PRIMARY KEY' error, got {err:?}"
    );
}

#[test]
fn rebuild_on_already_clustered_table_fails() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut txn,
    );

    let err = run_result("ALTER TABLE users REBUILD", &mut storage, &mut txn).unwrap_err();
    assert!(
        format!("{err:?}").contains("already clustered"),
        "expected 'already clustered' error, got {err:?}"
    );
}

fn load_table_bundle(
    storage: &MemoryStorage,
    txn: &TxnManager,
    table_name: &str,
) -> (TableDef, Vec<axiomdb_catalog::ColumnDef>, Vec<IndexDef>) {
    let mut reader = CatalogReader::new(storage, txn.snapshot()).expect("catalog reader");
    let table = reader
        .get_table("public", table_name)
        .expect("get table")
        .unwrap_or_else(|| panic!("table {table_name} missing"));
    let columns = reader.list_columns(table.id).expect("list columns");
    let indexes = reader.list_indexes(table.id).expect("list indexes");
    (table, columns, indexes)
}

fn install_primary_index(
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    table_name: &str,
    index_name: &str,
    pk_columns: &[&str],
) {
    let (table_def, columns, _) = load_table_bundle(storage, txn, table_name);
    assert_eq!(table_def.storage_layout, TableStorageLayout::Heap);

    let index_columns: Vec<IndexColumnDef> = pk_columns
        .iter()
        .map(|name| {
            let col = columns
                .iter()
                .find(|col| col.name == *name)
                .unwrap_or_else(|| panic!("missing PK column {name} on {table_name}"));
            IndexColumnDef {
                col_idx: col.col_idx,
                order: CatalogSortOrder::Asc,
            }
        })
        .collect();

    let root_page_id = alloc_empty_index_root(storage);
    let root_pid = AtomicU64::new(root_page_id);
    let rows = TableEngine::scan_table(storage, &table_def, &columns, txn.snapshot(), None)
        .expect("scan heap rows for PK rebuild");
    for (rid, row) in rows {
        let key_vals: Vec<Value> = index_columns
            .iter()
            .map(|col| row[col.col_idx as usize].clone())
            .collect();
        let key = encode_index_key(&key_vals).expect("encode PK key");
        BTree::insert_in(storage, &root_pid, &key, rid, 90).expect("insert PK entry");
    }

    let final_root = root_pid.load(Ordering::Acquire);
    txn.begin().expect("begin catalog txn for PK install");
    {
        let mut writer = CatalogWriter::new(storage, txn).expect("catalog writer");
        writer
            .create_index(IndexDef {
                index_id: 0,
                table_id: table_def.id,
                name: index_name.to_string(),
                root_page_id: final_root,
                is_unique: true,
                is_primary: true,
                columns: index_columns,
                predicate: None,
                fillfactor: 90,
                is_fk_index: false,
                include_columns: vec![],
                index_type: 0,
                pages_per_range: 128,
            })
            .expect("create primary index catalog row");
    }
    txn.commit().expect("commit PK install");
}

fn alloc_empty_index_root(storage: &mut MemoryStorage) -> u64 {
    let pid = storage
        .alloc_page(PageType::Index)
        .expect("alloc index root");
    let mut page = Page::new(PageType::Index, pid);
    {
        let leaf = cast_leaf_mut(&mut page);
        leaf.is_leaf = 1;
        leaf.set_num_keys(0);
        leaf.set_next_leaf(NULL_PAGE);
    }
    page.update_checksum();
    storage
        .write_page(pid, &page)
        .expect("write empty index root");
    pid
}
