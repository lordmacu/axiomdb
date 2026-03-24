#!/usr/bin/env python3
"""
AxiomDB vs MariaDB vs MySQL 8.0 vs PostgreSQL 16 — comprehensive local benchmark.
All engines run on localhost (no Docker), dedicated bench instances.

Ports:
  MariaDB 12.1  :3308  root/bench   (bench-only; app MariaDB on :3306)
  MySQL 8.0     :3310  root/bench
  PostgreSQL 16 :5433  postgres/bench
  AxiomDB       :3309  root/root

Start AxiomDB first:
  AXIOMDB_PORT=3309 AXIOMDB_DATA=/tmp/axiomdb_local ./target/release/axiomdb-server &

Scenarios:
  insert      — INSERT N rows in one transaction (reset outside timing)
  select      — SELECT * full scan  (data pre-loaded)
  select_where— SELECT * WHERE active = TRUE  (~50% rows)
  update      — UPDATE SET score=score+1 WHERE active=TRUE  (~50% rows)
  delete      — DELETE FROM t  (no WHERE, fast path)
  delete_where— DELETE WHERE id > N/2  (50% rows)
  all         — run all six

Usage:
  python3 benches/comparison/local_bench.py --scenario all --rows 10000 --table
  python3 benches/comparison/local_bench.py --scenario insert --rows 50000 --table
"""
import argparse, json, statistics, sys, time
import pymysql

try:
    import psycopg2
    HAS_PG = True
except ImportError:
    HAS_PG = False

WARMUP = 2
RUNS   = 5

# ── Engine configs ─────────────────────────────────────────────────────────────

MYSQL_ENGINES = {
    "MariaDB 12.1":   dict(host="127.0.0.1", port=3308, user="root", password="bench",
                           database="bench", autocommit=True),
    "MySQL 8.0":      dict(host="127.0.0.1", port=3310, user="root", password="bench",
                           database="bench", autocommit=True),
    "AxiomDB (wire)": dict(host="127.0.0.1", port=3309, user="root", password="root",
                           autocommit=True),
}

PG_ENGINE = {
    "PostgreSQL 16": dict(host="127.0.0.1", port=5433, user="postgres", password="bench",
                          dbname="bench"),
}

DDL_DROP_M  = "DROP TABLE IF EXISTS bench_users"
DDL_DROP_PG = "DROP TABLE IF EXISTS bench_users CASCADE"

DDL_CREATE_M = """CREATE TABLE bench_users (
    id     INT  NOT NULL,
    name   TEXT NOT NULL,
    age    INT  NOT NULL,
    active BOOL NOT NULL,
    score  REAL NOT NULL,
    email  TEXT NOT NULL,
    PRIMARY KEY (id)
)"""

DDL_CREATE_PG = """CREATE TABLE bench_users (
    id     INT              NOT NULL PRIMARY KEY,
    name   TEXT             NOT NULL,
    age    INT              NOT NULL,
    active BOOLEAN          NOT NULL,
    score  DOUBLE PRECISION NOT NULL,
    email  TEXT             NOT NULL
)"""

# ── Helpers ────────────────────────────────────────────────────────────────────

def connect_mysql(cfg):    return pymysql.connect(**cfg)
def connect_pg(cfg):
    c = psycopg2.connect(**cfg)
    c.autocommit = True
    return c

def rows_data(n):
    return [(i, f"user_{i:06d}", 18+(i%62), i%2==0,
             round(100.0+(i%1000)*0.1, 2), f"u{i}@b.local")
            for i in range(1, n+1)]

def reset_mysql(conn):
    cur = conn.cursor()
    cur.execute(DDL_DROP_M); cur.execute(DDL_CREATE_M); cur.close()

def reset_pg(conn):
    cur = conn.cursor()
    cur.execute(DDL_DROP_PG); cur.execute(DDL_CREATE_PG); cur.close()
    conn.commit()

def load_mysql(conn, data):
    cur = conn.cursor()
    cur.execute("BEGIN")
    cur.executemany("INSERT INTO bench_users VALUES (%s,%s,%s,%s,%s,%s)", data)
    cur.execute("COMMIT"); cur.close()

def load_pg(conn, data):
    was_autocommit = conn.autocommit
    conn.autocommit = False
    cur = conn.cursor()
    cur.executemany("INSERT INTO bench_users VALUES (%s,%s,%s,%s,%s,%s)", data)
    conn.commit(); cur.close()
    conn.autocommit = was_autocommit

def emit(engine, scenario, n_ops, mean_s, note=""):
    ops = int(n_ops / mean_s) if mean_s > 0 else 0
    print(json.dumps({
        "engine": engine, "scenario": scenario,
        "rows": n_ops, "mean_ms": round(mean_s*1000, 1),
        "ops_per_s": ops, "note": note,
    }), flush=True)

