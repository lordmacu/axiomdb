"""
AxiomDB Python binding — ctypes wrapper over libaxiomdb_embedded.

Usage:
    from axiomdb import AxiomDB

    db = AxiomDB("./myapp.db")
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)")
    db.execute("INSERT INTO users VALUES (1, 'Alice')")
    db.execute("INSERT INTO users VALUES (2, 'Bob')")

    for row in db.query("SELECT * FROM users"):
        print(row)  # {'id': 1, 'name': 'Alice'}

    db.close()

    # Or as context manager:
    with AxiomDB("./test.db") as db:
        db.execute("CREATE TABLE t (x INT)")
        rows = db.query("SELECT * FROM t")
"""

import ctypes
import os
import platform
import struct
import sys
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

# ── Library loading ──────────────────────────────────────────────────────────

def _find_library() -> str:
    """Find libaxiomdb_embedded shared library."""
    system = platform.system()
    if system == "Darwin":
        ext = "dylib"
    elif system == "Windows":
        ext = "dll"
    else:
        ext = "so"

    name = f"libaxiomdb_embedded.{ext}"

    # Search paths in order of priority
    search_paths = [
        # 1. Same directory as this script
        Path(__file__).parent / name,
        # 2. Relative to project root (development)
        Path(__file__).parent / ".." / ".." / "target" / "release" / name,
        Path(__file__).parent / ".." / ".." / "target" / "debug" / name,
        # 3. System library paths
        Path("/usr/local/lib") / name,
        Path("/usr/lib") / name,
    ]

    for path in search_paths:
        resolved = path.resolve()
        if resolved.exists():
            return str(resolved)

    # Fallback: let ctypes search system paths
    return name


_lib = ctypes.cdll.LoadLibrary(_find_library())

# ── Type codes ───────────────────────────────────────────────────────────────

TYPE_NULL = 0
TYPE_INTEGER = 1
TYPE_REAL = 2
TYPE_TEXT = 3
TYPE_BLOB = 4

# ── Packed buffer format (must match crates/axiomdb-embedded/src/lib.rs) ───────

_PACKED_MAGIC = 0x41584D31  # "AXM1"
_COLUMNAR_MAGIC = 0x41584D32  # "AXM2"
_U32 = struct.Struct("<I")
_U64 = struct.Struct("<Q")
_I64 = struct.Struct("<q")
_F64 = struct.Struct("<d")


def _parse_packed(buf: bytes) -> Tuple[List[str], List[tuple]]:
    """Parses a packed result buffer into (column_names, rows-as-tuples).

    Single pass over the buffer; all per-cell work stays in this loop. Hot-path
    locals are bound up front to avoid attribute lookups inside the loop.
    """
    u32_from = _U32.unpack_from
    i64_from = _I64.unpack_from
    f64_from = _F64.unpack_from

    magic = u32_from(buf, 0)[0]
    if magic != _PACKED_MAGIC:
        raise AxiomDBError(f"corrupt packed buffer (magic={magic:#x})")
    n_cols = u32_from(buf, 4)[0]
    n_rows = _U64.unpack_from(buf, 8)[0]
    off = 16

    col_names: List[str] = []
    for _ in range(n_cols):
        ln = u32_from(buf, off)[0]
        off += 4
        col_names.append(buf[off:off + ln].decode("utf-8"))
        off += ln

    # Hot loop: slicing a `bytes` object (`buf[a:b]`) and `.decode()` are both
    # C-implemented and measured ~13% faster here than a memoryview + str().
    rows: List[tuple] = []
    append = rows.append
    for _ in range(n_rows):
        row = [None] * n_cols
        for c in range(n_cols):
            tag = buf[off]
            off += 1
            if tag == TYPE_INTEGER:
                row[c] = i64_from(buf, off)[0]
                off += 8
            elif tag == TYPE_TEXT:
                ln = u32_from(buf, off)[0]
                off += 4
                row[c] = buf[off:off + ln].decode("utf-8")
                off += ln
            elif tag == TYPE_REAL:
                row[c] = f64_from(buf, off)[0]
                off += 8
            elif tag == TYPE_BLOB:
                ln = u32_from(buf, off)[0]
                off += 4
                row[c] = buf[off:off + ln]
                off += ln
            # tag == TYPE_NULL → leave None
        append(tuple(row))
    return col_names, rows


