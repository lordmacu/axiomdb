#!/usr/bin/env python3
"""
AxiomDB vs MariaDB vs MySQL 8.0 vs PostgreSQL 16 — fair local benchmark.

All engines run on localhost (no Docker), dedicated bench instances.

Ports:
  MariaDB 12.1  :3308  root/bench   (bench-only; app MariaDB on :3306)
  MySQL 8.0     :3310  root/bench
  PostgreSQL 16 :5433  postgres/bench
  AxiomDB       :3309  root/root

Start AxiomDB first:
  AXIOMDB_PORT=3309 AXIOMDB_DATA=/tmp/axiomdb_local ./target/release/axiomdb-server &

Scenarios:
  insert              — N single-row INSERT statements inside one explicit txn
  insert_multi_values — one or more INSERT ... VALUES (...),(...),... statements
  insert_autocommit   — one INSERT per transaction (worst-case durability)
  select              — SELECT * full scan  (data pre-loaded)
  select_where        — SELECT * WHERE active = TRUE  (~50% rows)
  select_pk           — point lookups by primary key
  select_range        — contiguous primary-key range scan
  count               — SELECT COUNT(*)
  aggregate           — GROUP BY age + AVG(score)
  update              — UPDATE score WHERE active = TRUE  (~50% rows)
  update_range        — UPDATE score over a primary-key range
  delete              — DELETE FROM t  (no WHERE, fast path)
  delete_where        — DELETE WHERE id > N/2  (50% rows)
  all                 — run the full fair scenario set above

Usage:
  python3 benches/comparison/local_bench.py --scenario all --rows 50000 --table
  python3 benches/comparison/local_bench.py --scenario insert_multi_values --rows 50000
  python3 benches/comparison/local_bench.py --scenario select_where --rows 50000 --indexes active

Fairness rules:
  - Timed INSERT paths avoid executemany(); drivers batch INSERT differently.
  - The same schema and optional secondary indexes are created on every engine.
  - Point lookups and range scans use deterministic key sets across engines.
"""

import argparse
import json
import statistics
import sys
import time

import pymysql

try:
    import psycopg2

    HAS_PG = True
except ImportError:
    HAS_PG = False

WARMUP = 2
RUNS = 5

VALID_INDEXES = {"active", "age", "score"}

# ── Engine configs ─────────────────────────────────────────────────────────────

ENGINE_CONFIGS = {
    "mariadb": (
        "MariaDB 12.1",
        dict(
        kind="mysql",
        host="127.0.0.1",
        port=3308,
        user="root",
        password="bench",
        database="bench",
        autocommit=True,
        ),
    ),
    "mysql": (
        "MySQL 8.0",
        dict(
        kind="mysql",
        host="127.0.0.1",
        port=3310,
        user="root",
        password="bench",
        database="bench",
        autocommit=True,
        ),
    ),
    "axiomdb": (
        "AxiomDB",
        dict(
        kind="axiomdb",
        host="127.0.0.1",
        port=3309,
        user="root",
        password="",
        autocommit=True,
        ),
    ),
    "postgres": (
        "PostgreSQL 16",
        dict(
        kind="pg",
        host="127.0.0.1",
        port=5433,
        user="postgres",
        password="bench",
        dbname="bench",
        ),
    ),
}

DEFAULT_ENGINES = ["mariadb", "mysql", "axiomdb"]

PRINT_ORDER = [
    "insert",
    "insert_multi_values",
    "insert_autocommit",
    "select",
    "select_where",
    "select_pk",
    "select_range",
    "count",
    "aggregate",
    "update",
    "update_range",
    "delete",
    "delete_where",
]

PRELOADED_SCENARIOS = {
    "select",
    "select_where",
    "select_pk",
    "select_range",
    "count",
    "aggregate",
    "update",
    "update_range",
}


# ── Helpers ────────────────────────────────────────────────────────────────────

