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
  join_inner          — INNER JOIN bench_users × bench_orders + filter
  join_left           — LEFT JOIN bench_users × bench_orders
  join_aggregate      — JOIN + GROUP BY + HAVING
  subquery_in         — WHERE id IN (SELECT ... FROM bench_orders)
  subquery_exists     — WHERE EXISTS (correlated subquery)
  subquery_scalar     — scalar correlated subquery in SELECT list
  order_limit         — ORDER BY score DESC LIMIT 100
  order_offset        — ORDER BY score DESC LIMIT 100 OFFSET N/2
  distinct            — SELECT DISTINCT age
  like_pattern        — WHERE name LIKE 'user_00%'
  multi_aggregate     — GROUP BY age with COUNT, AVG, MIN, MAX
  complex_where       — compound OR/AND with arithmetic predicates
  insert_select       — INSERT INTO ... SELECT * FROM ...
  between_range       — WHERE age BETWEEN 25 AND 35
  json_extract        — JSON_EXTRACT path filter
  jsonb_extract       — JSONB -> path extract + filter
  jsonb_contains      — JSON_CONTAINS containment filter
  jsonb_path_query    — JSONPath / path-based filter
  jsonb_gin_contains  — JSONB @> containment through GIN where supported
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
import atexit
import json
import os
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
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

ROOT_DIR = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
AXIOMDB_BIN = os.path.join(ROOT_DIR, "target", "release", "axiomdb-server")
AXIOMDB_DATA_PREFIX = "axiomdb_localbench."
PORT_WAIT_TIMEOUT_S = 20.0
CONNECT_WAIT_TIMEOUT_S = 20.0
BUILD_TIMEOUT_S = 30 * 60

MANAGED_ENGINES = {"mariadb", "axiomdb"}
PROCESS_HANDLES = []

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
    # ── joins & subqueries ──
    "join_inner",
    "join_left",
    "join_aggregate",
    "subquery_in",
    "subquery_exists",
    "subquery_scalar",
    # ── sorting & filtering ──
    "order_limit",
    "order_offset",
    "distinct",
    "like_pattern",
    "multi_aggregate",
    "complex_where",
    "between_range",
    # ── bulk ──
    "insert_select",
    # ── JSON (Phase 11.4) ──
    "json_extract",
    # ── JSONB (Phase 11.16) ──
    "jsonb_extract",
    "jsonb_contains",
    "jsonb_path_query",
    "jsonb_gin_contains",
    # ── FTS (Phase 11.6/11.7) ──
    "fts_match",
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
    "order_limit",
    "order_offset",
    "distinct",
    "like_pattern",
    "multi_aggregate",
    "complex_where",
    "between_range",
}

# Scenarios that need the bench_json table created and populated.
NEEDS_JSON = {
    "json_extract",
    "jsonb_extract",
    "jsonb_contains",
    "jsonb_path_query",
    "jsonb_gin_contains",
}

# Scenarios that need the bench_fts table created and populated.
NEEDS_FTS = {
    "fts_match",
}

# Scenarios that need the bench_orders table created and populated.
NEEDS_ORDERS = {
    "join_inner",
    "join_left",
    "join_aggregate",
    "subquery_in",
    "subquery_exists",
    "subquery_scalar",
}

# Scenarios that need the bench_users_copy table.
NEEDS_USERS_COPY = {
    "insert_select",
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


def run_cmd(cmd, *, cwd=None, check=True, capture_output=True, text=True, timeout=None):
    return subprocess.run(
        cmd,
        cwd=cwd,
        check=check,
        capture_output=capture_output,
        text=text,
        timeout=timeout,
    )


def port_is_open(host, port):
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.settimeout(0.5)
        return sock.connect_ex((host, port)) == 0


def wait_for_port(host, port, timeout_s):
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        if port_is_open(host, port):
            return True
        time.sleep(0.2)
    return False


def wait_for_connection(engine_key, cfg, timeout_s):
    deadline = time.time() + timeout_s
    last_error = "connection timed out"
    while time.time() < deadline:
        try:
            conn = open_connection(engine_key, cfg)
            conn.close()
            return None
        except Exception as exc:
            last_error = str(exc)
            time.sleep(0.3)
    return last_error


def pid_for_port(port):
    try:
        result = run_cmd(
            ["bash", "-lc", f"sudo fuser -n tcp {port} 2>/dev/null || true"],
            check=False,
        )
    except Exception:
        return []
    output = result.stdout.strip()
    if not output:
        return []
    pids = []
    for token in output.split():
        if token.isdigit():
            pids.append(int(token))
    return pids


def free_port(port):
    pids = pid_for_port(port)
    if not pids:
        return {"action": "free", "details": "port already free"}
    run_cmd(["sudo", "fuser", "-k", f"{port}/tcp"], check=False)
    if wait_for_port("127.0.0.1", port, 2.0):
        return {"action": "failed", "details": f"port still busy after kill: {pids}"}
    return {"action": "killed", "details": f"terminated pids {pids}"}


def build_axiomdb():
    run_cmd(
        ["cargo", "build", "--release", "-p", "axiomdb-server"],
        cwd=ROOT_DIR,
        timeout=BUILD_TIMEOUT_S,
    )


def stop_managed_processes():
    while PROCESS_HANDLES:
        handle = PROCESS_HANDLES.pop()
        proc = handle["proc"]
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)
        if handle.get("data_dir"):
            shutil.rmtree(handle["data_dir"], ignore_errors=True)


