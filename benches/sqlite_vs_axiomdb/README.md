# SQLite vs AxiomDB embedded benchmark

Apples-to-apples comparison between SQLite (Python stdlib) and AxiomDB
embedded (Python ctypes binding). Both engines run **in the same process** —
no network, no IPC, no Docker.

This is the canonical benchmark for the **embedded-first release**.
For server-mode comparisons see `../comparison/` (MySQL/PostgreSQL/MariaDB).

---

## Fairness rules

Both engines are configured for full crash-safe durability:

| Engine  | Durability config |
|---------|-------------------|
| SQLite  | `PRAGMA journal_mode=WAL` + `PRAGMA synchronous=FULL` (fsync per COMMIT) |
| AxiomDB | default (`fsync=true`, WAL-native) |

Both:
- Run in-process, no network.
- Use a fresh temp file for every iteration (no warm cache between runs).
- Run the same SQL text (no parameter binding, both engines parse identically).
- Run the same schema and same number of iterations.

The first iteration of every scenario is a **warmup** and discarded.

---

## Usage

```bash
# Build the AxiomDB embedded library once
cargo build --release -p axiomdb-embedded

# Run the full benchmark with default 10K rows
python3 benches/sqlite_vs_axiomdb/bench.py

# Smaller dataset for a quick smoke test
python3 benches/sqlite_vs_axiomdb/bench.py --rows 1000

# Heavier dataset (slower)
python3 benches/sqlite_vs_axiomdb/bench.py --rows 100000

# Single scenario
python3 benches/sqlite_vs_axiomdb/bench.py --scenario select_pk

# Only one engine (useful for debugging the harness)
python3 benches/sqlite_vs_axiomdb/bench.py --engines axiomdb
```

No third-party packages are required: `sqlite3` is in the Python standard
library and `axiomdb` is loaded from `bindings/python/axiomdb.py`.

---

## Scenarios

| Name                  | What it measures |
|-----------------------|------------------|
| `insert`              | N single-row `INSERT`s inside one explicit transaction. |
| `insert_autocommit`   | 1 transaction per row (durability worst case, capped at 1000 rows). |
| `select`              | Full-table scan (`SELECT *`). |
| `select_where`        | Filtered scan (`active = 1`, ~50% selectivity). |
| `select_pk`           | 200 primary-key point lookups by id. |
| `select_range`        | 10% range scan (`id >= a AND id < b`). |
| `count`               | `SELECT COUNT(*)` aggregation. |
| `aggregate`           | `GROUP BY age, AVG(score)` hash aggregation. |
| `update_where`        | `UPDATE ... SET score = score + 1 WHERE active = 1`. |
| `delete_where`        | Large selective delete (`id > N/2`). |

---

## Reading the output

```
Scenario                  SQLite ms   SQLite ops/s    AxiomDB ms  AxiomDB ops/s      Ratio
------------------------------------------------------------------------------------------
insert                       123.4         81,037         98.7        101,317      1.25x
select_pk                     12.0         16,667          4.1         48,780      2.93x
```

`Ratio = SQLite_ms / AxiomDB_ms`:
- `> 1.0` → AxiomDB is faster than SQLite.
- `< 1.0` → SQLite is faster than AxiomDB.
- `= 1.0` → tied.

Each scenario uses a separate temp DB. Numbers are means over
`--iterations` (default 5) timed runs after a discarded warmup.

---

## Why no `:memory:` mode

Embedded users overwhelmingly use SQLite/AxiomDB with a **file** path. The
`:memory:` mode hides disk and fsync costs, which is exactly where SQLite's
WAL and AxiomDB's storage engine spend most of their time. Benchmarking
purely in-memory would tell us about CPU+parser+planner cost only, not the
end-to-end picture an embedded user actually sees.

If a memory-only comparison is later needed (e.g. to isolate CPU), add a
`--memory` flag — it is not the default for a reason.
