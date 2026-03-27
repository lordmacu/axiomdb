use axiomdb_catalog::CatalogReader;
use axiomdb_core::error::DbError;
use axiomdb_network::mysql::Database;
use axiomdb_sql::{SchemaCache, SessionContext};
use axiomdb_storage::{MmapStorage, StorageEngine};
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
    txn.begin().expect("begin catalog txn");
    {
        let mut writer =
            axiomdb_catalog::CatalogWriter::new(&mut storage, &mut txn).expect("catalog writer");
        writer
            .update_index_root(target.index_id, new_root)
            .expect("rewrite root");
    }
    txn.commit().expect("commit catalog txn");
    storage.flush().expect("flush corrupted index");
}

#[test]
fn test_database_open_fails_for_unreadable_unique_index() {
    let dir = tempfile::tempdir().expect("tempdir");

    {
        let mut db = Database::open(dir.path()).expect("open server db");
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
