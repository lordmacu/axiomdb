#!/usr/bin/env python3
"""
SQLite vs AxiomDB embedded benchmark
====================================

Apples-to-apples comparison between SQLite (stdlib) and AxiomDB embedded.

Fairness rules:
- Both engines run in-process (no network, no IPC).
- Both use full crash-safe durability:
    * SQLite:  journal_mode=WAL + synchronous=FULL  (fsync on every COMMIT).
    * AxiomDB: default (fsync=true, WAL-native).
- Identical schema, identical SQL text where possible, identical iteration counts.
- Both use a temp file (NOT :memory:) so disk I/O is exercised on both.
- Timed sections measure the same operation: parsing + planning + execution +
  result materialization on the client side.

Usage:
    # Build the embedded library first:
    cargo build --release -p axiomdb-embedded

    # Run with default 10K rows:
    python3 benches/sqlite_vs_axiomdb/bench.py

    # Other sizes:
    python3 benches/sqlite_vs_axiomdb/bench.py --rows 1000
    python3 benches/sqlite_vs_axiomdb/bench.py --rows 100000

    # Only run a single scenario:
    python3 benches/sqlite_vs_axiomdb/bench.py --scenario select_pk
"""

import argparse
import os
import sqlite3
import statistics
import sys
import tempfile
import time
from pathlib import Path

# Locate the existing AxiomDB Python binding
REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT / "bindings" / "python"))

try:
    from axiomdb import AxiomDB
except ImportError as exc:
    print(f"Cannot import AxiomDB binding: {exc}", file=sys.stderr)
    print(
        f"Hint: build the library first with "
        f"`cargo build --release -p axiomdb-embedded` "
        f"(expected at {REPO_ROOT / 'target' / 'release'}).",
        file=sys.stderr,
    )
    sys.exit(1)


# ── Configuration ─────────────────────────────────────────────────────────────

ITERATIONS = 5             # how many times each scenario is repeated
WARMUP_ITERATIONS = 1      # discarded before timing
DEFAULT_ROWS = 10_000
PK_LOOKUPS = 200           # for select_pk
RANGE_ROWS_PCT = 0.10      # for select_range


SCENARIOS = [
    "insert",              # N single-row INSERTs in ONE explicit transaction
    "insert_autocommit",   # 1 transaction per row (durability worst case)
    "select",              # full table scan
    "select_where",        # filtered scan (active=1, ~50% selectivity)
    "select_pk",           # PK point lookups
    "select_range",        # PK range scan
    "count",               # COUNT(*)
    "aggregate",           # GROUP BY age, AVG(score)
    "update_where",        # UPDATE ... WHERE active=1
    "delete_where",        # DELETE ... WHERE id > N/2
]


SCHEMA_SQL = (
    "CREATE TABLE bench_users ("
    "id INT PRIMARY KEY, "
    "name TEXT, "
    "age INT, "
    "active INT, "
    "score INT, "
    "email TEXT)"
)


def build_row_values(i: int) -> tuple:
    return (
        i,
        f"user_{i:06d}",
        18 + i % 62,
        1 if i % 2 == 0 else 0,
        100 + i % 1000,
        f"u{i}@b.local",
    )


def build_insert_sql(values: tuple) -> str:
    # Built without parameter binding so both engines parse the same text.
    return (
        f"INSERT INTO bench_users VALUES "
        f"({values[0]}, '{values[1]}', {values[2]}, {values[3]}, {values[4]}, '{values[5]}')"
    )


# ── SQLite harness ────────────────────────────────────────────────────────────


class SqliteHarness:
    name = "SQLite"

    def __init__(self, db_path: str):
        self.db_path = db_path
        self.conn: sqlite3.Connection | None = None

    def open(self) -> None:
        # Disable Python-side implicit transactions so BEGIN/COMMIT are
        # under our control and behave like AxiomDB's autocommit model.
        self.conn = sqlite3.connect(self.db_path, isolation_level=None)
        # Apples-to-apples durability config:
        self.conn.execute("PRAGMA journal_mode=WAL")
        self.conn.execute("PRAGMA synchronous=FULL")
        self.conn.execute("PRAGMA temp_store=MEMORY")

    def close(self) -> None:
        if self.conn is not None:
            self.conn.close()
            self.conn = None

    def execute(self, sql: str) -> None:
        assert self.conn is not None
        self.conn.execute(sql)

    def query(self, sql: str) -> list:
        assert self.conn is not None
        cur = self.conn.execute(sql)
        return cur.fetchall()

    def begin(self) -> None:
        assert self.conn is not None
        self.conn.execute("BEGIN")

    def commit(self) -> None:
        assert self.conn is not None
        self.conn.execute("COMMIT")


