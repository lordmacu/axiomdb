# Spec: specialized prepared-INSERT execute (VDBE compile-once model)

Phase: perf-sqlite-gap — Parity lever #1 (executor scaffolding)
Task: a fast execute path for an explicit `PreparedStatement` whose statement is an INSERT
Status: approved

## Context

`insert_batch` is ~2.6× SQLite. Profiling (`--diagnose-prepared-insert`, 50K) shows the
per-row cost splits ~half executor (`execute_with_ctx` ≈ 2.78µs/row) and ~half commit
(B-tree apply 1.38µs + root_persist 0.49 + wal 0.24). Of the executor half, the dominant
removable cost is the **per-statement scaffolding paid on every row**: `ExecutionContext::new`,
the `Stmt` matches in `execute_with_ctx_locked`, the `dispatch_ctx` routing, the per-statement
resolve probe, and the `execute_insert_ctx_with_resolved` setup. SQLite pays this **once** at
`prepare` (the VDBE program is compiled once; per row it runs only `OP_MakeRecord` + `OP_Insert`,
`research/sqlite/src/vdbe.c`). This task ports that model to AxiomDB's explicit
`PreparedStatement`: resolve + plan once, per row bind + codec + enqueue.

The 6e split already exists (`resolve_insert_target` + `execute_insert_ctx_with_resolved`,
insert_heap_ctx.rs) and its doc already anticipates "the prepared INSERT fast path" — this task
builds that path.

## Goal

A `PreparedStatement` over an eligible INSERT executes per-row WITHOUT re-running the generic
per-statement dispatch scaffolding, producing byte-identical results + identical transactional
semantics to the generic path.

## Non-goals

- **Auto-prepare / SQL-text or shape-hash plan cache** — banned (feedback_writes_no_cache;
  the existing `get_cached_plan`/`statement_cache` shape-hash path is NOT used here). This is a
  faster executor for an **explicit** `PreparedStatement` only.
- **The B-tree apply lever** (tree_ms 1.38µs) — the next parity sprint task; this task does not
  touch the commit/apply path.
- **The codec lever** (prepare_row 0.59µs) — separate.
- **Server (`axiomdb-network`) prepared statements** — this task targets the embedded
  `PreparedStatement`; the server path can reuse the axiomdb-sql primitives in a follow-up.
- **Making redo/frame-log faster** — wal is only 0.24µs/row, not a parity lever.
- **Bench fairness fix** — the `--compare insert_batch` currently uses raw SQL
  (`insert_batch_pure`); switching it to the prepared path to fairly reflect this lever is a
  separate, optional bench change (see Performance budget).

## Behavior

### Public API

```rust
// axiomdb-sql: the cached, DDL-revalidated INSERT plan.
pub struct PreparedInsertPlan {
    table_id: u32,
    is_clustered: bool,
    // resolved column targets + codec layout, computed once
    col_positions: Arc<[usize]>,
    primary_idx: Arc<PrimaryKeyLayout>, // or the existing primary-idx type
    value_template: ValueTemplate,      // per-column: literal | param slot | (deferred) expr
    catalog_epoch_at_build: u64,        // revalidation token (DDL-only)
    // ... whatever resolve_insert_target/execute_insert_ctx_with_resolved need, minus the root
}

// Try to build the plan from an analyzed statement. None ⇒ ineligible ⇒ caller uses the
// generic path. Eligibility: single-table INSERT ... VALUES, no ON CONFLICT/REPLACE/ODKU,
// no RETURNING, no triggers, no SELECT source, simple (literal/param) value exprs.
pub fn try_prepare_insert_plan(
    analyzed: &Stmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    ctx: &mut SessionContext,
) -> Option<PreparedInsertPlan>;

// Execute one INSERT row-set against a cached plan. Returns Ok(Some(result)) on the fast
// path; Ok(None) when the plan is stale/ineligible at execute time (epoch changed, etc.) and
// the caller MUST fall back to the generic execute. Errors propagate (statement-atomic).
pub fn execute_prepared_insert(
    plan: &PreparedInsertPlan,
    params: &[Value],
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    bloom: &BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<Option<QueryResult>, DbError>;
```