atexit.register(stop_managed_processes)


def restart_mariadb():
    run_cmd(["sudo", "systemctl", "restart", "mariadb"])


def stop_mariadb():
    run_cmd(["sudo", "systemctl", "stop", "mariadb"])


def start_axiomdb(cfg):
    data_dir = tempfile.mkdtemp(prefix=AXIOMDB_DATA_PREFIX, dir="/tmp")
    env = os.environ.copy()
    env["AXIOMDB_PORT"] = str(cfg["port"])
    env["AXIOMDB_DATA"] = data_dir
    proc = subprocess.Popen(
        [AXIOMDB_BIN],
        cwd=ROOT_DIR,
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        text=True,
        start_new_session=True,
    )
    PROCESS_HANDLES.append({"name": "axiomdb", "proc": proc, "data_dir": data_dir})
    return proc, data_dir


def open_connection(engine_key, cfg):
    kind = cfg["kind"]
    if kind == "pg":
        return connect_pg(cfg)
    if kind in {"mysql", "axiomdb"}:
        return connect_mysql(cfg)
    raise ValueError(f"unknown engine kind: {kind}")


def preflight_status_icon(status):
    return {
        "ready": "\U0001f7e2",
        "warning": "\U0001f7e1",
        "error": "\U0001f534",
        "skipped": "\u26aa",
    }.get(status, "\u26aa")


def semaforo_text(light):
    return {
        "\U0001f7e2": "VERDE",
        "\U0001f7e1": "AMARILLO",
        "\U0001f534": "ROJO",
    }.get(light, "N/A")


def print_preflight_table(statuses, selected_engines):
    print()
    print("Preflight")
    print("---------")
    header = (
        f"{'Engine':<14}  {'Port':<7}  {'Build':<10}  {'Port Check':<12}  "
        f"{'Health':<8}  {'Action':<10}  Details"
    )
    print(header)
    print("-" * len(header))
    for engine_key in selected_engines:
        engine, cfg = ENGINE_CONFIGS[engine_key]
        status = statuses[engine_key]
        build = status.get("build", "\u2014")
        port = status.get("port_check", "\u2014")
        health = status.get("health", "\u2014")
        action = status.get("action", "\u2014")
        details = status.get("details", "")
        print(
            f"{engine:<14}  {cfg['port']:<7}  {build:<10}  {port:<12}  "
            f"{health:<8}  {action:<10}  {details}"
        )
    print()