# ── AxiomDB harness ───────────────────────────────────────────────────────────


class AxiomDBHarness:
    name = "AxiomDB"

    def __init__(self, db_path: str):
        self.db_path = db_path
        self.db: AxiomDB | None = None

    def open(self) -> None:
        self.db = AxiomDB(self.db_path)

    def close(self) -> None:
        if self.db is not None:
            self.db.close()
            self.db = None

    def execute(self, sql: str) -> None:
        assert self.db is not None
        self.db.execute(sql)

    def query(self, sql: str) -> list:
        assert self.db is not None
        return self.db.query(sql)

    def begin(self) -> None:
        assert self.db is not None
        self.db.execute("BEGIN")

    def commit(self) -> None:
        assert self.db is not None
        self.db.execute("COMMIT")


# ── Scenario runners ──────────────────────────────────────────────────────────


def time_section(fn) -> float:
    start = time.perf_counter()
    fn()
    return time.perf_counter() - start


def run_insert(h, rows: int) -> float:
    """N single-row INSERTs in one explicit transaction."""
    h.begin()
    for i in range(1, rows + 1):
        h.execute(build_insert_sql(build_row_values(i)))
    h.commit()
    return 0  # caller times the wrapper


def run_insert_autocommit(h, n: int) -> None:
    """1 transaction per row (durability worst case). Uses a smaller N because slow."""
    for i in range(1, n + 1):
        h.execute(build_insert_sql(build_row_values(i)))


def run_select(h) -> int:
    rows = h.query("SELECT * FROM bench_users")
    return len(rows)


def run_select_where(h) -> int:
    rows = h.query("SELECT * FROM bench_users WHERE active = 1")
    return len(rows)