def timed_runs(setup_fn, bench_fn):
    """WARMUP+RUNS iterations: setup outside timing, bench inside."""
    for _ in range(WARMUP): setup_fn(); bench_fn()
    t = []
    for _ in range(RUNS): setup_fn(); t0 = time.perf_counter(); bench_fn(); t.append(time.perf_counter()-t0)
    return statistics.mean(t)

# ── Scenarios — MySQL wire ─────────────────────────────────────────────────────

def run_insert_mysql(conn, engine, n_rows, data):
    def do():
        cur = conn.cursor()
        cur.executemany("INSERT INTO bench_users VALUES (%s,%s,%s,%s,%s,%s)", data)
        cur.execute("COMMIT"); cur.close()
    mean = timed_runs(
        lambda: (reset_mysql(conn), conn.cursor().execute("BEGIN"), None)[-1],
        do
    )
    emit(engine, "insert", n_rows, mean, "BEGIN+reset outside")

def run_select_mysql(conn, engine, n_rows, _data):
    def do():
        cur = conn.cursor()
        cur.execute("SELECT * FROM bench_users"); cur.fetchall(); cur.close()
    mean = timed_runs(lambda: None, do)
    emit(engine, "select", n_rows, mean)

def run_select_where_mysql(conn, engine, n_rows, _data):
    half = n_rows // 2
    def do():
        cur = conn.cursor()
        cur.execute("SELECT * FROM bench_users WHERE active = TRUE"); cur.fetchall(); cur.close()
    mean = timed_runs(lambda: None, do)
    emit(engine, "select_where", half, mean, "active=TRUE ~50%")

def run_update_mysql(conn, engine, n_rows, _data):
    half = n_rows // 2
    def do():
        cur = conn.cursor()
        cur.execute("BEGIN")
        cur.execute("UPDATE bench_users SET score = score + 1.0 WHERE active = TRUE")
        cur.execute("COMMIT"); cur.close()
    # After each timed run, reset score so next run updates same rows
    def setup():
        cur = conn.cursor()
        cur.execute("BEGIN")
        cur.execute("UPDATE bench_users SET score = 100.0 WHERE active = TRUE")
        cur.execute("COMMIT"); cur.close()
    mean = timed_runs(setup, do)
    emit(engine, "update", half, mean, "WHERE active=TRUE ~50%")

def run_delete_mysql(conn, engine, n_rows, _data):
    def do():
        cur = conn.cursor()
        cur.execute("BEGIN"); cur.execute("DELETE FROM bench_users"); cur.execute("COMMIT"); cur.close()
    mean = timed_runs(lambda: load_mysql(conn, _data), do)
    emit(engine, "delete", n_rows, mean, "no WHERE")

def run_delete_where_mysql(conn, engine, n_rows, data):
    half = n_rows // 2
    threshold = half
    def do():
        cur = conn.cursor()
        cur.execute("BEGIN")
        cur.execute(f"DELETE FROM bench_users WHERE id > {threshold}")
        cur.execute("COMMIT"); cur.close()
    mean = timed_runs(lambda: (reset_mysql(conn), load_mysql(conn, data)), do)
    emit(engine, "delete_where", half, mean, f"WHERE id>{threshold}")

# ── Scenarios — PostgreSQL ─────────────────────────────────────────────────────

def _pg_exec(conn, sql, commit=False):
    conn.autocommit = not commit
    cur = conn.cursor(); cur.execute(sql); cur.close()
    if commit: conn.commit()
    conn.autocommit = True

def run_insert_pg(conn, engine, n_rows, data):
    def setup():
        reset_pg(conn)
        _pg_exec(conn, "BEGIN", commit=False)
    def do():
        conn.autocommit = False
        cur = conn.cursor()
        cur.executemany("INSERT INTO bench_users VALUES (%s,%s,%s,%s,%s,%s)", data)
        conn.commit(); cur.close()
        conn.autocommit = True
    mean = timed_runs(setup, do)
    emit(engine, "insert", n_rows, mean, "reset outside")

def run_select_pg(conn, engine, n_rows, _data):
    def do():
        cur = conn.cursor(); cur.execute("SELECT * FROM bench_users"); cur.fetchall(); cur.close()
    mean = timed_runs(lambda: None, do)
    emit(engine, "select", n_rows, mean)

def run_select_where_pg(conn, engine, n_rows, _data):
    half = n_rows // 2
    def do():
        cur = conn.cursor(); cur.execute("SELECT * FROM bench_users WHERE active = TRUE"); cur.fetchall(); cur.close()
    mean = timed_runs(lambda: None, do)
    emit(engine, "select_where", half, mean, "active=TRUE ~50%")

def run_update_pg(conn, engine, n_rows, _data):
    half = n_rows // 2
    def do():
        conn.autocommit = False
        cur = conn.cursor()
        cur.execute("UPDATE bench_users SET score = score + 1.0 WHERE active = TRUE")
        conn.commit(); cur.close()
        conn.autocommit = True
    def setup():
        conn.autocommit = False
        cur = conn.cursor()
        cur.execute("UPDATE bench_users SET score = 100.0 WHERE active = TRUE")
        conn.commit(); cur.close()
        conn.autocommit = True
    mean = timed_runs(setup, do)
    emit(engine, "update", half, mean, "WHERE active=TRUE ~50%")

