#!/usr/bin/env python3
"""
AxiomDB vs MySQL 8.0 vs PostgreSQL 16 — comparison benchmark.
All engines run locally (no Docker).

Ports:
  MySQL 8.0     :3310  root/bench   (brew install mysql@8.0)
  PostgreSQL 16 :5433  postgres/bench (brew install postgresql@16)
  AxiomDB       :3311  root/bench   (axiomdb-server --port 3311)

Usage:
  python3 bench_runner.py              # MySQL + PostgreSQL
  python3 bench_runner.py --rows 50000
  python3 bench_runner.py --all        # + AxiomDB

Prerequisites:
  pip3 install pymysql psycopg2-binary
"""

import argparse, statistics, sys, time

try:
    import pymysql
except ImportError:
    print("pip3 install pymysql"); sys.exit(1)
try:
    import psycopg2
except ImportError:
    print("pip3 install psycopg2-binary"); sys.exit(1)

WARMUP = 2
RUNS   = 5

# ── Connections ───────────────────────────────────────────────────────────────

def conn_mysql():
    c = pymysql.connect(host="127.0.0.1", port=3310,
                        user="root", password="bench", database="bench",
                        autocommit=False)
    return c

def conn_pg():
    c = psycopg2.connect(host="127.0.0.1", port=5433,
                         user="postgres", password="bench", database="bench")
    c.autocommit = False
    return c

def conn_axiomdb():
    c = pymysql.connect(host="127.0.0.1", port=3311,
                        user="root", password="bench", database="bench",
                        autocommit=False)
    return c

# ── Schema ────────────────────────────────────────────────────────────────────

DDL = {
    "mysql": (
        "DROP TABLE IF EXISTS bench_users",
        """CREATE TABLE bench_users (
            id INT NOT NULL PRIMARY KEY, name VARCHAR(255) NOT NULL,
            age INT NOT NULL, active TINYINT(1) NOT NULL,
            score DOUBLE NOT NULL, email VARCHAR(255) NOT NULL
        ) ENGINE=InnoDB"""
    ),
    "pg": (
        "DROP TABLE IF EXISTS bench_users CASCADE",
        """CREATE TABLE bench_users (
            id INT NOT NULL PRIMARY KEY, name TEXT NOT NULL,
            age INT NOT NULL, active BOOLEAN NOT NULL,
            score DOUBLE PRECISION NOT NULL, email TEXT NOT NULL
        )"""
    ),
    "axiomdb": (
        "DROP TABLE IF EXISTS bench_users",
        """CREATE TABLE bench_users (
            id INT NOT NULL, name TEXT NOT NULL, age INT NOT NULL,
            active BOOL NOT NULL, score REAL NOT NULL, email TEXT NOT NULL,
            PRIMARY KEY (id)
        )"""
    ),
}

def reset(conn, engine):
    cur = conn.cursor()
    drop, create = DDL[engine]
    cur.execute(drop)
    cur.execute(create)
    cur.close()
    conn.commit()

def truncate(conn):
    cur = conn.cursor()
    cur.execute("DELETE FROM bench_users")
    cur.close()
    conn.commit()

def rows(n):
    return [(i, f"user_{i:06d}", 18+(i%62), i%2==0,
             round(100.0+(i%1000)*0.1, 2), f"u{i}@b.local")
            for i in range(1, n+1)]

# ── Workloads ─────────────────────────────────────────────────────────────────

def w_insert_batch(conn, data):
    truncate(conn)
    cur = conn.cursor()
    cur.executemany("INSERT INTO bench_users VALUES (%s,%s,%s,%s,%s,%s)", data)
    cur.close()
    conn.commit()

def w_insert_ac(conn, data):
    truncate(conn)
    for row in data:
        cur = conn.cursor()
        cur.execute("INSERT INTO bench_users VALUES (%s,%s,%s,%s,%s,%s)", row)
        cur.close()
        conn.commit()

def w_select_all(conn):
    cur = conn.cursor()
    cur.execute("SELECT * FROM bench_users")
    cur.fetchall()
    cur.close()

def w_select_where(conn):
    cur = conn.cursor()
    cur.execute("SELECT * FROM bench_users WHERE active IS NOT FALSE")
    cur.fetchall()
    cur.close()

