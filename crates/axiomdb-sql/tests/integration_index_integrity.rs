//! Integration tests for Phase 6.15: startup-time index integrity verification.
//!
//! These tests create real on-disk databases, corrupt index state outside the
//! normal WAL/catalog flows, then validate that `verify_and_repair_indexes_on_open`
//! either rebuilds the index from heap-visible rows or fails open when the
//! index root is unreadable.

use axiomdb_catalog::{bootstrap::CatalogBootstrap, CatalogReader, CatalogWriter, IndexDef};
use axiomdb_core::error::DbError;
use axiomdb_index::{
    page_layout::{cast_leaf_mut, NULL_PAGE},
    BTree,
};
use axiomdb_sql::{
    analyze, execute_with_ctx, parse, verify_and_repair_indexes_on_open, BloomRegistry,
    QueryResult, SessionContext,
};
use axiomdb_storage::{MmapStorage, Page, PageType, StorageEngine};
use axiomdb_wal::TxnManager;
use std::path::PathBuf;

struct DiskDb {
    _dir: tempfile::TempDir,
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl DiskDb {
    fn create(sqls: &[&str]) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("integrity.db");
        let wal_path = dir.path().join("integrity.wal");

        let mut storage = MmapStorage::create(&db_path).expect("create db");
        CatalogBootstrap::init(&mut storage).expect("init catalog");
        let mut txn = TxnManager::create(&wal_path).expect("create wal");
        let mut bloom = BloomRegistry::new();
        let mut ctx = SessionContext::new();

        for sql in sqls {
            run_sql(&mut storage, &mut txn, &mut bloom, &mut ctx, sql)
                .unwrap_or_else(|e| panic!("setup SQL failed: {sql}\nerror: {e}"));
        }
        storage.flush().expect("flush setup db");

        Self {
            _dir: dir,
            db_path,
            wal_path,
        }
    }

    fn open_recovered(&self) -> (MmapStorage, TxnManager) {
        let mut storage = MmapStorage::open(&self.db_path).expect("open db");
        let (txn, _recovery) =
            TxnManager::open_with_recovery(&mut storage, &self.wal_path).expect("open wal");
        (storage, txn)
    }
}

fn run_sql(
    storage: &mut MmapStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
    sql: &str,
) -> Result<QueryResult, DbError> {
    let stmt = parse(sql, None)?;
    let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    let analyzed = analyze(stmt, storage, snap)?;
    execute_with_ctx(analyzed, storage, txn, bloom, ctx)
}

fn load_index(
    storage: &MmapStorage,
    txn: &TxnManager,
    table_name: &str,
    index_name: &str,
) -> IndexDef {
    let mut reader = CatalogReader::new(storage, txn.snapshot()).expect("catalog reader");
    let table = reader
        .get_table("public", table_name)
        .expect("catalog read")
        .expect("table exists");
    reader
        .list_indexes(table.id)
        .expect("list indexes")
        .into_iter()
        .find(|idx| idx.name == index_name)
        .unwrap_or_else(|| panic!("index {index_name} missing on {table_name}"))
}

fn rewrite_index_root(db: &DiskDb, table_name: &str, index_name: &str, new_root_page_id: u64) {
    let mut storage = MmapStorage::open(&db.db_path).expect("open db");
    let mut txn = TxnManager::open(&db.wal_path).expect("open wal");
    let idx = load_index(&storage, &txn, table_name, index_name);
    txn.begin().expect("begin catalog txn");
    {
        let mut writer = CatalogWriter::new(&mut storage, &mut txn).expect("catalog writer");
        writer
            .update_index_root(idx.index_id, new_root_page_id)
            .expect("update index root");
    }
    txn.commit().expect("commit catalog txn");
    storage.flush().expect("flush rewritten catalog");
}

fn alloc_empty_index_root(storage: &mut MmapStorage) -> u64 {
    let pid = storage
        .alloc_page(PageType::Index)
        .expect("alloc index page");
    let mut page = Page::new(PageType::Index, pid);
    let leaf = cast_leaf_mut(&mut page);
    leaf.is_leaf = 1;
    leaf.set_num_keys(0);
    leaf.set_next_leaf(NULL_PAGE);
    page.update_checksum();
    storage
        .write_page(pid, &page)
        .expect("write empty index root");
    pid
}

fn rewrite_index_root_to_existing_index(db: &DiskDb, table_name: &str, target_index_name: &str) {
    let mut storage = MmapStorage::open(&db.db_path).expect("open db");
    let new_root = alloc_empty_index_root(&mut storage);
    storage.flush().expect("flush empty root");
    drop(storage);
    rewrite_index_root(db, table_name, target_index_name, new_root);
}