def connect_mysql(cfg):
    params = dict(cfg)
    params.pop("kind", None)
    return pymysql.connect(connect_timeout=3, **params)


def connect_pg(cfg):
    params = dict(cfg)
    params.pop("kind", None)
    conn = psycopg2.connect(**params)
    conn.autocommit = True
    return conn


def rows_data(n):
    return [
        (
            i,
            f"user_{i:06d}",
            18 + (i % 62),
            i % 2 == 0,
            round(100.0 + (i % 1000) * 0.1, 2),
            f"u{i}@b.local",
        )
        for i in range(1, n + 1)
    ]


def parse_indexes(raw):
    if not raw:
        return []
    indexes = sorted({part.strip().lower() for part in raw.split(",") if part.strip()})
    invalid = [col for col in indexes if col not in VALID_INDEXES]
    if invalid:
        raise SystemExit(
            f"Unsupported --indexes value(s): {', '.join(invalid)}. "
            f"Valid columns: {', '.join(sorted(VALID_INDEXES))}"
        )
    return indexes


def parse_engines(raw):
    engines = [part.strip().lower() for part in raw.split(",") if part.strip()]
    if not engines:
        raise SystemExit("At least one engine must be selected in --engines")
    invalid = [name for name in engines if name not in ENGINE_CONFIGS]
    if invalid:
        raise SystemExit(
            f"Unsupported --engines value(s): {', '.join(invalid)}. "
            f"Valid engines: {', '.join(ENGINE_CONFIGS)}"
        )
    if "postgres" in engines and not HAS_PG:
        raise SystemExit("PostgreSQL selected in --engines but psycopg2 is not installed")
    seen = set()
    ordered = []
    for name in engines:
        if name not in seen:
            seen.add(name)
            ordered.append(name)
    return ordered


def schema_statements(kind, indexes):
    if kind == "pg":
        statements = [
            "DROP TABLE IF EXISTS bench_users CASCADE",
            """CREATE TABLE bench_users (
    id     INT              NOT NULL PRIMARY KEY,
    name   TEXT             NOT NULL,
    age    INT              NOT NULL,
    active BOOLEAN          NOT NULL,
    score  DOUBLE PRECISION NOT NULL,
    email  TEXT             NOT NULL
)""",
        ]
    elif kind == "mysql":
        statements = [
            "DROP TABLE IF EXISTS bench_users",
            """CREATE TABLE bench_users (
    id     INT          NOT NULL,
    name   VARCHAR(255) NOT NULL,
    age    INT          NOT NULL,
    active BOOL         NOT NULL,
    score  DOUBLE       NOT NULL,
    email  VARCHAR(255) NOT NULL,
    PRIMARY KEY (id)
) ENGINE=InnoDB""",
        ]
    elif kind == "axiomdb":
        statements = [
            "DROP TABLE IF EXISTS bench_users",
            """CREATE TABLE bench_users (
    id     INT  NOT NULL,
    name   TEXT NOT NULL,
    age    INT  NOT NULL,
    active BOOL NOT NULL,
    score  REAL NOT NULL,
    email  TEXT NOT NULL,
    PRIMARY KEY (id)
)""",
        ]
    else:
        raise ValueError(f"unknown engine kind: {kind}")

    for col in indexes:
        statements.append(f"CREATE INDEX idx_bench_users_{col} ON bench_users ({col})")
    return statements


def exec_statements(conn, statements, transactional=False):
    cur = conn.cursor()
    if transactional:
        cur.execute("BEGIN")
    for stmt in statements:
        cur.execute(stmt)
    if transactional:
        cur.execute("COMMIT")
    cur.close()


def reset_table(conn, kind, indexes):
    exec_statements(conn, schema_statements(kind, indexes))


def sql_literal(value):
    if isinstance(value, bool):
        return "TRUE" if value else "FALSE"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        return f"{value:.2f}"
    return "'" + str(value).replace("'", "''") + "'"


