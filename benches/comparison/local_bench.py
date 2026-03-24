#!/usr/bin/env python3
"""
Local native benchmark: MariaDB 12 vs MySQL 8.0 vs PostgreSQL 16 vs AxiomDB.
All engines run on localhost — no Docker, no container overhead.
All instances are dedicated bench instances (never the app DBs).

Ports:
  MariaDB 12.1  :3308   user=root       password=bench  (bench-only; app MariaDB on :3306)
  MySQL 8.0     :3310   user=root       password=bench
  PostgreSQL 16 :5433   user=postgres   password=bench
  AxiomDB       :3309   user=root       password=root

Start AxiomDB before running:
  AXIOMDB_PORT=3309 AXIOMDB_DATA=/tmp/axiomdb_local ./target/release/axiomdb-server &

Usage:
  python3 benches/comparison/local_bench.py --scenario crud_flow --rows 10000
  python3 benches/comparison/local_bench.py --scenario insert_batch --rows 10000
  python3 benches/comparison/local_bench.py --scenario all --rows 10000 --table
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

# pymysql-compatible engines (MySQL wire protocol)
MYSQL_ENGINES = {
    "MariaDB 12.1":  dict(host="127.0.0.1", port=3308, user="root",     password="bench",
                          database="bench", autocommit=True),
    "MySQL 8.0":     dict(host="127.0.0.1", port=3310, user="root",     password="bench",
                          database="bench", autocommit=True),
    "AxiomDB (wire)":dict(host="127.0.0.1", port=3309, user="root",     password="root",
                          autocommit=True),
}

# psycopg2-compatible engine
PG_ENGINE = {
    "PostgreSQL 16": dict(host="127.0.0.1", port=5433, user="postgres", password="bench",
                          dbname="bench"),
}

# ── DDL ───────────────────────────────────────────────────────────────────────

DDL_DROP_MYSQL  = "DROP TABLE IF EXISTS bench_users"
DDL_DROP_PG     = "DROP TABLE IF EXISTS bench_users CASCADE"

DDL_CREATE_MYSQL = """CREATE TABLE bench_users (
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

# ── Helpers ───────────────────────────────────────────────────────────────────

def connect_mysql(cfg):
    return pymysql.connect(**cfg)

def connect_pg(cfg):
    c = psycopg2.connect(**cfg)
    c.autocommit = True
    return c

def rows_data(n):
    return [(i, f"user_{i:06d}", 18 + (i % 62), i % 2 == 0,
             round(100.0 + (i % 1000) * 0.1, 2), f"u{i}@b.local")
            for i in range(1, n + 1)]

def reset_mysql(conn):
    cur = conn.cursor()
    cur.execute(DDL_DROP_MYSQL)
    cur.execute(DDL_CREATE_MYSQL)
    cur.close()

def reset_pg(conn):
    cur = conn.cursor()
    cur.execute(DDL_DROP_PG)
    cur.execute(DDL_CREATE_PG)
    cur.close()
    conn.commit()

def emit(engine, scenario, n_rows, mean_s, note=""):
    ops = int(n_rows / mean_s) if mean_s > 0 else 0
    print(json.dumps({
        "engine":    engine,
        "scenario":  scenario,
        "rows":      n_rows,
        "mean_ms":   round(mean_s * 1000, 1),
        "ops_per_s": ops,
        "note":      note,
    }), flush=True)

# ── Scenarios — MySQL wire (MariaDB / MySQL / AxiomDB) ────────────────────────

def run_crud_flow_mysql(conn, engine, n_rows, data):
    ins_t, sel_t, del_t = [], [], []
    for i in range(WARMUP + RUNS):
        reset_mysql(conn)
        cur = conn.cursor()
        cur.execute("BEGIN")
        t0 = time.perf_counter()
        cur.executemany("INSERT INTO bench_users VALUES (%s,%s,%s,%s,%s,%s)", data)
        cur.execute("COMMIT")
        t_ins = time.perf_counter() - t0
        cur.close()

        cur = conn.cursor()
        t0 = time.perf_counter()
        cur.execute("SELECT * FROM bench_users"); cur.fetchall()
        t_sel = time.perf_counter() - t0
        cur.close()

        cur = conn.cursor()
        cur.execute("BEGIN")
        t0 = time.perf_counter()
        cur.execute("DELETE FROM bench_users"); cur.execute("COMMIT")
        t_del = time.perf_counter() - t0
        cur.close()

        if i >= WARMUP:
            ins_t.append(t_ins); sel_t.append(t_sel); del_t.append(t_del)

    emit(engine, "crud_flow/insert", n_rows, statistics.mean(ins_t))
    emit(engine, "crud_flow/select", n_rows, statistics.mean(sel_t))
    emit(engine, "crud_flow/delete", n_rows, statistics.mean(del_t))


def run_insert_batch_mysql(conn, engine, n_rows, data):
    def do_insert():
        cur = conn.cursor()
        cur.executemany("INSERT INTO bench_users VALUES (%s,%s,%s,%s,%s,%s)", data)
        cur.execute("COMMIT"); cur.close()
    for _ in range(WARMUP):
        reset_mysql(conn)
        cur = conn.cursor(); cur.execute("BEGIN"); cur.close()
        do_insert()
    t = []
    for _ in range(RUNS):
        reset_mysql(conn)
        cur = conn.cursor(); cur.execute("BEGIN"); cur.close()
        t0 = time.perf_counter(); do_insert(); t.append(time.perf_counter() - t0)
    emit(engine, "insert_batch", n_rows, statistics.mean(t), "BEGIN+reset outside timing")


# ── Scenarios — PostgreSQL ────────────────────────────────────────────────────

# Approximate bytes per row in bench_users (6 columns, mixed types)
_BENCH_ROW_BYTES = 60

def pg_bust_cache(conn, target_bytes):
    """Fill PostgreSQL shared_buffers with synthetic data to evict bench_users pages.

    Creates a temp table large enough to cycle through `target_bytes` of buffer
    pool space, forcing bench_users pages to be evicted before the next timed read.

    target_bytes should be >= shared_buffers size to guarantee full eviction.
    On our bench instance: shared_buffers = 8MB → target_bytes = 9MB.
    """
    # Each row: int (4B) + char(100) = ~104B. 90000 rows ≈ 9.4MB → exceeds 8MB.
    nrows = max(1, target_bytes // 104)
    cur = conn.cursor()
    conn.autocommit = True
    cur.execute(f"""
        CREATE TEMP TABLE IF NOT EXISTS _cache_bust (x INT, pad TEXT);
        TRUNCATE _cache_bust;
        INSERT INTO _cache_bust
            SELECT g, repeat('x', 100)
            FROM generate_series(1, {nrows}) g;
        SELECT count(*) FROM _cache_bust;
    """)
    cur.fetchall()
    cur.close()

# shared_buffers of bench PG instance (8MB). Bust with 9MB to guarantee eviction.
_PG_SHARED_BUFFERS = 8 * 1024 * 1024
_PG_BUST_BYTES     = _PG_SHARED_BUFFERS + 1 * 1024 * 1024  # 9MB


def run_crud_flow_pg(conn, engine, n_rows, data, cold=False):
    """INSERT → SELECT * → DELETE.

    cold=True: evict bench_users from shared_buffers before each SELECT and DELETE
    by flooding the buffer pool with a 9MB synthetic table. This simulates a
    realistic workload where data is not hot from a recent INSERT.

    cold=False (default): 'warm' measurement — data stays in shared_buffers
    between operations, same as production workloads with active buffer pools.
    """
    ins_t, sel_t, del_t = [], [], []
    note = "cold" if cold else "warm"

    for i in range(WARMUP + RUNS):
        reset_pg(conn)
        # INSERT
        conn.autocommit = False
        cur = conn.cursor()
        t0 = time.perf_counter()
        cur.executemany("INSERT INTO bench_users VALUES (%s,%s,%s,%s,%s,%s)", data)
        conn.commit()
        t_ins = time.perf_counter() - t0
        cur.close()

        if cold:
            pg_bust_cache(conn, _PG_BUST_BYTES)

        # SELECT *
        conn.autocommit = True
        cur = conn.cursor()
        t0 = time.perf_counter()
        cur.execute("SELECT * FROM bench_users"); cur.fetchall()
        t_sel = time.perf_counter() - t0
        cur.close()

        if cold:
            pg_bust_cache(conn, _PG_BUST_BYTES)

        # DELETE all
        conn.autocommit = False
        cur = conn.cursor()
        t0 = time.perf_counter()
        cur.execute("DELETE FROM bench_users"); conn.commit()
        t_del = time.perf_counter() - t0
        cur.close()
        conn.autocommit = True

        if i >= WARMUP:
            ins_t.append(t_ins); sel_t.append(t_sel); del_t.append(t_del)

    emit(engine, "crud_flow/insert", n_rows, statistics.mean(ins_t), note)
    emit(engine, "crud_flow/select", n_rows, statistics.mean(sel_t), note)
    emit(engine, "crud_flow/delete", n_rows, statistics.mean(del_t), note)


def run_insert_batch_pg(conn, engine, n_rows, data):
    def do_insert():
        cur = conn.cursor()
        cur.executemany("INSERT INTO bench_users VALUES (%s,%s,%s,%s,%s,%s)", data)
        conn.commit(); cur.close()
    conn.autocommit = False
    for _ in range(WARMUP):
        reset_pg(conn)
        do_insert()
    t = []
    for _ in range(RUNS):
        reset_pg(conn)
        t0 = time.perf_counter(); do_insert(); t.append(time.perf_counter() - t0)
    emit(engine, "insert_batch", n_rows, statistics.mean(t), "reset outside timing")


# ── Runner ────────────────────────────────────────────────────────────────────

SCENARIO_MYSQL = {
    "crud_flow":    run_crud_flow_mysql,
    "insert_batch": run_insert_batch_mysql,
}
SCENARIO_PG = {
    "crud_flow":    run_crud_flow_pg,
    "insert_batch": run_insert_batch_pg,
}

def run_scenario(scenario, n_rows, cold=False):
    data = rows_data(n_rows)

    # MySQL-protocol engines (no cache-bust concept — mmap + OS cache)
    for engine, cfg in MYSQL_ENGINES.items():
        try:
            conn = connect_mysql(cfg)
            SCENARIO_MYSQL[scenario](conn, engine, n_rows, data)
            conn.close()
        except Exception as e:
            print(json.dumps({"engine": engine, "scenario": scenario, "error": str(e)}),
                  flush=True)

    # PostgreSQL — supports cold mode via buffer pool eviction
    if HAS_PG:
        for engine, cfg in PG_ENGINE.items():
            try:
                conn = connect_pg(cfg)
                if scenario == "crud_flow":
                    run_crud_flow_pg(conn, engine, n_rows, data, cold=cold)
                else:
                    SCENARIO_PG[scenario](conn, engine, n_rows, data)
                conn.close()
            except Exception as e:
                print(json.dumps({"engine": engine, "scenario": scenario, "error": str(e)}),
                      flush=True)
    else:
        print(json.dumps({"engine": "PostgreSQL 16", "scenario": scenario,
                          "error": "psycopg2 not installed"}), flush=True)


def print_table(results):
    from collections import defaultdict
    by_scenario = defaultdict(dict)
    for r in results:
        sc = r.get("scenario", "?")
        if "error" in r:
            by_scenario[sc][r["engine"]] = f"ERR"
        else:
            by_scenario[sc][r["engine"]] = (r["mean_ms"], r["ops_per_s"])

    all_engines = list(MYSQL_ENGINES.keys()) + (list(PG_ENGINE.keys()) if HAS_PG else [])
    w = 24
    print()
    hdr = f"{'Scenario':<28}" + "".join(f"  {e:>22}" for e in all_engines)
    print(hdr)
    print("-" * len(hdr))
    for sc in sorted(by_scenario):
        row = f"  {sc:<26}"
        for eng in all_engines:
            v = by_scenario[sc].get(eng)
            if v is None:
                row += f"  {'—':>22}"
            elif isinstance(v, tuple):
                row += f"  {v[0]:>7.1f}ms {v[1]:>9,} r/s"
            else:
                row += f"  {str(v):>22}"
        print(row)
    print()


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("--scenario",
                   choices=list(SCENARIO_MYSQL) + ["all"],
                   default="crud_flow",
                   metavar="{crud_flow,insert_batch,all}")
    p.add_argument("--rows",  type=int, default=10_000)
    p.add_argument("--table", action="store_true", help="pretty-print comparison table")
    p.add_argument("--cold",  action="store_true",
                   help="PostgreSQL: evict shared_buffers before each SELECT/DELETE "
                        "(floods buffer pool with 9MB synthetic data). "
                        "Requires PG bench shared_buffers=8MB. "
                        "Simulates realistic cold-cache workload.")
    a = p.parse_args()

    scenarios = list(SCENARIO_MYSQL) if a.scenario == "all" else [a.scenario]

    if a.table:
        import io, contextlib
        results, buf = [], io.StringIO()
        with contextlib.redirect_stdout(buf):
            for s in scenarios:
                run_scenario(s, a.rows, cold=a.cold)
        for line in buf.getvalue().splitlines():
            try: results.append(json.loads(line))
            except Exception: pass
        sys.stdout.write(buf.getvalue())
        print_table(results)
    else:
        for s in scenarios:
            run_scenario(s, a.rows, cold=a.cold)
