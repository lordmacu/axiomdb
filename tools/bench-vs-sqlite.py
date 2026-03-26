#!/usr/bin/env python3
"""
CRUD benchmark: AxiomDB (via MySQL wire protocol) vs SQLite (stdlib).

Measures wall-clock time for identical workloads on both engines.
Run after: cargo build --bin axiomdb-server

Usage:
    python3 tools/bench-vs-sqlite.py
    python3 tools/bench-vs-sqlite.py --rows 5000
"""

import argparse
import os
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time

import pymysql

# ── Config ────────────────────────────────────────────────────────────────────

PORT      = 13307   # different port to avoid collision with wire-test.py
N_ROWS    = 1_000   # default rows per scenario (override with --rows)

# ── Server lifecycle ──────────────────────────────────────────────────────────

_server_proc = None
_data_dir    = None


def _check_binary_freshness(binary):
    import glob
    binary_mtime = os.path.getmtime(binary)
    stale = [
        f for f in glob.glob("crates/**/*.rs", recursive=True)
        if os.path.getmtime(f) > binary_mtime
    ]
    if stale:
        print(f"\nERROR: binary '{binary}' is stale.")
        print(f"  {len(stale)} source file(s) newer than the binary, e.g.:")
        for f in stale[:3]:
            print(f"    {f}")
        print("\nFix: cargo build --bin axiomdb-server")
        sys.exit(1)


