# Spec: skip the per-row statement-trigger table lookup on the prepared INSERT fast path

Phase: perf-sqlite-gap — insert/execute hot-path
Task: pass the already-resolved `ResolvedTable` to `run_statement_triggers_for_result` so a
prepared INSERT execute does not re-`get_table` (String key + SipHash) per row just to read the
table's trigger list.
Status: **IMPLEMENTED (2026-05-25)** — measured **+8% on macOS native** (insert_batch A/B vs the
carry-only baseline: 537,633 vs 495,179 ops/s median, +8.6% median / +7.4% mean, two converging
runs). Bigger than the ~2.3% self-time estimate because removing `get_table` also cuts 2 String
allocs/row charged to the ~36% allocator. Re-profile confirms: `run_statement_triggers_for_result`
fell 2.54%→0.86% and `get_table`/SipHash vanished from the trigger path. Correctness gates green
(trigger fires via prepared fast path; DDL-add-trigger re-resolves). Workspace 4747 pass; clippy+fmt.

## Context

After carry-resolved-table landed (+6.4% macOS), a fresh `perf` re-profile of `insert_batch`
(Lima, symbols, 9570 samples) shows the prepared INSERT still pays a per-row table lookup that
is NOT the resolve we already cut: `execute_prepared_insert` calls
`run_statement_triggers_for_result` (trigger.rs:116) once per row, which does
`ctx.get_table(database, schema, name)` — i.e. `SessionContext::key` (a `db\0schema\0table`
String + a default-**SipHash** HashMap probe) plus a `search_path[i].clone()` String when the
schema is implicit — **only to read `rt.def.triggers`**. The profile attributes
`run_statement_triggers_for_result → get_table → key` ≈ **2.3%** of `insert_batch` (and is the
bulk of the remaining ~3% SipHash). But `execute_prepared_insert` already holds the resolved
`Arc<ResolvedTable>` (from the carry plan or `resolve_insert_target`) — the trigger list is right
there in `resolved.def.triggers`.

## Goal

Pass the already-known `&ResolvedTable` into `run_statement_triggers_for_result` so it reads
`resolved.def.triggers` directly and skips the per-row `get_table` (String key + SipHash +
`search_path` clone). Same pattern as carry-resolved-table.

## Non-goals

- Not changing trigger semantics or execution — only how the trigger list is obtained.
- Not touching the generic dispatch path's behavior: the 3 `exec_dispatch.rs` call sites pass
  `None` and keep today's `get_table` lookup (bit-identical). (A later pass may thread their
  resolved table too, but the bench's prepared path is the target here.)
- Not the staging alloc churn (encode_row/coerce/materialize) — that is Fase 1 item #2, a
  separate spec.

## Behavior

### Public/internal API

```rust
// trigger.rs — add a `resolved` hint parameter:
fn run_statement_triggers_for_result(
    event: TriggerEvent,
    table: &TableRef,
    resolved: Option<&ResolvedTable>,   // NEW: when Some, read .def.triggers directly
    result: QueryResult,
    exec_ctx: &ExecutionContext,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError>;
```

### Semantics

- `resolved = Some(rt)`: use `rt.def.triggers` / `rt.def.table_name` directly — **no `get_table`,
  no `resolve_table_cached` fallback**. Empty trigger list ⇒ early `Ok(result)` (unchanged).
- `resolved = None`: today's behavior exactly — `ctx.get_table` probe, then the
  `resolve_table_cached` miss-fallback.
- `execute_prepared_insert`: `Arc::clone(&resolved)` (a cheap refcount bump, not an alloc/hash)
  before `resolved` is moved into `execute_insert_ctx_with_resolved`, then pass
  `Some(&resolved_arc)`.

- Precondition: `resolved` (when Some) is the table this INSERT targeted, current as of this
  statement (it is — the carry plan is DDL-invalidated via `catalog_epoch`+`write_commit_seq`;
  on a miss it came from `resolve_insert_target`, freshly resolved).
- Postcondition: the set of fired statement triggers and the returned `QueryResult` are identical
  to the `None` path.
- Invariant: trigger definitions only change via DDL, which bumps `catalog_epoch` (invalidating
  the carry plan → re-resolve) — so `resolved.def.triggers` is never stale.

## Edge cases

- [ ] Table with an INSERT statement trigger → trigger fires (via prepared execute).
- [ ] Table with only UPDATE/DELETE triggers → no INSERT trigger fires (event filter unchanged).
- [ ] Table with no triggers → early return, no lookup (the common bench path).
- [ ] DDL adds a trigger between prepared executes → `catalog_epoch` bumps → carry invalidated →
      `resolved` comes from `resolve_insert_target` (fresh) → the new trigger fires.
- [ ] DROP TRIGGER between executes → same invalidation → trigger no longer fires.
- [ ] Generic dispatch path (`exec_dispatch.rs`, `resolved = None`) → identical to today.

## Performance budget

| Metric | Target |
|---|---|
| insert_batch (macOS native, A/B) | + >0% (eliminate per-row get_table String+SipHash+search_path clone) |
| per-row `get_table` / SipHash on the prepared path | 0 (perf re-profile: `run_statement_triggers → get_table` gone) |
| reads / generic INSERT / triggered tables | within ±2% (None path unchanged; triggers identical) |

Reference: re-profile attributes ~2.3% to this lookup; realistic win ~1-2% on insert_batch.

## Dependencies

- Depends on: carry-resolved-table (landed) — `execute_prepared_insert` already holds `resolved`.
- Blocks: nothing.

## Open questions

- None. (`get_table` returns `&ResolvedTable`; `resolved.def.triggers` is the same data the probe
  reads; the Arc clone is a refcount bump.)

## Done criteria

- [ ] `run_statement_triggers_for_result` takes `resolved: Option<&ResolvedTable>`; `Some` skips
      `get_table`, `None` is bit-identical to today.
- [ ] `execute_prepared_insert` passes the resolved table (no per-row `get_table`).
- [ ] Trigger correctness tests (embedded): an INSERT statement trigger fires via a prepared
      execute; a DDL that adds a trigger between executes fires the new trigger; a no-trigger
      table runs none.
- [ ] `cargo nextest run --workspace` green; clippy + fmt clean.
- [ ] A/B (macOS native): insert_batch ≥ baseline (no regression), measurable small gain; reads
      unaffected.
- [ ] rustdoc on the new parameter.

## References

- `crates/axiomdb-sql/src/executor/trigger.rs:116` — `run_statement_triggers_for_result`.
- `crates/axiomdb-sql/src/executor/prepared_insert.rs` — `execute_prepared_insert` (holds `resolved`).
- `crates/axiomdb-sql/src/executor/exec_dispatch.rs` — the 3 generic call sites (pass `None`).
- PostgreSQL analog: `plancache.c` revalidates cached plan metadata against relation
  invalidation rather than re-looking-up per execution — same "carry resolved metadata, validate
  on DDL" idea.
