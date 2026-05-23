# Spec: carry the resolved table in the prepared statement (skip per-execute resolve)

Phase: perf-sqlite-gap — insert/execute hot-path
Task: Carry the resolved table in `PreparedInsertPlan` so a prepared INSERT execute skips
`resolve_table_cached` when no DDL has run — the "Attack 2 carry-resolved-tables" finally
done, now profiler-confirmed as the #1 removable bulk-insert cost.
Status: **IMPLEMENTED (2026-05-23)** — landed; **measured +6.4% on macOS native** (the official
bench platform; +3.0% on Lima), vs the estimated ≥8% (measure-first — see "Measured RESULT").
Real, clean (macOS A/B has zero overlap), no-regression; all correctness gates green.
Validator = 2-scalar (catalog_epoch + write_commit_seq).

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

## Measured RESULT (2026-05-23, order-balanced A/B via `AXIOMDB_NO_CARRY_RESOLVE`)

**macOS native (official bench platform): +6.4% median / +7.0% mean** — insert_batch 50K,
carry-on 495,787 vs carry-off 466,008 ops/s; the 6 carry-on and 6 carry-off runs **do not
overlap** (clean, not noise). **Lima VM: +3.0%** (100K; median 528,621 vs 512,992) — smaller
because virtio I/O fixed cost dilutes the per-row CPU saving. In `--compare` (vs SQLite via
rusqlite) this moves insert_batch from ~2.3× to **2.16× slower** than SQLite. Knob isolates
*this* lever (unlike `AXIOMDB_NO_PREPARED_FAST`, which disables the whole fast path).

The ≥8% estimate was **optimistic** — corrected by measure-first:
- The A-baseline (carry off) does NOT pay a full re-resolve: `resolve_insert_target`'s
  `get_table_arc` fast path (clustered batch active) already skips the catalog probe. The carry
  only removes the per-row **String allocs + SipHash** of that lookup → ~3%, matching the
  profile's ~3.8% SipHash.
- The "~31% allocator" is dominated by OTHER per-row allocs (encode_row Vec, DataType::clone,
  materialize_insert_row, the WAL record), not the resolve. Those are the next levers.

**Decision: LAND.** +6.4% (macOS) is real, clean (zero-overlap A/B), profiler-grounded, fully
correctness-tested, zero-regression (fallback bit-identical; reads structurally untouched —
full_scan/select_where/range_scan still beat SQLite). It stacks toward SQLite parity (the parity
game is many small attacks). Not measured-out like hole-skip/minimal-WAL (those were ~0%).

## Dependencies

- Depends on: `SessionContext::catalog_epoch()` + `txn.write_commit_seq()` +
  `is_table_epoch_current` (the proven validators); `PreparedInsertPlan` (exists);
  `resolve_table_cached` (the fallback); `Arc<ResolvedTable>`.
- Blocks: nothing. Independent of the single-fsync commit (landed).

## Open questions — RESOLVED

- [x] Validator granularity → **2-scalar compare** `(catalog_epoch, write_commit_seq)`, fully
      alloc/hash-free. `write_commit_seq` bumps on every WRITE commit (DML or DDL), NOT read-only
      (txn_begin_commit.rs:163,228) — so it is STABLE within one `BEGIN..COMMIT` (the bulk-insert
      shape → full win) and conservatively re-resolves across data-commit batches (safe, no win
      there; v1 accepts this).
- [x] Build eagerly at `Db::prepare` (storage+txn available; `execute` is `&self`). Done.
- [x] `execute_prepared_insert` now takes `cached_resolved: Option<Arc<ResolvedTable>>` and skips
      its internal `resolve_insert_target` when `Some`.
- [x] **Gate correction (found during impl):** a clustered PK *does* create an `IndexDef` row
      (ddl_create_table.rs:621), so the planned `indexes.is_empty()` gate would have excluded EVERY
      PK-only clustered table (the whole bench). Verified the clustered insert reads only the PK
      index's *columns* (clustered_table.rs:131), never its root → corrected gate to
      `def.is_clustered() && indexes.iter().all(|i| i.is_primary)` (PK fine to cache; only SECONDARY
      roots mutate). bench_users now qualifies.

## Done criteria

- [x] `PreparedInsertPlan` carries `resolved: Option<Arc<ResolvedTable>>` + the two stamps
      (`catalog_epoch_at_build`, `write_seq_at_build`) + `resolved_if_current`.
- [x] Per execute on the current path, the per-row resolve (`resolve_insert_target`) is **not
      called** — asserted by `fast_execute_skips_per_row_resolve_when_cacheable` (counter delta 0
      across the batch, with a generic-insert sanity that the counter is live).
- [x] DDL between executes re-resolves correctly — `ddl_on_target_between_executes_uses_new_schema`
      (ALTER target → leaves the fast path, new schema) + the pre-existing
      `ddl_between_executes_falls_back_to_generic`.
- [~] Cross-session DDL: covered by the validator (`write_commit_seq` is global, bumps on any
      session's write commit). Embedded is single-session so it is exercised via the same-session
      data-commit invalidation; the wire/multi-session path does not use this embedded fast path yet.
- [~] No regression: reads healthy (full_scan/select_where/range_scan beat SQLite; point_lookup
      ~1.2×; INSERT-only change). bulk insert A/B = **+6.4% macOS / +3.0% Lima** (vs the ≥8%
      estimate — measure-first correction; real + no-regression → LANDED).
- [x] `cargo nextest run --workspace` green; clippy + fmt clean (my files).
- [x] rustdoc on the new fields/method + the gate rationale.

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
