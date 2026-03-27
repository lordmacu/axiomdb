# Plan: Indexed multi-row INSERT batch path (Phase 6.18)

## Files to create / modify

- `crates/axiomdb-sql/src/executor/insert.rs`
  - route eligible multi-row `InsertSource::Values` statements through the
    existing batch heap + grouped index path even when indexes exist
  - keep single-row and `INSERT ... SELECT` logic unchanged

- `crates/axiomdb-sql/src/executor/staging.rs`
  - extract shared flush/helper pieces if the immediate path can reuse them
    cleanly without depending on `SessionContext::pending_inserts`

- `crates/axiomdb-sql/src/index_maintenance.rs`
  - confirm `batch_insert_into_indexes(...)` is safe for immediate multi-row
    statements with `skip_unique_check=false`
  - extend helper inputs if statement-local duplicate handling needs to be more
    explicit

- `crates/axiomdb-sql/tests/integration_executor.rs`
  - add indexed multi-row INSERT regressions for PK/UNIQUE/partial-index cases

- `benches/comparison/local_bench.py`
  - benchmark target is `insert_multi_values`

## Reviewed first

These AxiomDB files were reviewed before writing this plan:

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-sql/src/executor/insert.rs`
- `crates/axiomdb-sql/src/executor/staging.rs`
- `crates/axiomdb-sql/src/index_maintenance.rs`
- `crates/axiomdb-sql/src/table.rs`
- `crates/axiomdb-sql/src/fk_enforcement.rs`
- `benches/comparison/local_bench.py`
- `specs/fase-05/spec-5.21-transactional-insert-staging.md`

These research files were reviewed for guidance:

- `research/postgres/src/backend/access/heap/heapam.c`
- `research/postgres/src/backend/executor/nodeModifyTable.c`
- `research/mariadb-server/sql/sql_insert.cc`
- `research/mariadb-server/sql/handler.cc`
- `research/duckdb/src/main/appender.cpp`
- `research/oceanbase/src/sql/engine/dml/ob_table_insert_op.cpp`
- `research/sqlite/src/insert.c`

## Research synthesis

### Why the change belongs here

The current executor still says:

- multi-row + no indexes -> batch
- multi-row + any indexes -> per-row fallback

That is now outdated because `5.21` already built the grouped index helper
needed for batched insert flushes. The debt is therefore mostly wiring and
correctness closure, not missing storage primitives.

### Borrow / reject / adapt

#### PostgreSQL / MariaDB / DuckDB / OceanBase
- Borrow: bulk insert is a distinct physical path, even when the SQL surface is
  still plain INSERT.
- Reject: adding a new public appender or engine-specific bulk-insert lifecycle.
- Adapt: statement-local batching inside the existing executor.

## Algorithm / data structure

### 1. Eligibility

Only the immediate multi-row `VALUES` path is in scope:

```text
InsertSource::Values with rows.len() > 1
```

Keep as-is:
- single-row INSERT
- `INSERT ... SELECT`
- `DEFAULT VALUES`
- staged flush path from `5.21`

### 2. Batch path shape

For eligible statements:

```text
1. build full row values for every VALUES tuple
2. run current CHECK/FK/auto-inc/coercion logic for each row
3. heap insert all rows via insert_rows_batch[_with_ctx](...)
4. run batch_insert_into_indexes(...) once across the whole statement
5. persist changed roots once per index
6. update stats once for the whole statement
```

This replaces:

```text
for each row:
    insert_row(...)
    insert_into_indexes(...)
    update_index_root(...)
```

### 3. Shared helper extraction

If the immediate path and staged flush path now do the same physical work,
extract a shared helper such as:

```rust
fn apply_insert_batch(...)
```

Responsibilities:
- heap batch insert
- grouped index maintenance
- root persistence
- stats update

Do not couple this helper to `pending_inserts`.

### 4. Uniqueness and duplicate handling

Closed decision:
- reuse `batch_insert_into_indexes(...)` as the core grouped helper
- default to `skip_unique_check=false` for the immediate executor path
- only use the optimized skip path where uniqueness was already verified
  earlier (`5.21` staged inserts)

If intra-statement duplicates or empty-index bulk-load cases expose a gap,
extend the helper explicitly rather than silently relying on staged-insert
assumptions.

## Implementation phases

1. Extract or write a shared batch-apply helper for heap + indexes.
2. Replace immediate multi-row indexed fallback in ctx path.
3. Replace immediate multi-row indexed fallback in non-ctx path.
4. Keep staged `5.21` path using the same helper where possible.
5. Add PK/UNIQUE/partial-index regressions.
6. Benchmark `insert_multi_values`.

## Tests to write

- unit:
  - `batch_insert_into_indexes(...)` immediate-path semantics if helper changes
- integration:
  - multi-row INSERT with PK only
  - multi-row INSERT with UNIQUE secondary index
  - partial-index membership correctness
  - FK child validation across the batch
- bench:
  - `python3 benches/comparison/local_bench.py --scenario insert_multi_values --rows 5000`

## Anti-patterns to avoid

- Do not reimplement a second grouped-insert algorithm beside `5.21`.
- Do not silently enable staged-insert assumptions (`skip_unique_check=true`)
  for immediate statements.
- Do not widen scope into `INSERT ... SELECT` or autocommit group-commit work.
- Do not regress single-row INSERT to force everything through the batch path.

## Risks

- Risk: the grouped helper was originally written for staged inserts and bakes
  in assumptions that do not hold for immediate statements.
  Mitigation: use the strict path (`skip_unique_check=false`) by default and
  add explicit immediate-path regressions.

- Risk: extracted shared helper couples immediate INSERT and staged flush too
  tightly.
  Mitigation: keep helper focused on physical apply only; keep staging policy
  outside it.
