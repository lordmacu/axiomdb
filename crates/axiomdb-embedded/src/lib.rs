//! # axiomdb-embedded — Embedded in-process mode
//! Compiles as .so/.dll/.dylib with C FFI.
//! Stub — implementation in Phase 10.

/// Open or create a database.
/// # Safety
/// `path` must be a valid pointer to a null-terminated C string.
#[no_mangle]
pub extern "C" fn axiomdb_open(_path: *const std::os::raw::c_char) -> *mut std::ffi::c_void {
    std::ptr::null_mut() // stub
}

/// Close the database.
/// # Safety
/// `db` must be a pointer returned by `axiomdb_open`.
#[no_mangle]
pub extern "C" fn axiomdb_close(_db: *mut std::ffi::c_void) {}