def _parse_columnar(buf: bytes) -> Tuple[List[str], List[tuple]]:
    """Parses a columnar (AXM2) buffer into (column_names, rows-as-tuples).

    Homogeneous columns are bulk-decoded with a single ``struct.unpack_from``
    (one C call for the whole column), and rows are assembled with ``zip`` (also
    C-level) — far less interpreted per-cell work than the row-major parser.
    Columns with NULLs/mixed types use a per-cell ('M') fallback.
    """
    u32_from = _U32.unpack_from
    magic = u32_from(buf, 0)[0]
    if magic != _COLUMNAR_MAGIC:
        raise AxiomDBError(f"corrupt columnar buffer (magic={magic:#x})")
    n_cols = u32_from(buf, 4)[0]
    n_rows = _U64.unpack_from(buf, 8)[0]
    off = 16

    names: List[str] = []
    for _ in range(n_cols):
        ln = u32_from(buf, off)[0]
        off += 4
        names.append(buf[off:off + ln].decode("utf-8"))
        off += ln

    columns: List[tuple] = []
    for _ in range(n_cols):
        kind = buf[off]
        off += 1
        if kind == 0x49:  # 'I' — bulk int64
            columns.append(struct.unpack_from(f"<{n_rows}q", buf, off))
            off += n_rows * 8
        elif kind == 0x46:  # 'F' — bulk float64
            columns.append(struct.unpack_from(f"<{n_rows}d", buf, off))
            off += n_rows * 8
        elif kind in (0x54, 0x42):  # 'T' / 'B' — bulk lengths, then slice
            lens = struct.unpack_from(f"<{n_rows}I", buf, off)
            off += n_rows * 4
            binary = kind == 0x42
            arr = [None] * n_rows
            for i in range(n_rows):
                ln = lens[i]
                s = buf[off:off + ln]
                arr[i] = s if binary else s.decode("utf-8")
                off += ln
            columns.append(arr)
        else:  # 'M' — per-cell (handles NULL / mixed)
            i64_from = _I64.unpack_from
            f64_from = _F64.unpack_from
            arr = [None] * n_rows
            for i in range(n_rows):
                tag = buf[off]
                off += 1
                if tag == TYPE_INTEGER:
                    arr[i] = i64_from(buf, off)[0]
                    off += 8
                elif tag == TYPE_TEXT:
                    ln = u32_from(buf, off)[0]
                    off += 4
                    arr[i] = buf[off:off + ln].decode("utf-8")
                    off += ln
                elif tag == TYPE_REAL:
                    arr[i] = f64_from(buf, off)[0]
                    off += 8
                elif tag == TYPE_BLOB:
                    ln = u32_from(buf, off)[0]
                    off += 4
                    arr[i] = buf[off:off + ln]
                    off += ln
                # TYPE_NULL → leave None
            columns.append(arr)

    rows = list(zip(*columns)) if (n_rows and n_cols) else []
    return names, rows

# ── C function signatures ───────────────────────────────────────────────────

_lib.axiomdb_open.argtypes = [ctypes.c_char_p]
_lib.axiomdb_open.restype = ctypes.c_void_p

_lib.axiomdb_execute.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
_lib.axiomdb_execute.restype = ctypes.c_int64

_lib.axiomdb_query.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
_lib.axiomdb_query.restype = ctypes.c_void_p

_lib.axiomdb_close.argtypes = [ctypes.c_void_p]
_lib.axiomdb_close.restype = None

_lib.axiomdb_rows_count.argtypes = [ctypes.c_void_p]
_lib.axiomdb_rows_count.restype = ctypes.c_int64

_lib.axiomdb_rows_columns.argtypes = [ctypes.c_void_p]
_lib.axiomdb_rows_columns.restype = ctypes.c_int32