def chunked(seq, size):
    for i in range(0, len(seq), size):
        yield seq[i : i + size]


def evenly_spaced_ids(n_rows, count):
    if n_rows <= 0:
        return []
    count = max(1, min(count, n_rows))
    if count == n_rows:
        return list(range(1, n_rows + 1))
    if count == 1:
        return [1]
    step = max(1, n_rows // count)
    ids = list(range(1, n_rows + 1, step))
    return ids[:count]


def prepare_workload(n_rows, multi_values_chunk, autocommit_rows, point_lookups, range_rows):
    data = rows_data(n_rows)
    row_values_sql = ["(" + ",".join(sql_literal(v) for v in row) + ")" for row in data]
    single_insert_sqls = [f"INSERT INTO bench_users VALUES {values}" for values in row_values_sql]

    multi_values_chunk = max(1, multi_values_chunk)
    insert_multi_values_sqls = [
        "INSERT INTO bench_users VALUES " + ",".join(chunk)
        for chunk in chunked(row_values_sql, multi_values_chunk)
    ]

    autocommit_target = autocommit_rows if autocommit_rows is not None else min(n_rows, 1000)
    point_lookup_target = point_lookups if point_lookups is not None else min(n_rows, 100)
    range_target = range_rows if range_rows is not None else max(1, n_rows // 10)

    autocommit_n = min(n_rows, max(1, autocommit_target))
    point_lookup_n = min(n_rows, max(1, point_lookup_target))
    range_target = min(n_rows, max(1, range_target))

    range_start = max(1, n_rows // 4)
    range_end = min(n_rows + 1, range_start + range_target)
    range_count = max(0, range_end - range_start)
    half = n_rows // 2

    return {
        "n_rows": n_rows,
        "data": data,
        "insert_single_sqls": single_insert_sqls,
        "insert_multi_values_sqls": insert_multi_values_sqls,
        "insert_autocommit_sqls": single_insert_sqls[:autocommit_n],
        "autocommit_rows": autocommit_n,
        "point_lookup_ids": evenly_spaced_ids(n_rows, point_lookup_n),
        "point_lookup_rows": point_lookup_n,
        "select_sql": "SELECT * FROM bench_users",
        "select_where_sql": "SELECT * FROM bench_users WHERE active = TRUE",
        "count_sql": "SELECT COUNT(*) FROM bench_users",
        "aggregate_sql": (
            "SELECT age, COUNT(*) AS c, AVG(score) AS a "
            "FROM bench_users GROUP BY age ORDER BY age"
        ),
        "range_start": range_start,
        "range_end": range_end,
        "range_rows": range_count,
        "select_range_sql": (
            f"SELECT * FROM bench_users WHERE id >= {range_start} AND id < {range_end}"
        ),
        "update_where_sql": "UPDATE bench_users SET score = score + 1.0 WHERE active = TRUE",
        "reset_where_sql": "UPDATE bench_users SET score = 100.0 WHERE active = TRUE",
        "update_range_sql": (
            f"UPDATE bench_users SET score = score + 1.0 "
            f"WHERE id >= {range_start} AND id < {range_end}"
        ),
        "reset_range_sql": (
            f"UPDATE bench_users SET score = 100.0 "
            f"WHERE id >= {range_start} AND id < {range_end}"
        ),
        "delete_sql": "DELETE FROM bench_users",
        "delete_where_sql": f"DELETE FROM bench_users WHERE id > {half}",
        "delete_where_rows": n_rows - half,
    }


def preload_table(conn, workload):
    exec_statements(conn, workload["insert_multi_values_sqls"], transactional=True)


def emit(engine, scenario, n_ops, mean_s, note=""):
    ops = int(n_ops / mean_s) if mean_s > 0 else 0
    print(
        json.dumps(
            {
                "engine": engine,
                "scenario": scenario,
                "rows": n_ops,
                "mean_ms": round(mean_s * 1000, 1),
                "ops_per_s": ops,
                "note": note,
            }
        ),
        flush=True,
    )


def timed_runs(setup_fn, bench_fn):
    """WARMUP+RUNS iterations: setup outside timing, bench inside."""
    for _ in range(WARMUP):
        setup_fn()
        bench_fn()
    samples = []
    for _ in range(RUNS):
        setup_fn()
        t0 = time.perf_counter()
        bench_fn()
        samples.append(time.perf_counter() - t0)
    return statistics.mean(samples)


# ── Scenarios ─────────────────────────────────────────────────────────────────

def run_insert(conn, engine, kind, indexes, workload):
    def do():
        exec_statements(conn, workload["insert_single_sqls"], transactional=True)

    mean = timed_runs(lambda: reset_table(conn, kind, indexes), do)
    emit(engine, "insert", workload["n_rows"], mean, "single-row INSERTs in 1 txn")


def run_insert_multi_values(conn, engine, kind, indexes, workload):
    def do():
        exec_statements(conn, workload["insert_multi_values_sqls"], transactional=True)

    mean = timed_runs(lambda: reset_table(conn, kind, indexes), do)
    emit(
        engine,
        "insert_multi_values",
        workload["n_rows"],
        mean,
        f"chunked VALUES statements ({len(workload['insert_multi_values_sqls'])} stmt)",
    )


def run_insert_autocommit(conn, engine, kind, indexes, workload):
    def do():
        exec_statements(conn, workload["insert_autocommit_sqls"], transactional=False)

    mean = timed_runs(lambda: reset_table(conn, kind, indexes), do)
    emit(
        engine,
        "insert_autocommit",
        workload["autocommit_rows"],
        mean,
        "one INSERT per transaction",
    )


def run_select(conn, engine, _kind, _indexes, workload):
    def do():
        cur = conn.cursor()
        cur.execute(workload["select_sql"])
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "select", workload["n_rows"], mean)


def run_select_where(conn, engine, _kind, _indexes, workload):
    half = workload["n_rows"] // 2

    def do():
        cur = conn.cursor()
        cur.execute(workload["select_where_sql"])
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "select_where", half, mean, "active=TRUE ~50%")


def run_select_pk(conn, engine, _kind, _indexes, workload):
    queries = [f"SELECT * FROM bench_users WHERE id = {row_id}" for row_id in workload["point_lookup_ids"]]

    def do():
        cur = conn.cursor()
        for sql in queries:
            cur.execute(sql)
            cur.fetchone()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "select_pk", workload["point_lookup_rows"], mean, "primary-key lookups")


