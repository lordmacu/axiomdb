#!/usr/bin/env python3
"""
MySQL internal benchmark — runs INSIDE the MySQL container.
Connects to localhost:3306 (no network overhead).
Outputs one JSON line per scenario to stdout.

Usage (from host via docker exec):
  docker exec axiomdb_bench_mysql python3 /bench/bench.py \
    --scenario insert_batch --rows 10000
"""
import argparse, json, statistics, sys, time
import pymysql

WARMUP = 2
RUNS   = 5

DDL_DROP   = "DROP TABLE IF EXISTS bench_users"
DDL_CREATE = """CREATE TABLE bench_users (
    id     INT          NOT NULL PRIMARY KEY,
    name   VARCHAR(255) NOT NULL,
    age    INT          NOT NULL,
    active TINYINT(1)   NOT NULL,
    score  DOUBLE       NOT NULL,
    email  VARCHAR(255) NOT NULL
) ENGINE=InnoDB"""

def connect():
    return pymysql.connect(
        host="127.0.0.1", port=3306,
        user="root", password="bench", database="bench",
        autocommit=False,
    )

def rows(n):
    return [(i, f"user_{i:06d}", 18+(i%62), i%2,
             round(100.0+(i%1000)*0.1, 2), f"u{i}@b.local")
            for i in range(1, n+1)]

def reset(cur, conn):
    cur.execute(DDL_DROP)
    cur.execute(DDL_CREATE)
    conn.commit()

def measure(fn):
    for _ in range(WARMUP): fn()
    t = []
    for _ in range(RUNS):
        t0 = time.perf_counter(); fn(); t.append(time.perf_counter()-t0)
    return statistics.mean(t)

def out(scenario, rows_n, mean_s, note=""):
    ops = int(rows_n / mean_s) if mean_s > 0 else 0
    print(json.dumps({"engine":"MySQL 8.0","scenario":scenario,
                      "rows":rows_n,"mean_ms":round(mean_s*1000,1),
                      "ops_per_s":ops,"note":note}), flush=True)

# ── Scenarios ─────────────────────────────────────────────────────────────────

def s_insert_batch(conn, data):
    cur = conn.cursor()
    reset(cur, conn)                    # DROP+CREATE, not DELETE
    cur.executemany("INSERT INTO bench_users VALUES (%s,%s,%s,%s,%s,%s)", data)
    conn.commit(); cur.close()

def s_insert_autocommit(conn, data):
    cur = conn.cursor()
    reset(cur, conn)
    for row in data:
        cur.execute("INSERT INTO bench_users VALUES (%s,%s,%s,%s,%s,%s)", row)
        conn.commit()
    cur.close()

def s_full_scan(conn):
    cur = conn.cursor(); cur.execute("SELECT * FROM bench_users"); cur.fetchall(); cur.close()

def s_select_where(conn):
    cur = conn.cursor(); cur.execute("SELECT * FROM bench_users WHERE active=1"); cur.fetchall(); cur.close()

def s_point_lookup(conn, n):
    cur = conn.cursor()
    step = max(1, n//100)
    for i in range(1, n+1, step):
        cur.execute("SELECT * FROM bench_users WHERE id=%s", (i,)); cur.fetchone()
    cur.close()

def s_range_scan(conn, n):
    start, end = n//4, n//4 + n//10
    cur = conn.cursor()
    cur.execute("SELECT * FROM bench_users WHERE id>=%s AND id<%s", (start, end))
    cur.fetchall(); cur.close()

def s_count_star(conn):
    cur = conn.cursor(); cur.execute("SELECT COUNT(*) FROM bench_users"); cur.fetchone(); cur.close()

def s_group_by(conn):
    cur = conn.cursor()
    cur.execute("SELECT age,COUNT(*),AVG(score) FROM bench_users GROUP BY age")
    cur.fetchall(); cur.close()

# ── Runner ────────────────────────────────────────────────────────────────────

def run(scenario, n_rows):
    conn = connect()
    cur  = conn.cursor(); reset(cur, conn); cur.close()
    data = rows(n_rows)
    ac   = data[:min(n_rows, 300)]

    if scenario == "insert_batch":
        mean = measure(lambda: s_insert_batch(conn, data))
        out(scenario, n_rows, mean)

    elif scenario == "insert_autocommit":
        mean = measure(lambda: s_insert_autocommit(conn, ac))
        out(scenario, len(ac), mean)

    else:
        # Pre-load data for read benchmarks
        cur = conn.cursor(); reset(cur, conn)
        cur.executemany("INSERT INTO bench_users VALUES (%s,%s,%s,%s,%s,%s)", data)
        conn.commit(); cur.close()

        if scenario == "full_scan":
            mean = measure(lambda: s_full_scan(conn))
            out(scenario, n_rows, mean)
        elif scenario == "select_where":
            mean = measure(lambda: s_select_where(conn))
            out(scenario, n_rows//2, mean)
        elif scenario == "point_lookup":
            mean = measure(lambda: s_point_lookup(conn, n_rows))
            out(scenario, 100, mean)
        elif scenario == "range_scan":
            mean = measure(lambda: s_range_scan(conn, n_rows))
            out(scenario, n_rows//10, mean)
        elif scenario == "count_star":
            mean = measure(lambda: s_count_star(conn))
            out(scenario, 1, mean)
        elif scenario == "group_by":
            mean = measure(lambda: s_group_by(conn))
            out(scenario, 1, mean)
        else:
            print(json.dumps({"error": f"unknown scenario: {scenario}"})); sys.exit(1)

    conn.close()

if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("--scenario", required=True)
    p.add_argument("--rows", type=int, default=10_000)
    a = p.parse_args()
    run(a.scenario, a.rows)
