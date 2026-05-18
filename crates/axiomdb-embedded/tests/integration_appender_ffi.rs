//! C FFI tests for the embedded Appender (Attack 8).
//!
//! Calls the `#[no_mangle] extern "C"` functions through Rust paths
//! to exercise the unsafe FFI boundary. Real C callers see the same
//! ABI; this just lets us test without linking a C binary.
//!
//! Spec: `specs/fase-perf-sqlite-gap/spec-appender-typed-and-ffi.md`
//! Plan: `specs/fase-perf-sqlite-gap/plan-appender-typed-and-ffi.md`

use std::ffi::CString;

use axiomdb_embedded::{
    axiomdb_appender_append_bigint, axiomdb_appender_append_bool, axiomdb_appender_append_bytes,
    axiomdb_appender_append_int, axiomdb_appender_append_null, axiomdb_appender_append_real,
    axiomdb_appender_append_text, axiomdb_appender_end_row, axiomdb_appender_finish,
    axiomdb_appender_flush, axiomdb_appender_free, axiomdb_appender_open, axiomdb_close,
    axiomdb_execute, axiomdb_last_error, axiomdb_open,
};
use tempfile::TempDir;

unsafe fn open_temp_db() -> (TempDir, *mut axiomdb_embedded::Db) {
    let dir = TempDir::new().unwrap();
    let path =
        CString::new(dir.path().join("ffi_test.db").to_str().unwrap().to_string()).unwrap();
    let db = axiomdb_open(path.as_ptr());
    assert!(!db.is_null(), "axiomdb_open returned NULL");
    (dir, db)
}

unsafe fn run_sql(db: *mut axiomdb_embedded::Db, sql: &str) {
    let c = CString::new(sql).unwrap();
    let rc = axiomdb_execute(db, c.as_ptr());
    assert!(rc >= 0, "axiomdb_execute({sql}) failed: rc={rc}");
}

#[test]
fn ffi_appender_open_finish_roundtrip() {
    unsafe {
        let (_dir, db) = open_temp_db();
        run_sql(db, "CREATE TABLE t (i INT, s TEXT)");

        let table = CString::new("t").unwrap();
        let app = axiomdb_appender_open(db, table.as_ptr());
        assert!(!app.is_null());

        assert_eq!(axiomdb_appender_append_int(app, 42), 0);
        let s = CString::new("hello").unwrap();
        assert_eq!(axiomdb_appender_append_text(app, s.as_ptr()), 0);
        assert_eq!(axiomdb_appender_end_row(app), 0);

        let n = axiomdb_appender_finish(app);
        assert_eq!(n, 1, "finish returned wrong count");

        axiomdb_close(db);
    }
}

#[test]
fn ffi_appender_open_missing_table_returns_null() {
    unsafe {
        let (_dir, db) = open_temp_db();
        let table = CString::new("ghost").unwrap();
        let app = axiomdb_appender_open(db, table.as_ptr());
        assert!(app.is_null(), "expected NULL, got {app:?}");
        // Error message is set on db.
        let err = axiomdb_last_error(db);
        assert!(!err.is_null(), "last_error must be set");
        axiomdb_close(db);
    }
}

#[test]
fn ffi_appender_null_inputs_return_error() {
    unsafe {
        let (_dir, db) = open_temp_db();
        // NULL db → NULL appender
        let table = CString::new("t").unwrap();
        assert!(axiomdb_appender_open(std::ptr::null_mut(), table.as_ptr()).is_null());
        // NULL table name → NULL appender
        assert!(axiomdb_appender_open(db, std::ptr::null()).is_null());

        // NULL appender on append_int → -1
        assert_eq!(axiomdb_appender_append_int(std::ptr::null_mut(), 1), -1);
        // NULL appender on end_row → -1
        assert_eq!(axiomdb_appender_end_row(std::ptr::null_mut()), -1);
        // NULL appender on finish → -1
        assert_eq!(axiomdb_appender_finish(std::ptr::null_mut()), -1);
        // NULL appender on free → no-op (no crash)
        axiomdb_appender_free(std::ptr::null_mut());

        axiomdb_close(db);
    }
}

#[test]
fn ffi_appender_text_null_ptr_returns_error() {
    unsafe {
        let (_dir, db) = open_temp_db();
        run_sql(db, "CREATE TABLE t (s TEXT)");
        let table = CString::new("t").unwrap();
        let app = axiomdb_appender_open(db, table.as_ptr());
        assert!(!app.is_null());

        // NULL text pointer → -1
        assert_eq!(axiomdb_appender_append_text(app, std::ptr::null()), -1);

        axiomdb_appender_free(app);
        axiomdb_close(db);
    }
}