_lib.axiomdb_rows_column_name.argtypes = [ctypes.c_void_p, ctypes.c_int32]
_lib.axiomdb_rows_column_name.restype = ctypes.c_char_p

_lib.axiomdb_rows_type.argtypes = [ctypes.c_void_p, ctypes.c_int64, ctypes.c_int32]
_lib.axiomdb_rows_type.restype = ctypes.c_int32

_lib.axiomdb_rows_get_int.argtypes = [ctypes.c_void_p, ctypes.c_int64, ctypes.c_int32]
_lib.axiomdb_rows_get_int.restype = ctypes.c_int64

_lib.axiomdb_rows_get_double.argtypes = [ctypes.c_void_p, ctypes.c_int64, ctypes.c_int32]
_lib.axiomdb_rows_get_double.restype = ctypes.c_double

_lib.axiomdb_rows_get_text.argtypes = [ctypes.c_void_p, ctypes.c_int64, ctypes.c_int32]
_lib.axiomdb_rows_get_text.restype = ctypes.c_char_p

_lib.axiomdb_rows_get_blob.argtypes = [ctypes.c_void_p, ctypes.c_int64, ctypes.c_int32, ctypes.POINTER(ctypes.c_size_t)]
_lib.axiomdb_rows_get_blob.restype = ctypes.c_void_p

_lib.axiomdb_rows_free.argtypes = [ctypes.c_void_p]
_lib.axiomdb_rows_free.restype = None

_lib.axiomdb_last_error.argtypes = [ctypes.c_void_p]
_lib.axiomdb_last_error.restype = ctypes.c_char_p

# ── Packed result buffer (single-FFI-call materialization) ────────────────────
# axiomdb_query_packed serializes the whole result into ONE contiguous buffer so
# the binding crosses the FFI boundary once per query instead of ~2× per cell.

_lib.axiomdb_query_packed.argtypes = [
    ctypes.c_void_p, ctypes.c_char_p, ctypes.POINTER(ctypes.c_size_t)
]
_lib.axiomdb_query_packed.restype = ctypes.c_void_p

# Columnar (AXM2): bulk-decodable layout — far faster to parse in Python.
_lib.axiomdb_query_packed_columnar.argtypes = [
    ctypes.c_void_p, ctypes.c_char_p, ctypes.POINTER(ctypes.c_size_t)
]
_lib.axiomdb_query_packed_columnar.restype = ctypes.c_void_p

_lib.axiomdb_packed_free.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
_lib.axiomdb_packed_free.restype = None

# ── Appender FFI (Attack 8) ──────────────────────────────────────────────────

_lib.axiomdb_appender_open.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
_lib.axiomdb_appender_open.restype = ctypes.c_void_p

_lib.axiomdb_appender_append_int.argtypes = [ctypes.c_void_p, ctypes.c_int32]
_lib.axiomdb_appender_append_int.restype = ctypes.c_int

_lib.axiomdb_appender_append_bigint.argtypes = [ctypes.c_void_p, ctypes.c_int64]
_lib.axiomdb_appender_append_bigint.restype = ctypes.c_int

_lib.axiomdb_appender_append_bool.argtypes = [ctypes.c_void_p, ctypes.c_int]
_lib.axiomdb_appender_append_bool.restype = ctypes.c_int

_lib.axiomdb_appender_append_real.argtypes = [ctypes.c_void_p, ctypes.c_double]
_lib.axiomdb_appender_append_real.restype = ctypes.c_int

_lib.axiomdb_appender_append_text.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
_lib.axiomdb_appender_append_text.restype = ctypes.c_int

_lib.axiomdb_appender_append_bytes.argtypes = [
    ctypes.c_void_p, ctypes.POINTER(ctypes.c_uint8), ctypes.c_size_t
]
_lib.axiomdb_appender_append_bytes.restype = ctypes.c_int

_lib.axiomdb_appender_append_null.argtypes = [ctypes.c_void_p]
_lib.axiomdb_appender_append_null.restype = ctypes.c_int