def manage_engine(engine_key, statuses):
    engine, cfg = ENGINE_CONFIGS[engine_key]
    status = {
        "engine": engine,
        "port": cfg["port"],
        "build": "\u2014",
        "port_check": "\u26aa pending",
        "health": "\u26aa pending",
        "action": "\u2014",
        "details": "",
    }
    statuses[engine_key] = status

    if engine_key not in MANAGED_ENGINES:
        status["build"] = "\u26aa skipped"
        if not wait_for_port(cfg["host"], cfg["port"], 1.0):
            status["port_check"] = "\U0001f534 closed"
            status["health"] = "\U0001f534 failed"
            status["action"] = "manual"
            status["details"] = "engine not auto-managed and port is closed"
            return False
        health_error = wait_for_connection(engine_key, cfg, 3.0)
        if health_error is None:
            status["port_check"] = "\U0001f7e2 open"
            status["health"] = "\U0001f7e2 ready"
            status["action"] = "reuse"
            status["details"] = "existing engine accepted connections"
            return True
        status["port_check"] = "\U0001f7e1 open"
        status["health"] = "\U0001f534 failed"
        status["action"] = "manual"
        status["details"] = health_error
        return False

    try:
        if engine_key == "axiomdb":
            status["build"] = "\U0001f7e1 building"
            build_axiomdb()
            status["build"] = "\U0001f7e2 ready"
        else:
            status["build"] = "\u26aa skipped"

        if engine_key == "mariadb":
            stop_mariadb()
            if wait_for_port(cfg["host"], cfg["port"], 3.0):
                port_result = free_port(cfg["port"])
            else:
                port_result = {"action": "stopped", "details": "service stopped cleanly"}
            status["action"] = "restart"
            status["details"] = port_result["details"]
            if port_result["action"] == "failed":
                status["port_check"] = "\U0001f534 busy"
                status["health"] = "\U0001f534 failed"
                return False
            restart_mariadb()
        elif engine_key == "axiomdb":
            port_result = free_port(cfg["port"])
            status["action"] = port_result["action"]
            status["details"] = port_result["details"]
            if port_result["action"] == "failed":
                status["port_check"] = "\U0001f534 busy"
                status["health"] = "\U0001f534 failed"
                return False
            start_axiomdb(cfg)

        if not wait_for_port(cfg["host"], cfg["port"], PORT_WAIT_TIMEOUT_S):
            status["port_check"] = "\U0001f534 closed"
            status["health"] = "\U0001f534 failed"
            status["details"] = "port did not open before timeout"
            return False
        status["port_check"] = "\U0001f7e2 open"

        health_error = wait_for_connection(engine_key, cfg, CONNECT_WAIT_TIMEOUT_S)
        if health_error is not None:
            status["health"] = "\U0001f534 failed"
            status["details"] = health_error
            return False

        status["health"] = "\U0001f7e2 ready"
        if not status["details"]:
            status["details"] = "engine ready"
        return True
    except subprocess.CalledProcessError as exc:
        output = (exc.stderr or exc.stdout or str(exc)).strip()
        status["build"] = "\U0001f534 failed" if engine_key == "axiomdb" else status["build"]
        status["port_check"] = "\U0001f534 failed"
        status["health"] = "\U0001f534 failed"
        status["action"] = "error"
        status["details"] = output.splitlines()[-1] if output else str(exc)
        return False
    except Exception as exc:
        status["port_check"] = "\U0001f534 failed"
        status["health"] = "\U0001f534 failed"
        status["action"] = "error"
        status["details"] = str(exc)
        return False


def prepare_selected_engines(selected_engines):
    statuses = {}
    ok = True
    for engine_key in selected_engines:
        if not manage_engine(engine_key, statuses):
            ok = False
    return ok, statuses


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


def rows_data_orders(n_users):
    """Generate ~3 orders per user.  Returns list of (id, user_id, amount, status)."""
    statuses = ["pending", "shipped", "delivered"]
    rows = []
    oid = 1
    for uid in range(1, n_users + 1):
        for j in range(3):
            amount = round(10.0 + ((uid * 3 + j) % 200) * 0.5, 2)
            status = statuses[j % 3]
            rows.append((oid, uid, amount, status))
            oid += 1
    return rows


def rows_data_json(n):
    rows = []
    tenants = ["acme", "globex", "initech"]
    plans = ["free", "pro", "enterprise"]
    countries = ["US", "CO", "ES"]
    for i in range(1, n + 1):
        payload = {
            "id": i,
            "name": f"user_{i:06d}",
            "age": 18 + (i % 62),
            "active": 1 if i % 2 == 0 else 0,
            "tenant": tenants[(i - 1) % len(tenants)],
            "role": "admin" if i % 10 == 0 else "user",
            "profile": {
                "plan": plans[i % len(plans)],
                "country": countries[i % len(countries)],
            },
            "tags": ["mobile", "beta"] if i % 5 == 0 else ["web", "paid"],
        }
        rows.append((i, json.dumps(payload, separators=(",", ":"))))
    return rows


# Predefined word pools for FTS data generation.
_FTS_WORDS = [
    "database", "engine", "storage", "index", "query", "optimizer", "transaction",
    "concurrency", "isolation", "durability", "checkpoint", "recovery", "buffer",
    "page", "tuple", "column", "table", "schema", "catalog", "constraint",
    "primary", "foreign", "unique", "cluster", "partition", "shard", "replica",
    "snapshot", "vacuum", "compaction", "bloom", "filter", "hash", "btree",
    "sequential", "parallel", "vectorized", "aggregate", "window", "subquery",
    "correlated", "materialized", "view", "trigger", "procedure", "function",
    "expression", "predicate", "selectivity", "cardinality", "histogram",
    "statistics", "planner", "executor", "scanner", "parser", "lexer",
    "analyzer", "resolver", "rewriter", "optimizer", "pipeline", "morsel",
    "cache", "eviction", "prefetch", "write", "ahead", "log", "journal",
]


def rows_data_fts(n):
    """Generate (id, body TEXT) rows with semi-random searchable content."""
    rows = []
    nw = len(_FTS_WORDS)
    for i in range(1, n + 1):
        # Each doc gets 8-15 words picked pseudo-deterministically from the pool.
        count = 8 + (i % 8)
        words = [_FTS_WORDS[(i * 7 + j * 13) % nw] for j in range(count)]
        body = " ".join(words)
        rows.append((i, body))
    return rows


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


