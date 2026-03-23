#!/usr/bin/env python3
"""
AxiomDB vs MySQL vs PostgreSQL — comparison benchmark runner.

Measures the same workloads on all three engines with identical conditions:
  - Same schema, same data, same queries
  - Full durability enabled (fsync ON) on all three
  - Docker containers with same CPU/RAM limits for MySQL and PostgreSQL
  - AxiomDB runs natively (no Docker overhead) until Phase 8

Prerequisites:
  pip install pymysql psycopg2-binary
  ./setup.sh    (starts MySQL + PostgreSQL containers)

Usage:
  python3 bench_runner.py                  # MySQL + PostgreSQL only
  python3 bench_runner.py --all            # + AxiomDB (Phase 8+)
  python3 bench_runner.py --rows 10000     # larger dataset
"""

import argparse
import statistics
import sys
import time

try:
    import pymysql
except ImportError:
    print("Missing: pip install pymysql"); sys.exit(1)

try:
    import psycopg2
except ImportError:
    print("Missing: pip install psycopg2-binary"); sys.exit(1)

WARMUP_RUNS = 2
BENCH_RUNS  = 5

# ── Connections ───────────────────────────────────────────────────────────────

def connect_mysql():
    return pymysql.connect(
        host="127.0.0.1", port=3310,
        user="root", password="bench", database="bench",
        autocommit=False,
    )

def connect_pg():
    conn = psycopg2.connect(
        host="127.0.0.1", port=5433,
        user="postgres", password="bench", database="bench",
    )
    conn.autocommit = False
    return conn

def connect_axiomdb():
    # Phase 8+: AxiomDB speaks MySQL wire protocol on port 3311
    return pymysql.connect(
        host="127.0.0.1", port=3311,
        user="root", password="bench", database="bench",
        autocommit=False,
    )

# ── Schema ────────────────────────────────────────────────────────────────────

MYSQL_DDL = """CREATE TABLE IF NOT EXISTS bench_users (
    id     INT          NOT NULL PRIMARY KEY,
    name   VARCHAR(255) NOT NULL,
    age    INT          NOT NULL,
    active TINYINT(1)   NOT NULL,
    score  DOUBLE       NOT NULL,
    email  VARCHAR(255) NOT NULL
) ENGINE=InnoDB"""

PG_DDL = """CREATE TABLE IF NOT EXISTS bench_users (
    id     INT              NOT NULL PRIMARY KEY,
    name   TEXT             NOT NULL,
    age    INT              NOT NULL,
    active BOOLEAN          NOT NULL,
    score  DOUBLE PRECISION NOT NULL,
    email  TEXT             NOT NULL
)"""

AXIOMDB_DDL = """CREATE TABLE IF NOT EXISTS bench_users (
    id     INT  NOT NULL,
    name   TEXT NOT NULL,
    age    INT  NOT NULL,
    active BOOL NOT NULL,
    score  REAL NOT NULL,
    email  TEXT NOT NULL,
    PRIMARY KEY (id)
)"""

def setup_table(conn, ddl, engine):
    cur = conn.cursor()
    drop = "DROP TABLE IF EXISTS bench_users CASCADE" if engine == "pg" \
           else "DROP TABLE IF EXISTS bench_users"
    cur.execute(drop)
    cur.execute(ddl)
    conn.commit()

def generate_rows(n):
    return [
        (i, f"user_{i:06d}", 18 + (i % 62), i % 2 == 0,
         round(100.0 + (i % 1000) * 0.1, 2), f"user{i}@bench.local")
        for i in range(1, n + 1)
    ]

# ── Measurement ───────────────────────────────────────────────────────────────

def measure(fn):
    for _ in range(WARMUP_RUNS):
        fn()
    times = [((t := time.perf_counter()), fn(), time.perf_counter() - t)[2]
             for _ in range(BENCH_RUNS)]
    times = []
    for _ in range(BENCH_RUNS):
        t0 = time.perf_counter()
        fn()
        times.append(time.perf_counter() - t0)
    return statistics.mean(times), (statistics.stdev(times) if len(times) > 1 else 0.0)

# ── Workloads ─────────────────────────────────────────────────────────────────

def insert_batch(conn, rows):
    cur = conn.cursor()
    cur.execute("DELETE FROM bench_users")
    conn.commit()
    cur.executemany("INSERT INTO bench_users VALUES (%s,%s,%s,%s,%s,%s)", rows)
    conn.commit()

def insert_autocommit(conn, rows, engine):
    cur = conn.cursor()
    cur.execute("DELETE FROM bench_users")
    conn.commit()
    if engine == "pg":
        conn.autocommit = True
    for row in rows:
        cur.execute("INSERT INTO bench_users VALUES (%s,%s,%s,%s,%s,%s)", row)
        if engine in ("mysql", "axiomdb"):
            conn.commit()
    if engine == "pg":
        conn.autocommit = False

def select_all(conn):
    cur = conn.cursor(); cur.execute("SELECT * FROM bench_users"); cur.fetchall()

def select_where(conn):
    cur = conn.cursor()
    cur.execute("SELECT * FROM bench_users WHERE active = 1"); cur.fetchall()

