#!/usr/bin/env python3
"""
Embedded vs Server benchmark — Phase 10.7

Compares in-process embedded AxiomDB (via ctypes/cdylib) against
server-mode AxiomDB (via MySQL wire protocol) to measure the TCP
overhead per query.

Expected: embedded is 5-50× faster for point lookups (no TCP round-trip).

Usage:
  # 1. Build release: cargo build --release -p axiomdb-embedded -p axiomdb-server
  # 2. Start server:  AXIOMDB_PORT=3309 AXIOMDB_DATA=/tmp/axiomdb_bench ./target/release/axiomdb-server &
  # 3. Run:           python3 embedded_vs_server.py
"""

import os
import sys
import time
import tempfile
import statistics

# Add the python binding to path
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "bindings", "python"))

from axiomdb import AxiomDB

ROWS = 5000
ITERATIONS = 20
PK_LOOKUPS = 100


def bench_embedded():
    """Benchmark embedded mode — in-process, no TCP."""
    results = {}

    with tempfile.TemporaryDirectory() as tmpdir:
        db_path = os.path.join(tmpdir, "bench.db")
        db = AxiomDB(db_path)

        # Setup
        db.execute("CREATE TABLE bench_users (id INT PRIMARY KEY, name TEXT, age INT, active BOOL, score INT, email TEXT)")
        for i in range(1, ROWS + 1):
            db.execute(f"INSERT INTO bench_users VALUES ({i}, 'user_{i:06d}', {18 + i % 62}, {1 if i % 2 == 0 else 0}, {100 + i % 1000}, 'u{i}@b.local')")

        # Benchmark: SELECT * (full scan)
        times = []
        for _ in range(ITERATIONS):
            start = time.perf_counter()
            rows = db.query("SELECT * FROM bench_users")
            elapsed = time.perf_counter() - start
            times.append(elapsed)
        mean_ms = statistics.mean(times) * 1000
        results["select"] = {"mean_ms": round(mean_ms, 2), "rows": len(rows), "ops_per_s": round(len(rows) / statistics.mean(times))}

        # Benchmark: SELECT WHERE (filtered scan)
        times = []
        for _ in range(ITERATIONS):
            start = time.perf_counter()
            rows = db.query("SELECT * FROM bench_users WHERE active = TRUE")
            elapsed = time.perf_counter() - start
            times.append(elapsed)
        mean_ms = statistics.mean(times) * 1000
        results["select_where"] = {"mean_ms": round(mean_ms, 2), "rows": len(rows), "ops_per_s": round(len(rows) / statistics.mean(times))}

        # Benchmark: SELECT pk (point lookups)
        times = []
        step = max(1, ROWS // PK_LOOKUPS)
        for _ in range(ITERATIONS):
            start = time.perf_counter()
            for i in range(1, ROWS + 1, step):
                db.query(f"SELECT * FROM bench_users WHERE id = {i}")
            elapsed = time.perf_counter() - start
            times.append(elapsed)
        mean_ms = statistics.mean(times) * 1000
        n_lookups = len(range(1, ROWS + 1, step))
        results["select_pk"] = {"mean_ms": round(mean_ms, 2), "rows": n_lookups, "ops_per_s": round(n_lookups / statistics.mean(times))}

        # Benchmark: COUNT(*)
        times = []
        for _ in range(ITERATIONS):
            start = time.perf_counter()
            db.query("SELECT COUNT(*) FROM bench_users")
            elapsed = time.perf_counter() - start
            times.append(elapsed)
        mean_ms = statistics.mean(times) * 1000
        results["count"] = {"mean_ms": round(mean_ms, 2), "rows": 1, "ops_per_s": round(1 / statistics.mean(times))}

        db.close()

    return results


def bench_server():
    """Benchmark server mode — TCP wire protocol."""
    try:
        import pymysql
    except ImportError:
        print("pymysql not installed — skipping server benchmark", file=sys.stderr)
        return None

    try:
        conn = pymysql.connect(host="127.0.0.1", port=3309, user="root",
                               password="root", database="axiomdb", connect_timeout=3)
    except Exception as e:
        print(f"Cannot connect to AxiomDB server on :3309 — {e}", file=sys.stderr)
        print("Start it: AXIOMDB_PORT=3309 AXIOMDB_DATA=/tmp/axiomdb_bench ./target/release/axiomdb-server &", file=sys.stderr)
        return None

    results = {}
    cur = conn.cursor()
    cur.execute("SET autocommit=1")

    # Setup
    cur.execute("DROP TABLE IF EXISTS bench_users")
    cur.execute("CREATE TABLE bench_users (id INT PRIMARY KEY, name TEXT, age INT, active BOOL, score INT, email TEXT)")
    cur.execute("BEGIN")
    for i in range(1, ROWS + 1):
        cur.execute(f"INSERT INTO bench_users VALUES ({i}, 'user_{i:06d}', {18 + i % 62}, {1 if i % 2 == 0 else 0}, {100 + i % 1000}, 'u{i}@b.local')")
    cur.execute("COMMIT")

    # Benchmark: SELECT *
    times = []
    for _ in range(ITERATIONS):
        start = time.perf_counter()
        cur.execute("SELECT * FROM bench_users")
        rows = cur.fetchall()
        elapsed = time.perf_counter() - start
        times.append(elapsed)
    mean_ms = statistics.mean(times) * 1000
    results["select"] = {"mean_ms": round(mean_ms, 2), "rows": len(rows), "ops_per_s": round(len(rows) / statistics.mean(times))}

    # Benchmark: SELECT WHERE
    times = []
    for _ in range(ITERATIONS):
        start = time.perf_counter()
        cur.execute("SELECT * FROM bench_users WHERE active = TRUE")
        rows = cur.fetchall()
        elapsed = time.perf_counter() - start
        times.append(elapsed)
    mean_ms = statistics.mean(times) * 1000
    results["select_where"] = {"mean_ms": round(mean_ms, 2), "rows": len(rows), "ops_per_s": round(len(rows) / statistics.mean(times))}

    # Benchmark: SELECT pk
    times = []
    step = max(1, ROWS // PK_LOOKUPS)
    for _ in range(ITERATIONS):
        start = time.perf_counter()
        for i in range(1, ROWS + 1, step):
            cur.execute(f"SELECT * FROM bench_users WHERE id = {i}")
            cur.fetchall()
        elapsed = time.perf_counter() - start
        times.append(elapsed)
    mean_ms = statistics.mean(times) * 1000
    n_lookups = len(range(1, ROWS + 1, step))
    results["select_pk"] = {"mean_ms": round(mean_ms, 2), "rows": n_lookups, "ops_per_s": round(n_lookups / statistics.mean(times))}

    # Benchmark: COUNT(*)
    times = []
    for _ in range(ITERATIONS):
        start = time.perf_counter()
        cur.execute("SELECT COUNT(*) FROM bench_users")
        cur.fetchall()
        elapsed = time.perf_counter() - start
        times.append(elapsed)
    mean_ms = statistics.mean(times) * 1000
    results["count"] = {"mean_ms": round(mean_ms, 2), "rows": 1, "ops_per_s": round(1 / statistics.mean(times))}

    cur.execute("DROP TABLE IF EXISTS bench_users")
    cur.close()
    conn.close()
    return results


def main():
    print(f"Benchmarking embedded vs server ({ROWS} rows, {ITERATIONS} iterations)\n")

    print("Running embedded benchmark...")
    embedded = bench_embedded()

    print("Running server benchmark...")
    server = bench_server()

    # Results
    print(f"\n{'Scenario':<16} {'Embedded':>14} {'Server':>14} {'Speedup':>10}")
    print("-" * 58)

    for scenario in ["select", "select_where", "select_pk", "count"]:
        e = embedded[scenario]
        if server and scenario in server:
            s = server[scenario]
            speedup = s["mean_ms"] / e["mean_ms"] if e["mean_ms"] > 0 else 0
            print(f"{scenario:<16} {e['mean_ms']:>10.1f}ms {s['mean_ms']:>10.1f}ms {speedup:>9.1f}×")
        else:
            print(f"{scenario:<16} {e['mean_ms']:>10.1f}ms {'N/A':>14} {'N/A':>10}")

    if server:
        print(f"\n{'Scenario':<16} {'Embedded r/s':>14} {'Server r/s':>14}")
        print("-" * 46)
        for scenario in ["select", "select_where", "select_pk", "count"]:
            e = embedded[scenario]
            s = server[scenario]
            print(f"{scenario:<16} {e['ops_per_s']:>14,} {s['ops_per_s']:>14,}")


if __name__ == "__main__":
    main()
