#!/usr/bin/env python3
"""
AxiomDB concurrent throughput benchmark (Phase 40.12).

Measures multi-client DML throughput over the MySQL wire protocol.

Scenarios:
  insert          — N clients each insert rows/N rows via autocommit
  update_random   — N clients update random rows (low contention)
  update_hotspot  — N clients update same 100 rows (high contention)
  mixed           — N clients: 50% INSERT, 25% UPDATE, 15% SELECT, 10% DELETE

Usage:
  # Start server first:
  cargo build --release -p axiomdb-server
  AXIOMDB_PORT=13307 AXIOMDB_DATA=/tmp/axiomdb_conc ./target/release/axiomdb-server &

  # Run benchmark:
  python3 benches/comparison/concurrent_bench.py --clients 1,2,4,8 --scenario insert --rows 10000
  python3 benches/comparison/concurrent_bench.py --clients 1,2,4,8 --scenario update_random --rows 5000
  python3 benches/comparison/concurrent_bench.py --clients 4 --scenario mixed --rows 5000
  python3 benches/comparison/concurrent_bench.py --clients 1,2,4,8 --scenario all --rows 5000

Output:
  clients  scenario        total_rows  throughput   latency_p50  latency_p99  errors
  1        insert          10000       14,200 r/s   0.07ms       0.15ms       0
  2        insert          10000       25,100 r/s   0.08ms       0.20ms       0
  4        insert          10000       44,800 r/s   0.09ms       0.35ms       0
  8        insert          10000       68,500 r/s   0.12ms       0.80ms       0
"""

import argparse
import os
import signal
import subprocess
import sys
import tempfile
import threading
import time
import random

try:
    import pymysql
except ImportError:
    print("pymysql not installed: pip install pymysql")
    sys.exit(1)

PORT = int(os.environ.get("AXIOMDB_BENCH_PORT", "13307"))
_server_proc = None
_data_dir = None


# ── Server lifecycle ──────────────────────────────────────────────────────────

def _find_binary():
    explicit = os.environ.get("AXIOMDB_SERVER_BIN")
    if explicit:
        return explicit
    debug = "target/debug/axiomdb-server"
    release = "target/release/axiomdb-server"
    if os.path.isfile(release):
        return release
    if os.path.isfile(debug):
        return debug
    print("Server binary not found — build first: cargo build --release -p axiomdb-server")
    sys.exit(1)