def schema_statements(kind, indexes, with_orders=False, with_users_copy=False, with_json=False, with_fts=False):
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

    if with_orders:
        statements += orders_schema_statements(kind)

    if with_users_copy:
        statements += users_copy_schema_statements(kind)

    if with_json:
        statements += json_schema_statements(kind)

    if with_fts:
        statements += fts_schema_statements(kind)

    return statements


def orders_schema_statements(kind):
    if kind == "pg":
        return [
            "DROP TABLE IF EXISTS bench_orders CASCADE",
            """CREATE TABLE bench_orders (
    id       INT              NOT NULL PRIMARY KEY,
    user_id  INT              NOT NULL,
    amount   DOUBLE PRECISION NOT NULL,
    status   TEXT             NOT NULL
)""",
            "CREATE INDEX idx_orders_user_id ON bench_orders (user_id)",
        ]
    elif kind == "mysql":
        return [
            "DROP TABLE IF EXISTS bench_orders",
            """CREATE TABLE bench_orders (
    id       INT          NOT NULL,
    user_id  INT          NOT NULL,
    amount   DOUBLE       NOT NULL,
    status   VARCHAR(50)  NOT NULL,
    PRIMARY KEY (id)
) ENGINE=InnoDB""",
            "CREATE INDEX idx_orders_user_id ON bench_orders (user_id)",
        ]
    elif kind == "axiomdb":
        return [
            "DROP TABLE IF EXISTS bench_orders",
            """CREATE TABLE bench_orders (
    id       INT  NOT NULL,
    user_id  INT  NOT NULL,
    amount   REAL NOT NULL,
    status   TEXT NOT NULL,
    PRIMARY KEY (id)
)""",
            "CREATE INDEX idx_orders_user_id ON bench_orders (user_id)",
        ]
    else:
        raise ValueError(f"unknown engine kind: {kind}")