def w_point_lookup(conn, n):
    cur = conn.cursor()
    step = max(1, n//100)
    for i in range(1, n+1, step):
        cur.execute("SELECT * FROM bench_users WHERE id = %s", (i,))
        cur.fetchone()
    cur.close()

def w_range_scan(conn, n):
    start, end = n//4, n//4 + n//10
    cur = conn.cursor()
    cur.execute("SELECT * FROM bench_users WHERE id >= %s AND id < %s", (start, end))
    cur.fetchall()
    cur.close()

def w_count(conn):
    cur = conn.cursor()
    cur.execute("SELECT COUNT(*) FROM bench_users")
    cur.fetchone()
    cur.close()

def w_aggregate(conn):
    cur = conn.cursor()
    cur.execute("SELECT age, COUNT(*) AS c, AVG(score) AS a "
                "FROM bench_users GROUP BY age ORDER BY age")
    cur.fetchall()
    cur.close()

# ── Measurement ───────────────────────────────────────────────────────────────

def measure(fn):
    for _ in range(WARMUP):
        fn()
    t = []
    for _ in range(RUNS):
        t0 = time.perf_counter()
        fn()
        t.append(time.perf_counter() - t0)
    return statistics.mean(t), statistics.stdev(t) if len(t) > 1 else 0.0

# ── Output ────────────────────────────────────────────────────────────────────

COL = 24

def print_header(engines):
    h = f"  {'Benchmark':<38}"
    for label, *_ in engines:
        h += f"  {label:^{COL}}"
    sep = "=" * (40 + (COL+2)*len(engines))
    print(sep); print(h); print("-"*len(sep))

def print_row(name, results, n_ops):
    line = f"  {name:<38}"
    for mean_s, _ in results:
        ms  = mean_s * 1000
        ops = int(n_ops / mean_s) if mean_s > 0 else 0
        cell = f"{ms:6.0f} ms  {ops:>9,}/s"
        line += f"  {cell:^{COL}}"
    print(line)

# ── Runner ────────────────────────────────────────────────────────────────────

def run(n_rows, include_axiomdb):
    data    = rows(n_rows)
    ac_data = data[:min(n_rows, 300)]

    print(f"\nConnecting to MySQL 8.0 (port 3310)...")
    mysql = conn_mysql()
    reset(mysql, "mysql")

    print(f"Connecting to PostgreSQL 16 (port 5433)...")
    pg = conn_pg()
    reset(pg, "pg")

    engines = [("MySQL 8.0", mysql, "mysql"), ("PostgreSQL 16", pg, "pg")]

    if include_axiomdb:
        print(f"Connecting to AxiomDB (port 3311)...")
        axdb = conn_axiomdb()
        reset(axdb, "axiomdb")
        engines.append(("AxiomDB", axdb, "axiomdb"))

    def bench(name, fns, n_ops=None):
        print_row(name, [measure(fn) for fn in fns], n_ops or n_rows)

    print(f"\n  {n_rows:,} rows | {RUNS} runs + {WARMUP} warmup | fsync ON | native local")
    print_header(engines)

    # ── INSERT batch ─────────────────────────────────────────────────────────
    print("\n  INSERT (batch — 1 txn)")
    bench(f"insert_batch_{n_rows//1000}K",
          [lambda c=c: w_insert_batch(c, data) for _, c, _ in engines])

    # ── Reload full dataset ──────────────────────────────────────────────────
    for _, c, e in engines:
        reset(c, e)
        w_insert_batch(c, data)

    # ── SELECT / SCAN ────────────────────────────────────────────────────────
    print(f"\n  SELECT / SCAN  ({n_rows:,} rows loaded)")
    bench("select_* full scan",
          [lambda c=c: w_select_all(c) for _, c, _ in engines])
    bench("select_where active=1 (~50%)",
          [lambda c=c: w_select_where(c) for _, c, _ in engines])
    bench("point_lookup PK × 100",
          [lambda c=c, n=n_rows: w_point_lookup(c, n) for _, c, _ in engines],
          n_ops=100)
    bench(f"range_scan 10% ({n_rows//10:,} rows)",
          [lambda c=c, n=n_rows: w_range_scan(c, n) for _, c, _ in engines])

    # ── AGGREGATION ──────────────────────────────────────────────────────────
    print(f"\n  AGGREGATION")
    bench("count(*)",
          [lambda c=c: w_count(c) for _, c, _ in engines], n_ops=1)
    bench("group by age + avg(score)",
          [lambda c=c: w_aggregate(c) for _, c, _ in engines], n_ops=1)

    print("=" * (40 + (COL+2)*len(engines)), "\n")

    for _, c, _ in engines:
        c.close()

if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("--rows", type=int, default=10_000)
    p.add_argument("--all",  action="store_true", help="Include AxiomDB (Phase 8+)")
    a = p.parse_args()
    run(a.rows, a.all)