def start_server():
    global _server_proc, _data_dir
    binary = _find_binary()
    _data_dir = tempfile.mkdtemp(prefix="axiomdb-conc-bench-")
    env = os.environ.copy()
    env["AXIOMDB_DATA"] = _data_dir
    env["AXIOMDB_PORT"] = str(PORT)
    _server_proc = subprocess.Popen(
        [binary], env=env,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    import socket
    for _ in range(50):
        try:
            with socket.create_connection(("127.0.0.1", PORT), timeout=0.1):
                return
        except OSError:
            time.sleep(0.1)
    stop_server()
    print(f"Server did not start on :{PORT} within 5s")
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
        import shutil
        shutil.rmtree(_data_dir, ignore_errors=True)
        _data_dir = None


def connect():
    return pymysql.connect(
        host="127.0.0.1", port=PORT, user="root", password="",
        autocommit=True,
    )


# ── Schema setup ──────────────────────────────────────────────────────────────

def setup_schema(total_rows=0):
    """Create bench table and optionally seed rows."""
    c = connect()
    cur = c.cursor()
    cur.execute("DROP TABLE IF EXISTS bench")
    cur.execute("""
        CREATE TABLE bench (
            id INT NOT NULL,
            val INT,
            label TEXT,
            PRIMARY KEY(id)
        )
    """)
    if total_rows > 0:
        for i in range(1, total_rows + 1):
            cur.execute("INSERT INTO bench VALUES (%s, %s, %s)", (i, 0, f"row_{i}"))
    c.close()


# ── Per-thread worker functions ───────────────────────────────────────────────

class WorkerResult:
    __slots__ = ("ops", "errors", "latencies", "elapsed")

    def __init__(self):
        self.ops = 0
        self.errors = 0
        self.latencies = []
        self.elapsed = 0.0


def worker_insert(worker_id, total_clients, total_rows, result):
    """Each worker inserts its share of rows via autocommit."""
    per_worker = total_rows // total_clients
    base = worker_id * per_worker + 1
    conn = connect()
    cur = conn.cursor()
    t0 = time.monotonic()
    for i in range(per_worker):
        row_id = base + i
        t_start = time.monotonic()
        try:
            cur.execute(
                "INSERT INTO bench VALUES (%s, %s, %s)",
                (row_id, worker_id, f"w{worker_id}_{i}"),
            )
            result.ops += 1
        except Exception:
            result.errors += 1
        result.latencies.append(time.monotonic() - t_start)
    result.elapsed = time.monotonic() - t0
    conn.close()


def worker_update_random(worker_id, total_clients, total_rows, result):
    """Each worker updates random rows (low contention)."""
    per_worker = total_rows // total_clients
    conn = connect()
    cur = conn.cursor()
    rng = random.Random(worker_id)
    t0 = time.monotonic()
    for _ in range(per_worker):
        row_id = rng.randint(1, total_rows)
        t_start = time.monotonic()
        try:
            cur.execute(
                "UPDATE bench SET val = val + 1 WHERE id = %s", (row_id,)
            )
            result.ops += 1
        except Exception:
            result.errors += 1
        result.latencies.append(time.monotonic() - t_start)
    result.elapsed = time.monotonic() - t0
    conn.close()


def worker_update_hotspot(worker_id, total_clients, total_rows, result):
    """Each worker updates same 100 rows (high contention)."""
    per_worker = total_rows // total_clients
    conn = connect()
    cur = conn.cursor()
    rng = random.Random(worker_id)
    hotspot = min(100, total_rows)
    t0 = time.monotonic()
    for _ in range(per_worker):
        row_id = rng.randint(1, hotspot)
        t_start = time.monotonic()
        try:
            cur.execute(
                "UPDATE bench SET val = val + 1 WHERE id = %s", (row_id,)
            )
            result.ops += 1
        except Exception:
            result.errors += 1
        result.latencies.append(time.monotonic() - t_start)
    result.elapsed = time.monotonic() - t0
    conn.close()


def worker_mixed(worker_id, total_clients, total_rows, result):
    """Mixed workload: 50% INSERT, 25% UPDATE, 15% SELECT, 10% DELETE."""
    per_worker = total_rows // total_clients
    conn = connect()
    cur = conn.cursor()
    rng = random.Random(worker_id)
    insert_base = total_rows + worker_id * per_worker + 1
    t0 = time.monotonic()
    for i in range(per_worker):
        action = rng.random()
        t_start = time.monotonic()
        try:
            if action < 0.50:
                row_id = insert_base + i
                cur.execute(
                    "INSERT INTO bench VALUES (%s, %s, %s)",
                    (row_id, worker_id, f"mix_{i}"),
                )
            elif action < 0.75:
                row_id = rng.randint(1, total_rows)
                cur.execute(
                    "UPDATE bench SET val = val + 1 WHERE id = %s", (row_id,)
                )
            elif action < 0.90:
                row_id = rng.randint(1, total_rows)
                cur.execute("SELECT id, val FROM bench WHERE id = %s", (row_id,))
                cur.fetchall()
            else:
                row_id = insert_base + i + per_worker
                cur.execute(
                    "INSERT INTO bench VALUES (%s, %s, %s)",
                    (row_id, worker_id, f"del_{i}"),
                )
                cur.execute("DELETE FROM bench WHERE id = %s", (row_id,))
            result.ops += 1
        except Exception:
            result.errors += 1
        result.latencies.append(time.monotonic() - t_start)
    result.elapsed = time.monotonic() - t0
    conn.close()


WORKERS = {
    "insert": worker_insert,
    "update_random": worker_update_random,
    "update_hotspot": worker_update_hotspot,
    "mixed": worker_mixed,
}


# ── Benchmark runner ──────────────────────────────────────────────────────────

def percentile(sorted_list, pct):
    if not sorted_list:
        return 0.0
    idx = int(len(sorted_list) * pct / 100)
    return sorted_list[min(idx, len(sorted_list) - 1)]


def run_scenario(scenario, num_clients, total_rows):
    """Run one scenario with num_clients threads, return aggregated results."""
    worker_fn = WORKERS[scenario]
    needs_seed = scenario in ("update_random", "update_hotspot", "mixed")
    setup_schema(total_rows if needs_seed else 0)

    results = [WorkerResult() for _ in range(num_clients)]
    threads = []
    for wid in range(num_clients):
        t = threading.Thread(
            target=worker_fn,
            args=(wid, num_clients, total_rows, results[wid]),
        )
        threads.append(t)

    wall_start = time.monotonic()
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    wall_elapsed = time.monotonic() - wall_start

    total_ops = sum(r.ops for r in results)
    total_errors = sum(r.errors for r in results)
    all_latencies = sorted(
        lat for r in results for lat in r.latencies
    )
    throughput = total_ops / wall_elapsed if wall_elapsed > 0 else 0
    p50 = percentile(all_latencies, 50) * 1000  # ms
    p99 = percentile(all_latencies, 99) * 1000  # ms

    return {
        "clients": num_clients,
        "scenario": scenario,
        "total_ops": total_ops,
        "throughput": throughput,
        "p50_ms": p50,
        "p99_ms": p99,
        "errors": total_errors,
        "wall_s": wall_elapsed,
    }


def format_row(r):
    return (
        f"  {r['clients']:>3}   {r['scenario']:<16} "
        f"{r['total_ops']:>8}   {r['throughput']:>10,.0f} r/s   "
        f"{r['p50_ms']:>7.2f}ms   {r['p99_ms']:>7.2f}ms   "
        f"{r['errors']:>4}"
    )


# ── Main ──────────────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(description="AxiomDB concurrent benchmark")
    parser.add_argument(
        "--clients", default="1,2,4",
        help="comma-separated client counts (default: 1,2,4)",
    )
    parser.add_argument(
        "--scenario", default="insert",
        help="insert | update_random | update_hotspot | mixed | all",
    )
    parser.add_argument(
        "--rows", type=int, default=5000,
        help="total rows per run (default: 5000)",
    )
    parser.add_argument(
        "--no-server", action="store_true",
        help="skip server start (use external server)",
    )
    args = parser.parse_args()

    client_counts = [int(c) for c in args.clients.split(",")]
    scenarios = list(WORKERS.keys()) if args.scenario == "all" else [args.scenario]

    if not args.no_server:
        print(f"Starting AxiomDB server on :{PORT}...")
        start_server()
        import atexit
        atexit.register(stop_server)

    print(f"\n{'clients':>7}   {'scenario':<16} {'total_ops':>8}   {'throughput':>13}   "
          f"{'p50':>9}   {'p99':>9}   {'errors':>4}")
    print("  " + "─" * 85)

    all_results = []
    for scenario in scenarios:
        if scenario not in WORKERS:
            print(f"Unknown scenario: {scenario}")
            continue
        for nc in client_counts:
            r = run_scenario(scenario, nc, args.rows)
            print(format_row(r))
            all_results.append(r)
        print()

    # Scaling summary.
    for scenario in scenarios:
        scenario_results = [r for r in all_results if r["scenario"] == scenario]
        if len(scenario_results) >= 2:
            base = scenario_results[0]["throughput"]
            if base > 0:
                print(f"  {scenario} scaling:")
                for r in scenario_results:
                    factor = r["throughput"] / base
                    print(f"    {r['clients']} clients: {factor:.1f}x")
                print()

    if not args.no_server:
        stop_server()


if __name__ == "__main__":
    main()
