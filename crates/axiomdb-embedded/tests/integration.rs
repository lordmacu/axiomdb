//! Integration tests for axiomdb-embedded.
//!
//! Tests the full pipeline: open → DDL → DML → SELECT → close → reopen.
//! Exercises the public Rust API and indirectly validates all C FFI logic.

use axiomdb_catalog::CatalogReader;
use axiomdb_core::DbError;
use axiomdb_embedded::Db;
use axiomdb_storage::{MmapStorage, StorageEngine};
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn open_fresh() -> (Db, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test");
    let db = Db::open(&path).unwrap();
    (db, dir)
}

fn file_uri(path: &std::path::Path) -> String {
    format!("file:{}", path.display())
}

fn rewrite_embedded_index_root(
    path: &std::path::Path,
    table_name: &str,
    target_index_name: &str,
    new_root: u64,
) {
    let db_path = path.with_extension("db");
    let wal_path = path.with_extension("wal");
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

// ── Open / DDL ────────────────────────────────────────────────────────────────

#[test]
fn open_creates_new_database() {
    let (mut db, _dir) = open_fresh();
    db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    let (cols, rows) = db.query_with_columns("SELECT id FROM t").unwrap();
    assert_eq!(cols, vec!["id"]);
    assert_eq!(rows.len(), 0);
}

#[test]
fn open_existing_database_recovers_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("persist");

    {
        let mut db = Db::open(&path).unwrap();
        db.execute("CREATE TABLE items (id INT, label TEXT)")
            .unwrap();
        db.execute("INSERT INTO items VALUES (1, 'alpha')").unwrap();
        db.execute("INSERT INTO items VALUES (2, 'beta')").unwrap();
    }

    let mut db = Db::open(&path).unwrap();
    let rows = db.query("SELECT * FROM items").unwrap();
    assert_eq!(rows.len(), 2);
}

#[test]
fn open_existing_database_fails_for_unreadable_unique_index() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("repair");

    {
        let mut db = Db::open(&path).unwrap();
        db.execute("CREATE TABLE users (id INT PRIMARY KEY, email TEXT)")
            .unwrap();
        db.execute("CREATE UNIQUE INDEX uq_email ON users(email)")
            .unwrap();
        db.execute("INSERT INTO users VALUES (1, 'alice@x.com')")
            .unwrap();
        db.execute("INSERT INTO users VALUES (2, 'bob@x.com')")
            .unwrap();
    }

    rewrite_embedded_index_root(&path, "users", "uq_email", 9_999_999);

    let err = match Db::open(&path) {
        Ok(_) => panic!("embedded open must fail on unreadable index"),
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
fn open_dsn_file_uri_recovers_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("persist.db");
    let dsn = file_uri(&path);

    {
        let mut db = Db::open_dsn(&dsn).unwrap();
        db.execute("CREATE TABLE items (id INT, label TEXT)")
            .unwrap();
        db.execute("INSERT INTO items VALUES (1, 'alpha')").unwrap();
        db.execute("INSERT INTO items VALUES (2, 'beta')").unwrap();
    }

    let mut db = Db::open_dsn(&dsn).unwrap();
    let rows = db.query("SELECT * FROM items ORDER BY id").unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], Value::Text("alpha".into()));
    assert_eq!(rows[1][1], Value::Text("beta".into()));
}

#[test]
fn open_dsn_axiomdb_local_recovers_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("persist");
    let dsn = format!("axiomdb://{}", path.display());

    {
        let mut db = Db::open_dsn(&dsn).unwrap();
        db.execute("CREATE TABLE items (id INT, label TEXT)")
            .unwrap();
        db.execute("INSERT INTO items VALUES (7, 'persisted')")
            .unwrap();
    }

    let mut db = Db::open_dsn(&dsn).unwrap();
    let rows = db.query("SELECT id, label FROM items").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(7));
    assert_eq!(rows[0][1], Value::Text("persisted".into()));
}

#[test]
fn open_dsn_rejects_remote_wire_endpoints() {
    let err = Db::open_dsn("postgres://user@127.0.0.1:5432/app")
        .err()
        .expect("remote DSN should be rejected");
    assert!(matches!(
        err,
        DbError::InvalidDsn { reason }
            if reason.contains("embedded open_dsn only supports local-path DSNs")
    ));
}