def run_select_range(conn, engine, _kind, _indexes, workload):
    def do():
        cur = conn.cursor()
        cur.execute(workload["select_range_sql"])
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(
        engine,
        "select_range",
        workload["range_rows"],
        mean,
        f"id range [{workload['range_start']}, {workload['range_end']})",
    )


def run_count(conn, engine, _kind, _indexes, workload):
    def do():
        cur = conn.cursor()
        cur.execute(workload["count_sql"])
        cur.fetchone()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "count", 1, mean)


def run_aggregate(conn, engine, _kind, _indexes, workload):
    def do():
        cur = conn.cursor()
        cur.execute(workload["aggregate_sql"])
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "aggregate", 1, mean, "group by age + avg(score)")


def run_update(conn, engine, _kind, _indexes, workload):
    half = workload["n_rows"] // 2

    def do():
        exec_statements(conn, [workload["update_where_sql"]], transactional=True)

    def setup():
        exec_statements(conn, [workload["reset_where_sql"]], transactional=True)

    mean = timed_runs(setup, do)
    emit(engine, "update", half, mean, "WHERE active=TRUE ~50%")


def run_update_range(conn, engine, _kind, _indexes, workload):
    def do():
        exec_statements(conn, [workload["update_range_sql"]], transactional=True)

    def setup():
        exec_statements(conn, [workload["reset_range_sql"]], transactional=True)

    mean = timed_runs(setup, do)
    emit(
        engine,
        "update_range",
        workload["range_rows"],
        mean,
        f"id range [{workload['range_start']}, {workload['range_end']})",
    )