_lib.axiomdb_appender_end_row.argtypes = [ctypes.c_void_p]
_lib.axiomdb_appender_end_row.restype = ctypes.c_int

_lib.axiomdb_appender_flush.argtypes = [ctypes.c_void_p]
_lib.axiomdb_appender_flush.restype = ctypes.c_int

_lib.axiomdb_appender_finish.argtypes = [ctypes.c_void_p]
_lib.axiomdb_appender_finish.restype = ctypes.c_int64

_lib.axiomdb_appender_free.argtypes = [ctypes.c_void_p]
_lib.axiomdb_appender_free.restype = None

# ── Python API ───────────────────────────────────────────────────────────────


class AxiomDBError(Exception):
    """Raised when an AxiomDB operation fails."""
    pass


class AxiomDB:
    """AxiomDB embedded database — in-process, no server needed.

    Compatible with SQLite-style usage: open a file, execute SQL, query rows.
    Uses the AxiomDB engine (B+ Tree, MVCC, WAL) under the hood.
    """

    def __init__(self, path: str):
        """Open or create a database at the given file path."""
        self._ptr = _lib.axiomdb_open(path.encode("utf-8"))
        if not self._ptr:
            raise AxiomDBError(f"Failed to open database at '{path}'")

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc_val, exc_tb):
        self.close()

    def close(self):
        """Close the database. Safe to call multiple times."""
        if self._ptr:
            _lib.axiomdb_close(self._ptr)
            self._ptr = None

    def execute(self, sql: str) -> int:
        """Execute a SQL statement (INSERT, UPDATE, DELETE, DDL).

        Returns the number of rows affected.
        Raises AxiomDBError on failure.
        """
        self._check_open()
        result = _lib.axiomdb_execute(self._ptr, sql.encode("utf-8"))
        if result < 0:
            raise AxiomDBError(self._last_error() or "execute failed")
        return result

    def _query_packed(self, sql: str) -> Tuple[List[str], List[tuple]]:
        """Run a query and materialize it via the columnar buffer (one FFI call).

        Returns (column_names, rows-as-tuples). Internal — used by `query`,
        `query_tuples`, and `query_with_columns`. The columnar (AXM2) layout
        lets Python bulk-decode numeric columns with one ``struct.unpack`` and
        assemble rows with ``zip`` — much less interpreted per-cell work than
        row-major.
        """
        self._check_open()
        length = ctypes.c_size_t(0)
        ptr = _lib.axiomdb_query_packed_columnar(
            self._ptr, sql.encode("utf-8"), ctypes.byref(length)
        )
        if not ptr:
            raise AxiomDBError(self._last_error() or "query failed")
        try:
            # One copy of the whole buffer into a Python bytes object, then a
            # single columnar parse (no per-cell FFI crossings).
            buf = ctypes.string_at(ptr, length.value)
        finally:
            _lib.axiomdb_packed_free(ptr, length.value)
        return _parse_columnar(buf)

    def query(self, sql: str) -> List[Dict[str, Any]]:
        """Execute a SELECT and return rows as a list of dicts.

        Each dict maps column name → Python value.
        Types: int, float, str, bytes, None (NULL).

        Uses the packed-buffer path (one FFI call); for the fastest
        materialization use `query_tuples` (skips dict construction).
        """
        col_names, rows = self._query_packed(sql)
        return [dict(zip(col_names, r)) for r in rows]

    def query_tuples(self, sql: str) -> List[tuple]:
        """Execute a SELECT and return rows as a list of tuples.

        Fastest materialization path — matches `sqlite3.Cursor.fetchall()`
        shape (positional tuples, no column-name dict construction).
        """
        _col_names, rows = self._query_packed(sql)
        return rows

    def query_with_columns(self, sql: str) -> Tuple[List[str], List[tuple]]:
        """Execute a SELECT and return (column_names, rows-as-tuples).

        Use when you need both the column names and the fast tuple shape.
        """
        return self._query_packed(sql)

    def last_error(self) -> Optional[str]:
        """Return the last error message, or None if no error."""
        return self._last_error()

    def _check_open(self):
        if not self._ptr:
            raise AxiomDBError("Database is closed")

    def _last_error(self) -> Optional[str]:
        if not self._ptr:
            return None
        err = _lib.axiomdb_last_error(self._ptr)
        if err:
            return err.decode("utf-8")
        return None

    def appender(self, table: str) -> "Appender":
        """Open a fast-path Appender for high-throughput INSERT.

        Skips the SQL parser/analyzer/dispatcher — typed values are
        written directly to the heap. ~5-50× faster than `INSERT`
        statements for bulk loads.

        Use as a context manager OR call `finish()` explicitly.
        Dropping without `finish()` rolls back the appender's txn.

        Example:
            with db.appender("users") as app:
                for i in range(10000):
                    app.append_int(i)
                    app.append_text(f"user_{i}")
                    app.end_row()
                # finish() called automatically on __exit__
        """
        self._check_open()
        ptr = _lib.axiomdb_appender_open(self._ptr, table.encode("utf-8"))
        if not ptr:
            raise AxiomDBError(
                self._last_error() or f"appender_open('{table}') failed"
            )
        return Appender(self, ptr)

    def __del__(self):
        self.close()