#[test]
fn open_dsn_rejects_query_params_in_embedded_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("query.db");
    let dsn = format!("{}?mode=rw", file_uri(&path));
    let err = Db::open_dsn(&dsn)
        .err()
        .expect("embedded DSN query params should be rejected");
    assert!(matches!(
        err,
        DbError::InvalidDsn { reason } if reason.contains("does not support query parameters")
    ));
}

// ── INSERT / SELECT ───────────────────────────────────────────────────────────

#[test]
fn insert_and_select_all_value_types() {
    let (mut db, _dir) = open_fresh();
    db.execute(
        "CREATE TABLE types (
            i INT, b BIGINT, r REAL, t TEXT, bl BOOL
        )",
    )
    .unwrap();
    db.execute("INSERT INTO types VALUES (42, 9000000000, 3.14, 'hello', TRUE)")
        .unwrap();

    let rows = db.query("SELECT * FROM types").unwrap();
    assert_eq!(rows.len(), 1);

    let row = &rows[0];
    assert_eq!(row[0], Value::Int(42));
    assert_eq!(row[1], Value::BigInt(9_000_000_000));
    let expected_real = "3.14".parse::<f64>().unwrap();
    assert!(matches!(row[2], Value::Real(f) if (f - expected_real).abs() < 1e-9));
    assert_eq!(row[3], Value::Text("hello".into()));
    assert_eq!(row[4], Value::Bool(true));
}

#[test]
fn insert_null_values() {
    let (mut db, _dir) = open_fresh();
    db.execute("CREATE TABLE nullable (id INT, name TEXT)")
        .unwrap();
    db.execute("INSERT INTO nullable VALUES (1, NULL)").unwrap();

    let rows = db.query("SELECT * FROM nullable").unwrap();
    assert_eq!(rows[0][1], Value::Null);
}

#[test]
fn execute_returns_affected_count() {
    let (mut db, _dir) = open_fresh();
    db.execute("CREATE TABLE c (id INT, v INT)").unwrap();
    for i in 1..=5 {
        db.execute(&format!("INSERT INTO c VALUES ({i}, {i})"))
            .unwrap();
    }

    let n = db.execute("UPDATE c SET v = 0 WHERE id <= 3").unwrap();
    assert_eq!(n, 3);

    let n = db.execute("DELETE FROM c WHERE id > 3").unwrap();
    assert_eq!(n, 2);
}

// ── query_with_columns ────────────────────────────────────────────────────────

#[test]
fn query_with_columns_returns_correct_names() {
    let (mut db, _dir) = open_fresh();
    db.execute("CREATE TABLE products (id INT, name TEXT, price REAL)")
        .unwrap();
    db.execute("INSERT INTO products VALUES (1, 'Widget', 9.99)")
        .unwrap();
    db.execute("INSERT INTO products VALUES (2, 'Gadget', 24.50)")
        .unwrap();

    let (cols, rows) = db
        .query_with_columns("SELECT id, name FROM products")
        .unwrap();

    assert_eq!(cols, vec!["id", "name"]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[0][1], Value::Text("Widget".into()));
}

#[test]
fn query_with_columns_empty_result() {
    let (mut db, _dir) = open_fresh();
    db.execute("CREATE TABLE empty (id INT)").unwrap();

    let (cols, rows) = db
        .query_with_columns("SELECT id FROM empty WHERE id = 999")
        .unwrap();
    assert_eq!(cols, vec!["id"]);
    assert!(rows.is_empty());
}

#[test]
fn query_with_columns_where_filter() {
    let (mut db, _dir) = open_fresh();
    db.execute("CREATE TABLE users (id INT, active BOOL)")
        .unwrap();
    db.execute("INSERT INTO users VALUES (1, TRUE)").unwrap();
    db.execute("INSERT INTO users VALUES (2, FALSE)").unwrap();
    db.execute("INSERT INTO users VALUES (3, TRUE)").unwrap();

    let (cols, rows) = db
        .query_with_columns("SELECT id FROM users WHERE active = TRUE")
        .unwrap();
    assert_eq!(cols, vec!["id"]);
    assert_eq!(rows.len(), 2);
}

// ── Transactions ──────────────────────────────────────────────────────────────

#[test]
fn explicit_transaction_commit() {
    let (mut db, _dir) = open_fresh();
    db.execute("CREATE TABLE tx (id INT)").unwrap();

    db.begin().unwrap();
    db.execute("INSERT INTO tx VALUES (1)").unwrap();
    db.execute("INSERT INTO tx VALUES (2)").unwrap();
    db.commit().unwrap();

    let rows = db.query("SELECT * FROM tx").unwrap();
    assert_eq!(rows.len(), 2);
}

