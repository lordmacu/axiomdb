#!/usr/bin/env python3
"""
Focused A/B wire benchmark for a single axiomdb-server binary.

Isolates the wire SELECT path (the one Attack 23b routes through the statement
cache) so a pre-23b vs post-23b binary can be compared apples-to-apples.

Usage:
    python3 tools/bench-wire-ab.py <server-binary> <label> [--rows N] [--runs N]

Connection is autocommit=False with NO explicit BEGIN, so read queries take the
server's concurrent read-only path (execute_read_query -> run_cached on post-23b).
Reports median ops/s over many runs to dampen host noise.
"""
import argparse
import os
import socket
import statistics
import subprocess
import sys
import tempfile
import time

import pymysql

ap = argparse.ArgumentParser()
ap.add_argument("binary")
ap.add_argument("label")
ap.add_argument("--rows", type=int, default=5000)
ap.add_argument("--runs", type=int, default=15)
ap.add_argument("--warmup", type=int, default=3)
ap.add_argument("--port", type=int, default=13411)
args = ap.parse_args()

N = args.rows


def wait_port(port, timeout=10):
    t0 = time.time()
    while time.time() - t0 < timeout:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return True
        except OSError:
            time.sleep(0.1)
    return False


def measure(fn):
    for _ in range(args.warmup):
        fn()
    samples = []
    for _ in range(args.runs):
        t0 = time.perf_counter()
        fn()
        samples.append(time.perf_counter() - t0)
    return statistics.median(samples)


def ops(n, sec):
    if sec <= 0:
        return 0.0
    return n / sec


data_dir = tempfile.mkdtemp(prefix="axiom_wire_ab_")
env = os.environ.copy()
env["AXIOMDB_DATA"] = data_dir
env["AXIOMDB_PORT"] = str(args.port)
proc = subprocess.Popen([args.binary], env=env,
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
try:
    if not wait_port(args.port):
        print(f"{args.label}: server failed to start", file=sys.stderr)
        sys.exit(1)
    conn = pymysql.connect(host="127.0.0.1", port=args.port, user="root",
                           password="", autocommit=False)
    cur = conn.cursor()
    cur.execute("CREATE TABLE bench_users (id INT, name TEXT, age INT, "
                "active BOOL, score REAL, email TEXT, PRIMARY KEY (id))")
    conn.begin()
    for i in range(1, N + 1):
        active = "TRUE" if i % 2 == 0 else "FALSE"
        cur.execute(
            f"INSERT INTO bench_users VALUES ({i}, 'user_{i:06d}', "
            f"{18 + i % 62}, {active}, {100.0 + (i % 1000) * 0.1:.1f}, 'u{i}@b.local')"
        )
    conn.commit()

    step = max(N // 100, 1)
    lookup_ids = list(range(1, N + 1, step))[:100]
    start = N // 4
    end = start + N // 10

    def point_lookup():
        for i in lookup_ids:
            cur.execute(f"SELECT * FROM bench_users WHERE id = {i}")
            cur.fetchall()

    def range_scan():
        cur.execute(f"SELECT * FROM bench_users WHERE id >= {start} AND id < {end}")
        cur.fetchall()

    def select_where():
        cur.execute("SELECT * FROM bench_users WHERE active = TRUE")
        cur.fetchall()

    def count_star():
        cur.execute("SELECT COUNT(*) FROM bench_users")
        cur.fetchall()

    pl = measure(point_lookup)
    rs = measure(range_scan)
    sw = measure(select_where)
    cs = measure(count_star)

    print(f"{args.label}:")
    print(f"  point_lookup (100 same-shape) : {ops(100, pl):>12,.0f} ops/s  ({pl*1000:.2f} ms)")
    print(f"  range_scan   ({end-start} rows)        : {ops(end-start, rs):>12,.0f} rows/s")
    print(f"  select_where (~{N//2} rows)      : {ops(N//2, sw):>12,.0f} rows/s")
    print(f"  count_star                    : {ops(1, cs):>12,.0f} ops/s  ({cs*1000:.2f} ms)")
    conn.close()
finally:
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
    import shutil
    shutil.rmtree(data_dir, ignore_errors=True)
