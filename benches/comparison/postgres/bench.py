#!/usr/bin/env python3
"""
PostgreSQL internal benchmark — runs INSIDE the PostgreSQL container.
Connects to localhost:5432 (no network overhead).
"""
import argparse, json, statistics, sys, time
import psycopg2

WARMUP = 2
RUNS   = 5

DDL_DROP   = "DROP TABLE IF EXISTS bench_users CASCADE"
DDL_CREATE = """CREATE TABLE bench_users (
    id     INT              NOT NULL PRIMARY KEY,
    name   TEXT             NOT NULL,
    age    INT              NOT NULL,
    active BOOLEAN          NOT NULL,
    score  DOUBLE PRECISION NOT NULL,
    email  TEXT             NOT NULL
)"""

def connect():
    c = psycopg2.connect(host="127.0.0.1", port=5432,
                         user="postgres", password="bench", database="bench")
    c.autocommit = False
    return c

def rows(n):
    return [(i, f"user_{i:06d}", 18+(i%62), i%2==0,
             round(100.0+(i%1000)*0.1, 2), f"u{i}@b.local")
            for i in range(1, n+1)]

def reset(cur, conn):
    cur.execute(DDL_DROP)
    cur.execute(DDL_CREATE)
    conn.commit()

def truncate(conn):
    """O(1) table reset — used as setup outside INSERT timing."""
    cur = conn.cursor()
    cur.execute("TRUNCATE TABLE bench_users")
    conn.commit(); cur.close()

def measure(fn):
    for _ in range(WARMUP): fn()
    t = []
    for _ in range(RUNS):
        t0 = time.perf_counter(); fn(); t.append(time.perf_counter()-t0)
    return statistics.mean(t)

def measure_with_setup(setup_fn, fn):
    """Run setup_fn before each iteration (warmup + timed), measure only fn()."""
    for _ in range(WARMUP): setup_fn(); fn()
    t = []
    for _ in range(RUNS):
        setup_fn()
        t0 = time.perf_counter(); fn(); t.append(time.perf_counter()-t0)
    return statistics.mean(t)

def out(scenario, rows_n, mean_s, note=""):
    ops = int(rows_n / mean_s) if mean_s > 0 else 0
    print(json.dumps({"engine":"PostgreSQL 16","scenario":scenario,
                      "rows":rows_n,"mean_ms":round(mean_s*1000,1),
                      "ops_per_s":ops,"note":note}), flush=True)

# ── Scenarios ─────────────────────────────────────────────────────────────────

def s_insert_batch(conn, data):
    """Only INSERT — reset/TRUNCATE happens outside measure via measure_with_setup."""
    cur = conn.cursor()
    cur.executemany("INSERT INTO bench_users VALUES (%s,%s,%s,%s,%s,%s)", data)
    conn.commit(); cur.close()

def s_insert_autocommit(conn, data):
    """Only INSERT — reset/TRUNCATE happens outside measure via measure_with_setup."""
    cur = conn.cursor()
    for row in data:
        cur.execute("INSERT INTO bench_users VALUES (%s,%s,%s,%s,%s,%s)", row)
        conn.commit()
    cur.close()

def s_full_scan(conn):
    cur = conn.cursor(); cur.execute("SELECT * FROM bench_users"); cur.fetchall(); cur.close()

def s_select_where(conn):
    cur = conn.cursor(); cur.execute("SELECT * FROM bench_users WHERE active=TRUE"); cur.fetchall(); cur.close()

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
        # TRUNCATE outside timing — measures INSERT only (fair comparison)
        mean = measure_with_setup(lambda: truncate(conn),
                                  lambda: s_insert_batch(conn, data))
        out(scenario, n_rows, mean, "TRUNCATE outside timing")
    elif scenario == "insert_autocommit":
        mean = measure_with_setup(lambda: truncate(conn),
                                  lambda: s_insert_autocommit(conn, ac))
        out(scenario, len(ac), mean, "TRUNCATE outside timing")
    elif scenario == "crud_flow":
        # Full cycle: INSERT → SELECT * → DELETE, measured separately per phase.
        ins_t, sel_t, del_t = [], [], []
        for i in range(WARMUP + RUNS):
            cur2 = conn.cursor(); reset(cur2, conn); cur2.close()
            # INSERT
            cur2 = conn.cursor()
            t0 = time.perf_counter()
            cur2.executemany("INSERT INTO bench_users VALUES (%s,%s,%s,%s,%s,%s)", data)
            conn.commit()
            t_ins = time.perf_counter() - t0
            cur2.close()
            # SELECT *
            cur2 = conn.cursor()
            t0 = time.perf_counter()
            cur2.execute("SELECT * FROM bench_users"); cur2.fetchall()
            t_sel = time.perf_counter() - t0
            cur2.close()
            # DELETE
            cur2 = conn.cursor()
            t0 = time.perf_counter()
            cur2.execute("DELETE FROM bench_users"); conn.commit()
            t_del = time.perf_counter() - t0
            cur2.close()
            if i >= WARMUP:
                ins_t.append(t_ins); sel_t.append(t_sel); del_t.append(t_del)
        out("crud_flow/insert", n_rows, statistics.mean(ins_t))
        out("crud_flow/select", n_rows, statistics.mean(sel_t))
        out("crud_flow/delete", n_rows, statistics.mean(del_t))
    else:
        cur = conn.cursor(); reset(cur, conn)
        cur.executemany("INSERT INTO bench_users VALUES (%s,%s,%s,%s,%s,%s)", data)
        conn.commit(); cur.close()

        fns = {
            "full_scan":    (lambda: s_full_scan(conn),         n_rows),
            "select_where": (lambda: s_select_where(conn),       n_rows//2),
            "point_lookup": (lambda: s_point_lookup(conn,n_rows), 100),
            "range_scan":   (lambda: s_range_scan(conn,n_rows),   n_rows//10),
            "count_star":   (lambda: s_count_star(conn),          1),
            "group_by":     (lambda: s_group_by(conn),            1),
        }
        if scenario not in fns:
            print(json.dumps({"error": f"unknown scenario: {scenario}"})); sys.exit(1)
        fn, n_ops = fns[scenario]
        out(scenario, n_ops, measure(fn))

    conn.close()

if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("--scenario", required=True)
    p.add_argument("--rows", type=int, default=10_000)
    a = p.parse_args()
    run(a.scenario, a.rows)
