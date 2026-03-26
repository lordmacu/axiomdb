//! Integration tests for axiomdb-embedded.
//!
//! Tests the full pipeline: open → DDL → DML → SELECT → close → reopen.
//! Exercises the public Rust API and indirectly validates all C FFI logic.

use axiomdb_embedded::Db;
use axiomdb_types::Value;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn open_fresh() -> (Db, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test");
    let db = Db::open(&path).unwrap();
    (db, dir)
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
    assert!(matches!(row[2], Value::Real(f) if (f - 3.14).abs() < 1e-9));
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

#[cfg(feature = "c-ffi")]
mod c_ffi {
    use std::ffi::CString;
    use std::os::raw::c_char;

    // The #[no_mangle] functions are compiled into the rlib that integration
    // tests link against. We forward-declare them here as extern "C".
    extern "C" {
        fn axiomdb_open(path: *const c_char) -> *mut std::ffi::c_void;
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
}
