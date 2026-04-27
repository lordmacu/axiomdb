# Spec: 13.14 — Custom aggregate functions

Phase: 13 — Advanced PostgreSQL
Task: 13.14 Custom aggregate functions
Status: draft

## Context

`docs/progreso.md` describes `13.14` as support for `CREATE AGGREGATE` with
transition state (`SFUNC` / `STYPE`) and finalization (`FINALFUNC`), enabling
aggregates beyond `SUM` / `COUNT` / `AVG` / `MIN` / `MAX`.

The current repo is much narrower:

- aggregate detection is hardcoded by function name in analyzer and executor
- aggregate descriptors and accumulators are enum-based and built-in only
- no parser / AST / catalog support exists for `CREATE AGGREGATE`
- no generic `CREATE FUNCTION` runtime exists yet despite older roadmap text
  mentioning future scalar UDFs in Phase `16.1`

That means `13.14` should not be framed as “arbitrary user code runs inside the
aggregate engine now”. The real deliverable must be a catalog-backed
`CREATE AGGREGATE` system that composes **registered internal transition/final
functions**, not a full procedural UDF subsystem.

## Goal

Deliver real SQL `CREATE AGGREGATE` / `DROP AGGREGATE` support with catalog
persistence and executor lookup, using a bounded registry of safe internal
aggregate helper routines.

The result should let users define new aggregate names and signatures, for
example:

```sql
CREATE AGGREGATE median(FLOAT) (
  SFUNC = median_state,
  STYPE = FLOAT[],
  FINALFUNC = median_final
);

SELECT median(latency_ms) FROM samples;
```

without requiring general `CREATE FUNCTION` or arbitrary SQL/WASM callbacks in
this subphase.

## Non-goals

- Not implementing generic scalar SQL UDFs (`CREATE FUNCTION ... AS $$ ... $$`).
- Not implementing WASM or plugin-defined aggregate callbacks.
- Not implementing `INITCOND`, `COMBINEFUNC`, `SERIALFUNC`, `DESERIALFUNC`,
  parallel aggregation contracts, or moving-window support.
- Not implementing ordered-set / hypothetical-set aggregates in this slice.
- Not implementing window execution for custom aggregates.
- Not implementing `FILTER (WHERE ...)` specifically for custom aggregates if
  the base aggregate path does not already support it.
- Not treating `SFUNC` / `FINALFUNC` names as invocable SQL functions outside
  the aggregate registry.
- Not promising every aspirational example from `docs/progreso.md` in the MVP;
  multi-argument and statistically advanced aggregates may remain deferred.

## Recommended cut for 13.14

Close `13.14` as a **catalog-backed aggregate-definition system** over an
internal runtime registry:

1. `CREATE AGGREGATE name(type[, ...]) (...)`
2. `DROP AGGREGATE name(type[, ...])`
3. aggregate lookup during normal `SELECT ... GROUP BY ...`
4. dedicated registry of allowed helper routines (`SFUNC`, `FINALFUNC`,
   supported `STYPE` identifiers)
5. wire-visible errors and metadata honest enough for round-trip testing

This gives a real feature with real DDL semantics now, while explicitly
deferring arbitrary function bodies to later phases.

## Public SQL surface

### Accepted syntax

```sql
CREATE AGGREGATE median(FLOAT) (
  SFUNC = median_state,
  STYPE = FLOAT[],
  FINALFUNC = median_final
);

DROP AGGREGATE median(FLOAT);
```

Optional stretch if cheap and consistent with existing DDL metadata tooling:

```sql
SHOW CREATE AGGREGATE median(FLOAT);
```

### Invocation

```sql
SELECT median(v) FROM t;
SELECT grp, median(v) FROM t GROUP BY grp;
```

### Validation rules

- aggregate name resolution becomes catalog-backed first, built-ins second
  (or vice versa if required for compatibility), but behavior must be
  deterministic and documented
- input arity and argument types must match the stored aggregate signature
- `SFUNC`, `FINALFUNC`, and `STYPE` must resolve against the internal registry
  at `CREATE AGGREGATE` time