def run_delete(conn, engine, _kind, _indexes, workload):
    def do():
        exec_statements(conn, [workload["delete_sql"]], transactional=True)

    mean = timed_runs(lambda: preload_table(conn, workload), do)
    emit(engine, "delete", workload["n_rows"], mean, "no WHERE")


def run_delete_where(conn, engine, kind, indexes, workload):
    def do():
        exec_statements(conn, [workload["delete_where_sql"]], transactional=True)

    mean = timed_runs(
        lambda: (reset_table(conn, kind, indexes), preload_table(conn, workload)),
        do,
    )
    emit(
        engine,
        "delete_where",
        workload["delete_where_rows"],
        mean,
        f"id > {workload['n_rows'] // 2}",
    )


SCENARIOS = {
    "insert": run_insert,
    "insert_multi_values": run_insert_multi_values,
    "insert_autocommit": run_insert_autocommit,
    "select": run_select,
    "select_where": run_select_where,
    "select_pk": run_select_pk,
    "select_range": run_select_range,
    "count": run_count,
    "aggregate": run_aggregate,
    "update": run_update,
    "update_range": run_update_range,
    "delete": run_delete,
    "delete_where": run_delete_where,
}

ALL_SCENARIOS = list(SCENARIOS)


# ── Runner ─────────────────────────────────────────────────────────────────────

def run_scenario(scenario, workload, indexes, selected_engines):
    for engine_key in selected_engines:
        engine, cfg = ENGINE_CONFIGS[engine_key]
        try:
            kind = cfg["kind"]
            if kind == "pg":
                conn = connect_pg(cfg)
            else:
                conn = connect_mysql(cfg)
            reset_table(conn, kind, indexes)
            if scenario in PRELOADED_SCENARIOS:
                preload_table(conn, workload)
            SCENARIOS[scenario](conn, engine, kind, indexes, workload)
            conn.close()
        except Exception as exc:
            print(
                json.dumps({"engine": engine, "scenario": scenario, "error": str(exc)}),
                flush=True,
            )


def traffic_light(axiom_ops, best_other_ops):
    """Return a traffic-light emoji comparing AxiomDB against the best competitor.

    Green:  AxiomDB >= best competitor (ratio >= 1.0)
    Yellow: AxiomDB within 25% of best (ratio >= 0.75)
    Red:    AxiomDB more than 25% behind (ratio < 0.75)
    """
    if best_other_ops <= 0:
        return "\U0001f7e2"  # green if competitor has no data
    ratio = axiom_ops / best_other_ops
    if ratio >= 1.0:
        return "\U0001f7e2"  # green
    if ratio >= 0.75:
        return "\U0001f7e1"  # yellow
    return "\U0001f534"      # red


