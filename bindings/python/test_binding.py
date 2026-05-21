"""Correctness tests for the AxiomDB ctypes binding.

Cross-checks results against the stdlib ``sqlite3`` module (an independent
engine) and exercises parameter binding (``?`` placeholders).

    python bindings/python/test_binding.py
"""

import os
import sqlite3
import tempfile

from axiomdb import AxiomDB, AxiomDBError


def test_basic_types_match_sqlite():
    with tempfile.TemporaryDirectory() as d:
        db = AxiomDB(os.path.join(d, "a.db"))
        db.execute("CREATE TABLE t (id INT, name TEXT, score REAL, m INT)")
        db.execute("INSERT INTO t VALUES (1, 'alice', 3.5, NULL)")
        db.execute("INSERT INTO t VALUES (2, 'héllo', 2.25, 99)")
        rows = db.query_tuples("SELECT id, name, score, m FROM t ORDER BY id")
        assert rows == [(1, "alice", 3.5, None), (2, "héllo", 2.25, 99)], rows
        db.close()


def test_param_execute_and_query():
    with tempfile.TemporaryDirectory() as d:
        db = AxiomDB(os.path.join(d, "a.db"))
        db.execute("CREATE TABLE t (id INT, name TEXT, score REAL, avatar BLOB)")
        db.execute("INSERT INTO t VALUES (?, ?, ?, ?)", [1, "alice", 3.5, None])
        db.execute("INSERT INTO t VALUES (?, ?, ?, ?)", [2, "bøb", 2.25, b"\x09\x08\x07"])

        rows = db.query_tuples(
            "SELECT id, name, score, avatar FROM t WHERE id = ?", [2]
        )
        assert rows == [(2, "bøb", 2.25, b"\x09\x08\x07")], rows

        maps = db.query("SELECT id, name FROM t WHERE name = ?", ["alice"])
        assert maps == [{"id": 1, "name": "alice"}], maps


def test_param_matches_sqlite():
    rows = [(1, "a", 1.5), (2, "b", 2.5), (3, "c", 3.5)]
    with tempfile.TemporaryDirectory() as d:
        db = AxiomDB(os.path.join(d, "a.db"))
        db.execute("CREATE TABLE t (id INT, name TEXT, score REAL)")
        for r in rows:
            db.execute("INSERT INTO t VALUES (?, ?, ?)", r)
        ax = db.query_tuples("SELECT id, name, score FROM t WHERE id > ? ORDER BY id", [1])
        db.close()

        sq = sqlite3.connect(os.path.join(d, "s.db"))
        sq.execute("CREATE TABLE t (id INT, name TEXT, score REAL)")
        sq.executemany("INSERT INTO t VALUES (?, ?, ?)", rows)
        sq.commit()
        ref = sq.execute("SELECT id, name, score FROM t WHERE id > ? ORDER BY id", [1]).fetchall()
        sq.close()
        assert ax == ref, (ax, ref)


def test_param_injection_safe():
    with tempfile.TemporaryDirectory() as d:
        db = AxiomDB(os.path.join(d, "a.db"))
        db.execute("CREATE TABLE t (id INT, name TEXT)")
        evil = "x'; DROP TABLE t; --"
        db.execute("INSERT INTO t VALUES (?, ?)", [1, evil])
        got = db.query_tuples("SELECT name FROM t WHERE id = ?", [1])
        assert got == [(evil,)], got
        # table survived — value was bound, not executed
        assert db.query_tuples("SELECT COUNT(*) FROM t")[0][0] == 1


def test_param_count_mismatch_raises():
    with tempfile.TemporaryDirectory() as d:
        db = AxiomDB(os.path.join(d, "a.db"))
        db.execute("CREATE TABLE t (id INT, name TEXT)")
        try:
            db.execute("INSERT INTO t VALUES (?, ?)", [1])  # too few
            assert False, "expected AxiomDBError"
        except AxiomDBError:
            pass
        db.close()


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
