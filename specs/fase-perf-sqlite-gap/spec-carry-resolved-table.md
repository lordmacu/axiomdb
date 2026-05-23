# Spec: carry the resolved table in the prepared statement (skip per-execute resolve)

Phase: perf-sqlite-gap — insert/execute hot-path
Task: Carry the resolved table in `PreparedInsertPlan` so a prepared INSERT execute skips
`resolve_table_cached` when no DDL has run — the "Attack 2 carry-resolved-tables" finally
done, now profiler-confirmed as the #1 removable bulk-insert cost.
Status: approved (2026-05-23) — open questions delegated to the plan (validator = 2-scalar).

## Context

A clean `perf` profile (2026-05-23, Lima, real symbols) of `--scenario insert_batch` showed
the bulk insert is **~31% in the allocator (malloc/free/memcpy) + ~3.8% SipHash + ~6.7%
kernel** — costs the per-phase instrumented timers always hid (they smear across phases).
ROOT CAUSE: `insert_batch_prepared` issues one `stmt.execute` per row, and **every execute
re-runs `resolve_table_cached`** (`crates/axiomdb-sql/src/executor/shared.rs`) for the SAME
table: `effective_database_for_ref` → String, `search_path[i].clone()` → String,
`try_cached_with_version` → builds a `db+schema+table` String key + a HashMap lookup with the
default **SipHash** + a version check. That is **~3 String allocs + 1 SipHash lookup per row**.
SQLite resolves the table once into the compiled VDBE. `PreparedInsertPlan` was already
*designed* to carry this (its doc: "Step 5 adds: table_id, is_clustered, col_positions, …").

## Goal

A prepared INSERT execute reuses a cached `Arc<ResolvedTable>` from its `PreparedInsertPlan`
and **does not call `resolve_table_cached`** while no DDL has run — eliminating the per-row
resolve String allocs + SipHash.

## Non-goals

- **Prepared SELECT / point_lookup** — `resolve_table_cached` is shared, but this spec scopes
  to the INSERT fast path first (where the win was measured). SELECT is a follow-on.
- **Multi-table / multi-statement** prepared stmts — single-table INSERT VALUES only (the
  existing `insert_is_fast_eligible` gate).
- **The other allocator targets** (`encode_row`→Vec/row, `DataType::clone`,
  `materialize_insert_row`) — separate follow-on specs.
- Changing the durability / commit path.

## Behavior

### Public API (internal)

```rust
// crates/axiomdb-sql/src/executor/prepared_insert.rs
pub struct PreparedInsertPlan {
    catalog_epoch_at_build: u64,
    write_seq_at_build: u64,            // NEW — cross-session DDL validator (txn.write_commit_seq)
    resolved: Arc<ResolvedTable>,       // NEW — the table resolved once at build
    // (later: col_positions, primary_idx, value_template — out of scope here)
}

impl PreparedInsertPlan {
    /// `Some(resolved)` when the cached table is still valid (no DDL since build);
    /// `None` ⇒ caller MUST re-resolve via `resolve_table_cached` (and may rebuild the plan).
    pub fn resolved_if_current(
        &self, ctx: &SessionContext, txn: &TxnManager,
    ) -> Option<&Arc<ResolvedTable>>;
}
```

### Semantics

- **Build (at `Db::prepare`, or lazily on first execute):** resolve the table once via
  `resolve_table_cached`; store `resolved`, `catalog_epoch_at_build = ctx.catalog_epoch()`,
  `write_seq_at_build = txn.write_commit_seq()`.
- **Validation (per execute, the hot path):**
  `catalog_epoch_at_build == ctx.catalog_epoch() && write_seq_at_build == txn.write_commit_seq()`.
  Both are cheap scalar reads — **no String, no HashMap, no SipHash.** `catalog_epoch` catches
  this session's DDL (bumped only by `invalidate_all`); `write_commit_seq` catches another
  session's committed DDL (the existing cross-session validator, commit `61ae9014`).
- **Current ⇒** reuse `resolved`, skip `resolve_table_cached` entirely → into the fast insert.
- **Stale (either differs) ⇒** fall back to today's path (`resolve_table_cached`), which is
  unchanged + always correct; the plan is rebuilt with the fresh resolve + new stamps.
- **Invariant:** the engine NEVER inserts against a stale `ResolvedTable` — any DDL (CREATE/
  ALTER/DROP/TRUNCATE, this session or another committed) bumps `catalog_epoch` or
  `write_commit_seq`, forcing the fallback re-resolve. (Conservative: any DDL invalidates the
  cached resolve, even for an unrelated table — correct, and DDL is rare vs inserts.)

### Error cases

| Case | Behavior |
|------|----------|
| Table dropped between executes | epoch/seq bump → fallback `resolve_table_cached` → `TableNotFound` (same as today) |
| Table altered (column added) between executes | epoch/seq bump → re-resolve → new schema used |
| First execute (no cached resolve yet) | build the plan (resolve once), then proceed |

