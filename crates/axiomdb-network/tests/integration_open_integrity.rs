use std::os::unix::fs::FileExt;

use axiomdb_catalog::CatalogReader;
use axiomdb_core::error::DbError;
use axiomdb_network::mysql::Database;
use axiomdb_sql::{SchemaCache, SessionContext};
use axiomdb_storage::{
    meta::CLEAN_SHUTDOWN_BODY_OFFSET,
    page::{Page, HEADER_SIZE, PAGE_SIZE},
    MmapStorage, StorageEngine,
};
use axiomdb_wal::TxnManager;

fn rewrite_server_index_root(
    data_dir: &std::path::Path,
    table_name: &str,
    target_index_name: &str,
    new_root: u64,
) {
    let db_path = data_dir.join("axiomdb.db");
    let wal_path = data_dir.join("axiomdb.wal");
    let mut storage = MmapStorage::open(&db_path).expect("open db");
    let mut txn = TxnManager::open(&wal_path).expect("open wal");
    let mut reader = CatalogReader::new(&storage, txn.snapshot()).expect("catalog reader");
    let table = reader
        .get_table("public", table_name)
        .expect("catalog read")
        .expect("table exists");
    let target = reader
        .list_indexes(table.id)
        .expect("list indexes")
        .into_iter()
        .find(|idx| idx.name == target_index_name)
        .unwrap_or_else(|| panic!("index {target_index_name} missing on {table_name}"));
    let mut conn_txn = txn.begin().expect("begin catalog txn");
    {
        let mut writer = axiomdb_catalog::CatalogWriter::new(&mut storage, &mut txn, &mut conn_txn)
            .expect("catalog writer");
        writer
            .update_index_root(target.index_id, new_root)
            .expect("rewrite root");
    }
    txn.commit(conn_txn).expect("commit catalog txn");
    storage.flush().expect("flush corrupted index");
}

fn mark_database_dirty_open(data_dir: &std::path::Path) {
    let db_path = data_dir.join("axiomdb.db");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&db_path)
        .expect("open db file");

    let mut page = [0u8; PAGE_SIZE];
    file.read_exact_at(&mut page, 0).expect("read page 0");
    let mut page = Page::from_bytes(page).expect("decode page 0");
    page.as_bytes_mut()[HEADER_SIZE + CLEAN_SHUTDOWN_BODY_OFFSET] = 0;
    page.update_checksum();
    file.write_all_at(page.as_bytes(), 0).expect("write page 0");
    file.sync_all().expect("fsync dirty-open marker");
}

#[test]
fn test_database_open_fails_for_unreadable_unique_index() {
    let dir = tempfile::tempdir().expect("tempdir");

    {
        let db = Database::open(dir.path()).expect("open server db");
        let mut session = SessionContext::new();
        let mut cache = SchemaCache::new();
        db.execute_query(
            "CREATE TABLE users (id INT PRIMARY KEY, email TEXT)",
            &mut session,
            &mut cache,
        )
        .expect("create table");
        db.execute_query(
            "CREATE UNIQUE INDEX uq_email ON users(email)",
            &mut session,
            &mut cache,
        )
        .expect("create index");
        db.execute_query(
            "INSERT INTO users VALUES (1, 'alice@x.com')",
            &mut session,
            &mut cache,
        )
        .expect("insert 1");
        db.execute_query(
            "INSERT INTO users VALUES (2, 'bob@x.com')",
            &mut session,
            &mut cache,
        )
        .expect("insert 2");
    }

    rewrite_server_index_root(dir.path(), "users", "uq_email", 9_999_999);

    let err = match Database::open(dir.path()) {
        Ok(_) => panic!("server open must fail on unreadable index"),
        Err(err) => err,
    };
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

#[test]
fn test_dirty_open_truncates_unlogged_tables_only() {
    let dir = tempfile::tempdir().expect("tempdir");

    {
        let db = Database::open(dir.path()).expect("open server db");
        let mut session = SessionContext::new();
        let mut cache = SchemaCache::new();
        db.execute_query(
            "CREATE TABLE logged_rows (id INT PRIMARY KEY)",
            &mut session,
            &mut cache,
        )
        .expect("create logged table");
        db.execute_query(
            "CREATE UNLOGGED TABLE scratch_rows (id INT PRIMARY KEY)",
            &mut session,
            &mut cache,
        )
        .expect("create unlogged table");
        db.execute_query(
            "INSERT INTO logged_rows VALUES (1)",
            &mut session,
            &mut cache,
        )
        .expect("insert logged row");
        db.execute_query(
            "INSERT INTO scratch_rows VALUES (1)",
            &mut session,
            &mut cache,
        )
        .expect("insert unlogged row");
    }

    mark_database_dirty_open(dir.path());

    let db = Database::open(dir.path()).expect("reopen server db");
    let mut session = SessionContext::new();
    let mut cache = SchemaCache::new();

    let (logged, _) = db
        .execute_query("SELECT COUNT(*) FROM logged_rows", &mut session, &mut cache)
        .expect("count logged rows");
    let (scratch, _) = db
        .execute_query(
            "SELECT COUNT(*) FROM scratch_rows",
            &mut session,
            &mut cache,
        )
        .expect("count unlogged rows");

    let axiomdb_sql::QueryResult::Rows { rows, .. } = logged else {
        panic!("expected rows");
    };
    assert_eq!(rows[0][0], axiomdb_types::Value::BigInt(1));

    let axiomdb_sql::QueryResult::Rows { rows, .. } = scratch else {
        panic!("expected rows");
    };
    assert_eq!(rows[0][0], axiomdb_types::Value::BigInt(0));
}

#[test]
fn test_clean_reopen_preserves_unlogged_tables() {
    let dir = tempfile::tempdir().expect("tempdir");

    {
        let db = Database::open(dir.path()).expect("open server db");
        let mut session = SessionContext::new();
        let mut cache = SchemaCache::new();
        db.execute_query(
            "CREATE UNLOGGED TABLE scratch_rows (id INT PRIMARY KEY)",
            &mut session,
            &mut cache,
        )
        .expect("create unlogged table");
        db.execute_query(
            "INSERT INTO scratch_rows VALUES (1)",
            &mut session,
            &mut cache,
        )
        .expect("insert unlogged row");
    }

    let db = Database::open(dir.path()).expect("reopen server db");
    let mut session = SessionContext::new();
    let mut cache = SchemaCache::new();
    let (scratch, _) = db
        .execute_query(
            "SELECT COUNT(*) FROM scratch_rows",
            &mut session,
            &mut cache,
        )
        .expect("count unlogged rows");

    let axiomdb_sql::QueryResult::Rows { rows, .. } = scratch else {
        panic!("expected rows");
    };
    assert_eq!(rows[0][0], axiomdb_types::Value::BigInt(1));
}