def run_delete_pg(conn, engine, n_rows, data):
    def do():
        conn.autocommit = False
        cur = conn.cursor(); cur.execute("DELETE FROM bench_users"); conn.commit(); cur.close()
        conn.autocommit = True
    mean = timed_runs(lambda: load_pg(conn, data), do)
    emit(engine, "delete", n_rows, mean, "no WHERE")

def run_delete_where_pg(conn, engine, n_rows, data):
    half = n_rows // 2
    threshold = half
    def do():
        conn.autocommit = False
        cur = conn.cursor()
        cur.execute(f"DELETE FROM bench_users WHERE id > {threshold}")
        conn.commit(); cur.close()
        conn.autocommit = True
    mean = timed_runs(lambda: (reset_pg(conn), load_pg(conn, data)), do)
    emit(engine, "delete_where", half, mean, f"WHERE id>{threshold}")

# ── Runner ─────────────────────────────────────────────────────────────────────

SCENARIOS_MYSQL = {
    "insert":       run_insert_mysql,
    "select":       run_select_mysql,
    "select_where": run_select_where_mysql,
    "update":       run_update_mysql,
    "delete":       run_delete_mysql,
    "delete_where": run_delete_where_mysql,
}
SCENARIOS_PG = {
    "insert":       run_insert_pg,
    "select":       run_select_pg,
    "select_where": run_select_where_pg,
    "update":       run_update_pg,
    "delete":       run_delete_pg,
    "delete_where": run_delete_where_pg,
}
READ_ONLY = {"select", "select_where"}    # don't need reset before each run
NEEDS_PRELOAD = READ_ONLY | {"update"}    # must load data before timing

def run_scenario(scenario, n_rows):
    data = rows_data(n_rows)

    for engine, cfg in MYSQL_ENGINES.items():
        try:
            conn = connect_mysql(cfg)
            reset_mysql(conn)
            if scenario in NEEDS_PRELOAD:
                load_mysql(conn, data)
            SCENARIOS_MYSQL[scenario](conn, engine, n_rows, data)
            conn.close()
        except Exception as e:
            print(json.dumps({"engine": engine, "scenario": scenario, "error": str(e)}), flush=True)

    if HAS_PG:
        for engine, cfg in PG_ENGINE.items():
            try:
                conn = connect_pg(cfg)
                reset_pg(conn)
                if scenario in NEEDS_PRELOAD:
                    load_pg(conn, data)
                SCENARIOS_PG[scenario](conn, engine, n_rows, data)
                conn.close()
            except Exception as e:
                print(json.dumps({"engine": engine, "scenario": scenario, "error": str(e)}), flush=True)

def print_table(results):
    from collections import defaultdict
    by_sc = defaultdict(dict)
    for r in results:
        sc = r.get("scenario", "?")
        if "error" in r:
            by_sc[sc][r["engine"]] = "ERR"
        else:
            by_sc[sc][r["engine"]] = (r["mean_ms"], r["ops_per_s"])

    all_engines = list(MYSQL_ENGINES) + (list(PG_ENGINE) if HAS_PG else [])
    print()
    hdr = f"{'Scenario':<18}" + "".join(f"  {e:>26}" for e in all_engines)
    print(hdr)
    print("-" * len(hdr))
    for sc in ["insert","select","select_where","update","delete","delete_where"]:
        if sc not in by_sc: continue
        row = f"  {sc:<16}"
        for eng in all_engines:
            v = by_sc[sc].get(eng)
            if v is None:   row += f"  {'—':>26}"
            elif isinstance(v, tuple): row += f"  {v[0]:>8.1f}ms  {v[1]:>10,} r/s"
            else:           row += f"  {str(v):>26}"
        print(row)
    print()

# ── Main ───────────────────────────────────────────────────────────────────────

ALL_SCENARIOS = list(SCENARIOS_MYSQL)

if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("--scenario", choices=ALL_SCENARIOS + ["all"], default="all",
                   metavar="{" + ",".join(ALL_SCENARIOS) + ",all}")
    p.add_argument("--rows",  type=int, default=10_000)
    p.add_argument("--table", action="store_true", help="pretty-print comparison table")
    p.add_argument("--cold",  action="store_true",
                   help="PostgreSQL: evict shared_buffers before SELECT/DELETE via cache-bust table")
    a = p.parse_args()

    scenarios = ALL_SCENARIOS if a.scenario == "all" else [a.scenario]

    if a.table:
        import io, contextlib
        results, buf = [], io.StringIO()
        with contextlib.redirect_stdout(buf):
            for s in scenarios: run_scenario(s, a.rows)
        for line in buf.getvalue().splitlines():
            try: results.append(json.loads(line))
            except Exception: pass
        sys.stdout.write(buf.getvalue())
        print_table(results)
    else:
        for s in scenarios: run_scenario(s, a.rows)
