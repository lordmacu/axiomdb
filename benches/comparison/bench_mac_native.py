#!/usr/bin/env python3
"""Mac-native multi-engine bench: MariaDB + PostgreSQL + AxiomDB.

All engines run natively on macOS (no Docker). MariaDB via Unix socket as
the current user, PostgreSQL via libpq default, AxiomDB via the embedded
binary (axiomdb_bench).

Each engine runs its scenario serially (avoid network contention). Each
scenario is reset between runs (DROP + CREATE TABLE).

Usage:
  python3 bench_mac_native.py                  # all scenarios
  python3 bench_mac_native.py --rows 5000      # custom dataset size
  python3 bench_mac_native.py insert_batch     # specific scenarios
"""

import argparse
import json
import statistics
import subprocess
import sys
import time
from contextlib import contextmanager

import pymysql
import psycopg2

# ── Config ────────────────────────────────────────────────────────────────────

MARIA_CONN = dict(
    unix_socket="/tmp/mysql80-bench.sock",
    user="cristian",
    database="axbench",
    autocommit=True,
)
PG_CONN_STR = "host=127.0.0.1 dbname=axbench"
AXIOMDB_BIN = "/Users/cristian/nexusdb/.claude/worktrees/priceless-montalcini-945319/target/release/axiomdb_bench"
WARMUP = 2
RUNS = 5

SCENARIOS = [
    "insert_batch",
    "insert_autocommit",
    "full_scan",
    "select_where",
    "point_lookup",
    "range_scan",
    "count_star",
    "group_by",
]

# ── Helpers ───────────────────────────────────────────────────────────────────


def maria_conn():
    return pymysql.connect(**MARIA_CONN)


def pg_conn():
    c = psycopg2.connect(PG_CONN_STR)
    c.autocommit = True
    return c


def gen_inserts(n):
    rows = []
    for i in range(1, n + 1):
        active = "TRUE" if i % 2 == 0 else "FALSE"
        score = 100.0 + (i % 1000) * 0.1
        age = 18 + (i % 62)
        rows.append(
            f"INSERT INTO bench_users VALUES "
            f"({i}, 'user_{i:06}', {age}, {active}, {score:.1f}, 'u{i}@b.local')"
        )
    return rows


def reset_table(cur, dialect):
    cur.execute("DROP TABLE IF EXISTS bench_users")
    if dialect == "maria":
        cur.execute(
            """
            CREATE TABLE bench_users (
              id INT NOT NULL PRIMARY KEY,
              name VARCHAR(64) NOT NULL,
              age INT NOT NULL,
              active BOOLEAN NOT NULL,
              score DOUBLE NOT NULL,
              email VARCHAR(128) NOT NULL
            ) ENGINE=InnoDB
            """
        )
    else:  # postgres
        cur.execute(
            """
            CREATE TABLE bench_users (
              id INT NOT NULL PRIMARY KEY,
              name TEXT NOT NULL,
              age INT NOT NULL,
              active BOOLEAN NOT NULL,
              score DOUBLE PRECISION NOT NULL,
              email TEXT NOT NULL
            )
            """
        )


def measure(closure):
    for _ in range(WARMUP):
        closure()
    times = []
    for _ in range(RUNS):
        t0 = time.perf_counter()
        closure()
        times.append(time.perf_counter() - t0)
    return statistics.median(times)


# ── Engine runners ────────────────────────────────────────────────────────────