fn count_index_entries(
    storage: &MmapStorage,
    txn: &TxnManager,
    table_name: &str,
    index_name: &str,
) -> usize {
    let idx = load_index(storage, txn, table_name, index_name);
    BTree::range_in(storage, idx.root_page_id, None, None)
        .expect("index range")
        .len()
}

#[test]
fn test_verify_and_repair_rebuilds_missing_unique_index_entry() {
    let db = DiskDb::create(&[
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT)",
        "CREATE UNIQUE INDEX uq_email ON users(email)",
        "INSERT INTO users VALUES (1, 'alice@x.com')",
        "INSERT INTO users VALUES (2, 'bob@x.com')",
        "INSERT INTO users VALUES (3, 'carol@x.com')",
    ]);

    rewrite_index_root_to_existing_index(&db, "users", "uq_email");

    let (mut storage, mut txn) = db.open_recovered();
    let report =
        verify_and_repair_indexes_on_open(&mut storage, &mut txn).expect("rebuild missing entry");

    assert_eq!(report.tables_checked, 1);
    assert_eq!(report.indexes_checked, 2, "PK + unique secondary");
    assert_eq!(
        report.rebuilt_indexes.len(),
        1,
        "only uq_email should rebuild"
    );
    assert_eq!(report.rebuilt_indexes[0].table_name, "users");
    assert_eq!(report.rebuilt_indexes[0].index_name, "uq_email");
    assert_eq!(
        count_index_entries(&storage, &txn, "users", "users_pkey"),
        3
    );
    assert_eq!(count_index_entries(&storage, &txn, "users", "uq_email"), 3);
    drop(txn);
    drop(storage);

    let (mut storage, mut txn) = db.open_recovered();
    assert_eq!(
        count_index_entries(&storage, &txn, "users", "users_pkey"),
        3,
        "repair must not damage the primary key index across reopen"
    );
    assert_eq!(
        count_index_entries(&storage, &txn, "users", "uq_email"),
        3,
        "rebuilt unique index must remain durable across reopen"
    );

    let mut bloom = BloomRegistry::new();
    let mut ctx = SessionContext::new();
    let err = run_sql(
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
        "INSERT INTO users VALUES (99, 'alice@x.com')",
    )
    .expect_err("repaired unique index must reject duplicates");
    assert!(matches!(err, DbError::UniqueViolation { .. }), "{err}");
}

#[test]
fn test_verify_and_repair_rebuilds_partial_index_with_predicate_semantics() {
    let db = DiskDb::create(&[
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT, deleted_at TEXT)",
        "CREATE UNIQUE INDEX uq_email ON users(email) WHERE deleted_at IS NULL",
        "INSERT INTO users VALUES (1, 'alice@x.com', NULL)",
        "INSERT INTO users VALUES (2, 'alice@x.com', '2025-01-01')",
    ]);

    rewrite_index_root_to_existing_index(&db, "users", "uq_email");

    let (mut storage, mut txn) = db.open_recovered();
    let report =
        verify_and_repair_indexes_on_open(&mut storage, &mut txn).expect("rebuild partial index");

    assert_eq!(report.rebuilt_indexes.len(), 1);
    assert_eq!(report.rebuilt_indexes[0].index_name, "uq_email");
    assert_eq!(
        count_index_entries(&storage, &txn, "users", "uq_email"),
        1,
        "only active row should remain indexed"
    );

    let mut bloom = BloomRegistry::new();
    let mut ctx = SessionContext::new();

    let err = run_sql(
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
        "INSERT INTO users VALUES (3, 'alice@x.com', NULL)",
    )
    .expect_err("active duplicate must fail after rebuild");
    assert!(matches!(err, DbError::UniqueViolation { .. }), "{err}");

    run_sql(
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
        "INSERT INTO users VALUES (4, 'alice@x.com', '2026-01-01')",
    )
    .expect("deleted duplicate should remain outside the partial index");
}

#[test]
fn test_verify_and_repair_fails_open_for_unreadable_index_root() {
    let db = DiskDb::create(&[
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT)",
        "CREATE UNIQUE INDEX uq_email ON users(email)",
        "INSERT INTO users VALUES (1, 'alice@x.com')",
    ]);

    rewrite_index_root(&db, "users", "uq_email", 9_999_999);

    let (mut storage, mut txn) = db.open_recovered();
    let err = verify_and_repair_indexes_on_open(&mut storage, &mut txn)
        .expect_err("unreadable root must fail open");

    assert!(matches!(
        err,
        DbError::IndexIntegrityFailure {
            table,
            index,
            reason,
        } if table == "public.users"
            && index == "uq_email"
            && (reason.contains("page") || reason.contains("B+ tree"))
    ));
}