def print_table(results, selected_engines):
    from collections import defaultdict

    by_scenario = defaultdict(dict)
    for result in results:
        scenario = result.get("scenario", "?")
        if "error" in result:
            by_scenario[scenario][result["engine"]] = "ERR"
        else:
            by_scenario[scenario][result["engine"]] = (result["mean_ms"], result["ops_per_s"])

    all_engines = [ENGINE_CONFIGS[key][0] for key in selected_engines]
    axiom_label = ENGINE_CONFIGS["axiomdb"][0]
    has_axiom = axiom_label in all_engines
    other_engines = [e for e in all_engines if e != axiom_label]

    print()
    header = f"{'Scenario':<22}" + "".join(f"  {engine:>24}" for engine in all_engines)
    if has_axiom:
        header += "   vs"
    print(header)
    print("-" * len(header))
    for scenario in PRINT_ORDER:
        if scenario not in by_scenario:
            continue
        row = f"  {scenario:<20}"
        for engine in all_engines:
            value = by_scenario[scenario].get(engine)
            if value is None:
                row += f"  {'—':>24}"
            elif isinstance(value, tuple):
                row += f"  {value[0]:>8.1f}ms  {value[1]:>10,} r/s"
            else:
                row += f"  {str(value):>24}"

        if has_axiom:
            axiom_val = by_scenario[scenario].get(axiom_label)
            if axiom_val and isinstance(axiom_val, tuple):
                axiom_ops = axiom_val[1]
                best_other = 0
                for oe in other_engines:
                    ov = by_scenario[scenario].get(oe)
                    if ov and isinstance(ov, tuple):
                        best_other = max(best_other, ov[1])
                light = traffic_light(axiom_ops, best_other)
                ratio = axiom_ops / best_other if best_other > 0 else 0
                if ratio >= 1.0:
                    row += f"  {light} {ratio:.2f}x"
                else:
                    row += f"  {light} {ratio:.2f}x"
            else:
                row += "      "

        print(row)

    if has_axiom:
        print()
        print("  \U0001f7e2 AxiomDB >= best competitor  "
              "\U0001f7e1 within 25%  "
              "\U0001f534 >25% behind")
    print()


# ── Main ───────────────────────────────────────────────────────────────────────

if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--scenario",
        choices=ALL_SCENARIOS + ["all"],
        default="all",
        metavar="{" + ",".join(ALL_SCENARIOS) + ",all}",
    )
    parser.add_argument("--rows", type=int, default=10_000)
    parser.add_argument(
        "--engines",
        default=",".join(DEFAULT_ENGINES),
        help="comma-separated engines to compare "
        f"(default: {','.join(DEFAULT_ENGINES)}; available: {','.join(ENGINE_CONFIGS)})",
    )
    parser.add_argument(
        "--indexes",
        default="",
        help="comma-separated secondary indexes shared by all engines "
        "(supported: active,age,score)",
    )
    parser.add_argument(
        "--point-lookups",
        type=int,
        default=None,
        help="number of PK point lookups for select_pk (default: min(rows,100))",
    )
    parser.add_argument(
        "--range-rows",
        type=int,
        default=None,
        help="rows touched by select_range/update_range (default: rows/10)",
    )
    parser.add_argument(
        "--multi-values-chunk",
        type=int,
        default=1000,
        help="rows per INSERT ... VALUES statement in insert_multi_values/preload",
    )
    parser.add_argument(
        "--autocommit-rows",
        type=int,
        default=None,
        help="rows used by insert_autocommit (default: min(rows,1000))",
    )
    parser.add_argument("--table", action="store_true", help="pretty-print comparison table")
    args = parser.parse_args()

    selected_engines = parse_engines(args.engines)
    selected_indexes = parse_indexes(args.indexes)
    workload = prepare_workload(
        n_rows=args.rows,
        multi_values_chunk=args.multi_values_chunk,
        autocommit_rows=args.autocommit_rows,
        point_lookups=args.point_lookups,
        range_rows=args.range_rows,
    )
    scenarios = ALL_SCENARIOS if args.scenario == "all" else [args.scenario]

    if args.table:
        import contextlib
        import io

        results = []
        buffer = io.StringIO()
        with contextlib.redirect_stdout(buffer):
            for scenario_name in scenarios:
                run_scenario(scenario_name, workload, selected_indexes, selected_engines)
        for line in buffer.getvalue().splitlines():
            try:
                results.append(json.loads(line))
            except Exception:
                pass
        sys.stdout.write(buffer.getvalue())
        print_table(results, selected_engines)
    else:
        for scenario_name in scenarios:
            run_scenario(scenario_name, workload, selected_indexes, selected_engines)
