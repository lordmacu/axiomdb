# Spec + Plan: subphase 6e — specialized prepared-INSERT execute (dispatch bypass)

Phase: redo-recovery (project B) — subphase 6e (SQLite write-parity, lever 1 of 3)
Status: in-progress
Effort: **max** (executor hot path, shared crate, correctness-critical with fallback)

## ⚠️ MEASURED REFRAME (2026-05-22) — implemented as a generic-dispatch optimization, NOT a prepared bypass

The original plan below (cache `Arc<ResolvedTable>` + `schema_version` on
`PreparedStatement`, bypass dispatch) was **superseded by measurement**. Two findings:

1. **`schema_version` is NOT a valid revalidation token for inserts** — `update_table_root`
   bumps it on every B-tree root split during the write, so a cached `schema_version` would
   mismatch after the first split. (SQLite's schema cookie only moves on DDL; AxiomDB's moves
   on data writes too.)
2. **The bypass is the wrong trade.** `--diagnose-insert-deep` (50K, low-noise) located the
   `~0.85µs/row` "generic dispatch remainder". The dispatch *structure* (matches) is cheap;
   the per-statement `txn.savepoint` is also cheap (3 length reads, no WAL). The remainder is
   dominated by **per-INSERT catalog round-trips**: `run_statement_triggers_for_result`
   (`database.to_string()` + `get_table` + clone of table_name + trigger list) and
   `invalidate_table_epoch_for_ref` (`database.to_string()` + key build + cache probe). A
   prepared bypass would also have to re-implement the per-statement savepoint + `on_error`
   handling (and `enqueue` is NOT fully atomic on non-PK-dup errors), risking statement
   atomicity — high risk for marginal extra capture.

**Implemented (Direction B):** optimize the **generic** INSERT dispatch (`dispatch_ctx`),
benefiting prepared **and** wire **and** raw-SQL paths, with zero bypass:
- `dispatch_ctx` resolves the target ONCE (`resolve_insert_target` +
  `execute_insert_ctx_with_resolved`, the 6e-1 split) and invalidates the epoch by id
  (`SessionContext::invalidate_table_epoch_for_id`) — no second name resolution.
- `run_statement_triggers_for_result`: `database` as `&str` (no alloc), and clones the
  table_name + trigger list **only when the table has triggers**. Keeps the `get_table`/
  resolve probe, which repopulates the resolve cache after an autocommit write that evicted it
  (guarded by `integration_resolve_table_cache::cache_serves_select_after_insert`).

**Measured:** dispatch remainder `0.85 → 0.56µs/row`, `execute_with_ctx 1.67 → 1.36µs/row`
(−0.3µs) with the full skip; the safe in-place version recovers most of it. insert_batch
`~2.8–3.2× → ~2.6–2.7×` vs SQLite. **Parity still needs lever 2 (commit/WAL: page-cache +
WAL-file reuse, ~1.2µs/row, structural) and lever 3 (codec).** The original prepared-bypass
plan is kept below as historical context.

---

## Goal

Cut the **generic dispatch (~0.9µs/row, ~30% of the ~2.9µs insert)** — the per-statement
machinery (`execute_with_ctx_locked` → `dispatch_ctx` → `execute_insert_ctx` wrapping +
per-statement resolve/setup) that SQLite's compiled VDBE avoids (it opens the table cursor
ONCE at prepare, then per row does only `OP_MakeRecord` + `OP_Insert`). Target: insert_batch
~2.9µs/row → ~2.0µs (~2.8× → ~2×). This is lever 1 of 3 toward SQLite write parity
(roadmap in memory `project_insert_perf.md`); levers 2 (commit page-cache / WAL reuse) and 3
(phases/codec) follow.

## Approach (port SQLite's "open cursor once at prepare")

