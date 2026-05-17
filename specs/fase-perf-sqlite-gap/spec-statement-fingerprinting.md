# Spec: statement-fingerprinting — auto-prepared statements

Phase: perf-sqlite-gap — close embedded INSERT gap with SQLite
Task: Attack 2 — automatic plan reuse by query shape (literals stripped)
Status: partially implemented (infrastructure landed; wire-up reverted — see plan)

## Context

After Attacks 3.A + 3.B, INSERT throughput is **21K rows/s** vs SQLite
**1.02M** (49× gap). The per-row engine work in
`enqueue_clustered_insert_ctx` is **1.02 µs** (competitive). The remaining
gap is **per-statement scaffolding**: parse + analyze + resolve_table +
build_insert_column_positions + ExecutionContext::new + dispatcher match
+ conn_txn take/restore + trigger wrapper — roughly **43 µs per call**.

SQLite's `sqlite3_prepare_v2` + `sqlite3_bind_*` + `sqlite3_step` pattern
amortizes all this. AxiomDB already has a manual `PreparedStatement`
(Phase 10.8 — [embedded/lib.rs:369](crates/axiomdb-embedded/src/lib.rs)) but
it requires user code to call `db.prepare(sql)` explicitly. The benchmark
(and most ad-hoc SQL) does not — it calls `db.run(sql)` with literals
interpolated, paying full per-statement cost every time.

We also have the `PlanDeps` infrastructure
([plan_deps.rs](crates/axiomdb-sql/src/plan_deps.rs)) that tracks
`(table_id, schema_version)` per compiled plan — ready for invalidation
use but currently unwired.

This spec adds **auto-prepared statements**: a session-local cache keyed
by query **shape** (literal-stripped fingerprint). On `db.run(sql)`, we
rewrite the AST replacing `Expr::Literal(_)` with `Expr::Param { idx }`,
hash the shape, look up a cached plan, validate dependencies via
`PlanDeps`, substitute the extracted literals back as `Expr::Param` →
`Expr::Literal`, and execute. Cache miss = full compile (parse + analyze)
+ cache write.

DuckDB calls this "auto-parameterization"; SQLite calls it "prepared
statement reuse" when its host bindings (Python's sqlite3 module)
implement it.

## Goal

Make repeated `db.run(sql)` calls with the same **shape** but different
literals reuse the analyzed AST + resolved table + col_positions, so the
per-statement scaffolding cost drops from ~43 µs to ~5-10 µs.

## Non-goals

- **Statement caching across sessions / connections** — out of scope.
  Each `SessionContext` has its own cache. Cross-session sharing is
  Attack 2.7 (deferred).
- **Pre-compilation at parse time** — we do shape extraction AFTER
  parse + analyze on first call (lazy). A future optimization could
  do it during parse if profiling justifies it.
- **Rewriting subqueries / CTEs whose literals are inside their bodies** —
  the walker covers the same Expr variants `substitute_params` does today
  (BinaryOp, UnaryOp, IsNull, Between, In, Like, Function, Cast). DML
  source covers SELECT WHERE + INSERT VALUES + UPDATE SET + DELETE WHERE.
  Other variants (e.g. JSON_TABLE column exprs, CASE WHEN, COALESCE,
  CTE recursive bodies) are deferred to Attack 2.8 — they parse to plans
  that simply MISS the cache (correct, just slower).
- **Cache persistence across DB restart** — purely in-memory.
- **Disabling the cache via SQL hint** — out of scope (config flag is
  enough for now).
- **Replacing the existing manual `PreparedStatement` API** — coexists.
  Manual `prepare()` remains for users who want explicit control.

## Behavior

### Public API

No new public API on `axiomdb-embedded::Db`. Existing `Db::run(sql)` /
`Db::execute(sql)` / `Db::query(sql)` transparently get the cache.

Internal additions:

```rust
// crates/axiomdb-sql/src/expr.rs (or new module)
/// Rewrites every `Expr::Literal(v)` in `stmt` to `Expr::Param { idx }`,
/// pushing the original `v` onto `extracted` in walk order. The resulting
/// AST is shape-only.
pub fn extract_literals(stmt: &mut Stmt) -> Vec<Value>;

/// Computes a 64-bit hash from the shape-only AST. Two ASTs with
/// identical structure (modulo `Param` indices) hash to the same value.
pub fn shape_hash(stmt: &Stmt) -> u64;
```