def run_sql_engine(dialect, scenario, n_rows):
    """Returns (mean_seconds, n_ops)."""
    inserts = gen_inserts(n_rows)
    ac_n = min(n_rows, 300)
    ac_inserts = inserts[:ac_n]

    conn = maria_conn() if dialect == "maria" else pg_conn()
    cur = conn.cursor()

    if scenario == "insert_batch":
        def fn():
            reset_table(cur, dialect)
            t0 = time.perf_counter()
            cur.execute("START TRANSACTION" if dialect == "maria" else "BEGIN")
            for sql in inserts:
                cur.execute(sql)
            cur.execute("COMMIT")
            return time.perf_counter() - t0
        return _measure_timed(fn), n_rows

    if scenario == "insert_autocommit":
        def fn():
            reset_table(cur, dialect)
            t0 = time.perf_counter()
            # Autocommit ON via connection settings (set per engine).
            for sql in ac_inserts:
                cur.execute(sql)
            return time.perf_counter() - t0
        return _measure_timed(fn), ac_n

    # Read scenarios: load batch first, then measure reads.
    reset_table(cur, dialect)
    cur.execute("START TRANSACTION" if dialect == "maria" else "BEGIN")
    for sql in inserts:
        cur.execute(sql)
    cur.execute("COMMIT")

    if scenario == "full_scan":
        def fn():
            cur.execute("SELECT * FROM bench_users")
            cur.fetchall()
        return measure(fn), n_rows

    if scenario == "select_where":
        def fn():
            cur.execute("SELECT * FROM bench_users WHERE active = TRUE")
            cur.fetchall()
        return measure(fn), n_rows // 2

    if scenario == "point_lookup":
        step = max(n_rows // 100, 1)
        ids = list(range(1, n_rows + 1, step))[:100]
        def fn():
            for i in ids:
                cur.execute(f"SELECT * FROM bench_users WHERE id = {i}")
                cur.fetchall()
        return measure(fn), 100

    if scenario == "range_scan":
        start = n_rows // 4
        end = start + n_rows // 10
        def fn():
            cur.execute(
                f"SELECT * FROM bench_users WHERE id >= {start} AND id < {end}"
            )
            cur.fetchall()
        return measure(fn), n_rows // 10

    if scenario == "count_star":
        def fn():
            cur.execute("SELECT COUNT(*) FROM bench_users")
            cur.fetchall()
        return measure(fn), 1

    if scenario == "group_by":
        def fn():
            cur.execute(
                "SELECT age, COUNT(*), AVG(score) FROM bench_users GROUP BY age"
            )
            cur.fetchall()
        return measure(fn), 1

    raise ValueError(f"unknown scenario {scenario}")


def _measure_timed(fn):
    for _ in range(WARMUP):
        fn()
    times = []
    for _ in range(RUNS):
        times.append(fn())
    return statistics.median(times)


def run_axiomdb(scenario, n_rows):
    """Invokes the embedded bench binary as a subprocess (matches the
    Docker bench runner behaviour: each invocation is a fresh process).
    Returns (ops_per_s, n_ops) — using ops_per_s directly because mean_ms
    rounds to 0.0 for ultra-fast scenarios like COUNT(*)."""
    if scenario in ("insert_appender", "insert_appender_large", "create_index"):
        # Not supported in this comparison — embedded-only.
        return 0.0, 0
    r = subprocess.run(
        [
            AXIOMDB_BIN,
            "--scenario", scenario,
            "--rows", str(n_rows),
        ],
        capture_output=True, text=True, timeout=600,
    )
    if r.returncode != 0:
        return 0.0, 0
    out = json.loads(r.stdout.strip())
    return float(out["ops_per_s"]), out["rows"]


# ── Output ────────────────────────────────────────────────────────────────────


def fmt_ops(n):
    if n >= 1_000_000:
        return f"{n/1_000_000:.2f}M/s"
    if n >= 1_000:
        return f"{n/1_000:.1f}K/s"
    return f"{int(n)}/s"


def main():
    p = argparse.ArgumentParser()
    p.add_argument("scenarios", nargs="*", help="scenarios (default: all)")
    p.add_argument("--rows", type=int, default=10_000)
    args = p.parse_args()
    chosen = args.scenarios or SCENARIOS

    print()
    print("╔" + "═" * 78 + "╗")
    print(
        f"║  Mac-native bench — MariaDB 12.1 | PostgreSQL 16 | AxiomDB embedded"
        + " " * (78 - 66) + "║"
    )
    print(
        f"║  Rows: {args.rows} | runs: {WARMUP}w+{RUNS}m | fsync ON"
        + " " * (78 - 39 - len(str(args.rows))) + "║"
    )
    print("╚" + "═" * 78 + "╝")
    print()

    print(f"{'Scenario':<20} {'MariaDB':>12} {'PostgreSQL':>14} {'AxiomDB':>14}    Winner")
    print("─" * 80)

    for scenario in chosen:
        try:
            maria_t, maria_ops = run_sql_engine("maria", scenario, args.rows)
            maria_ops_s = maria_ops / maria_t if maria_t > 0 else 0
        except Exception as e:
            maria_ops_s = 0
            print(f"MariaDB error on {scenario}: {e}", file=sys.stderr)

        try:
            pg_t, pg_ops = run_sql_engine("pg", scenario, args.rows)
            pg_ops_s = pg_ops / pg_t if pg_t > 0 else 0
        except Exception as e:
            pg_ops_s = 0
            print(f"PostgreSQL error on {scenario}: {e}", file=sys.stderr)

        ax_ops_s, ax_ops = run_axiomdb(scenario, args.rows)

        best = max(maria_ops_s, pg_ops_s, ax_ops_s)
        winner = (
            "MariaDB" if best == maria_ops_s
            else "PG" if best == pg_ops_s
            else "AxiomDB"
        )

        print(
            f"{scenario:<20} {fmt_ops(maria_ops_s):>12} "
            f"{fmt_ops(pg_ops_s):>14} {fmt_ops(ax_ops_s):>14}    {winner}"
        )

    print("─" * 80)


if __name__ == "__main__":
    main()
