#!/usr/bin/env python3
"""
Join Algorithm Benchmark — Phase 9.10

Compares INNER/LEFT JOIN performance across AxiomDB, MariaDB, MySQL
at different table sizes to validate hash join O(n+m) vs nested loop O(n*m).

Usage:
  python3 join_bench.py [--engines axiomdb,mariadb,mysql]
"""

import argparse
import json
import sys
import time

import pymysql

ENGINE_CONFIGS = {
    "axiomdb": {"host": "127.0.0.1", "port": 3309, "user": "root", "password": "root", "database": "axiomdb"},
    "mariadb": {"host": "127.0.0.1", "port": 3308, "user": "root", "password": "bench", "database": "bench"},
    "mysql":   {"host": "127.0.0.1", "port": 3310, "user": "root", "password": "bench", "database": "bench"},
}

SIZES = [100, 500, 1000, 2000]


def connect(cfg):
    conn = pymysql.connect(host=cfg["host"], port=cfg["port"],
                           user=cfg["user"], password=cfg["password"],
                           database=cfg.get("database"),
                           connect_timeout=3)
    conn.cursor().execute("SET autocommit=1")
    return conn


def setup_tables(conn, n_left, n_right):
    cur = conn.cursor()
    cur.execute("DROP TABLE IF EXISTS jbench_orders")
    cur.execute("DROP TABLE IF EXISTS jbench_users")
    cur.execute("CREATE TABLE jbench_users (id INT PRIMARY KEY, name TEXT, age INT)")
    cur.execute("CREATE TABLE jbench_orders (id INT PRIMARY KEY, user_id INT, amount INT)")

    # Bulk insert users
    cur.execute("BEGIN")
    for i in range(1, n_left + 1):
        cur.execute("INSERT INTO jbench_users VALUES (%s, %s, %s)",
                    (i, f"user_{i}", 20 + i % 50))
    cur.execute("COMMIT")

    # Bulk insert orders (80% of users have orders — some unmatched for LEFT JOIN)
    cur.execute("BEGIN")
    for i in range(1, n_right + 1):
        uid = (i % int(n_left * 0.8)) + 1  # cycles through 80% of users
        cur.execute("INSERT INTO jbench_orders VALUES (%s, %s, %s)",
                    (i, uid, 10 + i % 100))
    cur.execute("COMMIT")
    cur.close()


def bench_query(conn, sql, iterations=20):
    cur = conn.cursor()
    # Warmup
    for _ in range(3):
        cur.execute(sql)
        cur.fetchall()

    times = []
    for _ in range(iterations):
        start = time.perf_counter()
        cur.execute(sql)
        rows = cur.fetchall()
        elapsed = time.perf_counter() - start
        times.append(elapsed)

    cur.close()
    mean_ms = (sum(times) / len(times)) * 1000
    row_count = len(rows)
    return mean_ms, row_count


def run_benchmarks(engines_str):
    engines = [e.strip() for e in engines_str.split(",")]
    results = []

    for size in SIZES:
        n_left = size
        n_right = size * 2  # orders table is 2× users

        for engine in engines:
            cfg = ENGINE_CONFIGS.get(engine)
            if not cfg:
                continue
            try:
                conn = connect(cfg)
            except Exception as e:
                print(f"  {engine}: connection failed ({e})", file=sys.stderr)
                continue

            try:
                setup_tables(conn, n_left, n_right)

                # INNER JOIN
                sql_inner = ("SELECT u.id, u.name, o.amount "
                             "FROM jbench_users u "
                             "INNER JOIN jbench_orders o ON u.id = o.user_id")
                ms_inner, rows_inner = bench_query(conn, sql_inner)

                # LEFT JOIN
                sql_left = ("SELECT u.id, u.name, o.amount "
                            "FROM jbench_users u "
                            "LEFT JOIN jbench_orders o ON u.id = o.user_id")
                ms_left, rows_left = bench_query(conn, sql_left)

                results.append({
                    "engine": engine, "left_size": n_left, "right_size": n_right,
                    "inner_ms": round(ms_inner, 2), "inner_rows": rows_inner,
                    "left_ms": round(ms_left, 2), "left_rows": rows_left,
                })

                # Cleanup
                cur = conn.cursor()
                cur.execute("DROP TABLE IF EXISTS jbench_orders")
                cur.execute("DROP TABLE IF EXISTS jbench_users")
                cur.close()
            finally:
                conn.close()

    return results


def print_results(results):
    print("\n## Join Benchmark Results\n")
    print("| Size (L×R) | Engine | INNER ms | INNER rows | LEFT ms | LEFT rows |")
    print("| --- | --- | --- | --- | --- | --- |")

    for r in sorted(results, key=lambda x: (x["left_size"], x["engine"])):
        print(f"| {r['left_size']}×{r['right_size']} | {r['engine']} | "
              f"{r['inner_ms']}ms | {r['inner_rows']} | "
              f"{r['left_ms']}ms | {r['left_rows']} |")


def main():
    parser = argparse.ArgumentParser(description="Join Algorithm Benchmark")
    parser.add_argument("--engines", default="axiomdb,mariadb,mysql")
    args = parser.parse_args()

    print("Running join benchmarks...", file=sys.stderr)
    results = run_benchmarks(args.engines)

    for r in results:
        print(json.dumps(r))

    print_results(results)


if __name__ == "__main__":
    main()
