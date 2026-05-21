//! Native PyO3 binding for the AxiomDB embedded engine.
//!
//! Unlike the ctypes binding (`bindings/python/axiomdb.py`), this builds the
//! Python result objects **directly in Rust** — one `PyList` of `PyTuple`s with
//! `PyLong`/`PyFloat`/`PyUnicode`/`PyBytes` cells — exactly mirroring how
//! CPython's `sqlite3` C extension materializes rows. There are zero per-cell
//! FFI/ctypes crossings, so `query()` matches `sqlite3.fetchall()` speed.
//!
//! Importable as `axiomdb_native`:
//!
//! ```python
//! import axiomdb_native as adb
//! conn = adb.connect("app.db")
//! conn.execute("CREATE TABLE t (id INT, name TEXT)")
//! rows = conn.query("SELECT * FROM t")          # list[tuple] (fast)
//! recs = conn.query_dict("SELECT * FROM t")     # list[dict]
//! ```

use pyo3::create_exception;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyTuple};

use axiomdb_core::error::DbError;
use axiomdb_embedded::Db;
use axiomdb_types::Value;

create_exception!(axiomdb_native, AxiomDBError, pyo3::exceptions::PyException);

fn map_err(e: DbError) -> PyErr {
    AxiomDBError::new_err(e.to_string())
}

/// Converts an AxiomDB [`Value`] into a Python object.
///
/// Mapping mirrors the C-FFI `value_to_cell` so all three bindings agree:
/// Bool/Int/BigInt/Date/Timestamp → int, Real/Decimal → float, Text/Json/Jsonb/
/// Uuid → str, Bytes → bytes, Null → None, everything else → its display string.
#[inline]
fn value_to_py(py: Python<'_>, v: &Value) -> PyObject {
    match v {
        Value::Null => py.None(),
        Value::Bool(b) => b.into_py(py),
        Value::Int(i) => i.into_py(py),
        Value::BigInt(i) => i.into_py(py),
        Value::Real(f) => f.into_py(py),
        Value::Decimal(m, s) => (*m as f64 / 10f64.powi(*s as i32)).into_py(py),
        Value::Date(d) => (*d as i64).into_py(py),
        Value::Timestamp(t) | Value::TimestampTz(t) => t.into_py(py),
        Value::Text(s) | Value::Json(s) => s.into_py(py),
        Value::Jsonb(blob) => axiomdb_types::JsonbDecoder::to_string(blob.as_ref())
            .unwrap_or_else(|_| "null".to_string())
            .into_py(py),
        Value::Bytes(b) => PyBytes::new_bound(py, b).to_object(py),
        Value::Uuid(u) => format!(
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            u32::from_be_bytes([u[0], u[1], u[2], u[3]]),
            u16::from_be_bytes([u[4], u[5]]),
            u16::from_be_bytes([u[6], u[7]]),
            u16::from_be_bytes([u[8], u[9]]),
            {
                let mut buf = [0u8; 8];
                buf[2..].copy_from_slice(&u[10..16]);
                u64::from_be_bytes(buf)
            }
        )
        .into_py(py),
        other => other.to_string().into_py(py),
    }
}

/// Builds a `list[tuple]` from owned engine rows, one tuple per row.
#[inline]
fn rows_to_tuples<'py>(py: Python<'py>, rows: &[Vec<Value>]) -> Bound<'py, PyList> {
    PyList::new_bound(
        py,
        rows.iter()
            .map(|row| PyTuple::new_bound(py, row.iter().map(|v| value_to_py(py, v)))),
    )
}

/// An in-process AxiomDB connection.
///
/// Single-threaded by construction (`unsendable`) — like `sqlite3` with
/// `check_same_thread=True`. Use one `Connection` per thread.
#[pyclass(unsendable)]
struct Connection {
    db: Option<Db>,
}

impl Connection {
    fn db_mut(&mut self) -> PyResult<&mut Db> {
        self.db
            .as_mut()
            .ok_or_else(|| AxiomDBError::new_err("connection is closed"))
    }
}

#[pymethods]
impl Connection {
    /// Opens or creates a database at `path` (`":memory:"` for ephemeral).
    #[new]
    fn new(path: &str) -> PyResult<Self> {
        let db = Db::open(path).map_err(map_err)?;
        Ok(Self { db: Some(db) })
    }

    /// Executes a DDL/DML statement. Returns rows affected.
    fn execute(&mut self, sql: &str) -> PyResult<u64> {
        self.db_mut()?.execute(sql).map_err(map_err)
    }

    /// Executes a SELECT and returns rows as `list[tuple]` (fastest path).
    fn query<'py>(&mut self, py: Python<'py>, sql: &str) -> PyResult<Bound<'py, PyList>> {
        let rows = self.db_mut()?.query(sql).map_err(map_err)?;
        Ok(rows_to_tuples(py, &rows))
    }

    /// Executes a SELECT and returns rows as `list[dict]` (column name → value).
    fn query_dict<'py>(&mut self, py: Python<'py>, sql: &str) -> PyResult<Bound<'py, PyList>> {
        let (cols, rows) = self.db_mut()?.query_with_columns(sql).map_err(map_err)?;
        let dicts = rows.iter().map(|row| {
            let d = PyDict::new_bound(py);
            for (name, v) in cols.iter().zip(row.iter()) {
                // Column names are unique per result; set_item cannot fail here.
                let _ = d.set_item(name, value_to_py(py, v));
            }
            d
        });
        Ok(PyList::new_bound(py, dicts))
    }

    /// Executes a SELECT and returns `(column_names, list[tuple])`.
    fn query_with_columns<'py>(
        &mut self,
        py: Python<'py>,
        sql: &str,
    ) -> PyResult<(Vec<String>, Bound<'py, PyList>)> {
        let (cols, rows) = self.db_mut()?.query_with_columns(sql).map_err(map_err)?;
        Ok((cols, rows_to_tuples(py, &rows)))
    }

    /// Begins an explicit transaction.
    fn begin(&mut self) -> PyResult<()> {
        self.db_mut()?.begin().map_err(map_err)
    }

    /// Commits the active explicit transaction.
    fn commit(&mut self) -> PyResult<()> {
        self.db_mut()?.commit().map_err(map_err)
    }

    /// Rolls back the active explicit transaction.
    fn rollback(&mut self) -> PyResult<()> {
        self.db_mut()?.rollback().map_err(map_err)
    }

    /// Returns the last error message, or `None`.
    fn last_error(&self) -> Option<String> {
        self.db.as_ref().and_then(|d| d.last_error().map(str::to_string))
    }

    /// Closes the connection. Safe to call multiple times.
    fn close(&mut self) {
        self.db = None;
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __exit__(
        &mut self,
        _exc_type: &Bound<'_, PyAny>,
        _exc_val: &Bound<'_, PyAny>,
        _exc_tb: &Bound<'_, PyAny>,
    ) {
        self.close();
    }
}

/// Opens or creates a database at `path`. Returns a [`Connection`].
#[pyfunction]
fn connect(path: &str) -> PyResult<Connection> {
    Connection::new(path)
}

/// The `axiomdb_native` extension module.
#[pymodule]
fn axiomdb_native(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Connection>()?;
    m.add_function(wrap_pyfunction!(connect, m)?)?;
    m.add("AxiomDBError", py.get_type_bound::<AxiomDBError>())?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
