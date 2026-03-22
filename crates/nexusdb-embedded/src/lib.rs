//! # nexusdb-embedded — Modo embebido in-process
//! Compila como .so/.dll/.dylib con C FFI.
//! Stub — implementación en Fase 10.

/// Abrir o crear una base de datos.
/// # Safety
/// `path` debe ser un puntero válido a una string C terminada en null.
#[no_mangle]
pub extern "C" fn nexusdb_open(_path: *const std::os::raw::c_char) -> *mut std::ffi::c_void {
    std::ptr::null_mut() // stub
}

/// Cerrar la base de datos.
/// # Safety
/// `db` debe ser un puntero retornado por `nexusdb_open`.
#[no_mangle]
pub extern "C" fn nexusdb_close(_db: *mut std::ffi::c_void) {}
