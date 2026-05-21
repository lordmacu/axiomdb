"""Correctness tests for the native PyO3 AxiomDB binding.

Run after `maturin develop` in the crate venv:

    .venv/bin/python -m pytest tests/test_binding.py        # if pytest installed
    .venv/bin/python tests/test_binding.py                   # standalone

Each test cross-checks AxiomDB results against the stdlib `sqlite3` module so
correctness is verified against an independent engine, per the project's
benchmark methodology ("verify correctness before trusting a speedup").
"""

import os
import sqlite3
import tempfile

import axiomdb_native as adb


def _roundtrip(make_rows_sql, select_sql):
    """Runs the same DDL+DML+SELECT on AxiomDB and sqlite3, returns both row lists."""
    with tempfile.TemporaryDirectory() as d:
        ax = adb.connect(os.path.join(d, "ax.db"))
        for sql in make_rows_sql:
            ax.execute(sql)
        ax_rows = ax.query(select_sql)
        ax.close()

        sq = sqlite3.connect(os.path.join(d, "sq.db"))
        for sql in make_rows_sql:
            sq.execute(sql)
        sq.commit()
        sq_rows = sq.execute(select_sql).fetchall()
        sq.close()
    return ax_rows, sq_rows


def test_int_text_real_match_sqlite():
    ddl = [
        "CREATE TABLE t (id INT, name TEXT, score REAL)",
        "INSERT INTO t VALUES (1, 'Alice', 3.5)",
        "INSERT INTO t VALUES (2, 'Bob', 2.25)",
        "INSERT INTO t VALUES (3, 'Carol', 9.0)",
    ]
    ax, sq = _roundtrip(ddl, "SELECT id, name, score FROM t ORDER BY id")
    assert ax == sq == [(1, "Alice", 3.5), (2, "Bob", 2.25), (3, "Carol", 9.0)]


def test_nulls_match_sqlite():
    ddl = [
        "CREATE TABLE t (id INT, maybe INT)",
        "INSERT INTO t VALUES (1, NULL)",
        "INSERT INTO t VALUES (2, 42)",
    ]
    ax, sq = _roundtrip(ddl, "SELECT id, maybe FROM t ORDER BY id")
    assert ax == sq == [(1, None), (2, 42)]


def test_unicode_text():
    ddl = [
        "CREATE TABLE t (s TEXT)",
        "INSERT INTO t VALUES ('héllo wörld')",
        "INSERT INTO t VALUES ('日本語')",
    ]
    ax, sq = _roundtrip(ddl, "SELECT s FROM t ORDER BY s")
    assert ax == sq


def test_empty_result():
    ddl = ["CREATE TABLE t (id INT)", "INSERT INTO t VALUES (1)"]
    ax, sq = _roundtrip(ddl, "SELECT id FROM t WHERE id = 999")
    assert ax == sq == []


def test_query_dict_shape():
    with tempfile.TemporaryDirectory() as d:
        c = adb.connect(os.path.join(d, "t.db"))
        c.execute("CREATE TABLE t (id INT, name TEXT)")
        c.execute("INSERT INTO t VALUES (1, 'x')")
        assert c.query_dict("SELECT id, name FROM t") == [{"id": 1, "name": "x"}]
        cols, rows = c.query_with_columns("SELECT id, name FROM t")
        assert cols == ["id", "name"]
        assert rows == [(1, "x")]
        c.close()


def test_transactions_commit_rollback():
    with tempfile.TemporaryDirectory() as d:
        c = adb.connect(os.path.join(d, "t.db"))
        c.execute("CREATE TABLE t (id INT)")
        c.begin()
        c.execute("INSERT INTO t VALUES (1)")
        c.commit()
        assert c.query("SELECT * FROM t") == [(1,)]
        c.begin()
        c.execute("INSERT INTO t VALUES (2)")
        c.rollback()
        assert c.query("SELECT * FROM t") == [(1,)]
        c.close()


def test_context_manager_and_close():
    with tempfile.TemporaryDirectory() as d:
        with adb.connect(os.path.join(d, "t.db")) as c:
            c.execute("CREATE TABLE t (id INT)")
            c.execute("INSERT INTO t VALUES (7)")
            assert c.query("SELECT id FROM t") == [(7,)]
        # after the with-block the connection is closed
        try:
            c.query("SELECT 1")
            assert False, "expected error on closed connection"
        except adb.AxiomDBError:
            pass


def test_bad_sql_raises():
    with tempfile.TemporaryDirectory() as d:
        c = adb.connect(os.path.join(d, "t.db"))
        try:
            c.query("SELECT * FROM nonexistent_table")
            assert False, "expected AxiomDBError"
        except adb.AxiomDBError:
            pass
        c.close()


def test_param_binding():
    with tempfile.TemporaryDirectory() as d:
        c = adb.connect(os.path.join(d, "t.db"))
        c.execute("CREATE TABLE t (id INT, name TEXT, score REAL, avatar BLOB)")
        c.execute("INSERT INTO t VALUES (?, ?, ?, ?)", [1, "alice", 3.5, None])
        c.execute("INSERT INTO t VALUES (?, ?, ?, ?)", (2, "bøb", 2.25, b"\x09\x08\x07"))
        rows = c.query("SELECT id, name, score, avatar FROM t WHERE id = ?", [2])
        assert rows == [(2, "bøb", 2.25, b"\x09\x08\x07")], rows
        dicts = c.query_dict("SELECT id, name FROM t WHERE name = ?", ["alice"])
        assert dicts == [{"id": 1, "name": "alice"}], dicts
        c.close()


def test_param_matches_sqlite():
    rows = [(1, "a", 1.5), (2, "b", 2.5), (3, "c", 3.5)]
    with tempfile.TemporaryDirectory() as d:
        c = adb.connect(os.path.join(d, "ax.db"))
        c.execute("CREATE TABLE t (id INT, name TEXT, score REAL)")
        for r in rows:
            c.execute("INSERT INTO t VALUES (?, ?, ?)", r)
        ax = c.query("SELECT id, name, score FROM t WHERE id > ? ORDER BY id", [1])
        c.close()

        sq = sqlite3.connect(os.path.join(d, "sq.db"))
        sq.execute("CREATE TABLE t (id INT, name TEXT, score REAL)")
        sq.executemany("INSERT INTO t VALUES (?, ?, ?)", rows)
        sq.commit()
        ref = sq.execute("SELECT id, name, score FROM t WHERE id > ? ORDER BY id", [1]).fetchall()
        sq.close()
        assert ax == ref, (ax, ref)


def test_param_injection_safe():
    with tempfile.TemporaryDirectory() as d:
        c = adb.connect(os.path.join(d, "t.db"))
        c.execute("CREATE TABLE t (id INT, name TEXT)")
        evil = "x'; DROP TABLE t; --"
        c.execute("INSERT INTO t VALUES (?, ?)", [1, evil])
        assert c.query("SELECT name FROM t WHERE id = ?", [1]) == [(evil,)]
        assert c.query("SELECT COUNT(*) FROM t") == [(1,)]
        c.close()


if __name__ == "__main__":
    fns = [v for k, v in sorted(globals().items()) if k.startswith("test_")]
    failed = 0
    for fn in fns:
        try:
            fn()
            print(f"PASS {fn.__name__}")
        except Exception as exc:  # noqa: BLE001
            failed += 1
            print(f"FAIL {fn.__name__}: {exc}")
    print(f"\n{len(fns) - failed}/{len(fns)} passed")
    raise SystemExit(1 if failed else 0)