#[test]
fn explicit_transaction_rollback() {
    let (mut db, _dir) = open_fresh();
    db.execute("CREATE TABLE tx (id INT)").unwrap();
    db.execute("INSERT INTO tx VALUES (1)").unwrap();

    db.begin().unwrap();
    db.execute("INSERT INTO tx VALUES (2)").unwrap();
    db.execute("INSERT INTO tx VALUES (3)").unwrap();
    db.rollback().unwrap();

    // Only the pre-transaction row survives
    let rows = db.query("SELECT * FROM tx").unwrap();
    assert_eq!(rows.len(), 1);
}

// ── Error handling + last_error ───────────────────────────────────────────────

#[test]
fn error_on_missing_table_sets_last_error() {
    let (mut db, _dir) = open_fresh();

    let result = db.query("SELECT * FROM does_not_exist");
    assert!(result.is_err());
    // last_error is set by run() on any error
    assert!(
        db.last_error().is_some(),
        "last_error should be set after a failed query"
    );
}

#[test]
fn success_clears_last_error() {
    let (mut db, _dir) = open_fresh();
    db.execute("CREATE TABLE t (id INT)").unwrap();

    // Cause an error
    let _ = db.query("SELECT * FROM nonexistent");
    assert!(db.last_error().is_some());

    // Successful query clears it
    db.query("SELECT * FROM t").unwrap();
    assert!(db.last_error().is_none());
}

#[test]
fn duplicate_create_table_returns_error() {
    let (mut db, _dir) = open_fresh();
    db.execute("CREATE TABLE dup (id INT)").unwrap();
    let result = db.execute("CREATE TABLE dup (id INT)");
    assert!(result.is_err());
}

// ── Scalar expressions (SELECT without FROM) ──────────────────────────────────

#[test]
fn select_without_from() {
    let (mut db, _dir) = open_fresh();
    let rows = db.query("SELECT 1 + 1").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(2));
}

// ── C FFI — smoke test via #[no_mangle] symbols ───────────────────────────────

#[cfg(feature = "async-api")]
mod async_api {
    use axiomdb_embedded::async_db::AsyncDb;
    use axiomdb_types::Value;

    use super::file_uri;

    #[tokio::test]
    async fn async_open_dsn_works_for_local_file_uri() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("async.db");
        let dsn = file_uri(&path);

        let db = AsyncDb::open_dsn(&dsn).await.unwrap();
        db.execute("CREATE TABLE t (id INT, name TEXT)")
            .await
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 'async')")
            .await
            .unwrap();

        let rows = db.query("SELECT id, name FROM t").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Int(1));
        assert_eq!(rows[0][1], Value::Text("async".into()));
    }
}

#[cfg(feature = "c-ffi")]
mod c_ffi {
    use std::ffi::CString;
    use std::os::raw::c_char;

    // The #[no_mangle] functions are compiled into the rlib that integration
    // tests link against. We forward-declare them here as extern "C".
    extern "C" {
        fn axiomdb_open(path: *const c_char) -> *mut std::ffi::c_void;
        fn axiomdb_open_dsn(path: *const c_char) -> *mut std::ffi::c_void;
        fn axiomdb_execute(db: *mut std::ffi::c_void, sql: *const c_char) -> i64;
        fn axiomdb_query(db: *mut std::ffi::c_void, sql: *const c_char) -> *mut std::ffi::c_void;
        fn axiomdb_rows_count(rows: *const std::ffi::c_void) -> i64;
        fn axiomdb_rows_columns(rows: *const std::ffi::c_void) -> i32;
        fn axiomdb_rows_column_name(rows: *const std::ffi::c_void, col: i32) -> *const c_char;
        fn axiomdb_rows_type(rows: *const std::ffi::c_void, row: i64, col: i32) -> i32;
        fn axiomdb_rows_get_int(rows: *const std::ffi::c_void, row: i64, col: i32) -> i64;
        fn axiomdb_rows_get_text(
            rows: *const std::ffi::c_void,
            row: i64,
            col: i32,
        ) -> *const c_char;
        fn axiomdb_rows_free(rows: *mut std::ffi::c_void);
        fn axiomdb_last_error(db: *const std::ffi::c_void) -> *const c_char;
        fn axiomdb_close(db: *mut std::ffi::c_void);
    }