- duplicate aggregate signature in the same schema must error cleanly

## Runtime model

Each custom aggregate definition persists:

- schema-qualified aggregate name
- input argument types
- transition function name (`SFUNC`)
- state type name (`STYPE`)
- optional final function name (`FINALFUNC`)

At execution time:

1. analyzer resolves the aggregate call to either a built-in descriptor or a
   catalog custom aggregate definition
2. executor instantiates a runtime accumulator through the internal registry
3. per-row updates call the registered transition routine
4. finalize calls the registered final routine once per group (or returns the
   final state directly if no final function is specified)

## Registry boundary

The registry is the critical scope cut.

For `13.14`, `SFUNC` / `FINALFUNC` names are **not** arbitrary executable SQL
objects. They are validated against a built-in Rust registry like:

- `median_state`
- `median_final`
- future compatible helpers added deliberately by the engine

Likewise, `STYPE` is validated against supported state kinds known by the same
registry. It is acceptable for these state kinds to be more permissive in DDL
than the general SQL type system, as long as they are validated and executed
deterministically.

## Initial supported scope

The MVP should stay narrow:

- plain grouped aggregation and whole-query aggregation only
- deterministic aggregates only
- unary aggregate signatures are sufficient for the first cut
- at least one end-to-end useful custom aggregate must ship, with `median` the
  most natural candidate because it matches the progress tracker example

If the implementation needs a second helper aggregate to prove catalog
generality, prefer another unary aggregate over widening to multi-arg support.

## Catalog and metadata

New catalog metadata is required for custom aggregates. Minimum fields:

- name
- schema
- argument type list
- transition function name
- state type name
- final function name (optional)

Nice-to-have metadata if cheap:

- `CREATE AGGREGATE` round-trip text
- owner / creation timestamp if function catalogs already follow that pattern

## Error cases

| Input | Expected error |
|-------|----------------|
| unknown `SFUNC` / `FINALFUNC` | `DbError::NotImplemented` or `InvalidValue` |
| unsupported `STYPE` | `DbError::InvalidValue` |
| duplicate aggregate signature | already exists / duplicate object error |
| wrong invocation arity | function/aggregate signature mismatch |
| use in unsupported context (window, ordered-set, etc.) | clear `NotImplemented` |
| dropping missing aggregate without `IF EXISTS` | missing object error |

## Edge cases

- [ ] custom aggregate name does not accidentally collide with built-in dispatch
- [ ] grouped and non-grouped aggregation both resolve custom definitions
- [ ] `NULL` handling follows the registered transition/final contract
- [ ] multiple custom aggregates in one `SELECT` do not alias the same state
- [ ] duplicate `CREATE AGGREGATE` with same signature errors
- [ ] `DROP AGGREGATE` removes the catalog entry and invalidates cached plans
- [ ] wire-visible error text is deterministic for invalid registry names

## Done criteria

- [ ] parser / AST accept `CREATE AGGREGATE` and `DROP AGGREGATE`
- [ ] catalog persists custom aggregate definitions
- [ ] analyzer resolves custom aggregate calls by name + signature
- [ ] executor supports a registry-backed custom accumulator path
- [ ] at least one real custom aggregate works end-to-end
- [ ] built-in aggregate behavior does not regress
- [ ] SQL integration coverage exists for create/use/drop/error paths
- [ ] optional `SHOW CREATE AGGREGATE` or equivalent metadata round-trip is
      implemented if chosen in the cut
- [ ] wire smoke includes one bounded `13.14` scenario
- [ ] docs stop implying generic UDF-backed aggregate bodies already exist
- [ ] `cargo test -p axiomdb-sql` for touched tests passes
- [ ] `python3 tools/wire-test.py` passes
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes

## Files reviewed

- `crates/axiomdb-sql/src/analyzer_stmt.rs`
- `crates/axiomdb-sql/src/parser/expr.rs`
- `crates/axiomdb-sql/src/executor/agg_descriptor.rs`
- `crates/axiomdb-sql/src/executor/agg_accum.rs`
- `docs/progreso.md`
- `db.md`
