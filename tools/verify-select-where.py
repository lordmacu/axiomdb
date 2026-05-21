#!/usr/bin/env python3
"""
Correctness + perf check for the clustered BatchPredicate (select_where).

Proves the optimized WHERE filter returns the EXACT same rows as SQLite (ground
truth) for a battery of predicates — both batch-predicate-eligible (numeric/bool
comparisons, AND) and fallback (OR / LIKE / IN / TEXT). A "fast but wrong" filter
(dropping or admitting wrong rows) is caught here. Also times select_where on
both engines (the Python "lápiz").

AxiomDB via MySQL wire (pymysql); SQLite via stdlib sqlite3 (in-memory).
Run after building the server: cargo build -p axiomdb-server --release
"""
import os
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time

import pymysql

PORT = 13455
N = 10000


def row_values(i):
    return (i, f"user_{i:06d}", 18 + i % 62, 1 if i % 2 == 0 else 0,
            round(100.0 + (i % 1000) * 0.1, 1), f"u{i}@b.local")


def insert_sql(v):
    active = "TRUE" if v[3] == 1 else "FALSE"
    return (f"INSERT INTO bench_users VALUES ({v[0]}, '{v[1]}', {v[2]}, "
            f"{active}, {v[4]:.1f}, '{v[5]}')")


DDL_AX = ("CREATE TABLE bench_users (id INT PRIMARY KEY, name TEXT, age INT, "
          "active BOOL, score REAL, email TEXT)")
DDL_SQ = ("CREATE TABLE bench_users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER, "
          "active INTEGER, score REAL, email TEXT)")

# (label, where) — where clause identical on both engines where possible.
PREDICATES = [
    ("active=TRUE (batch bool)",        "active = TRUE"),
    ("active=FALSE (batch bool)",       "active = FALSE"),
    ("id>5000 (batch int)",             "id > 5000"),
    ("id>=5000 AND id<6000 (batch AND)","id >= 5000 AND id < 6000"),
    ("id=42 (batch eq)",                "id = 42"),
    ("age>=50 (batch int)",             "age >= 50"),
    ("score<150.0 (batch real)",        "score < 150.0"),
    ("age=30 AND active=TRUE (batch)",  "age = 30 AND active = TRUE"),
    ("id<>7 (batch ne)",                "id <> 7"),
    ("name LIKE (fallback)",            "name LIKE 'user_0001%'"),
    ("email eq TEXT (fallback)",        "email = 'u500@b.local'"),
    ("active OR id (fallback OR)",      "active = TRUE OR id = 1"),
    ("id IN (fallback IN)",             "id IN (1, 2, 3, 9999)"),
    ("no where (full scan)",            None),
]


def start_server():
    d = tempfile.mkdtemp(prefix="verify_sw_")
    env = os.environ.copy()
    env["AXIOMDB_DATA"] = d
    env["AXIOMDB_PORT"] = str(PORT)
    p = subprocess.Popen(["target/release/axiomdb-server"], env=env,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(80):
        try:
            with socket.create_connection(("127.0.0.1", PORT), timeout=0.2):
                return p
        except OSError:
            time.sleep(0.1)
    p.terminate()
    print("server did not start"); sys.exit(1)


def ax_ids(cur, where):
    q = "SELECT id FROM bench_users" + (f" WHERE {where}" if where else "")
    cur.execute(q + " ORDER BY id")
    return [int(r[0]) for r in cur.fetchall()]


def sq_ids(conn, where):
    q = "SELECT id FROM bench_users" + (f" WHERE {where}" if where else "")
    return [int(r[0]) for r in conn.execute(q + " ORDER BY id").fetchall()]


def ax_full(cur, where):
    q = "SELECT * FROM bench_users" + (f" WHERE {where}" if where else "")
    cur.execute(q + " ORDER BY id")
    out = []
    for r in cur.fetchall():
        out.append((int(r[0]), str(r[1]), int(r[2]), int(r[3]),
                    round(float(r[4]), 1), str(r[5])))
    return out


def sq_full(conn, where):
    q = "SELECT * FROM bench_users" + (f" WHERE {where}" if where else "")
    out = []
    for r in conn.execute(q + " ORDER BY id").fetchall():
        out.append((int(r[0]), str(r[1]), int(r[2]), int(r[3]),
                    round(float(r[4]), 1), str(r[5])))
    return out


def main():
    proc = start_server()
    try:
        ax = pymysql.connect(host="127.0.0.1", port=PORT, user="root", password="",
                             autocommit=False)
        axc = ax.cursor()
        sq = sqlite3.connect(":memory:")

        axc.execute(DDL_AX)
        sq.execute(DDL_SQ)
        ax.begin()
        sq.execute("BEGIN")
        for i in range(1, N + 1):
            s = insert_sql(row_values(i))
            axc.execute(s)
            sq.execute(s)
        ax.commit()
        sq.execute("COMMIT")

        print(f"Loaded {N} rows on both engines.\n")
        print(f"{'Predicate':<34} {'AxiomDB':>8} {'SQLite':>8}  {'match':>6}")
        print("-" * 62)
        all_ok = True
        for label, where in PREDICATES:
            a = ax_ids(axc, where)
            s = sq_ids(sq, where)
            ok = a == s
            all_ok &= ok
            flag = "✓" if ok else "✗ MISMATCH"
            print(f"{label:<34} {len(a):>8} {len(s):>8}  {flag:>6}")
            if not ok:
                only_ax = sorted(set(a) - set(s))[:10]
                only_sq = sorted(set(s) - set(a))[:10]
                print(f"    only in AxiomDB: {only_ax}")
                print(f"    only in SQLite : {only_sq}")

        # Full-row value check (decode correctness) for select_where.
        af = ax_full(axc, "active = TRUE")
        sf = sq_full(sq, "active = TRUE")
        full_ok = af == sf
        all_ok &= full_ok
        print("-" * 62)
        print(f"full-row values (active=TRUE): {'✓ identical' if full_ok else '✗ MISMATCH'}"
              f"  ({len(af)} rows)")
        if not full_ok:
            for i in range(min(len(af), len(sf))):
                if af[i] != sf[i]:
                    print(f"    first diff @row {i}: Ax={af[i]}  SQ={sf[i]}")
                    break

        # Timing (Python lápiz): select_where, median of a few runs.
        def t_ax():
            t0 = time.perf_counter()
            axc.execute("SELECT * FROM bench_users WHERE active = TRUE")
            axc.fetchall()
            return time.perf_counter() - t0
        def t_sq():
            t0 = time.perf_counter()
            sq.execute("SELECT * FROM bench_users WHERE active = TRUE").fetchall()
            return time.perf_counter() - t0
        for _ in range(3):
            t_ax(); t_sq()
        ax_ms = sorted(t_ax() for _ in range(7))[3] * 1000
        sq_ms = sorted(t_sq() for _ in range(7))[3] * 1000
        print("-" * 62)
        print(f"select_where timing (Python wire vs sqlite3): "
              f"AxiomDB {ax_ms:.2f} ms | SQLite {sq_ms:.2f} ms")
        print()
        print("RESULT:", "✅ ALL PREDICATES MATCH SQLITE — not a false positive"
              if all_ok else "❌ MISMATCH DETECTED — filter is WRONG")
        ax.close()
        sys.exit(0 if all_ok else 1)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    main()
