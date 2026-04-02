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
import sys
from pathlib import Path
from typing import Any, Dict, List, Optional

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

    def query(self, sql: str) -> List[Dict[str, Any]]:
        """Execute a SELECT and return rows as list of dicts.

        Each dict maps column name → Python value.
        Types: int, float, str, bytes, None (NULL).
        """
        self._check_open()
        rows_ptr = _lib.axiomdb_query(self._ptr, sql.encode("utf-8"))
        if not rows_ptr:
            raise AxiomDBError(self._last_error() or "query failed")

        try:
            n_rows = _lib.axiomdb_rows_count(rows_ptr)
            n_cols = _lib.axiomdb_rows_columns(rows_ptr)

            # Column names
            col_names = []
            for c in range(n_cols):
                name = _lib.axiomdb_rows_column_name(rows_ptr, c)
                col_names.append(name.decode("utf-8") if name else f"col_{c}")

            # Extract rows
            result = []
            for r in range(n_rows):
                row = {}
                for c in range(n_cols):
                    typ = _lib.axiomdb_rows_type(rows_ptr, r, c)
                    if typ == TYPE_NULL:
                        row[col_names[c]] = None
                    elif typ == TYPE_INTEGER:
                        row[col_names[c]] = _lib.axiomdb_rows_get_int(rows_ptr, r, c)
                    elif typ == TYPE_REAL:
                        row[col_names[c]] = _lib.axiomdb_rows_get_double(rows_ptr, r, c)
                    elif typ == TYPE_TEXT:
                        val = _lib.axiomdb_rows_get_text(rows_ptr, r, c)
                        row[col_names[c]] = val.decode("utf-8") if val else None
                    elif typ == TYPE_BLOB:
                        length = ctypes.c_size_t(0)
                        ptr = _lib.axiomdb_rows_get_blob(rows_ptr, r, c, ctypes.byref(length))
                        if ptr:
                            row[col_names[c]] = ctypes.string_at(ptr, length.value)
                        else:
                            row[col_names[c]] = None
                    else:
                        row[col_names[c]] = None
                result.append(row)
            return result
        finally:
            _lib.axiomdb_rows_free(rows_ptr)

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

    def __del__(self):
        self.close()


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