class Appender:
    """Fast-path INSERT builder. Created via `Db.appender(table)`.

    Per-column setters (`append_int`, `append_text`, ...) followed by
    `end_row()` to commit each row. `finish()` flushes + commits +
    consumes the appender; dropping without `finish()` rolls back.

    Mirrors DuckDB's Appender + SQLite's `sqlite3_bind_*` patterns.

    v1 limitations: heap and clustered tables OK; tables with triggers
    are rejected at open. CHECK/FK/AUTO_INCREMENT/GENERATED ALWAYS
    work. UNIQUE indexes work (per-row path); empty NON-UNIQUE
    secondary indexes get a bulk-build fast path (~5× speedup).
    """

    def __init__(self, db: "AxiomDB", ptr: int):
        # Keep a reference to db so it outlives the appender (caller
        # responsibility per the C FFI safety contract).
        self._db = db
        self._ptr = ptr

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc_val, exc_tb):
        if self._ptr:
            if exc_type is None:
                # Normal exit — commit.
                self.finish()
            else:
                # Exception in the with-block — rollback.
                _lib.axiomdb_appender_free(self._ptr)
                self._ptr = None

    def _check_open(self):
        if not self._ptr:
            raise AxiomDBError("Appender is closed")

    def _check_rc(self, rc: int, op: str):
        if rc != 0:
            err = self._db._last_error() or f"{op} failed"
            raise AxiomDBError(err)

    def append_int(self, v: int):
        """Append an INT (i32) value."""
        self._check_open()
        self._check_rc(
            _lib.axiomdb_appender_append_int(self._ptr, v), "append_int"
        )

    def append_bigint(self, v: int):
        """Append a BIGINT (i64) value."""
        self._check_open()
        self._check_rc(
            _lib.axiomdb_appender_append_bigint(self._ptr, v), "append_bigint"
        )

    def append_bool(self, v: bool):
        """Append a BOOL value."""
        self._check_open()
        self._check_rc(
            _lib.axiomdb_appender_append_bool(self._ptr, 1 if v else 0),
            "append_bool",
        )

    def append_real(self, v: float):
        """Append a REAL/DOUBLE value."""
        self._check_open()
        self._check_rc(
            _lib.axiomdb_appender_append_real(self._ptr, v), "append_real"
        )

    def append_text(self, v: str):
        """Append a TEXT value (UTF-8)."""
        self._check_open()
        self._check_rc(
            _lib.axiomdb_appender_append_text(self._ptr, v.encode("utf-8")),
            "append_text",
        )

    def append_bytes(self, v: bytes):
        """Append a BLOB/BYTES value."""
        self._check_open()
        n = len(v)
        if n == 0:
            ptr = None
            rc = _lib.axiomdb_appender_append_bytes(self._ptr, None, 0)
        else:
            buf = (ctypes.c_uint8 * n)(*v)
            rc = _lib.axiomdb_appender_append_bytes(self._ptr, buf, n)
        self._check_rc(rc, "append_bytes")

    def append_null(self):
        """Append a NULL value."""
        self._check_open()
        self._check_rc(
            _lib.axiomdb_appender_append_null(self._ptr), "append_null"
        )

    def append_row(self, *values):
        """Append a full row from positional arguments.

        Inferred types per Python value: None → NULL, bool → BOOL,
        int → INT (or BIGINT if outside i32), float → REAL,
        str → TEXT, bytes → BLOB. Calls `end_row()` automatically.

        Example:
            app.append_row(1, "Alice", True, 3.14)
        """
        for v in values:
            if v is None:
                self.append_null()
            elif isinstance(v, bool):
                self.append_bool(v)
            elif isinstance(v, int):
                # i32 range fits append_int; else BIGINT.
                if -(2**31) <= v < 2**31:
                    self.append_int(v)
                else:
                    self.append_bigint(v)
            elif isinstance(v, float):
                self.append_real(v)
            elif isinstance(v, str):
                self.append_text(v)
            elif isinstance(v, (bytes, bytearray)):
                self.append_bytes(bytes(v))
            else:
                raise AxiomDBError(
                    f"append_row: unsupported Python type {type(v).__name__}"
                )
        self.end_row()

    def end_row(self):
        """Commit the in-progress row to the appender buffer."""
        self._check_open()
        self._check_rc(
            _lib.axiomdb_appender_end_row(self._ptr), "end_row"
        )

    def flush(self):
        """Write buffered rows to heap+WAL; transaction stays open."""
        self._check_open()
        self._check_rc(
            _lib.axiomdb_appender_flush(self._ptr), "flush"
        )

    def finish(self) -> int:
        """Flush + commit + close. Returns total rows inserted.

        The Appender is invalid after this call.
        """
        self._check_open()
        n = _lib.axiomdb_appender_finish(self._ptr)
        self._ptr = None
        if n < 0:
            err = self._db._last_error() or "finish failed"
            raise AxiomDBError(err)
        return n

    def __del__(self):
        if self._ptr:
            _lib.axiomdb_appender_free(self._ptr)
            self._ptr = None