#[test]
fn ffi_appender_all_types_roundtrip() {
    unsafe {
        let (_dir, db) = open_temp_db();
        run_sql(
            db,
            "CREATE TABLE t (i INT, big BIGINT, b BOOL, r REAL, s TEXT, bytes_col BLOB, nullable_col INT)",
        );
        let table = CString::new("t").unwrap();
        let app = axiomdb_appender_open(db, table.as_ptr());
        assert!(!app.is_null());

        assert_eq!(axiomdb_appender_append_int(app, 1), 0);
        assert_eq!(
            axiomdb_appender_append_bigint(app, 1_000_000_000_000),
            0
        );
        assert_eq!(axiomdb_appender_append_bool(app, 1), 0); // true
        assert_eq!(axiomdb_appender_append_real(app, 3.14), 0);
        let s = CString::new("hello").unwrap();
        assert_eq!(axiomdb_appender_append_text(app, s.as_ptr()), 0);
        let bytes: &[u8] = &[1, 2, 3, 4, 5];
        assert_eq!(
            axiomdb_appender_append_bytes(app, bytes.as_ptr(), bytes.len()),
            0
        );
        assert_eq!(axiomdb_appender_append_null(app), 0);
        assert_eq!(axiomdb_appender_end_row(app), 0);

        let n = axiomdb_appender_finish(app);
        assert_eq!(n, 1);

        axiomdb_close(db);
    }
}

#[test]
fn ffi_appender_flush_keeps_appender_open() {
    unsafe {
        let (_dir, db) = open_temp_db();
        run_sql(db, "CREATE TABLE t (i INT)");
        let table = CString::new("t").unwrap();
        let app = axiomdb_appender_open(db, table.as_ptr());

        for i in 0..10 {
            assert_eq!(axiomdb_appender_append_int(app, i), 0);
            assert_eq!(axiomdb_appender_end_row(app), 0);
        }
        assert_eq!(axiomdb_appender_flush(app), 0);
        // Still usable.
        for i in 10..20 {
            assert_eq!(axiomdb_appender_append_int(app, i), 0);
            assert_eq!(axiomdb_appender_end_row(app), 0);
        }
        let n = axiomdb_appender_finish(app);
        assert_eq!(n, 20);

        axiomdb_close(db);
    }
}

#[test]
fn ffi_appender_free_rolls_back() {
    unsafe {
        let (_dir, db) = open_temp_db();
        run_sql(db, "CREATE TABLE t (i INT)");
        let table = CString::new("t").unwrap();

        let app = axiomdb_appender_open(db, table.as_ptr());
        for i in 0..5 {
            assert_eq!(axiomdb_appender_append_int(app, i), 0);
            assert_eq!(axiomdb_appender_end_row(app), 0);
        }
        // Free instead of finish — rollback.
        axiomdb_appender_free(app);

        // Verify table is empty.
        // Use a fresh appender to keep the API consistent.
        let app2 = axiomdb_appender_open(db, table.as_ptr());
        assert_eq!(axiomdb_appender_append_int(app2, 999), 0);
        assert_eq!(axiomdb_appender_end_row(app2), 0);
        let n = axiomdb_appender_finish(app2);
        assert_eq!(n, 1, "only the new row should persist");

        axiomdb_close(db);
    }
}

#[test]
fn ffi_appender_arity_mismatch_sets_error() {
    unsafe {
        let (_dir, db) = open_temp_db();
        run_sql(db, "CREATE TABLE t (i INT, s TEXT)");
        let table = CString::new("t").unwrap();
        let app = axiomdb_appender_open(db, table.as_ptr());

        // Append only 1 value, then end_row with 2-col table → error.
        assert_eq!(axiomdb_appender_append_int(app, 1), 0);
        let rc = axiomdb_appender_end_row(app);
        assert_eq!(rc, -1);
        let err = axiomdb_last_error(db);
        assert!(!err.is_null(), "last_error must be set");

        axiomdb_appender_free(app);
        axiomdb_close(db);
    }
}

#[test]
fn ffi_appender_bytes_empty_ok() {
    unsafe {
        let (_dir, db) = open_temp_db();
        run_sql(db, "CREATE TABLE t (b BLOB)");
        let table = CString::new("t").unwrap();
        let app = axiomdb_appender_open(db, table.as_ptr());
        // len=0 with NULL data is OK.
        assert_eq!(
            axiomdb_appender_append_bytes(app, std::ptr::null(), 0),
            0
        );
        assert_eq!(axiomdb_appender_end_row(app), 0);
        let n = axiomdb_appender_finish(app);
        assert_eq!(n, 1);
        axiomdb_close(db);
    }
}

#[test]
fn ffi_appender_bytes_null_with_len_returns_error() {
    unsafe {
        let (_dir, db) = open_temp_db();
        run_sql(db, "CREATE TABLE t (b BLOB)");
        let table = CString::new("t").unwrap();
        let app = axiomdb_appender_open(db, table.as_ptr());
        // NULL data with non-zero len → error.
        assert_eq!(
            axiomdb_appender_append_bytes(app, std::ptr::null(), 4),
            -1
        );
        axiomdb_appender_free(app);
        axiomdb_close(db);
    }
}