def start_server():
    global _server_proc, _data_dir
    debug   = "target/debug/axiomdb-server"
    release = "target/release/axiomdb-server"
    if os.path.isfile(debug) and os.path.isfile(release):
        binary = debug if os.path.getmtime(debug) > os.path.getmtime(release) else release
    elif os.path.isfile(release):
        binary = release
    elif os.path.isfile(debug):
        binary = debug
    else:
        print("Server binary not found — build first: cargo build -p axiomdb-server")
        sys.exit(1)

    _check_binary_freshness(binary)
    _data_dir = tempfile.mkdtemp(prefix="axiomdb-bench-")
    env = os.environ.copy()
    env["AXIOMDB_DATA"] = _data_dir
    env["AXIOMDB_PORT"] = str(PORT)
    _server_proc = subprocess.Popen(
        [binary], env=env,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    for _ in range(80):
        try:
            with socket.create_connection(("127.0.0.1", PORT), timeout=0.1):
                return
        except OSError:
            time.sleep(0.1)
    stop_server()
    print(f"AxiomDB server did not start on :{PORT} within 8s")
    sys.exit(1)


def stop_server():
    global _server_proc, _data_dir
    if _server_proc:
        _server_proc.terminate()
        try:
            _server_proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            _server_proc.kill()
        _server_proc = None
    if _data_dir and os.path.isdir(_data_dir):
        shutil.rmtree(_data_dir, ignore_errors=True)
        _data_dir = None


def axiomdb_connect():
    return pymysql.connect(
        host="127.0.0.1", port=PORT, user="root", password="",
        autocommit=False,
    )


# ── Timing helpers ────────────────────────────────────────────────────────────

def measure(fn):
    """Return (seconds, result) for fn()."""
    t0 = time.perf_counter()
    result = fn()
    return time.perf_counter() - t0, result


def fmt_ops(n, seconds):
    ops = n / seconds
    if ops >= 1_000_000:
        return f"{ops/1_000_000:.2f}M ops/s"
    if ops >= 1_000:
        return f"{ops/1_000:.1f}K ops/s"
    return f"{ops:.0f} ops/s"


def fmt_ms(seconds):
    ms = seconds * 1000
    if ms < 1:
        return f"{ms*1000:.1f}µs"
    return f"{ms:.2f}ms"


# ── Result table ──────────────────────────────────────────────────────────────

results = []   # list of (scenario, n, axiomdb_s, sqlite_s)


def record(scenario, n, axiomdb_s, sqlite_s):
    results.append((scenario, n, axiomdb_s, sqlite_s))
    ratio = axiomdb_s / sqlite_s if sqlite_s > 0 else float("inf")
    verdict = "✅ faster" if ratio <= 1.0 else f"⚠️  {ratio:.1f}x slower"
    print(
        f"  AxiomDB: {fmt_ms(axiomdb_s):>10}  ({fmt_ops(n, axiomdb_s)})"
        f"  │  SQLite: {fmt_ms(sqlite_s):>10}  ({fmt_ops(n, sqlite_s)})"
        f"  │  {verdict}"
    )


def print_summary():
    print("\n" + "═" * 80)
    print("SUMMARY")
    print("═" * 80)
    print(f"{'Scenario':<35} {'AxiomDB':>14} {'SQLite':>14} {'Ratio':>8}")
    print("─" * 80)
    wins = 0
    for scenario, n, a_s, s_s in results:
        ratio = a_s / s_s if s_s > 0 else float("inf")
        flag  = "✅" if ratio <= 1.0 else "⚠️ "
        wins += 1 if ratio <= 1.0 else 0
        print(
            f"{scenario:<35} {fmt_ops(n, a_s):>14} {fmt_ops(n, s_s):>14} {ratio:>7.2f}x  {flag}"
        )
    print("─" * 80)
    total = len(results)
    print(f"\nAxiomDB wins: {wins}/{total} scenarios")
    if wins == total:
        print("🚀 AxiomDB faster than SQLite on all scenarios")
    elif wins >= total // 2:
        print("⚡ AxiomDB faster on majority — check ⚠️ scenarios")
    else:
        print("🔍 SQLite leads on majority — investigate bottlenecks")


# ── Scenarios ─────────────────────────────────────────────────────────────────

def bench_insert_sequential(n, ax_conn, sq_conn):
    """INSERT n rows one by one, committed in a single transaction."""
    print(f"\n[INSERT sequential, {n} rows, single txn]")

    # AxiomDB
    ax = ax_conn.cursor()
    ax.execute("CREATE TABLE bench_insert (id INT, name TEXT, score INT)")
    ax_conn.commit()

    def axiomdb_insert():
        for i in range(n):
            ax.execute(f"INSERT INTO bench_insert VALUES ({i}, 'user{i}', {i % 100})")
        ax_conn.commit()

    a_s, _ = measure(axiomdb_insert)

    # SQLite
    sq = sq_conn.cursor()
    sq.execute("CREATE TABLE bench_insert (id INT, name TEXT, score INT)")
    sq_conn.commit()

    def sqlite_insert():
        for i in range(n):
            sq.execute(f"INSERT INTO bench_insert VALUES ({i}, 'user{i}', {i % 100})")
        sq_conn.commit()

    s_s, _ = measure(sqlite_insert)
    record("INSERT sequential (single txn)", n, a_s, s_s)


def bench_insert_autocommit(n, ax_conn, sq_conn):
    """INSERT n rows, each in its own transaction (worst case for WAL engines)."""
    print(f"\n[INSERT autocommit, {n} rows, one txn per row]")
    n_small = min(n, 200)   # autocommit is slow — cap to keep total time sane

    # AxiomDB
    ax = ax_conn.cursor()
    ax.execute("CREATE TABLE bench_ac (id INT, val TEXT)")
    ax_conn.commit()

    def axiomdb_ac():
        for i in range(n_small):
            ax.execute(f"INSERT INTO bench_ac VALUES ({i}, 'v{i}')")
            ax_conn.commit()

    a_s, _ = measure(axiomdb_ac)

    # SQLite
    sq = sq_conn.cursor()
    sq.execute("CREATE TABLE bench_ac (id INT, val TEXT)")
    sq_conn.commit()

    def sqlite_ac():
        for i in range(n_small):
            sq.execute(f"INSERT INTO bench_ac VALUES ({i}, 'v{i}')")
            sq_conn.commit()

    s_s, _ = measure(sqlite_ac)
    record("INSERT autocommit (1 txn/row)", n_small, a_s, s_s)


def bench_point_lookup(n, ax_conn, sq_conn):
    """SELECT by primary key — point lookups on pre-loaded data."""
    print(f"\n[Point lookup (SELECT WHERE id=?), {n} lookups]")

    # AxiomDB — setup
    ax = ax_conn.cursor()
    ax.execute("CREATE TABLE bench_pk (id INT UNIQUE, val TEXT)")
    for i in range(n):
        ax.execute(f"INSERT INTO bench_pk VALUES ({i}, 'row{i}')")
    ax_conn.commit()

    def axiomdb_lookup():
        for i in range(n):
            ax.execute(f"SELECT val FROM bench_pk WHERE id = {i}")
            ax.fetchone()

    a_s, _ = measure(axiomdb_lookup)

    # SQLite — setup
    sq = sq_conn.cursor()
    sq.execute("CREATE TABLE bench_pk (id INT PRIMARY KEY, val TEXT)")
    for i in range(n):
        sq.execute(f"INSERT INTO bench_pk VALUES ({i}, 'row{i}')")
    sq_conn.commit()

    def sqlite_lookup():
        for i in range(n):
            sq.execute(f"SELECT val FROM bench_pk WHERE id = {i}")
            sq.fetchone()

    s_s, _ = measure(sqlite_lookup)
    record("Point lookup (UNIQUE index)", n, a_s, s_s)


def bench_seq_scan(n, ax_conn, sq_conn):
    """Full table scan — SELECT * FROM table (no index)."""
    print(f"\n[Sequential scan, {n} rows]")

    # AxiomDB — setup
    ax = ax_conn.cursor()
    ax.execute("CREATE TABLE bench_scan (id INT, name TEXT, score INT)")
    for i in range(n):
        ax.execute(f"INSERT INTO bench_scan VALUES ({i}, 'user{i}', {i % 100})")
    ax_conn.commit()

    def axiomdb_scan():
        ax.execute("SELECT * FROM bench_scan")
        return ax.fetchall()

    a_s, ax_rows = measure(axiomdb_scan)

    # SQLite — setup
    sq = sq_conn.cursor()
    sq.execute("CREATE TABLE bench_scan (id INT, name TEXT, score INT)")
    for i in range(n):
        sq.execute(f"INSERT INTO bench_scan VALUES ({i}, 'user{i}', {i % 100})")
    sq_conn.commit()

    def sqlite_scan():
        sq.execute("SELECT * FROM bench_scan")
        return sq.fetchall()

    s_s, sq_rows = measure(sqlite_scan)

    # Sanity: both must return same row count
    if len(ax_rows) != len(sq_rows):
        print(f"  ⚠️  row count mismatch: AxiomDB={len(ax_rows)}, SQLite={len(sq_rows)}")

    record("Sequential scan (full table)", n, a_s, s_s)


def bench_range_scan(n, ax_conn, sq_conn):
    """Range scan with WHERE clause on indexed column."""
    print(f"\n[Range scan (score BETWEEN 40 AND 60), {n} rows in table]")

    # AxiomDB — setup
    ax = ax_conn.cursor()
    ax.execute("CREATE TABLE bench_range (id INT, score INT)")
    ax.execute("CREATE INDEX idx_range_score ON bench_range (score)")
    for i in range(n):
        ax.execute(f"INSERT INTO bench_range VALUES ({i}, {i % 100})")
    ax_conn.commit()

    def axiomdb_range():
        ax.execute("SELECT id, score FROM bench_range WHERE score BETWEEN 40 AND 60")
        return ax.fetchall()

    a_s, _ = measure(axiomdb_range)

    # SQLite — setup
    sq = sq_conn.cursor()
    sq.execute("CREATE TABLE bench_range (id INT, score INT)")
    sq.execute("CREATE INDEX idx_range_score ON bench_range (score)")
    for i in range(n):
        sq.execute(f"INSERT INTO bench_range VALUES ({i}, {i % 100})")
    sq_conn.commit()

    def sqlite_range():
        sq.execute("SELECT id, score FROM bench_range WHERE score BETWEEN 40 AND 60")
        return sq.fetchall()

    s_s, _ = measure(sqlite_range)
    record("Range scan (indexed BETWEEN)", n, a_s, s_s)


def bench_update(n, ax_conn, sq_conn):
    """UPDATE n rows by id, single transaction."""
    print(f"\n[UPDATE {n} rows by id, single txn]")

    # AxiomDB — setup
    ax = ax_conn.cursor()
    ax.execute("CREATE TABLE bench_upd (id INT UNIQUE, val INT)")
    for i in range(n):
        ax.execute(f"INSERT INTO bench_upd VALUES ({i}, 0)")
    ax_conn.commit()

    def axiomdb_update():
        for i in range(n):
            ax.execute(f"UPDATE bench_upd SET val = {i * 2} WHERE id = {i}")
        ax_conn.commit()

    a_s, _ = measure(axiomdb_update)

    # SQLite — setup
    sq = sq_conn.cursor()
    sq.execute("CREATE TABLE bench_upd (id INT PRIMARY KEY, val INT)")
    for i in range(n):
        sq.execute(f"INSERT INTO bench_upd VALUES ({i}, 0)")
    sq_conn.commit()

    def sqlite_update():
        for i in range(n):
            sq.execute(f"UPDATE bench_upd SET val = {i * 2} WHERE id = {i}")
        sq_conn.commit()

    s_s, _ = measure(sqlite_update)
    record("UPDATE by id (single txn)", n, a_s, s_s)


def bench_delete(n, ax_conn, sq_conn):
    """DELETE n rows by id, single transaction."""
    print(f"\n[DELETE {n} rows by id, single txn]")

    # AxiomDB — setup
    ax = ax_conn.cursor()
    ax.execute("CREATE TABLE bench_del (id INT UNIQUE, val TEXT)")
    for i in range(n):
        ax.execute(f"INSERT INTO bench_del VALUES ({i}, 'x')")
    ax_conn.commit()

    def axiomdb_delete():
        for i in range(n):
            ax.execute(f"DELETE FROM bench_del WHERE id = {i}")
        ax_conn.commit()

    a_s, _ = measure(axiomdb_delete)

    # SQLite — setup
    sq = sq_conn.cursor()
    sq.execute("CREATE TABLE bench_del (id INT PRIMARY KEY, val TEXT)")
    for i in range(n):
        sq.execute(f"INSERT INTO bench_del VALUES ({i}, 'x')")
    sq_conn.commit()

    def sqlite_delete():
        for i in range(n):
            sq.execute(f"DELETE FROM bench_del WHERE id = {i}")
        sq_conn.commit()

    s_s, _ = measure(sqlite_delete)
    record("DELETE by id (single txn)", n, a_s, s_s)


# ── Entry point ────────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--rows", type=int, default=N_ROWS,
                        help=f"Rows per scenario (default: {N_ROWS})")
    args = parser.parse_args()
    n = args.rows

    print(f"Starting AxiomDB on :{PORT}...")
    start_server()
    print("Server ready\n")

    ax_conn = axiomdb_connect()

    # SQLite in-memory for a fair comparison (no fsync overhead on either side)
    sq_conn = sqlite3.connect(":memory:")
    sq_conn.isolation_level = None   # manual transaction control
    sq_conn.execute("PRAGMA journal_mode=WAL")
    sq_conn.execute("BEGIN")

    print(f"Benchmark: {n} rows per scenario")
    print("─" * 80)

    try:
        bench_insert_sequential(n, ax_conn, sq_conn)
        bench_insert_autocommit(n, ax_conn, sq_conn)
        bench_point_lookup(n, ax_conn, sq_conn)
        bench_seq_scan(n, ax_conn, sq_conn)
        bench_range_scan(n, ax_conn, sq_conn)
        bench_update(n, ax_conn, sq_conn)
        bench_delete(n, ax_conn, sq_conn)
    finally:
        ax_conn.close()
        sq_conn.close()
        stop_server()

    print_summary()


if __name__ == "__main__":
    main()