```rust
// crates/axiomdb-sql/src/session.rs — new field + API on SessionContext
struct CachedPlan {
    analyzed: Stmt,       // shape-only (Literals already → Params)
    param_count: usize,   // number of literals extracted
    deps: PlanDeps,
}

impl SessionContext {
    /// Returns the cached plan for `shape_hash` if (a) it exists and
    /// (b) its `PlanDeps` are still valid against the live catalog.
    /// Otherwise evicts the stale entry and returns `None`.
    pub fn get_cached_plan(
        &mut self,
        shape_hash: u64,
        reader: &mut CatalogReader<'_>,
    ) -> Option<&CachedPlan>;

    /// Inserts a fresh plan. LRU evicts the oldest entry when the cache
    /// exceeds `STATEMENT_CACHE_MAX_ENTRIES` (default 256).
    pub fn cache_plan(&mut self, shape_hash: u64, plan: CachedPlan);

    /// Diagnostic/test accessor.
    pub fn statement_cache_count(&self) -> usize;

    /// Test-only: clear the cache.
    pub fn invalidate_statement_cache(&mut self);
}
```

The existing `substitute_params` in
[embedded/lib.rs:440](crates/axiomdb-embedded/src/lib.rs) is **promoted**
to a public function in `axiomdb-sql` so both `PreparedStatement` and the
new auto-cache reuse it.

### Semantics

`Db::run_inner` new control flow:

```
1. Parse the raw SQL (always — needed for shape extraction)
   ↓
2. Walk the AST: extract literals into `Vec<Value>`, replace with `Expr::Param`
   ↓
3. Compute shape_hash from the shape-only AST
   ↓
4. SessionContext::get_cached_plan(shape_hash):
     - HIT (deps valid): clone the cached analyzed Stmt
     - MISS: analyze the shape AST → cache it
   ↓
5. substitute_params(plan.analyzed.clone(), &extracted_literals)
   ↓
6. execute_with_ctx(substituted, ...)
```

**Invariants:**
- Parse output is functionally identical to today — same `Stmt` type, same
  Expr variants, just with `Param` instead of `Literal` for extracted
  positions.
- `extracted` literal count == `param_count` of the cached plan
  (enforced; mismatch = bug, panic / internal error).
- `PlanDeps::is_stale` is consulted on every cache hit; stale entries are
  evicted and treated as miss.
- Cache is per-`SessionContext` (per-connection); does not share across
  threads/connections.

**DDL statements bypass the cache entirely** (CreateTable, AlterTable,
DropTable, etc.) — they always parse + analyze + execute fresh, and
their analyzed Stmt is NEVER cached. Same rule as `PlanDeps`: DDL has
empty deps and is opaque to the cache.

### Error cases

| Input | Expected error | Message |
|-------|----------------|---------|
| `extracted.len() != cached.param_count` | `DbError::Internal` | `"statement cache: literal count mismatch (expected N, got M)"` |
| AST walker encounters unknown Expr that contains a literal | (silent skip; literal stays in-place — falls back to fresh compile on every call for that shape, no correctness issue) | — |
| Cache full (256 entries) and new shape arrives | LRU evict oldest, no error | — |

### Cross-path impact

| Path | Expected speedup |
|------|------------------|
| Embedded Rust (`Db::execute`, `Db::run`) | **5-10× on bench-style INSERT batch** (per-statement scaffolding amortized) |
| C FFI / Python via ctypes | same as embedded |
| MySQL wire — INSERT batch in one txn | 3-5× (TCP + serialization still ~30-50% of cost) |
| MySQL wire — autocommit INSERT | 2-3× (TCP roundtrip becomes the dominant slice) |
| Single-row SELECT WHERE id=? | 2-3× (catalog probe + dispatcher + result framing share the win) |
| Manual `PreparedStatement` (Phase 10.8) | **No change** (already skips parse + analyze) |

## Edge cases

Each becomes a test case in the plan:

- [ ] Repeated INSERT with different literals same shape: 1 cache entry
  after 100 calls.
- [ ] Two INSERTs with different column lists (`INSERT INTO t(a,b)` vs
  `INSERT INTO t(a,c)`) get distinct cache entries.
- [ ] INSERT into different tables get distinct cache entries.
- [ ] ALTER TABLE between two INSERTs of the same shape: second INSERT
  detects stale plan (via `PlanDeps`), recompiles, replaces cache entry.
- [ ] DROP TABLE between cache write and reuse: second call surfaces
  `TableNotFound` (PlanDeps detects table missing, recompile fails).
- [ ] SELECT WHERE id = literal: cache hit on second call with different
  literal.
- [ ] SELECT with literal in IN list (`WHERE id IN (1, 2, 3)`): all
  literals extracted; second call with same list-length and different
  values hits cache. Different list length is a different shape.
- [ ] SELECT with literal in BETWEEN / LIKE / function argument: extracted.
- [ ] DDL statement (CreateTable / AlterTable): NOT cached, no entry
  created.
- [ ] LRU eviction: 257 distinct shapes → cache size capped at 256, oldest
  evicted.
- [ ] Cache survives transaction COMMIT (does NOT get cleared by
  `invalidate_all` — same logic as `insert_col_positions`).