def users_copy_schema_statements(kind):
    if kind == "pg":
        return [
            "DROP TABLE IF EXISTS bench_users_copy CASCADE",
            """CREATE TABLE bench_users_copy (
    id     INT              NOT NULL PRIMARY KEY,
    name   TEXT             NOT NULL,
    age    INT              NOT NULL,
    active BOOLEAN          NOT NULL,
    score  DOUBLE PRECISION NOT NULL,
    email  TEXT             NOT NULL
)""",
        ]
    elif kind == "mysql":
        return [
            "DROP TABLE IF EXISTS bench_users_copy",
            """CREATE TABLE bench_users_copy (
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
        return [
            "DROP TABLE IF EXISTS bench_users_copy",
            """CREATE TABLE bench_users_copy (
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


def json_schema_statements(kind):
    if kind == "pg":
        return [
            "DROP TABLE IF EXISTS bench_jsonb CASCADE",
            "DROP TABLE IF EXISTS bench_json CASCADE",
            """CREATE TABLE bench_json (
    id   INT   NOT NULL PRIMARY KEY,
    data JSONB NOT NULL
)""",
            """CREATE TABLE bench_jsonb (
    id   INT   NOT NULL PRIMARY KEY,
    data JSONB NOT NULL
)""",
        ]
    elif kind == "mysql":
        return [
            "DROP TABLE IF EXISTS bench_jsonb",
            "DROP TABLE IF EXISTS bench_json",
            """CREATE TABLE bench_json (
    id   INT  NOT NULL,
    data JSON NOT NULL,
    PRIMARY KEY (id)
) ENGINE=InnoDB""",
            """CREATE TABLE bench_jsonb (
    id   INT  NOT NULL,
    data JSON NOT NULL,
    PRIMARY KEY (id)
) ENGINE=InnoDB""",
        ]
    elif kind == "axiomdb":
        return [
            "DROP TABLE IF EXISTS bench_jsonb",
            "DROP TABLE IF EXISTS bench_json",
            """CREATE TABLE bench_json (
    id   INT  NOT NULL,
    data JSON NOT NULL,
    PRIMARY KEY (id)
)""",
            """CREATE TABLE bench_jsonb (
    id   INT   NOT NULL,
    data JSONB NOT NULL,
    PRIMARY KEY (id)
)""",
        ]
    else:
        raise ValueError(f"unknown engine kind: {kind}")


def fts_schema_statements(kind):
    if kind == "pg":
        return [
            "DROP TABLE IF EXISTS bench_fts CASCADE",
            """CREATE TABLE bench_fts (
    id   INT  NOT NULL PRIMARY KEY,
    body TEXT NOT NULL
)""",
        ]
    elif kind in ("mysql", "axiomdb"):
        return [
            "DROP TABLE IF EXISTS bench_fts",
            """CREATE TABLE bench_fts (
    id   INT  NOT NULL,
    body TEXT NOT NULL,
    PRIMARY KEY (id)
)""",
        ]
    else:
        raise ValueError(f"unknown engine kind: {kind}")


def exec_statements(conn, statements, transactional=False):
    cur = conn.cursor()
    if transactional:
        cur.execute("BEGIN")
    for stmt in statements:
        cur.execute(stmt)
    if transactional:
        cur.execute("COMMIT")
    cur.close()


def reset_table(conn, kind, indexes, with_orders=False, with_users_copy=False, with_json=False, with_fts=False):
    exec_statements(conn, schema_statements(kind, indexes,
                                            with_orders=with_orders,
                                            with_users_copy=with_users_copy,
                                            with_json=with_json,
                                            with_fts=with_fts))


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

    # ── orders data ──────────────────────────────────────────────────────────
    orders = rows_data_orders(n_rows)
    order_values_sql = [
        "(" + ",".join(sql_literal(v) for v in row) + ")" for row in orders
    ]
    insert_orders_sqls = [
        "INSERT INTO bench_orders VALUES " + ",".join(chunk)
        for chunk in chunked(order_values_sql, multi_values_chunk)
    ]

    # ── JSON data ────────────────────────────────────────────────────────────
    json_rows = rows_data_json(n_rows)
    json_values_sql = [
        "(" + ",".join(sql_literal(v) for v in row) + ")" for row in json_rows
    ]
    insert_json_sqls = [
        "INSERT INTO bench_json VALUES " + ",".join(chunk)
        for chunk in chunked(json_values_sql, multi_values_chunk)
    ]
    insert_jsonb_sqls = [
        "INSERT INTO bench_jsonb VALUES " + ",".join(chunk)
        for chunk in chunked(json_values_sql, multi_values_chunk)
    ]

    # ── FTS data ─────────────────────────────────────────────────────────────
    fts_rows = rows_data_fts(n_rows)
    fts_values_sql = [
        "(" + ",".join(sql_literal(v) for v in row) + ")" for row in fts_rows
    ]
    insert_fts_sqls = [
        "INSERT INTO bench_fts VALUES " + ",".join(chunk)
        for chunk in chunked(fts_values_sql, multi_values_chunk)
    ]

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
        # ── orders ────────────────────────────────────────────────────────────
        "n_orders": len(orders),
        "insert_orders_sqls": insert_orders_sqls,
        "n_json": len(json_rows),
        "insert_json_sqls": insert_json_sqls,
        "n_jsonb": len(json_rows),
        "insert_jsonb_sqls": insert_jsonb_sqls,
        # ── join queries ──────────────────────────────────────────────────────
        "join_inner_sql": (
            "SELECT u.name, o.amount "
            "FROM bench_users u INNER JOIN bench_orders o ON u.id = o.user_id "
            "WHERE o.amount > 50.00"
        ),
        "join_left_sql": (
            "SELECT u.name, o.amount "
            "FROM bench_users u LEFT JOIN bench_orders o ON u.id = o.user_id"
        ),
        "join_aggregate_sql": (
            "SELECT u.name, COUNT(*) AS c, SUM(o.amount) AS total "
            "FROM bench_users u INNER JOIN bench_orders o ON u.id = o.user_id "
            "GROUP BY u.id, u.name HAVING COUNT(*) > 1"
        ),
        # ── subquery queries ──────────────────────────────────────────────────
        "subquery_in_sql": (
            "SELECT * FROM bench_users "
            "WHERE id IN (SELECT user_id FROM bench_orders WHERE amount > 80.00)"
        ),
        "subquery_exists_sql": (
            "SELECT * FROM bench_users u "
            "WHERE EXISTS (SELECT 1 FROM bench_orders o "
            "WHERE o.user_id = u.id AND o.amount > 80.00)"
        ),
        "subquery_scalar_n": min(n_rows, 1000),
        "subquery_scalar_sql": (
            "SELECT u.name, "
            "(SELECT SUM(o.amount) FROM bench_orders o WHERE o.user_id = u.id) AS total "
            f"FROM bench_users u LIMIT {min(n_rows, 1000)}"
        ),
        # ── sorting / filtering ───────────────────────────────────────────────
        "order_limit_sql": "SELECT * FROM bench_users ORDER BY score DESC LIMIT 100",
        "order_offset_sql": (
            f"SELECT * FROM bench_users ORDER BY score DESC LIMIT 100 OFFSET {half}"
        ),
        "distinct_sql": "SELECT DISTINCT age FROM bench_users",
        "like_pattern_sql": "SELECT * FROM bench_users WHERE name LIKE 'user_00%'",
        "multi_aggregate_sql": (
            "SELECT age, COUNT(*) AS c, AVG(score) AS a, MIN(score) AS mn, MAX(score) AS mx "
            "FROM bench_users GROUP BY age"
        ),
        "complex_where_sql": (
            "SELECT * FROM bench_users "
            "WHERE (age > 30 AND score > 150.00) OR (active = TRUE AND age < 25)"
        ),
        "between_range_sql": "SELECT * FROM bench_users WHERE age BETWEEN 25 AND 35",
        # ── bulk ──────────────────────────────────────────────────────────────
        "insert_select_sql": "INSERT INTO bench_users_copy SELECT * FROM bench_users",
        # ── JSON (Phase 11.4) ────────────────────────────────────────────────
        "json_extract_n": n_rows,
        "json_extract_sql": "SELECT JSON_EXTRACT(data, '$.age') FROM bench_json WHERE JSON_EXTRACT(data, '$.active') = 1",
        # ── JSONB (Phase 11.16) ──────────────────────────────────────────────
        "jsonb_extract_n": n_rows,
        "jsonb_contains_n": n_rows,
        "jsonb_path_query_n": n_rows,
        "jsonb_gin_contains_n": n_rows,
        # ── FTS (Phase 11.6/11.7) ────────────────────────────────────────────
        "n_fts": len(fts_rows),
        "insert_fts_sqls": insert_fts_sqls,
        "fts_match_n": n_rows,
    }


def preload_table(conn, workload):
    exec_statements(conn, workload["insert_multi_values_sqls"], transactional=True)


def preload_orders(conn, workload):
    exec_statements(conn, workload["insert_orders_sqls"], transactional=True)


def preload_json(conn, workload):
    exec_statements(conn, workload["insert_json_sqls"], transactional=True)
    exec_statements(conn, workload["insert_jsonb_sqls"], transactional=True)


def preload_fts(conn, workload):
    exec_statements(conn, workload["insert_fts_sqls"], transactional=True)


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


# ── Join scenarios ─────────────────────────────────────────────────────────────

def run_join_inner(conn, engine, _kind, _indexes, workload):
    def do():
        cur = conn.cursor()
        cur.execute(workload["join_inner_sql"])
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "join_inner", workload["n_rows"], mean, "INNER JOIN + filter amount>50")