```rust
// axiomdb-embedded: PreparedStatement caches the plan (built at prepare or first execute).
pub struct PreparedStatement {
    analyzed: AnalyzedStmt,
    param_count: usize,
    insert_plan: Option<PreparedInsertPlan>, // Some ⇒ try the fast path
}
// PreparedStatement::execute(db, params): if insert_plan present →
//   execute_prepared_insert(...)?  → if Some(r) return r; if None fall back.
//   else → existing substitute_params + execute_with_ctx path.
```

### Semantics

- **Precondition:** `params.len() == plan.param_count`; the session is in the same txn context
  the generic path expects (autocommit OR explicit txn — both supported).
- **Revalidation:** the fast path runs only if `ctx.is_table_epoch_current(plan.table_id)` AND
  `plan.catalog_epoch_at_build == ctx.catalog_epoch()` (DDL-only token, session.rs:1600 — NOT
  `schema_version`, which bumps on every clustered root split). On mismatch → return `Ok(None)`
  (caller falls back to the generic path, which re-resolves; the plan may be rebuilt lazily).
- **Per-row work (fast path):** bind params into the value template → eval only non-literal
  exprs → `prepare_row` codec → enqueue into the clustered/heap staging batch via the SAME
  `enqueue_clustered_insert_ctx` / staging machinery the generic path uses. The B-tree root is
  resolved at flush/COMMIT, not here (staging never touches the tree).
- **Postcondition:** the resulting staged rows + `QueryResult` (affected count) are
  **byte-identical** to what the generic `execute_with_ctx` would produce for the same INSERT
  + params, and the COMMIT applies them identically.
- **Atomicity invariant (the 6e burn — MUST hold):** a fast-path execute is statement-atomic
  exactly like the generic path — on any error it leaves no partial staged rows / deferred-FK /
  notify residue. Achieved by reusing the generic per-statement savepoint + `on_error` +
  partial-staged-row cleanup wrapper around the fast inner loop (do NOT hand-roll a divergent
  cleanup).
- **Invariant:** result equivalence holds across DDL — a DDL between executes flips the epoch →
  fallback → correctness preserved.

### Error cases

| Input / situation | Behavior |
|---|---|
| `params.len() != param_count` | `Err(DbError)` (same message as the generic prepared path) |
| epoch changed (DDL since build) | `Ok(None)` → caller falls back to generic |
| statement turns out ineligible at build | `try_prepare_insert_plan` returns `None` (generic path) |
| PK duplicate (in-batch or vs table) | same error + same statement-atomic rollback as generic |
| eval/codec/constraint error mid-row | statement-atomic: no partial staged rows (wrapper cleanup) |

## Edge cases

- [ ] Eligible single-row `INSERT ... VALUES (?, ?, ...)` (the hot case) → fast path, identical result.
- [ ] Literal values mixed with params → fast path.
- [ ] Non-literal value expr (`?+1`, `NOW()`, subquery) → either eval-in-template or ineligible → generic.
- [ ] Multi-row `VALUES` in one execute → fast path if all rows simple, else generic.
- [ ] `ON CONFLICT` / `REPLACE` / ODKU / `RETURNING` / triggers / `INSERT...SELECT` → ineligible → generic.
- [ ] DDL between two executes of the same prepared stmt → epoch miss → fallback → correct.
- [ ] Autocommit execute (no open txn) AND inside explicit `BEGIN..COMMIT` → both correct.
- [ ] Error mid-statement (PK dup, NOT NULL, CHECK, FK) → statement-atomic, no partial residue.
- [ ] Default columns / AUTO_INCREMENT / generated columns → same values as generic (or ineligible).
- [ ] NULL / non-ASCII / max-size values → identical codec output.

## Performance budget

| Operation | Target |
|---|---|
| prepared INSERT execute (per row, eligible) | drop the per-statement scaffolding; `--diagnose-prepared-insert` execute_with_ctx portion measurably lower |
| insert_batch (prepared) vs SQLite | ~2.6× → ~2.1–2.3× (NOT parity alone — pairs with the B-tree apply lever) |
| generic / ineligible path | unchanged (no regression) |