- [ ] Cache cleared on `SET search_path = ...` because resolution rules
  change — added to `invalidate_all` since search_path change makes
  cached plans potentially unresolvable.
- [ ] Multi-row VALUES `INSERT INTO t VALUES (1,'a'), (2,'b'), (3,'c')`:
  walker extracts 6 literals (2 columns × 3 rows); shape is fixed for
  N-row VALUES blocks; second call with same N hits cache.
- [ ] Empty `Vec<Value>` extracted (no literals in the SQL): shape hash
  works, cache hit on identical structure.

## Performance budget

| Metric | After 3.B | Target after Attack 2 |
|--------|----------:|----------------------:|
| `execute_with_ctx` per row (`--diagnose-insert`) | ~44 µs | **≤ 10 µs** |
| INSERT batched throughput (10K rows / 1 txn) | 21K rows/s | **≥ 100K rows/s** |
| Gap vs SQLite (insert_batch) | 49× | **≤ 10×** |
| point_lookup throughput | 8.9K | ≥ 30K (gap ≤ 8×) |
| count_star throughput | 4.3K | ≥ 20K (gap ≤ 16×) |
| group_by throughput | 1.1K | ≥ 1.3K (already close — small change) |
| Workspace test runtime | baseline | within +5% |
| Memory per session | baseline | +~256 × ~1KB plan = ~256 KB max |

## On-disk format

No on-disk format change. Cache is purely in-memory.

## Dependencies

- Depends on:
  - Attack 3.A (commit `50930d99`) — `schema_version` infrastructure.
  - Attack 3.B (commit `accd6827`) — version-stamped cache pattern.
  - Existing `PlanDeps` (`plan_deps.rs`) — used for staleness check.
  - Existing `substitute_params` (will be promoted from embedded crate).
- Blocks:
  - Cross-session plan cache (Attack 2.7, deferred) — would build on this.
  - Attack 4 (per-row engine work) — only meaningful after Attack 2
    closes the per-statement cost.

## Open questions

All resolved during brainstorm. Implementation may surface trade-offs
(specifically around the walker's Expr coverage); revise the spec before
proceeding if so.

## Done criteria

- [ ] `axiomdb_bench --compare --rows 10000` shows
      `insert_batch ≥ 100K rows/s` (5× over current 21K, ~10× gap vs SQLite).
- [ ] `axiomdb_bench --diagnose-insert --rows 10000` shows
      `execute_with_ctx per row ≤ 10 µs`.
- [ ] Each edge case from the list above has a corresponding integration
      test in `crates/axiomdb-sql/tests/integration_statement_cache.rs`.
- [ ] `cargo nextest run --workspace` (Lima) — clean.
- [ ] `cargo clippy --workspace -- -D warnings` (Lima) — clean.
- [ ] `cargo fmt --check` — clean.
- [ ] `tools/wire-test.py` — clean (no wire regressions).
- [ ] Existing Phase 10.8 `PreparedStatement` tests still pass — no
      regression on the manual prepared-statement API.
- [ ] `extract_literals` + `shape_hash` + `substitute_params` are
      documented (rustdoc on every public item).
- [ ] `STATEMENT_CACHE_MAX_ENTRIES` is a `const` in `session.rs`, easy
      to tune in a follow-up.

## References

External:
- SQLite prepared statement lifecycle: `research/sqlite/src/vdbeapi.c`
  (`sqlite3_step` / `sqlite3_reset` / `sqlite3_bind_*`).
- SQLite plan cache layout: `research/sqlite/src/prepare.c`
  (`sqlite3LockAndPrepare` + `sqlite3Prepare`).
- DuckDB auto-parameterization: search "rebind literals" in
  `research/duckdb/src/planner/`.
- PostgreSQL `plancache.c` dual-dependency design (already mirrored in
  our `plan_deps.rs`).

Internal:
- Brainstorm: this conversation, 2026-05-17.
- Attack 3.A spec: `spec-insert-setup-dedup-A.md`.
- Attack 3.B spec: `spec-insert-setup-dedup-B.md`.
- `Expr::Literal` definition: `crates/axiomdb-sql/src/expr.rs:25`.
- `Expr::Param` definition: `crates/axiomdb-sql/src/expr.rs:264`.
- Existing `substitute_params`: `crates/axiomdb-embedded/src/lib.rs:440`.
- Existing `count_params`: `crates/axiomdb-embedded/src/lib.rs:430`
  (placeholder — uses Debug format; will be replaced by a proper walker).
- `PlanDeps`: `crates/axiomdb-sql/src/plan_deps.rs`.
- Diagnostic harness: `benches/comparison/axiomdb_bench/src/main.rs`
  (`--diagnose-insert`, `--diagnose-insert-deep`, `--compare`).
- User-facing doc: `docs/perf-sqlite-gap.md` (to be updated when this
  attack closes).
