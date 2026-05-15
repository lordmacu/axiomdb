# Spec: order-by-random

Phase: 20 — Types + import/export
Task: ORDER BY RANDOM() — random row ordering
Status: approved

## Context

The executor's `apply_order_by` in `shared.rs` is the central sort dispatch used by
all SELECT paths. `RAND()`/`RANDOM()` already exists as a scalar function but uses a
weak LCG seeded from subsec_nanos. When used as a sort key its random value is
re-evaluated on every comparator call, violating transitivity and producing
non-deterministic (and potentially corrupt) sort output. The `rand = "0.8"` crate is
already in the workspace.

## Goal

Make `ORDER BY RANDOM()` produce a correct, uniformly random permutation of result rows,
and fix `RAND()`/`RANDOM()` to use a proper PRNG.

## Non-goals

- `TABLESAMPLE` — separate subphase (20.11).
- Seeded / reproducible random (`ORDER BY RANDOM(seed)`) — deferred.
- Parallel shuffle — single-threaded is sufficient for V1.
- Changing the return type of `RAND()` (stays `Real`).

## Behavior

### RAND() / RANDOM() scalar function

Returns a uniformly distributed `Real` in `[0.0, 1.0)` using `rand::random::<f64>()`
(which sources from `thread_rng` internally). Accepts zero arguments; any non-zero
arity returns `DbError::InvalidValue`.

### ORDER BY RANDOM() semantics

```sql
SELECT * FROM t ORDER BY RANDOM();
SELECT * FROM t ORDER BY RANDOM() LIMIT N;
SELECT * FROM t ORDER BY col1, RANDOM();   -- sort by col1 first, then random within ties
```

#### Pure RANDOM() — single sort key that is RANDOM()/RAND() call

When the ORDER BY list contains exactly one item and that item's expression is a
zero-argument call to `random` or `rand` (case-insensitive), the executor performs an
in-place Fisher-Yates shuffle via `rand::seq::SliceRandom::shuffle` on the full result
set before applying LIMIT/OFFSET. Complexity: O(n).

#### Mixed ORDER BY — RANDOM() combined with other expressions

When ORDER BY contains a mix of RANDOM() and non-RANDOM() items, or multiple RANDOM()
calls, each RANDOM() expression is pre-materialized to a single `f64` per row before
sorting begins. The sort then uses those pre-materialized values for stability.
Non-RANDOM() expressions are evaluated lazily as usual.

#### LIMIT interaction

For `ORDER BY RANDOM() LIMIT N`:
- Pure case: shuffle the full result set, then slice `[offset..offset+N]`. No heap
  sort optimization (the top-N heap requires repeated comparisons, breaking RANDOM()).
- Mixed case: pre-materialize random keys, then apply normal top-N or full sort.

#### NULL semantics

`RANDOM()` never returns NULL; no special NULL handling needed for the random key.

#### Determinism

Two executions of the same query will (almost certainly) return different row orders.
No repeatability guarantee is provided or required in V1.

### is_rand_call helper (internal)

```rust
/// Returns true if `expr` is a zero-argument call to RAND or RANDOM.
fn is_rand_call(expr: &Expr) -> bool {
    matches!(expr,
        Expr::Function { name, args }
        if args.is_empty() && matches!(name.to_ascii_lowercase().as_str(), "rand" | "random")
    )
}
```

### apply_order_by changes (internal)

```rust
fn apply_order_by(rows: Vec<Row>, order_items: &[OrderByItem]) -> Result<Vec<Row>, DbError>
```

New behaviour before the existing sort:

1. If `order_items.len() == 1 && is_rand_call(&order_items[0].expr)` → shuffle and return.
2. If any item satisfies `is_rand_call` → pre-materialize one `f64` per row for each
   RANDOM() item; replace those ORDER BY expressions with a `Expr::Literal(Value::Real(f))`
   per row; run existing sort logic on the augmented rows.
3. Otherwise → existing behaviour unchanged.

`apply_order_by_top_n` falls back to `apply_order_by` when any ORDER BY item is a
RANDOM() call (top-N heap is incompatible with pre-materialization in current form).

## Edge cases

- [ ] Empty table → empty result (no panic in shuffle)
- [ ] Single-row table → that row returned (shuffle of 1 is identity)
- [ ] `ORDER BY RANDOM() LIMIT 0` → empty result
- [ ] `ORDER BY RANDOM() LIMIT N` where N > row count → all rows, shuffled
- [ ] `ORDER BY RANDOM() OFFSET M LIMIT N` → correct slice after shuffle
- [ ] `ORDER BY col1 ASC, RANDOM()` → col1 ordering respected; within ties, random
- [ ] `RAND()` with no arguments → valid, returns Real
- [ ] `RAND(1)` with one argument → `DbError::InvalidValue { reason: "RAND takes no arguments" }`
- [ ] Two sequential `ORDER BY RANDOM()` queries → (almost certainly) different orderings

## Performance budget

| Operation | Target |
|-----------|--------|
| `ORDER BY RANDOM()` on 10K rows | < 2 ms |
| `ORDER BY RANDOM() LIMIT 10` on 10K rows | < 2 ms (full shuffle, no heap optimization) |

Fisher-Yates via `rand` crate on 10K rows takes ~50 µs — well within budget.

## Dependencies

- Depends on: `rand = "0.8"` (already in workspace), existing `apply_order_by` in `shared.rs`.
- Blocks: nothing.

## Open questions

None — all resolved during brainstorm.

## Done criteria

- [ ] `RAND()` / `RANDOM()` use `rand::random::<f64>()` (not LCG/subsec_nanos).
- [ ] `ORDER BY RANDOM()` returns a permutation of all rows (verified by sorting the result and comparing to full table).
- [ ] `ORDER BY RANDOM() LIMIT N` returns exactly N distinct rows.
- [ ] `ORDER BY col1, RANDOM()` respects col1 ordering.
- [ ] `ORDER BY RANDOM() OFFSET M LIMIT N` returns correct slice.
- [ ] `RAND(1)` (non-zero arity) returns error.
- [ ] Empty table returns empty result.
- [ ] `cargo nextest run --workspace` passes.
- [ ] `cargo clippy --workspace -- -D warnings` clean.
- [ ] `cargo fmt --check` clean.
- [ ] Wire: 2+ new assertions (571 → 573+).
- [ ] `docs/progreso.md` updated: `20.12 ✅`.
- [ ] `docs-site/src/user-guide/sql-reference/expressions.md` updated with RANDOM() note.

## References

- PostgreSQL: `ORDER BY random()` — pre-materializes sort key, full shuffle.
- DuckDB: `ORDER BY random()` — same pre-materialization strategy.
- Existing RAND impl: `crates/axiomdb-sql/src/eval/functions/numeric.rs:290`
- apply_order_by: `crates/axiomdb-sql/src/executor/shared.rs:951`
- rand crate: `rand::seq::SliceRandom::shuffle`, `rand::random::<f64>()`