    fn cstr(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    unsafe fn ptr_to_str(ptr: *const c_char) -> &'static str {
        std::ffi::CStr::from_ptr(ptr).to_str().unwrap()
    }

    fn file_uri(path: &std::path::Path) -> CString {
        cstr(&format!("file:{}", path.display()))
    }

    #[test]
    fn c_ffi_open_execute_query_close() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c_test");
        let path_str = cstr(path.to_str().unwrap());

        unsafe {
            let db = axiomdb_open(path_str.as_ptr());
            assert!(!db.is_null(), "axiomdb_open should succeed");

            // DDL
            let n = axiomdb_execute(
                db,
                cstr("CREATE TABLE users (id INT, name TEXT, score REAL)").as_ptr(),
            );
            assert_eq!(n, 0);

            // INSERT
            axiomdb_execute(
                db,
                cstr("INSERT INTO users VALUES (1, 'Alice', 9.5)").as_ptr(),
            );
            axiomdb_execute(
                db,
                cstr("INSERT INTO users VALUES (2, 'Bob', 7.2)").as_ptr(),
            );
            axiomdb_execute(
                db,
                cstr("INSERT INTO users VALUES (3, 'Carol', 8.8)").as_ptr(),
            );

            // SELECT
            let rows = axiomdb_query(db, cstr("SELECT id, name, score FROM users").as_ptr());
            assert!(!rows.is_null(), "axiomdb_query should return rows");

            assert_eq!(axiomdb_rows_count(rows), 3);
            assert_eq!(axiomdb_rows_columns(rows), 3);

            // Column names
            assert_eq!(ptr_to_str(axiomdb_rows_column_name(rows, 0)), "id");
            assert_eq!(ptr_to_str(axiomdb_rows_column_name(rows, 1)), "name");
            assert_eq!(ptr_to_str(axiomdb_rows_column_name(rows, 2)), "score");

            // Type codes: id=INTEGER(1), name=TEXT(3), score=REAL(2)
            assert_eq!(axiomdb_rows_type(rows, 0, 0), 1); // INTEGER
            assert_eq!(axiomdb_rows_type(rows, 0, 1), 3); // TEXT
            assert_eq!(axiomdb_rows_type(rows, 0, 2), 2); // REAL

            // Values — row 0 = Alice
            assert_eq!(axiomdb_rows_get_int(rows, 0, 0), 1);
            assert_eq!(ptr_to_str(axiomdb_rows_get_text(rows, 0, 1)), "Alice");

            // Values — row 1 = Bob
            assert_eq!(axiomdb_rows_get_int(rows, 1, 0), 2);
            assert_eq!(ptr_to_str(axiomdb_rows_get_text(rows, 1, 1)), "Bob");

            axiomdb_rows_free(rows);

            // Error path: last_error is NULL after success
            assert!(axiomdb_last_error(db).is_null());

            // Error path: query on missing table sets last_error
            let bad = axiomdb_query(db, cstr("SELECT * FROM ghost").as_ptr());
            assert!(bad.is_null(), "bad query should return NULL");
            let err_ptr = axiomdb_last_error(db);
            assert!(!err_ptr.is_null(), "last_error should be set");
            let err_msg = ptr_to_str(err_ptr);
            assert!(!err_msg.is_empty(), "error message should not be empty");

            axiomdb_close(db);
        }
    }

    #[test]
    fn c_ffi_null_inputs_do_not_crash() {
        unsafe {
            // NULL path → NULL handle
            let db = axiomdb_open(std::ptr::null());
            assert!(db.is_null());

            // NULL db → safe no-op or -1
            let n = axiomdb_execute(std::ptr::null_mut(), cstr("SELECT 1").as_ptr());
            assert_eq!(n, -1);

            axiomdb_rows_free(std::ptr::null_mut()); // must not crash
            axiomdb_close(std::ptr::null_mut()); // must not crash
        }
    }

    #[test]
    fn c_ffi_open_dsn_accepts_local_file_uri() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c_dsn.db");
        let dsn = file_uri(&path);

        unsafe {
            let db = axiomdb_open_dsn(dsn.as_ptr());
            assert!(
                !db.is_null(),
                "axiomdb_open_dsn should succeed for file URI"
            );
            let n = axiomdb_execute(db, cstr("CREATE TABLE t (id INT)").as_ptr());
            assert_eq!(n, 0);
            axiomdb_close(db);
        }
    }
}
