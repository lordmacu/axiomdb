# Spec: tablesample

Phase: 20 — Types + import/export
Task: TABLESAMPLE — page-level and row-level random sampling
Status: approved

## Context

`TABLESAMPLE` is a SQL:2003 clause that restricts the rows returned from a table
to an approximate random fraction without scanning the entire result set. It appears
in the FROM clause after a table reference. The storage engine exposes a
page-level heap chain (`table_scan.rs`) that makes page-granularity skipping
straightforward for the SYSTEM method. The `rand = "0.8"` crate is already in
the workspace (added for Phase 20.12 ORDER BY RANDOM()). TABLESAMPLE is a
precursor to statistical analysis, ML data splits, and A/B test row sampling.

## Goal

Make `SELECT … FROM t TABLESAMPLE SYSTEM(p)` and `BERNOULLI(p)` return an
approximate p-percent random sample of heap table rows with correct per-method
semantics, without modifying the storage layer.

## Non-goals

- `REPEATABLE(seed)` — deterministic/seeded sampling; deferred to Phase 28.8.
- TABLESAMPLE on clustered (B+ tree) tables — falls back to post-scan Bernoulli
  filter; page-level skipping inside a B+ tree walk is out of scope.
- TABLESAMPLE in DML sources (UPDATE FROM, DELETE FROM) — deferred.
- TABLESAMPLE in subquery or JOIN table references — deferred (MVP: only direct
  FROM clause on a single table).
- `TABLESAMPLE SYSTEM_ROWS(n)` — non-standard; deferred.

## Behavior

### SQL syntax

```sql
SELECT * FROM t TABLESAMPLE SYSTEM(1);          -- ~1% of rows via page sampling
SELECT * FROM t TABLESAMPLE BERNOULLI(0.5);     -- ~0.5% of rows via row sampling
SELECT id FROM orders TABLESAMPLE SYSTEM(10) WHERE status = 'open';
SELECT * FROM t TABLESAMPLE BERNOULLI(100);     -- all rows (shortcut: no sampling)
SELECT * FROM t TABLESAMPLE SYSTEM(0);          -- empty result (shortcut: skip all)
```

The `p` argument is a percentage in `[0.0, 100.0]`. It is a literal numeric
expression (constant only in V1). Values outside `[0.0, 100.0]` return an error.

`TABLESAMPLE` is parsed **after** the table alias if any:
```sql
SELECT * FROM t AS t1 TABLESAMPLE SYSTEM(5);
```

### AST additions

```rust
/// `TABLESAMPLE SYSTEM(p)` or `BERNOULLI(p)` on a table reference.
#[derive(Debug, Clone, PartialEq)]
pub struct TableSample {
    pub method: TableSampleMethod,
    /// Percentage in [0.0, 100.0].
    pub percent: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableSampleMethod {
    System,
    Bernoulli,
}
```

`TableRef` gains one new field:

```rust
pub struct TableRef {
    pub database: Option<String>,
    pub schema: Option<String>,
    pub name: String,
    pub alias: Option<String>,
    pub tablesample: Option<TableSample>,   // NEW
}
```

### Sampling semantics

#### SYSTEM method (page-level)

For each page in the heap chain:
1. Generate a uniform `f64` in `[0, 1)` via `rand::thread_rng().gen::<f64>()`.
2. If `rng_val >= percent / 100.0` → skip the entire page (no `read_slot` calls,
   but `read_page` still happens to follow the chain pointer to the next page).
3. Otherwise → include all MVCC-visible rows from that page.

Shortcut: if `percent >= 100.0`, include every page (no coin flips).  
Shortcut: if `percent <= 0.0`, return empty immediately.

For **clustered tables**: fall back to post-scan Bernoulli (see below) — B+ tree
page layout makes page-level skipping semantically incorrect.

#### BERNOULLI method (row-level)

For each MVCC-visible row in the heap:
1. Generate a uniform `f64` via `rng.gen::<f64>()`.
2. If `rng_val < percent / 100.0` → include the row.
3. Otherwise → skip.

Shortcut: if `percent >= 100.0`, include every row.  
Shortcut: if `percent <= 0.0`, return empty immediately.

Applies to both heap and clustered tables (post-scan filter for clustered).

#### WHERE / ORDER BY / LIMIT interaction

TABLESAMPLE sampling is applied first (at the scan layer), then WHERE filtering,
then ORDER BY, then LIMIT/OFFSET — exactly as if TABLESAMPLE produced a virtual
table. This matches PostgreSQL behavior.

#### Determinism

Two executions of the same TABLESAMPLE query will return different rows (no
repeatability guarantee in V1). REPEATABLE(seed) deferred to 28.8.

#### REPEATABLE clause