def run_join_left(conn, engine, _kind, _indexes, workload):
    def do():
        cur = conn.cursor()
        cur.execute(workload["join_left_sql"])
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "join_left", workload["n_rows"], mean, "LEFT JOIN all rows")


def run_join_aggregate(conn, engine, _kind, _indexes, workload):
    def do():
        cur = conn.cursor()
        cur.execute(workload["join_aggregate_sql"])
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "join_aggregate", workload["n_rows"], mean, "JOIN + GROUP BY + HAVING")


# ── Subquery scenarios ────────────────────────────────────────────────────────

def run_subquery_in(conn, engine, _kind, _indexes, workload):
    def do():
        cur = conn.cursor()
        cur.execute(workload["subquery_in_sql"])
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "subquery_in", workload["n_rows"], mean, "IN (SELECT ...)")


def run_subquery_exists(conn, engine, _kind, _indexes, workload):
    def do():
        cur = conn.cursor()
        cur.execute(workload["subquery_exists_sql"])
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "subquery_exists", workload["n_rows"], mean, "EXISTS correlated")


def run_subquery_scalar(conn, engine, _kind, _indexes, workload):
    n = workload["subquery_scalar_n"]

    def do():
        cur = conn.cursor()
        cur.execute(workload["subquery_scalar_sql"])
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "subquery_scalar", n, mean, f"scalar SUM in SELECT (LIMIT {n})")


# ── Sorting & filtering scenarios ─────────────────────────────────────────────

def run_order_limit(conn, engine, _kind, _indexes, workload):
    def do():
        cur = conn.cursor()
        cur.execute(workload["order_limit_sql"])
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "order_limit", 100, mean, "ORDER BY score DESC LIMIT 100")


def run_order_offset(conn, engine, _kind, _indexes, workload):
    def do():
        cur = conn.cursor()
        cur.execute(workload["order_offset_sql"])
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "order_offset", 100, mean, f"LIMIT 100 OFFSET {workload['n_rows']//2}")


def run_distinct(conn, engine, _kind, _indexes, workload):
    def do():
        cur = conn.cursor()
        cur.execute(workload["distinct_sql"])
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "distinct", 62, mean, "DISTINCT age (62 unique values)")


def run_like_pattern(conn, engine, _kind, _indexes, workload):
    def do():
        cur = conn.cursor()
        cur.execute(workload["like_pattern_sql"])
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "like_pattern", workload["n_rows"], mean, "LIKE 'user_00%'")


def run_multi_aggregate(conn, engine, _kind, _indexes, workload):
    def do():
        cur = conn.cursor()
        cur.execute(workload["multi_aggregate_sql"])
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "multi_aggregate", 1, mean, "COUNT+AVG+MIN+MAX by age")


def run_complex_where(conn, engine, _kind, _indexes, workload):
    def do():
        cur = conn.cursor()
        cur.execute(workload["complex_where_sql"])
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "complex_where", workload["n_rows"], mean, "compound OR/AND predicates")


