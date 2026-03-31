# Spec: Literal-Normalized COM_QUERY Plan Cache (27.8b)

## Context

Point lookups like `SELECT * FROM users WHERE id = 42` spend ~52% of time in
parse + analyze (~100µs). The B-Tree lookup is ~1µs. For repeated queries that
differ only in literal values (e.g., `id = 42` vs `id = 43`), the parse+analyze
work is identical and wasted.

**Reference:** PostgreSQL caches plans per-prepared-statement and decides between
generic and custom plans after 5 executions. AxiomDB already has prepared statement
caching (COM_STMT_PREPARE), but COM_QUERY (ad-hoc SQL) always re-parses.

## What to build

A per-connection cache that normalizes COM_QUERY SQL strings by replacing literal
values with `?` placeholders, then reuses the parsed+analyzed `Stmt` for subsequent
queries with the same structure but different literal values.

```
"SELECT * FROM users WHERE id = 42"
  → normalize → "SELECT * FROM users WHERE id = ?"  + params = [Value::Int(42)]
  → hash normalized string → cache key
  → cache hit? → substitute params into cached Stmt → execute
  → cache miss? → parse + analyze → store → substitute → execute
```

## Design

### Normalization
Replace numeric literals (int, bigint, real), string literals, and boolean literals
in the token stream with `?`. Track extracted values in a `Vec<Value>`.

### Cache key
FNV-1a hash of the normalized SQL string (after literal replacement). Fast, no
collision issues for per-connection cache with ~100-1000 entries.

### Cache structure
Per-connection `HashMap<u64, CachedPlan>` stored in handler local state.

```rust
struct CachedPlan {
    analyzed_stmt: Stmt,           // parsed + analyzed AST
    schema_version: u64,           // for DDL invalidation
    param_count: usize,            // number of ? placeholders
}
```

### Invalidation
Compare `cached.schema_version` with current `schema_version.load()`.
If different → evict entry, re-parse.

### Parameter substitution
Reuse existing `substitute_params_in_ast()` from prepared.rs.

## Acceptance Criteria

- [ ] Literal normalization extracts int/bigint/real/string/bool values
- [ ] Normalized SQL hashed for cache lookup
- [ ] Cache hit skips parse + analyze (reuses cached Stmt)
- [ ] Cache miss: parse + analyze + store
- [ ] DDL invalidates cache (schema_version check)
- [ ] Repeated point lookups with different literals: cache hit
- [ ] Queries with different structure: cache miss (correct)
- [ ] `cargo test` passes

## Out of Scope
- Global shared cache across connections
- Cost-based generic vs custom plan decision (PostgreSQL style)
- Prepared statement integration