`PreparedStatement` caches the resolved insert plan at `prepare()`; eligible `execute()` calls
the per-row staging path directly, bypassing the generic dispatch. **A correct FALLBACK to the
generic `execute_with_ctx` covers every non-fast-path case** — this is what keeps it safe.

- **Eligibility (decided at prepare, re-checked at execute):** analyzed = `Stmt::Insert`,
  target is a **clustered** table, source is `VALUES`, NO `RETURNING`/`REPLACE`/
  `ON DUPLICATE`/`ON CONFLICT`. At execute additionally: an explicit txn is open
  (`session.conn_txn.is_some()` && `in_explicit_txn`) AND the cached plan's `schema_version`
  still matches (else re-resolve / fall back — SQLite schema-cookie pattern).
- **NOT a banned cache:** this is the explicit prepared-statement API caching its own resolved
  plan (re-validated by schema_version), i.e. "faster executor" — NOT the auto-prepare DML
  cache the user ruled out.

## Steps (TDD, each compiles + commits)

- **6e-1** — Cache the plan on `PreparedStatement`. Add `insert_plan: Option<InsertFastPlan>`
  (`{ resolved: Arc<ResolvedTable>, schema_version: u64 }`), populated in `prepare()` when
  eligible. No behavior change (unused yet). Test: prepare an INSERT → plan is Some; prepare a
  SELECT/heap-INSERT → None.
- **6e-2** — Specialized execute. In `PreparedStatement::execute`, if `insert_plan` is Some AND
  the execute-time conditions hold AND schema_version matches: substitute params into the row,
  build `ExecutionContext`, call `enqueue_clustered_insert_ctx(stmt, &exec_ctx, conn_txn, ctx,
  resolved.clone())` directly. Else fall back to the current generic path. Expose
  `enqueue_clustered_insert_ctx` (or a thin `stage_prepared_clustered_insert` wrapper) from
  axiomdb-sql. Test: behavior parity vs generic (constraints, FK, auto-inc, generated cols,
  enum, intra-batch PK dup → UniqueViolation, ROLLBACK discards). A/B: insert_batch dispatch
  drops; ~2.9→~2.0µs.
- **6e-3** — Edge/fallback tests + close. schema change between prepare/execute → re-resolve or
  fall back (no stale plan); autocommit (no open txn) → generic path; heap table → generic.
  Workspace/clippy/fmt; memory + roadmap update.

## Done criteria

- [ ] Eligible prepared clustered INSERT bypasses the generic dispatch (A/B shows the drop;
      `--diagnose-insert-deep` dispatch remainder shrinks).
- [ ] FULL behavior parity with the generic path (all the phase tests green) + correct fallback
      for autocommit / heap / RETURNING / REPLACE / conflict / schema change.
- [ ] insert_batch measurably faster (> noise) toward ~2×; no read regression; T0 + guard +
      6c/6d crash tests green; workspace/clippy/fmt clean.

## Risks

| Risk | Mitigation |
|------|-----------|
| Specialized path misses logic the generic path does | reuse `enqueue_clustered_insert_ctx` (the SAME per-row phases); only bypass the WRAPPING; fall back for everything non-eligible |
| Stale cached plan after DDL | re-validate by schema_version at execute (SQLite cookie); re-resolve or fall back on mismatch |
| Cross-crate exposure (enqueue is pub(crate)) | add a thin pub `stage_prepared_clustered_insert` entry in axiomdb-sql |

## References

- `crates/axiomdb-embedded/src/lib.rs` (`prepare`, `PreparedStatement::execute`).
- `crates/axiomdb-sql/src/executor/insert_heap_ctx.rs` (`execute_insert_ctx` — the dispatch +
  resolve being bypassed), `insert_clustered_ctx.rs` (`enqueue_clustered_insert_ctx` — reused).
- `research/sqlite/src/insert.c` / `vdbe.c` (OP_MakeRecord/OP_Insert, cursor opened once).
- memory `project_insert_perf.md` (the grounded parity roadmap; lever 1 of 3).