def run_between_range(conn, engine, _kind, _indexes, workload):
    def do():
        cur = conn.cursor()
        cur.execute(workload["between_range_sql"])
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "between_range", workload["n_rows"], mean, "BETWEEN 25 AND 35 (~17%)")


# ── Bulk scenarios ────────────────────────────────────────────────────────────

def run_insert_select(conn, engine, kind, indexes, workload):
    def setup():
        # Recreate the copy table (empty), keep users loaded
        cur = conn.cursor()
        cur.execute("DROP TABLE IF EXISTS bench_users_copy")
        for stmt in users_copy_schema_statements(kind):
            if "DROP" not in stmt:
                cur.execute(stmt)
        cur.close()

    def do():
        exec_statements(conn, [workload["insert_select_sql"]], transactional=True)

    mean = timed_runs(setup, do)
    emit(engine, "insert_select", workload["n_rows"], mean, "INSERT INTO ... SELECT *")


# ── JSON scenarios ────────────────────────────────────────────────────────────

def run_json_extract(conn, engine, kind, _indexes, workload):
    if kind == "pg":
        sql = "SELECT data->>'age' FROM bench_json WHERE (data->>'active') = '1'"
    else:
        sql = workload["json_extract_sql"]

    def do():
        cur = conn.cursor()
        cur.execute(sql)
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "json_extract", workload["json_extract_n"], mean, "JSON_EXTRACT path filter")


# ── JSONB scenarios (Phase 11.16) ─────────────────────────────────────────────

def run_jsonb_extract(conn, engine, kind, _indexes, workload):
    if kind == "pg":
        sql = "SELECT data->>'age' FROM bench_jsonb WHERE (data->>'active') = '1'"
    elif kind == "axiomdb":
        sql = "SELECT data->>'age' FROM bench_jsonb WHERE data->>'active' = '1'"
    else:
        # MySQL does not have JSONB; use JSON_EXTRACT
        sql = "SELECT JSON_EXTRACT(data, '$.age') FROM bench_jsonb WHERE JSON_EXTRACT(data, '$.active') = 1"

    def do():
        cur = conn.cursor()
        cur.execute(sql)
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "jsonb_extract", workload["jsonb_extract_n"], mean, "JSONB -> path extract + filter")


def run_jsonb_contains(conn, engine, kind, _indexes, workload):
    if kind == "pg":
        sql = "SELECT COUNT(*) FROM bench_jsonb WHERE data @> '{\"active\":1}'"
    elif kind == "axiomdb":
        sql = "SELECT COUNT(*) FROM bench_jsonb WHERE JSON_CONTAINS(data, '1', '$.active') = 1"
    else:
        sql = "SELECT COUNT(*) FROM bench_jsonb WHERE JSON_CONTAINS(data, '1', '$.active')"

    def do():
        cur = conn.cursor()
        cur.execute(sql)
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "jsonb_contains", workload["jsonb_contains_n"], mean, "JSON_CONTAINS filter")


def run_jsonb_gin_contains(conn, engine, kind, _indexes, workload):
    candidate = '{"tenant":"acme","profile":{"plan":"pro"},"tags":["web"]}'
    if kind == "pg":
        exec_statements(conn, ["CREATE INDEX idx_bench_jsonb_gin ON bench_jsonb USING gin (data)"])
        sql = f"SELECT COUNT(*) FROM bench_jsonb WHERE data @> '{candidate}'"
    elif kind == "axiomdb":
        exec_statements(conn, ["CREATE INDEX idx_bench_jsonb_gin ON bench_jsonb USING GIN (data)"])
        sql = f"SELECT COUNT(*) FROM bench_jsonb WHERE data @> '{candidate}'"
    else:
        sql = f"SELECT COUNT(*) FROM bench_jsonb WHERE JSON_CONTAINS(data, '{candidate}')"

    cur = conn.cursor()
    cur.execute(sql)
    matched = cur.fetchone()[0]
    cur.close()

    def do():
        cur = conn.cursor()
        cur.execute(sql)
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(
        engine,
        "jsonb_gin_contains",
        workload["jsonb_gin_contains_n"],
        mean,
        f"JSONB @> nested containment ({matched} matches)",
    )


def run_jsonb_path_query(conn, engine, kind, _indexes, workload):
    if kind == "pg":
        sql = "SELECT COUNT(*) FROM bench_jsonb WHERE jsonb_path_exists(data, '$.age ? (@ > 40)')"
    elif kind == "axiomdb":
        sql = "SELECT COUNT(*) FROM bench_jsonb WHERE JSON_PATH_EXISTS(data, '$.age') = TRUE AND JSON_EXTRACT(data, '$.age') > 40"
    else:
        sql = "SELECT COUNT(*) FROM bench_jsonb WHERE JSON_EXTRACT(data, '$.age') > 40"

    def do():
        cur = conn.cursor()
        cur.execute(sql)
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "jsonb_path_query", workload["jsonb_path_query_n"], mean, "JSONPath / path-based filter")