`TABLESAMPLE SYSTEM(1) REPEATABLE(42)` — parser accepts `REPEATABLE(expr)` and
returns `DbError::NotImplemented { feature: "TABLESAMPLE REPEATABLE" }` at
execution time. Parser must not silently drop the clause.

### Scan function signature (internal)

```rust
/// Samples a heap table according to `sample`.
/// Returns MVCC-visible rows with their RecordId.
pub fn scan_table_sampled(
    storage: &dyn StorageEngine,
    table_def: &TableDef,
    columns: &[ColumnDef],
    snap: TransactionSnapshot,
    column_mask: Option<&[bool]>,
    sample: &TableSample,
) -> Result<Vec<(RecordId, Vec<Value>)>, DbError>;
```

### Error cases

| Input | Expected error | Notes |
|-------|----------------|-------|
| `TABLESAMPLE SYSTEM(-1)` | `DbError::InvalidValue { reason: "TABLESAMPLE percent must be in [0, 100]" }` | |
| `TABLESAMPLE BERNOULLI(101)` | `DbError::InvalidValue { reason: "TABLESAMPLE percent must be in [0, 100]" }` | |
| `TABLESAMPLE SYSTEM(1) REPEATABLE(42)` | `DbError::NotImplemented { feature: "TABLESAMPLE REPEATABLE" }` | |
| `TABLESAMPLE UNKNOWN(5)` | `DbError::ParseError { message: "unknown TABLESAMPLE method 'UNKNOWN'; expected SYSTEM or BERNOULLI" }` | |

## Edge cases

- [ ] `TABLESAMPLE SYSTEM(0)` → empty result, no rows scanned
- [ ] `TABLESAMPLE SYSTEM(100)` → all rows returned (shortcut, no coin flips)
- [ ] `TABLESAMPLE BERNOULLI(100)` → all rows returned
- [ ] `TABLESAMPLE BERNOULLI(0)` → empty result
- [ ] Empty table → empty result (no panic)
- [ ] Single-row table with `SYSTEM(50)` → 0 or 1 row (non-deterministic)
- [ ] `WHERE` clause applied after sampling (not pushed into sample)
- [ ] `ORDER BY` + `LIMIT` compose correctly after sampling
- [ ] `FROM t AS alias TABLESAMPLE SYSTEM(10)` — alias works
- [ ] Negative percentage → error
- [ ] Percentage > 100 → error
- [ ] `REPEATABLE` clause → `NotImplemented` error (not silent drop)

## Performance budget

| Operation | Target |
|-----------|--------|
| `TABLESAMPLE SYSTEM(1)` on 10K-row table | < 2 ms (reads only ~1% of pages) |
| `TABLESAMPLE BERNOULLI(1)` on 10K-row table | < 5 ms (reads all pages, skips rows) |

## Dependencies

- Depends on: `rand = "0.8"` (already in workspace), `table_scan.rs` heap chain.
- Blocks: nothing.

## Open questions

None — all resolved during brainstorm.

## Done criteria

- [ ] `FROM t TABLESAMPLE SYSTEM(p)` parsed and executed with page-level skipping.
- [ ] `FROM t TABLESAMPLE BERNOULLI(p)` parsed and executed with row-level skipping.
- [ ] `SYSTEM(0)` and `BERNOULLI(0)` return empty results.
- [ ] `SYSTEM(100)` and `BERNOULLI(100)` return all rows.
- [ ] Invalid percentage (< 0 or > 100) → `DbError::InvalidValue`.
- [ ] Unknown method → `DbError::ParseError`.
- [ ] `REPEATABLE` clause → `DbError::NotImplemented` (parser accepts, executor rejects).
- [ ] `WHERE`, `ORDER BY`, `LIMIT` compose correctly with sampling.
- [ ] `cargo nextest run --workspace` passes.
- [ ] `cargo clippy --workspace -- -D warnings` clean.
- [ ] `cargo fmt --check` clean.
- [ ] Wire: 2+ new assertions (574 → 576+).
- [ ] `docs/progreso.md`: 20.11 ✅.
- [ ] `docs-site/src/user-guide/sql-reference/expressions.md` updated.

## References

- PostgreSQL: `src/backend/executor/nodeSamplescan.c` — SYSTEM uses BlockSampler
  (Vitter's algorithm for page-level sampling); BERNOULLI samples each tuple.
- SQL:2003 Part 2 Section 7.6 (table reference): `TABLESAMPLE` clause definition.
- `crates/axiomdb-sql/src/table_scan.rs` — heap chain scan loop (hook point for
  SYSTEM page skip and BERNOULLI row skip).
- `crates/axiomdb-sql/src/ast.rs:31` — `TableRef` struct to extend.
- `crates/axiomdb-sql/src/parser/dml.rs:1022` — `parse_from_item` (parse hook).
- `crates/axiomdb-sql/src/executor/select_core.rs:233` — `scan_table` call site.
