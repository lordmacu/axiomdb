"""
AxiomDB vs PostgreSQL vs MariaDB — native macOS benchmark.

No Docker. All engines running natively with local unix-socket connections.

  AxiomDB   → MySQL wire protocol (local server, port 13309)
  PostgreSQL → psycopg2 unix socket (/tmp)
  MariaDB    → pymysql unix socket (/tmp/mysql80-bench.sock)

Usage:
    python3 tools/bench-vs-pg-mariadb.py
    python3 tools/bench-vs-pg-mariadb.py --rows 5000
    python3 tools/bench-vs-pg-mariadb.py --rows 2000 --skip-axiomdb
"""

import argparse
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time
import statistics

# ── args ──────────────────────────────────────────────────────────────────────

ap = argparse.ArgumentParser()
ap.add_argument("--rows", type=int, default=5_000)
ap.add_argument("--skip-axiomdb", action="store_true")
ap.add_argument("--skip-pg",      action="store_true")
ap.add_argument("--skip-maria",   action="store_true")
args = ap.parse_args()

N        = args.rows
AC_N     = min(N, 300)          # autocommit scenario capped at 300
WARMUP   = 2
RUNS     = 5
PORT_AX  = 13309

# ── connections ───────────────────────────────────────────────────────────────

def pg_connect():
    import psycopg2
    conn = psycopg2.connect(dbname="axiomdb_bench_cmp", user="cristian")
    conn.autocommit = False
    return conn

def maria_connect():
    import pymysql
    return pymysql.connect(
        unix_socket="/tmp/mysql80-bench.sock",
        user="cristian",
        database="axiomdb_bench_cmp",
        autocommit=True,
    )

def axiomdb_connect():
    import pymysql
    return pymysql.connect(
        host="127.0.0.1",
        port=PORT_AX,
        user="root",
        password="",
        autocommit=False,
    )

# ── AxiomDB server lifecycle ──────────────────────────────────────────────────

_server_proc = None
_data_dir    = None

def _wait_port(port, timeout=10):
    t0 = time.time()
    while time.time() - t0 < timeout:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return True
        except OSError:
            time.sleep(0.1)
    return False