# ── FTS scenarios (Phase 11.6/11.7) ──────────────────────────────────────────

def run_fts_match(conn, engine, kind, _indexes, workload):
    if kind == "pg":
        sql = "SELECT id, body FROM bench_fts WHERE to_tsvector('english', body) @@ to_tsquery('english', 'database & engine')"
    elif kind == "axiomdb":
        sql = "SELECT id, body FROM bench_fts WHERE MATCH(body, '+database +engine') > 0"
    else:
        # MySQL FULLTEXT would require an index; use LIKE as fallback
        sql = "SELECT id, body FROM bench_fts WHERE body LIKE '%database%' AND body LIKE '%engine%'"

    def do():
        cur = conn.cursor()
        cur.execute(sql)
        cur.fetchall()
        cur.close()

    mean = timed_runs(lambda: None, do)
    emit(engine, "fts_match", workload["fts_match_n"], mean, "FTS MATCH boolean query")


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
    "join_inner": run_join_inner,
    "join_left": run_join_left,
    "join_aggregate": run_join_aggregate,
    "subquery_in": run_subquery_in,
    "subquery_exists": run_subquery_exists,
    "subquery_scalar": run_subquery_scalar,
    "order_limit": run_order_limit,
    "order_offset": run_order_offset,
    "distinct": run_distinct,
    "like_pattern": run_like_pattern,
    "multi_aggregate": run_multi_aggregate,
    "complex_where": run_complex_where,
    "between_range": run_between_range,
    "insert_select": run_insert_select,
    "json_extract": run_json_extract,
    "jsonb_extract": run_jsonb_extract,
    "jsonb_contains": run_jsonb_contains,
    "jsonb_path_query": run_jsonb_path_query,
    "jsonb_gin_contains": run_jsonb_gin_contains,
    "fts_match": run_fts_match,
}

ALL_SCENARIOS = list(SCENARIOS)


# ── Runner ─────────────────────────────────────────────────────────────────────

def run_scenario(scenario, workload, indexes, selected_engines):
    needs_orders = scenario in NEEDS_ORDERS
    needs_copy = scenario in NEEDS_USERS_COPY
    needs_json = scenario in NEEDS_JSON
    needs_fts = scenario in NEEDS_FTS
    for engine_key in selected_engines:
        engine, cfg = ENGINE_CONFIGS[engine_key]
        try:
            kind = cfg["kind"]
            if kind == "pg":
                conn = connect_pg(cfg)
            else:
                conn = connect_mysql(cfg)
            reset_table(conn, kind, indexes,
                        with_orders=needs_orders, with_users_copy=needs_copy,
                        with_json=needs_json, with_fts=needs_fts)
            if scenario in PRELOADED_SCENARIOS or needs_orders or needs_copy:
                preload_table(conn, workload)
            if needs_orders:
                preload_orders(conn, workload)
            if needs_json:
                preload_json(conn, workload)
            if needs_fts:
                preload_fts(conn, workload)
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
        header += f"  {'Semaforo':<10}  {'Ratio':>7}"
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
                row += f"  {light} {semaforo_text(light):<8}  {ratio:>6.2f}x"
            else:
                row += f"  {'\u26aa':<10}  {'n/a':>7}"

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
    parser.add_argument(
        "--no-manage",
        action="store_true",
        help="do not auto-build/restart/launch engines; only validate existing services",
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

    if args.no_manage:
        statuses = {}
        ok = True
        for engine_key in selected_engines:
            engine, cfg = ENGINE_CONFIGS[engine_key]
            status = {
                "engine": engine,
                "port": cfg["port"],
                "build": "\u26aa skipped",
                "port_check": "\u26aa pending",
                "health": "\u26aa pending",
                "action": "reuse",
                "details": "",
            }
            statuses[engine_key] = status
            if wait_for_port(cfg["host"], cfg["port"], 1.0):
                status["port_check"] = "\U0001f7e2 open"
            else:
                status["port_check"] = "\U0001f534 closed"
                status["health"] = "\U0001f534 failed"
                status["details"] = "port is closed"
                ok = False
                continue
            health_error = wait_for_connection(engine_key, cfg, 3.0)
            if health_error is None:
                status["health"] = "\U0001f7e2 ready"
                status["details"] = "existing engine accepted connections"
            else:
                status["health"] = "\U0001f534 failed"
                status["details"] = health_error
                ok = False
    else:
        ok, statuses = prepare_selected_engines(selected_engines)

    print_preflight_table(statuses, selected_engines)
    if not ok:
        raise SystemExit("preflight failed; fix the engines above or use --no-manage once they are ready")

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
