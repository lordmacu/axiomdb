# Spec: 8.1 — Vectorized Filter (Batch Predicate Evaluation)

## Context

AxiomDB's scan path evaluates WHERE predicates row-by-row: for each row,
decode all columns → eval(expr, &row) → is_truthy → push to result. This
causes per-row allocations (Vec<Value>, .to_vec()) and prevents CPU
pipeline/cache optimization.

**Research findings:**
- DuckDB processes 2,048 rows per chunk with SelectionVector (index array,
  zero-copy filtering). 3-10× speedup vs row-at-a-time.
- PostgreSQL and MariaDB are purely row-at-a-time.
- DataFusion uses Arrow RecordBatch (8,192 rows, columnar).

**Profiling findings:**
- `.to_vec()` per row: 50-100 cycles (biggest per-row cost)
- `decode_row()` per row: 100-500 cycles
- `eval_with()` per row: 7-200 cycles
- Total: ~200-800 cycles per row in scan path

## What to build

A batch scan path that processes rows in page-sized chunks (~200 rows):

1. Read one heap page
2. MVCC visibility: iterate all slots, collect visible slot offsets (no decode)
3. WHERE columns only: decode only columns needed by the WHERE clause
4. Evaluate WHERE predicate on decoded columns → build selection mask
5. Full decode: decode remaining SELECT columns only for passing rows
6. Push results

This eliminates per-row decode for filtered-out rows and reduces allocations
from O(total_rows) to O(passing_rows).

## Expected impact

For `SELECT * FROM t WHERE active = TRUE` (50% selectivity):
- Before: decode 5000 rows → eval 5000 → keep 2500
- After: decode WHERE col for 5000 → eval 5000 → decode full for 2500 only
- Saves: 2500 × full decode + 2500 × Vec<Value> allocations

## Acceptance Criteria

- [ ] New `scan_table_filtered` path in TableEngine
- [ ] WHERE columns decoded separately from SELECT columns
- [ ] Only passing rows get full decode
- [ ] Results identical to existing scan path
- [ ] `cargo test --workspace` passes
- [ ] Benchmark improvement on select_where scenario

## Out of Scope

- Columnar storage (data stays row-oriented in heap pages)
- SIMD instructions (Phase 8.2)
- Selection vectors (DuckDB-style index arrays — future optimization)
- Expression JIT compilation