def run_select_pk(h, total_rows: int) -> int:
    step = max(1, total_rows // PK_LOOKUPS)
    n = 0
    for i in range(1, total_rows + 1, step):
        h.query(f"SELECT * FROM bench_users WHERE id = {i}")
        n += 1
    return n


def run_select_range(h, total_rows: int) -> int:
    lo = total_rows // 4
    hi = lo + max(1, int(total_rows * RANGE_ROWS_PCT))
    rows = h.query(f"SELECT * FROM bench_users WHERE id >= {lo} AND id < {hi}")
    return len(rows)


def run_count(h) -> int:
    rows = h.query("SELECT COUNT(*) FROM bench_users")
    return len(rows)


def run_aggregate(h) -> int:
    rows = h.query("SELECT age, AVG(score) FROM bench_users GROUP BY age")
    return len(rows)


def run_update_where(h) -> int:
    h.execute("UPDATE bench_users SET score = score + 1 WHERE active = 1")
    return 1


def run_delete_where(h, total_rows: int) -> int:
    h.execute(f"DELETE FROM bench_users WHERE id > {total_rows // 2}")
    return 1


# ── Bench loop ────────────────────────────────────────────────────────────────


def bench_one_scenario(
    HarnessCls,
    scenario: str,
    rows: int,
    iterations: int,
) -> dict:
    """Returns: {'mean_ms', 'min_ms', 'p95_ms', 'ops_per_s', 'work_units'}."""

    samples_ms: list[float] = []
    work_units = 0

    for it in range(iterations + WARMUP_ITERATIONS):
        # Each iteration uses a FRESH database so state never leaks between runs.
        with tempfile.TemporaryDirectory() as tmpdir:
            db_path = os.path.join(tmpdir, "bench.db")
            h = HarnessCls(db_path)
            h.open()
            try:
                h.execute(SCHEMA_SQL)

                # ── Pre-populate if the scenario reads existing data ──
                if scenario not in ("insert", "insert_autocommit"):
                    h.begin()
                    for i in range(1, rows + 1):
                        h.execute(build_insert_sql(build_row_values(i)))
                    h.commit()

                # ── Timed section ──
                t0 = time.perf_counter()
                if scenario == "insert":
                    h.begin()
                    for i in range(1, rows + 1):
                        h.execute(build_insert_sql(build_row_values(i)))
                    h.commit()
                    units = rows
                elif scenario == "insert_autocommit":
                    # Use a smaller N (the durability stress is per-commit,
                    # not per-row). Cap at 1000 for reasonable runtime.
                    n = min(rows, 1000)
                    for i in range(1, n + 1):
                        h.execute(build_insert_sql(build_row_values(i)))
                    units = n
                elif scenario == "select":
                    units = run_select(h)
                elif scenario == "select_where":
                    units = run_select_where(h)
                elif scenario == "select_pk":
                    units = run_select_pk(h, rows)
                elif scenario == "select_range":
                    units = run_select_range(h, rows)
                elif scenario == "count":
                    units = run_count(h)
                elif scenario == "aggregate":
                    units = run_aggregate(h)
                elif scenario == "update_where":
                    units = run_update_where(h)
                elif scenario == "delete_where":
                    units = run_delete_where(h, rows)
                else:
                    raise ValueError(f"unknown scenario: {scenario}")
                elapsed = time.perf_counter() - t0

                if it >= WARMUP_ITERATIONS:
                    samples_ms.append(elapsed * 1000)
                    work_units = units
            finally:
                h.close()

    samples_ms.sort()
    mean_ms = statistics.mean(samples_ms)
    min_ms = samples_ms[0]
    p95_ms = samples_ms[int(0.95 * (len(samples_ms) - 1))]
    ops_per_s = (work_units / (mean_ms / 1000)) if mean_ms > 0 else 0
    return {
        "mean_ms": mean_ms,
        "min_ms": min_ms,
        "p95_ms": p95_ms,
        "ops_per_s": ops_per_s,
        "work_units": work_units,
    }


# ── Main ──────────────────────────────────────────────────────────────────────


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--rows", type=int, default=DEFAULT_ROWS,
                    help=f"dataset size (default {DEFAULT_ROWS})")
    ap.add_argument("--iterations", type=int, default=ITERATIONS,
                    help=f"timed iterations per scenario (default {ITERATIONS})")
    ap.add_argument("--scenario", choices=SCENARIOS + ["all"], default="all",
                    help="run only one scenario (default: all)")
    ap.add_argument("--engines", default="sqlite,axiomdb",
                    help="comma-separated engine list (default both)")
    args = ap.parse_args()

    engines = [e.strip().lower() for e in args.engines.split(",") if e.strip()]
    harnesses: list[tuple[str, type]] = []
    if "sqlite" in engines:
        harnesses.append(("SQLite",  SqliteHarness))
    if "axiomdb" in engines:
        harnesses.append(("AxiomDB", AxiomDBHarness))

    if not harnesses:
        print("No engines selected. Use --engines sqlite,axiomdb", file=sys.stderr)
        sys.exit(2)

    scenarios = SCENARIOS if args.scenario == "all" else [args.scenario]

    print(f"SQLite vs AxiomDB embedded benchmark")
    print(f"  rows:       {args.rows:,}")
    print(f"  iterations: {args.iterations}  (warmup: {WARMUP_ITERATIONS})")
    print(f"  durability: full fsync per COMMIT on both engines")
    print(f"  python:     {sys.version.split()[0]}, sqlite3 {sqlite3.sqlite_version}")
    print()

    # Header
    cols = ["Scenario"]
    for label, _ in harnesses:
        cols += [f"{label} ms", f"{label} ops/s"]
    if len(harnesses) == 2:
        cols.append("Ratio")
    fmt = "{:<22}" + " {:>14}" * (len(cols) - 1)
    print(fmt.format(*cols))
    print("-" * (22 + 15 * (len(cols) - 1)))

    summary: dict[str, dict[str, dict]] = {}

    for sc in scenarios:
        row = [sc]
        per_engine: dict[str, dict] = {}
        for label, HarnessCls in harnesses:
            try:
                r = bench_one_scenario(HarnessCls, sc, args.rows, args.iterations)
                per_engine[label] = r
                row += [f"{r['mean_ms']:.1f}", f"{r['ops_per_s']:,.0f}"]
            except Exception as exc:
                per_engine[label] = {"error": str(exc)}
                row += ["ERR", "ERR"]
                print(f"\n  {label} on {sc} failed: {exc}\n", file=sys.stderr)

        if len(harnesses) == 2:
            a_ms = per_engine.get(harnesses[0][0], {}).get("mean_ms")
            b_ms = per_engine.get(harnesses[1][0], {}).get("mean_ms")
            if a_ms and b_ms:
                ratio = a_ms / b_ms
                # Show as "Nx" where >1 means second engine is faster than the first
                # (AxiomDB faster than SQLite when ratio > 1, since SQLite is first).
                row.append(f"{ratio:.2f}x")
            else:
                row.append("-")

        print(fmt.format(*row))
        summary[sc] = per_engine

    # Footnote on ratio meaning
    if len(harnesses) == 2:
        a_name = harnesses[0][0]
        b_name = harnesses[1][0]
        print()
        print(f"Ratio = {a_name} ms / {b_name} ms.")
        print(f"  > 1.0  → {b_name} is faster than {a_name}")
        print(f"  < 1.0  → {a_name} is faster than {b_name}")


if __name__ == "__main__":
    main()