# ── Demo ─────────────────────────────────────────────────────────────────────

if __name__ == "__main__":
    import tempfile

    with tempfile.TemporaryDirectory() as tmpdir:
        db_path = os.path.join(tmpdir, "demo.db")

        print(f"Opening database at {db_path}")
        with AxiomDB(db_path) as db:
            # DDL
            db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")
            print("Created table 'users'")

            # INSERT
            db.execute("INSERT INTO users VALUES (1, 'Alice', 30)")
            db.execute("INSERT INTO users VALUES (2, 'Bob', 25)")
            db.execute("INSERT INTO users VALUES (3, 'Charlie', 35)")
            print("Inserted 3 rows")

            # SELECT
            rows = db.query("SELECT * FROM users")
            print(f"\nSELECT * FROM users ({len(rows)} rows):")
            for row in rows:
                print(f"  {row}")

            # Filtered SELECT
            rows = db.query("SELECT name, age FROM users WHERE age > 28")
            print(f"\nWHERE age > 28 ({len(rows)} rows):")
            for row in rows:
                print(f"  {row}")

            # COUNT
            rows = db.query("SELECT COUNT(*) AS total FROM users")
            print(f"\nCOUNT(*): {rows[0]}")

            # UPDATE
            affected = db.execute("UPDATE users SET age = 31 WHERE id = 1")
            print(f"\nUPDATE affected {affected} row(s)")

            # DELETE
            affected = db.execute("DELETE FROM users WHERE id = 3")
            print(f"DELETE affected {affected} row(s)")

            # Final state
            rows = db.query("SELECT * FROM users")
            print(f"\nFinal state ({len(rows)} rows):")
            for row in rows:
                print(f"  {row}")

        print("\nDone! Database closed automatically.")
