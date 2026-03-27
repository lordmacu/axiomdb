# Plan: Primary-key SELECT access path (Phase 6.16)

## Files to create / modify

- `crates/axiomdb-sql/src/planner.rs`
  - allow PRIMARY KEY indexes in the relevant SELECT planner helper(s)
  - add explicit "forced PK equality lookup" behavior
  - keep existing partial-index and collation guards intact

- `crates/axiomdb-sql/src/executor/select.rs`
  - no algorithmic redesign expected, but confirm both ctx and non-ctx paths
    consume the new PK planner output without special cases

- `crates/axiomdb-sql/tests/integration_executor.rs`
  - add PK SELECT regressions that prove planner behavior is now indexed

- `benches/comparison/local_bench.py`
  - no code change required unless the benchmark labels still describe the path
    as a full scan

## Reviewed first

These AxiomDB files were reviewed before writing this plan:

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-sql/src/planner.rs`
- `crates/axiomdb-sql/src/executor/select.rs`
- `crates/axiomdb-embedded/src/lib.rs`
- `benches/comparison/local_bench.py`
- `benches/comparison/axiomdb_bench/src/main.rs`

These research files were reviewed for guidance:

- `research/sqlite/src/where.c`
- `research/sqlite/src/insert.c`
- `research/postgres/src/backend/executor/nodeIndexscan.c`
- `research/postgres/src/backend/access/heap/heapam.c`

## Research synthesis

### Why the change belongs here

The executor already supports indexed point lookup and range scan. The blocking
gap is planner eligibility: PRIMARY KEY indexes are still excluded from
`find_index_on_col(...)`, so the access method is never chosen.

### Borrow / reject / adapt

#### SQLite — `research/sqlite/src/where.c`, `research/sqlite/src/insert.c`
- Borrow: primary key equality is a privileged access path.
- Reject: rowid-as-table-storage design.
- Adapt: planner-only change over AxiomDB's existing heap + B-Tree design.

#### PostgreSQL — `research/postgres/src/backend/executor/nodeIndexscan.c`,
#### `research/postgres/src/backend/access/heap/heapam.c`
- Borrow: index scan yields tuple identity, heap fetch returns the row.
- Reject: full scan node stack.
- Adapt: keep AxiomDB executor behavior, only expand planner eligibility.

## Algorithm / data structure

### 1. Planner eligibility

Refactor the index-selection helper so SELECT planning can optionally include
PRIMARY KEY indexes:

```rust
fn find_index_on_col(
    col_name: &str,
    indexes: &[IndexDef],
    columns: &[ColumnDef],
    query_where: Option<&Expr>,
    allow_primary: bool,
) -> Option<&IndexDef>
```

Rules:
- existing callers that should still exclude PK can pass `allow_primary=false`
- SELECT planner passes `allow_primary=true`
- FK auto-index exclusion remains unchanged
- partial-index implication remains unchanged

### 2. Forced PK equality

Add a planner rule before the normal cost gate:

```text
if WHERE is pk_col = literal on a PRIMARY KEY or unique leading column:
    emit IndexLookup immediately
```

Reason:
- the current stats/small-table gate is the wrong trade-off for the benchmarked
  PK equality case
- this keeps the fix scope tight without rewriting the whole cost model

### 3. PK range reuse

Allow the existing range extraction logic to see PRIMARY KEY indexes too. Keep
the current range cost rules unless profiling shows they still block the desired
cases after PK eligibility is added.

## Implementation phases

1. Refactor planner helper(s) to optionally include PRIMARY KEY indexes.
2. Add forced PK equality rule in `plan_select(...)`.
3. Thread the same behavior through `plan_select_ctx(...)`.
4. Confirm executor paths in `select.rs` need no semantic changes.
5. Add planner/executor regressions for PK equality and PK range.
6. Re-run `select_pk` benchmark scenario.

## Tests to write

- unit:
  - PK equality emits `AccessMethod::IndexLookup`
  - PK range can emit `AccessMethod::IndexRange`
  - text collation guard still rejects text index plans under non-binary
    collation
- integration:
  - `SELECT * FROM t WHERE id = 5` returns the right row and exercises the PK
    path
  - `SELECT * FROM t WHERE id >= lo AND id < hi` remains correct
- bench:
  - `python3 benches/comparison/local_bench.py --scenario select_pk --rows 5000`

## Anti-patterns to avoid

- Do not "fix" this in the benchmark harness.
- Do not bypass the executor and read rows directly from the index for
  `SELECT *`.
- Do not widen scope into composite planner work.
- Do not remove the existing partial-index or collation guards.

## Risks

- Risk: allowing PRIMARY KEY indexes everywhere regresses planner choices for
  non-benchmark cases.
  Mitigation: scope the forced rule to PK equality first and leave range under
  the existing cost machinery.

- Risk: partial or FK-specific indexes accidentally become planner candidates.
  Mitigation: preserve the current FK exclusion and predicate implication
  checks.