## Edge cases

- [ ] DDL (CREATE/ALTER/DROP/TRUNCATE) between two executes on a live prepared stmt → re-resolve.
- [ ] Cross-session DDL (another connection alters the table + commits) → `write_commit_seq`
      bump → re-resolve.
- [ ] Explicit txn (`BEGIN…COMMIT`) vs autocommit — validation holds in both (epoch + seq are
      session/global, read the same way).
- [ ] Fast path (`execute_prepared_insert`) AND the generic dispatch fallback — both correct.
- [ ] Table dropped then recreated with same name (new table_id) → seq/epoch bump → re-resolve.
- [ ] Plan eligible at prepare but the table becomes ineligible after DDL → fallback path.
- [ ] `resolve_table_cached` itself unchanged (the fallback) — no behavior change on epoch-miss.

## Performance budget

| Metric | Target |
|---|---|
| bulk insert (insert_batch, Lima) | **≥ +8%** (eliminate per-row resolve String allocs + SipHash) |
| per-row resolve String allocs / SipHash | **0** on the epoch-current path (perf profile) |
| point_lookup / select / reads | within ±2% (no regression; SELECT path untouched) |
| DDL-heavy / epoch-miss workloads | within ±2% (fallback == today) |

Reference: profile showed ~3.8% SipHash + a chunk of the ~31% allocator in the per-row resolve.

## Dependencies

- Depends on: `SessionContext::catalog_epoch()` + `txn.write_commit_seq()` +
  `is_table_epoch_current` (the proven validators); `PreparedInsertPlan` (exists);
  `resolve_table_cached` (the fallback); `Arc<ResolvedTable>`.
- Blocks: nothing. Independent of the single-fsync commit (landed).

## Open questions (resolve before approved)

- [ ] Validator granularity: the cheap **2-scalar compare** `(catalog_epoch, write_commit_seq)`
      (no HashMap, conservative — any DDL re-resolves) vs `is_table_epoch_current(table_id,
      write_seq)` (per-table, but a `HashMap<u32,_>` SipHash lookup on the u32). → recommend the
      2-scalar compare (fully alloc/hash-free); confirm `write_commit_seq` bumps on DDL only
      (not per data-commit) so it's stable during a bulk insert.
- [ ] Build eagerly at `Db::prepare` (resolve needs storage+txn — available there) vs lazily on
      first execute. → recommend eager (simpler; `execute` is `&self`).
- [ ] Does `execute_prepared_insert` currently re-resolve, or take the resolved table as an
      arg? (wire the cached `resolved` into it.)

## Done criteria

- [ ] `PreparedInsertPlan` carries `resolved: Arc<ResolvedTable>` + the two stamps.
- [ ] Per execute on the epoch-current path, `resolve_table_cached` is **not called** (assert
      via a counter or the perf profile: no resolve String allocs, no SipHash).
- [ ] DDL between executes on a live prepared stmt re-resolves correctly — test: prepare INSERT,
      execute, `ALTER TABLE add column` (and a separate `DROP`), execute → new schema / correct
      error.
- [ ] Cross-session DDL test (second connection alters+commits) → re-resolve.
- [ ] No regression: point_lookup / select / reads within ±2%; bulk insert A/B (Lima) ≥ +8%.
- [ ] `cargo nextest run --workspace` green; clippy + fmt clean.
- [ ] rustdoc on the new fields/method.

## References

- `crates/axiomdb-sql/src/executor/shared.rs` — `resolve_table_cached` (the per-row cost).
- `crates/axiomdb-sql/src/executor/prepared_insert.rs` — `PreparedInsertPlan` + `is_current`.
- `crates/axiomdb-sql/src/session.rs:1538` `is_table_epoch_current`, `:1546` `catalog_epoch`.
- `crates/axiomdb-embedded/src/lib.rs:594` `PreparedStatement`.
- PostgreSQL `src/backend/utils/cache/plancache.c` — cached plans revalidated against relation
  invalidation messages (the same invalidate-on-DDL idea; `catalog_epoch`/`write_commit_seq`
  are our analog).
- Memory: "Attack 2 carry-resolved-tables" (reverted TWICE on cache invalidation →
  `catalog_epoch` is the safe validator); the 2026-05-23 perf finding in
  `memory/project_single_fsync_commit.md`.

## Recommended effort for /plan-task

**high** — small surface (one struct + the execute branch + the fallback), but the cache-
invalidation correctness is the crux (reverted twice before); the plan must gate every step on
the DDL-between-executes + cross-session tests, and prove the epoch-current path skips the
resolve. Implementation effort: **high** (correctness-sensitive, not max — no on-disk/recovery
change).