Measure (macOS native): `target/release/axiomdb_bench --diagnose-prepared-insert --scenario
insert_batch --rows 50000` (clean build, no bench-timings, for the real number) before/after.
NOTE: the `--compare insert_batch` scenario currently uses **raw SQL** (`insert_batch_pure`),
so it will NOT reflect this lever — to show it in `--compare`, switch that scenario to the
prepared path (separate, optional bench change; reflects realistic prepared usage + matches
SQLite's `prepare_cached`).

## Dependencies

- Depends on: the 6e split (`resolve_insert_target`, `execute_insert_ctx_with_resolved`),
  `catalog_epoch` + `is_table_epoch_current` (session.rs), the staging machinery
  (`enqueue_clustered_insert_ctx`), the generic statement-atomicity wrapper.
- Blocks: full insert_batch parity (pairs with the B-tree apply lever, the next sprint task).

## Open questions

- [x] Revalidation token → `catalog_epoch` (DDL-only), NOT `schema_version`. RESOLVED.
- [x] Do not reuse the appender (owns its own txn; autocommit-only) — build the path so it
  respects the session txn. RESOLVED (brainstorm).
- [ ] **(plan-time)** Where the plan is built: eagerly in `Db::prepare` vs lazily on first
  `execute` (lazy avoids resolving for never-executed prepares + sidesteps prepare-time txn
  context). Lean lazy; confirm in `/plan-task`.
- [ ] **(plan-time)** Exact reuse vs. light-fork of the statement-atomicity wrapper so the fast
  loop is wrapped without duplicating cleanup logic — identify the precise seam in
  `execute_with_ctx_locked` / `dispatch_ctx`.
- [ ] **(plan-time)** Multi-row VALUES eligibility granularity (whole-stmt vs per-row).

## Done criteria

- [ ] `PreparedInsertPlan` + `try_prepare_insert_plan` + `execute_prepared_insert` (axiomdb-sql);
  `PreparedStatement.insert_plan` + routing (axiomdb-embedded).
- [ ] Eligible prepared INSERT takes the fast path; ineligible / epoch-miss falls back to generic.
- [ ] Result + transactional semantics byte-identical to the generic path (differential test:
  same INSERTs via prepared-fast vs generic produce identical table state + affected counts).
- [ ] Statement atomicity: error mid-statement leaves no partial staged rows (test for PK-dup,
  NOT NULL, CHECK — in autocommit AND in an explicit txn).
- [ ] DDL-between-executes → fallback → correct (test).
- [ ] `--diagnose-prepared-insert insert_batch 50000` shows a measurable per-row execute drop;
  no regression on the generic path or reads.
- [ ] `cargo nextest -p axiomdb-sql` + `-p axiomdb-embedded` green (Lima); clippy --workspace +
  fmt clean; full workspace nextest green at close.
- [ ] rustdoc on all new public items.

## References

- `crates/axiomdb-sql/src/executor/insert_heap_ctx.rs` (resolve_insert_target:19,
  execute_insert_ctx_with_resolved:78 — the 6e split + "prepared INSERT fast path" note)
- `crates/axiomdb-sql/src/executor/{exec_with_ctx.rs:40, exec_dispatch.rs:30, insert_clustered_ctx.rs:345}`
- `crates/axiomdb-sql/src/session.rs` (catalog_epoch:1600, is_table_epoch_current:1498,
  invalidate_table_epoch_for_id:1548; do NOT use get_cached_plan:1685 — the banned shape-hash cache)
- `crates/axiomdb-embedded/src/lib.rs` (PreparedStatement + execute:670)
- `research/sqlite/src/vdbe.c` (OP_MakeRecord / OP_Insert / sqlite3VdbeExec — compile-once-execute-many)
- `memory/project_insert_perf.md` (the 6e GENERIC-DISPATCH decision + why schema_version is not a token)