def point_lookup(conn, n):
    cur = conn.cursor()
    step = max(1, n // 100)
    for i in range(1, n + 1, step):
        cur.execute("SELECT * FROM bench_users WHERE id = %s", (i,)); cur.fetchone()

def range_scan(conn, n):
    start, end = n // 4, n // 4 + n // 10
    cur = conn.cursor()
    cur.execute("SELECT * FROM bench_users WHERE id >= %s AND id < %s", (start, end))
    cur.fetchall()

def count_star(conn):
    cur = conn.cursor(); cur.execute("SELECT COUNT(*) FROM bench_users"); cur.fetchone()

def aggregate(conn):
    cur = conn.cursor()
    cur.execute("SELECT age, COUNT(*) AS cnt, AVG(score) AS avg_score "
                "FROM bench_users GROUP BY age ORDER BY age")
    cur.fetchall()

# ── Printer ───────────────────────────────────────────────────────────────────

COL = 22

def print_header(engines):
    hdr = f"  {'Benchmark':<36}"
    for label, _, _ in engines:
        hdr += f"  {label:^{COL}}"
    sep = "=" * (38 + (COL + 2) * len(engines))
    print(sep); print(hdr); print("-" * (38 + (COL + 2) * len(engines)))

def print_row(name, results, n_for_ops):
    line = f"  {name:<36}"
    for mean_s, stdev_s in results:
        ms   = mean_s * 1000
        ops  = int(n_for_ops / mean_s) if mean_s > 0 else 0
        cell = f"{ms:6.0f}ms  {ops:>8,}/s"
        line += f"  {cell:^{COL}}"
    print(line)

# ── Main ──────────────────────────────────────────────────────────────────────

def run(n_rows: int, include_axiomdb: bool):
    rows    = generate_rows(n_rows)
    ac_rows = rows[:min(n_rows, 500)]

    print(f"\nConnecting to MySQL (localhost:3310)...")
    mysql = connect_mysql()
    setup_table(mysql, MYSQL_DDL, "mysql")

    print(f"Connecting to PostgreSQL (localhost:5433)...")
    pg = connect_pg()
    setup_table(pg, PG_DDL, "pg")

    engines = [("MySQL 8.0", mysql, "mysql"), ("PostgreSQL 16", pg, "pg")]

    if include_axiomdb:
        print(f"Connecting to AxiomDB (localhost:3311)...")
        axdb = connect_axiomdb()
        setup_table(axdb, AXIOMDB_DDL, "axiomdb")
        engines.append(("AxiomDB", axdb, "axiomdb"))

    # Pre-load for SELECT tests
    print(f"\nLoading {n_rows:,} rows...")
    for _, conn, _ in engines:
        insert_batch(conn, rows)
    print("Done.\n")

    print(f"  {n_rows:,} rows | {BENCH_RUNS} runs + {WARMUP_RUNS} warmup | fsync ON | Docker 2CPU/2GB")
    print_header(engines)

    def bench(name, fns, n_ops=None):
        results = [measure(fn) for fn in fns]
        print_row(name, results, n_ops or n_rows)

    # INSERT
    print(f"\n  INSERT (batch — 1 txn)")
    for _, conn, engine in engines:
        setup_table(conn, MYSQL_DDL if engine=="mysql" else PG_DDL if engine=="pg" else AXIOMDB_DDL, engine)
    bench(f"insert_batch_{n_rows//1000}K",
          [lambda c=conn: insert_batch(c, rows) for _, conn, _ in engines])

    print(f"\n  INSERT (autocommit — 1 txn/row, {len(ac_rows)} rows)")
    bench("insert_autocommit_500",
          [lambda c=conn, e=engine: insert_autocommit(c, ac_rows, e)
           for _, conn, engine in engines],
          n_ops=len(ac_rows))

    # Reload for SELECT tests
    for _, conn, _ in engines:
        insert_batch(conn, rows)

    # SELECT
    print(f"\n  SELECT / SCAN")
    bench("select_* full scan",
          [lambda c=conn: select_all(c) for _, conn, _ in engines])
    bench("select_where active=1 (~50%)",
          [lambda c=conn: select_where(c) for _, conn, _ in engines])
    bench("point_lookup PK × 100",
          [lambda c=conn, n=n_rows: point_lookup(c, n) for _, conn, _ in engines],
          n_ops=100)
    bench(f"range_scan 10% ({n_rows//10:,} rows)",
          [lambda c=conn, n=n_rows: range_scan(c, n) for _, conn, _ in engines])

    # AGGREGATION
    print(f"\n  AGGREGATION")
    bench("count(*)",
          [lambda c=conn: count_star(c) for _, conn, _ in engines], n_ops=1)
    bench("group by age + avg(score)",
          [lambda c=conn: aggregate(c) for _, conn, _ in engines], n_ops=1)

    print("=" * (38 + (COL + 2) * len(engines)), "\n")

    for _, conn, _ in engines:
        conn.close()


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("--rows", type=int, default=10_000)
    p.add_argument("--all",  action="store_true",
                   help="Include AxiomDB (Phase 8+ required)")
    args = p.parse_args()
    run(n_rows=args.rows, include_axiomdb=args.all)