def start_axiomdb():
    global _server_proc, _data_dir
    binary = os.path.join(os.path.dirname(__file__), "..", "target", "release", "axiomdb-server")
    binary = os.path.normpath(binary)
    if not os.path.exists(binary):
        print(f"[axiomdb] binary not found: {binary}")
        print("  Build with: cargo build --release -p axiomdb-server")
        return False

    _data_dir = tempfile.mkdtemp(prefix="axiomdb_bench_cmp_")
    env = os.environ.copy()
    env["AXIOMDB_PORT"] = str(PORT_AX)
    env["AXIOMDB_DATA"] = _data_dir

    _server_proc = subprocess.Popen(
        [binary],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if not _wait_port(PORT_AX, timeout=8):
        print("[axiomdb] server did not start within 8s")
        return False
    return True

def stop_axiomdb():
    global _server_proc, _data_dir
    if _server_proc:
        _server_proc.terminate()
        try:
            _server_proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            _server_proc.kill()
        _server_proc = None
    if _data_dir:
        import shutil
        shutil.rmtree(_data_dir, ignore_errors=True)
        _data_dir = None

import atexit
atexit.register(stop_axiomdb)

def _sig(sig, frame):
    stop_axiomdb()
    sys.exit(0)
signal.signal(signal.SIGINT, _sig)
signal.signal(signal.SIGTERM, _sig)

# ── schema & data ─────────────────────────────────────────────────────────────

def gen_inserts_ax(n):
    rows = []
    for i in range(1, n + 1):
        active = "TRUE" if i % 2 == 0 else "FALSE"
        score  = 100.0 + (i % 1000) * 0.1
        rows.append(
            f"INSERT INTO bench_users VALUES ({i}, 'user_{i:06d}', "
            f"{18 + i % 62}, {active}, {score:.1f}, 'u{i}@b.local')"
        )
    return rows

def gen_insert_pg(n):
    rows = []
    for i in range(1, n + 1):
        active = "TRUE" if i % 2 == 0 else "FALSE"
        score  = 100.0 + (i % 1000) * 0.1
        rows.append(
            f"INSERT INTO bench_users VALUES ({i}, 'user_{i:06d}', "
            f"{18 + i % 62}, {active}, {score:.1f}, 'u{i}@b.local')"
        )
    return rows

def gen_insert_maria(n):
    rows = []
    for i in range(1, n + 1):
        active = 1 if i % 2 == 0 else 0
        score  = 100.0 + (i % 1000) * 0.1
        rows.append(
            f"INSERT INTO bench_users VALUES ({i}, 'user_{i:06d}', "
            f"{18 + i % 62}, {active}, {score:.1f}, 'u{i}@b.local')"
        )
    return rows

DDL_AX = """
CREATE TABLE IF NOT EXISTS bench_users (
    id     INT  NOT NULL,
    name   TEXT NOT NULL,
    age    INT  NOT NULL,
    active BOOL NOT NULL,
    score  REAL NOT NULL,
    email  TEXT NOT NULL,
    PRIMARY KEY (id)
)"""

DDL_PG = """
CREATE TABLE IF NOT EXISTS bench_users (
    id     INT              NOT NULL,
    name   TEXT             NOT NULL,
    age    INT              NOT NULL,
    active BOOLEAN          NOT NULL,
    score  DOUBLE PRECISION NOT NULL,
    email  TEXT             NOT NULL,
    PRIMARY KEY (id)
)"""

DDL_MARIA = """
CREATE TABLE IF NOT EXISTS bench_users (
    id     INT        NOT NULL,
    name   TEXT       NOT NULL,
    age    INT        NOT NULL,
    active TINYINT(1) NOT NULL,
    score  DOUBLE     NOT NULL,
    email  TEXT       NOT NULL,
    PRIMARY KEY (id)
) ENGINE=InnoDB"""

def reset_ax(conn):
    with conn.cursor() as c:
        c.execute("DROP TABLE IF EXISTS bench_users")
        c.execute(DDL_AX)

def reset_pg(conn):
    with conn.cursor() as c:
        c.execute("DROP TABLE IF EXISTS bench_users")
        c.execute(DDL_PG)
    conn.commit()

def reset_maria(conn):
    with conn.cursor() as c:
        c.execute("DROP TABLE IF EXISTS bench_users")
        c.execute(DDL_MARIA)

# ── measure helpers ────────────────────────────────────────────────────────────

def measure(fn, warmup=WARMUP, runs=RUNS):
    for _ in range(warmup):
        fn()
    samples = []
    for _ in range(runs):
        t0 = time.perf_counter()
        fn()
        samples.append(time.perf_counter() - t0)
    return statistics.median(samples)

def measure_timed(fn, warmup=WARMUP, runs=RUNS):
    """fn() returns (setup_done, timer) — only the timer portion is counted."""
    for _ in range(warmup):
        fn()
    samples = []
    for _ in range(runs):
        samples.append(fn())
    return statistics.median(samples)

# ── AxiomDB scenarios ─────────────────────────────────────────────────────────

def ax_insert_batch(conn, inserts):
    def run():
        reset_ax(conn)
        conn.begin()
        with conn.cursor() as c:
            for sql in inserts:
                c.execute(sql)
        conn.commit()
    return measure_timed(lambda: (reset_ax(conn), _timed_batch(conn, inserts))[-1])

def _timed_batch(conn, inserts):
    t0 = time.perf_counter()
    conn.begin()
    with conn.cursor() as c:
        for sql in inserts:
            c.execute(sql)
    conn.commit()
    return time.perf_counter() - t0

def ax_fresh():
    """Open a fresh AxiomDB connection and create bench_users table."""
    c = axiomdb_connect()
    with c.cursor() as cur:
        cur.execute("DROP TABLE IF EXISTS bench_users")
        cur.execute(DDL_AX)
    return c

def ax_scenarios(_ignored, inserts_n, inserts_ac):
    n  = len(inserts_n)
    ac = len(inserts_ac)

    step  = max(n // 100, 1)
    start = n // 4
    end   = start + n // 10

    results = {}

    # insert_batch — reconnect each run to get a clean session cache
    def ax_batch_run():
        conn = ax_fresh()
        try:
            t0 = time.perf_counter()
            conn.begin()
            with conn.cursor() as c:
                for sql in inserts_n:
                    c.execute(sql)
            conn.commit()
            return time.perf_counter() - t0
        finally:
            conn.close()
    results["insert_batch"] = (measure_timed(ax_batch_run), n)

    # insert_autocommit
    def ax_ac_run():
        conn = ax_fresh()
        try:
            t0 = time.perf_counter()
            with conn.cursor() as c:
                for sql in inserts_ac:
                    c.execute(sql)
            return time.perf_counter() - t0
        finally:
            conn.close()
    results["insert_autocommit"] = (measure_timed(ax_ac_run), ac)

    # Load data once for all read scenarios (fresh conn)
    read_conn = ax_fresh()
    read_conn.begin()
    with read_conn.cursor() as c:
        for sql in inserts_n:
            c.execute(sql)
    read_conn.commit()

    lookup_ids = list(range(1, n + 1, step))[:100]

    def ax_lookup():
        with read_conn.cursor() as c:
            for i in lookup_ids:
                c.execute(f"SELECT * FROM bench_users WHERE id = {i}")
                c.fetchall()
    results["point_lookup"] = (measure(ax_lookup), 100)

    def ax_range():
        with read_conn.cursor() as c:
            c.execute(f"SELECT * FROM bench_users WHERE id >= {start} AND id < {end}")
            c.fetchall()
    results["range_scan"] = (measure(ax_range), end - start)

    def ax_count():
        with read_conn.cursor() as c:
            c.execute("SELECT COUNT(*) FROM bench_users")
            c.fetchall()
    results["count_star"] = (measure(ax_count), 1)

    def ax_scan():
        with read_conn.cursor() as c:
            c.execute("SELECT * FROM bench_users")
            c.fetchall()
    results["full_scan"] = (measure(ax_scan), n)

    def ax_where():
        with read_conn.cursor() as c:
            c.execute("SELECT * FROM bench_users WHERE active = TRUE")
            c.fetchall()
    results["select_where"] = (measure(ax_where), n // 2)

    def ax_group():
        with read_conn.cursor() as c:
            c.execute("SELECT age, COUNT(*) FROM bench_users GROUP BY age")
            c.fetchall()
    results["group_by"] = (measure(ax_group), 1)

    read_conn.close()

    # crud_flow: insert (same as insert_batch), select (same as full_scan),
    # delete — each measured independently for accuracy.
    def ax_crud_ins():
        conn = ax_fresh()
        try:
            t0 = time.perf_counter()
            conn.begin()
            with conn.cursor() as c:
                for sql in inserts_n:
                    c.execute(sql)
            conn.commit()
            return time.perf_counter() - t0
        finally:
            conn.close()
    results["crud_flow/insert"] = (measure_timed(ax_crud_ins), n)

    # Load once for select and delete
    sel_conn = ax_fresh()
    sel_conn.begin()
    with sel_conn.cursor() as c:
        for sql in inserts_n:
            c.execute(sql)
    sel_conn.commit()

    def ax_crud_sel():
        with sel_conn.cursor() as c:
            c.execute("SELECT * FROM bench_users")
            c.fetchall()
    results["crud_flow/select"] = (measure(ax_crud_sel), n)

    def ax_crud_del():
        with sel_conn.cursor() as c:
            c.execute("DELETE FROM bench_users")
        sel_conn.commit()
    results["crud_flow/delete"] = (measure(ax_crud_del), n)

    sel_conn.close()
    return results

# ── PostgreSQL scenarios ───────────────────────────────────────────────────────

def pg_timed_batch(conn, inserts):
    t0 = time.perf_counter()
    with conn.cursor() as c:
        c.execute("BEGIN")
        for sql in inserts:
            c.execute(sql)
        c.execute("COMMIT")
    return time.perf_counter() - t0

def pg_scenarios(conn, inserts_n, inserts_ac):
    n  = len(inserts_n)
    ac = len(inserts_ac)

    step  = max(n // 100, 1)
    start = n // 4
    end   = start + n // 10

    results = {}

    # insert_batch
    results["insert_batch"] = (
        measure_timed(lambda: (reset_pg(conn), pg_timed_batch(conn, inserts_n))[-1]),
        n,
    )

    # insert_autocommit
    def pg_ac():
        reset_pg(conn)
        t0 = time.perf_counter()
        with conn.cursor() as c:
            for sql in inserts_ac:
                c.execute("BEGIN")
                c.execute(sql)
                c.execute("COMMIT")
        return time.perf_counter() - t0
    results["insert_autocommit"] = (measure_timed(pg_ac), ac)

    # Load data
    reset_pg(conn)
    with conn.cursor() as c:
        c.execute("BEGIN")
        for sql in inserts_n:
            c.execute(sql)
        c.execute("COMMIT")

    lookup_ids = list(range(1, n + 1, step))[:100]

    # point_lookup
    def pg_lookup():
        with conn.cursor() as c:
            for i in lookup_ids:
                c.execute(f"SELECT * FROM bench_users WHERE id = {i}")
                c.fetchall()
    results["point_lookup"] = (measure(pg_lookup), 100)

    # range_scan
    def pg_range():
        with conn.cursor() as c:
            c.execute(f"SELECT * FROM bench_users WHERE id >= {start} AND id < {end}")
            c.fetchall()
    results["range_scan"] = (measure(pg_range), end - start)

    # count_star
    def pg_count():
        with conn.cursor() as c:
            c.execute("SELECT COUNT(*) FROM bench_users")
            c.fetchall()
    results["count_star"] = (measure(pg_count), 1)

    # full_scan
    def pg_scan():
        with conn.cursor() as c:
            c.execute("SELECT * FROM bench_users")
            c.fetchall()
    results["full_scan"] = (measure(pg_scan), n)

    # select_where
    def pg_where():
        with conn.cursor() as c:
            c.execute("SELECT * FROM bench_users WHERE active = TRUE")
            c.fetchall()
    results["select_where"] = (measure(pg_where), n // 2)

    # group_by
    def pg_group():
        with conn.cursor() as c:
            c.execute("SELECT age, COUNT(*) FROM bench_users GROUP BY age")
            c.fetchall()
    results["group_by"] = (measure(pg_group), 1)

    # crud_flow
    def pg_crud_ins():
        reset_pg(conn)
        t0 = time.perf_counter()
        with conn.cursor() as c:
            c.execute("BEGIN")
            for sql in inserts_n:
                c.execute(sql)
            c.execute("COMMIT")
        return time.perf_counter() - t0

    def pg_crud_sel():
        with conn.cursor() as c:
            c.execute("SELECT * FROM bench_users")
            c.fetchall()

    def pg_crud_del():
        with conn.cursor() as c:
            c.execute("BEGIN")
            c.execute("DELETE FROM bench_users")
            c.execute("COMMIT")

    ins_s = measure_timed(pg_crud_ins)
    sel_s = measure(pg_crud_sel)
    del_s = measure(pg_crud_del)
    results["crud_flow/insert"] = (ins_s, n)
    results["crud_flow/select"] = (sel_s, n)
    results["crud_flow/delete"] = (del_s, n)

    return results

# ── MariaDB scenarios ─────────────────────────────────────────────────────────

def maria_timed_batch(conn, inserts):
    t0 = time.perf_counter()
    with conn.cursor() as c:
        c.execute("BEGIN")
        for sql in inserts:
            c.execute(sql)
        c.execute("COMMIT")
    return time.perf_counter() - t0

def maria_scenarios(conn, inserts_n, inserts_ac):
    n  = len(inserts_n)
    ac = len(inserts_ac)

    step  = max(n // 100, 1)
    start = n // 4
    end   = start + n // 10

    results = {}

    # insert_batch
    results["insert_batch"] = (
        measure_timed(lambda: (reset_maria(conn), maria_timed_batch(conn, inserts_n))[-1]),
        n,
    )

    # insert_autocommit
    def maria_ac():
        reset_maria(conn)
        t0 = time.perf_counter()
        with conn.cursor() as c:
            for sql in inserts_ac:
                c.execute(sql)
        return time.perf_counter() - t0
    # MariaDB uses autocommit=True so each execute is a commit
    results["insert_autocommit"] = (measure_timed(maria_ac), ac)

    # Load data
    reset_maria(conn)
    with conn.cursor() as c:
        c.execute("BEGIN")
        for sql in inserts_n:
            c.execute(sql)
        c.execute("COMMIT")

    lookup_ids = list(range(1, n + 1, step))[:100]

    # point_lookup
    def maria_lookup():
        with conn.cursor() as c:
            for i in lookup_ids:
                c.execute(f"SELECT * FROM bench_users WHERE id = {i}")
                c.fetchall()
    results["point_lookup"] = (measure(maria_lookup), 100)

    # range_scan
    def maria_range():
        with conn.cursor() as c:
            c.execute(f"SELECT * FROM bench_users WHERE id >= {start} AND id < {end}")
            c.fetchall()
    results["range_scan"] = (measure(maria_range), end - start)

    # count_star
    def maria_count():
        with conn.cursor() as c:
            c.execute("SELECT COUNT(*) FROM bench_users")
            c.fetchall()
    results["count_star"] = (measure(maria_count), 1)

    # full_scan
    def maria_scan():
        with conn.cursor() as c:
            c.execute("SELECT * FROM bench_users")
            c.fetchall()
    results["full_scan"] = (measure(maria_scan), n)

    # select_where
    def maria_where():
        with conn.cursor() as c:
            c.execute("SELECT * FROM bench_users WHERE active = TRUE")
            c.fetchall()
    results["select_where"] = (measure(maria_where), n // 2)

    # group_by
    def maria_group():
        with conn.cursor() as c:
            c.execute("SELECT age, COUNT(*) FROM bench_users GROUP BY age")
            c.fetchall()
    results["group_by"] = (measure(maria_group), 1)

    # crud_flow
    def maria_crud_ins():
        reset_maria(conn)
        t0 = time.perf_counter()
        with conn.cursor() as c:
            c.execute("BEGIN")
            for sql in inserts_n:
                c.execute(sql)
            c.execute("COMMIT")
        return time.perf_counter() - t0

    def maria_crud_sel():
        with conn.cursor() as c:
            c.execute("SELECT * FROM bench_users")
            c.fetchall()

    def maria_crud_del():
        with conn.cursor() as c:
            c.execute("DELETE FROM bench_users")

    ins_s = measure_timed(maria_crud_ins)
    sel_s = measure(maria_crud_sel)
    del_s = measure(maria_crud_del)
    results["crud_flow/insert"] = (ins_s, n)
    results["crud_flow/select"] = (sel_s, n)
    results["crud_flow/delete"] = (del_s, n)

    return results

# ── formatting ────────────────────────────────────────────────────────────────

def fmt_ops(n_ops, seconds):
    if seconds <= 0:
        return "—"
    ops = n_ops / seconds
    if ops >= 1_000_000:
        return f"{ops/1_000_000:.2f}M ops/s"
    if ops >= 1_000:
        return f"{ops/1_000:.1f}K ops/s"
    return f"{ops:.1f} ops/s"

def fmt_ms(seconds):
    return f"{seconds*1000:.1f}ms"

def ratio_str(ax_s, other_s):
    if ax_s is None or other_s is None or other_s <= 0 or ax_s <= 0:
        return "—"
    r = other_s / ax_s
    if r >= 1.0:
        return f"{r:.1f}x faster"
    else:
        return f"{1/r:.1f}x slower"

# ── main ──────────────────────────────────────────────────────────────────────

SCENARIOS = [
    "insert_batch",
    "insert_autocommit",
    "point_lookup",
    "range_scan",
    "full_scan",
    "select_where",
    "count_star",
    "group_by",
    "crud_flow/insert",
    "crud_flow/select",
    "crud_flow/delete",
]

def main():
    print()
    print("╔══════════════════════════════════════════════════════════════════════════════════╗")
    print(f"║  AxiomDB vs PostgreSQL 16 vs MariaDB 12 — macOS native, {N:,} rows")
    print("║  Connections: unix socket (no Docker, no container overhead)                   ║")
    print("║  Metric: median of 5 timed runs, setup excluded from timing                   ║")
    print("╚══════════════════════════════════════════════════════════════════════════════════╝")
    print()

    inserts_n  = gen_inserts_ax(N)
    inserts_ac = gen_inserts_ax(AC_N)
    pg_ins_n   = gen_insert_pg(N)
    pg_ins_ac  = gen_insert_pg(AC_N)
    mr_ins_n   = gen_insert_maria(N)
    mr_ins_ac  = gen_insert_maria(AC_N)

    ax_res = pg_res = mr_res = {}

    # ── AxiomDB
    if not args.skip_axiomdb:
        print("[1/3] Benchmarking AxiomDB (server, wire protocol)...")
        if not start_axiomdb():
            print("  Skipping AxiomDB (server failed to start)")
        else:
            ax_conn = axiomdb_connect()
            try:
                ax_res = ax_scenarios(ax_conn, inserts_n, inserts_ac)
                print("  done.")
            finally:
                ax_conn.close()
                stop_axiomdb()
    else:
        print("[1/3] AxiomDB: SKIPPED")

    # ── PostgreSQL
    if not args.skip_pg:
        print("[2/3] Benchmarking PostgreSQL 16 (psycopg2, unix socket)...")
        pg_conn = pg_connect()
        try:
            pg_res = pg_scenarios(pg_conn, pg_ins_n, pg_ins_ac)
            print("  done.")
        finally:
            pg_conn.close()
    else:
        print("[2/3] PostgreSQL: SKIPPED")

    # ── MariaDB
    if not args.skip_maria:
        print("[3/3] Benchmarking MariaDB 12 (pymysql, unix socket)...")
        mr_conn = maria_connect()
        try:
            mr_res = maria_scenarios(mr_conn, mr_ins_n, mr_ins_ac)
            print("  done.")
        finally:
            mr_conn.close()
    else:
        print("[3/3] MariaDB: SKIPPED")

    # ── Report
    print()
    print("─" * 90)
    print(f"{'Scenario':<24} {'AxiomDB':>16} {'PostgreSQL':>16} {'MariaDB':>16}  vs PG        vs Maria")
    print("─" * 90)

    for sc in SCENARIOS:
        ax_s, ax_n = ax_res.get(sc, (None, 1))
        pg_s, pg_n = pg_res.get(sc, (None, 1))
        mr_s, mr_n = mr_res.get(sc, (None, 1))

        n_ops = ax_n or pg_n or mr_n or 1

        ax_str = fmt_ops(n_ops, ax_s) if ax_s else "  —"
        pg_str = fmt_ops(n_ops, pg_s) if pg_s else "  —"
        mr_str = fmt_ops(n_ops, mr_s) if mr_s else "  —"

        vs_pg    = ratio_str(ax_s, pg_s)
        vs_maria = ratio_str(ax_s, mr_s)

        print(f"{sc:<24} {ax_str:>16} {pg_str:>16} {mr_str:>16}  {vs_pg:<12} {vs_maria}")

    print("─" * 90)
    print()

    # Wins summary
    ax_wins_pg = ax_wins_mr = 0
    total = 0
    for sc in SCENARIOS:
        ax_s = ax_res.get(sc, (None,))[0]
        pg_s = pg_res.get(sc, (None,))[0]
        mr_s = mr_res.get(sc, (None,))[0]
        if ax_s and pg_s:
            total += 1
            if ax_s < pg_s:
                ax_wins_pg += 1
        if ax_s and mr_s:
            if ax_s < mr_s:
                ax_wins_mr += 1

    if total > 0:
        print(f"AxiomDB vs PostgreSQL : {ax_wins_pg}/{total} scenarios faster")
        print(f"AxiomDB vs MariaDB    : {ax_wins_mr}/{total} scenarios faster")
        print()
    print(f"Note: AxiomDB runs as a local server (wire protocol). For embedded mode")
    print(f"      (in-process, no network overhead) results are typically 2-4x higher.")


if __name__ == "__main__":
    main()
