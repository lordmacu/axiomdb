# Spec + Plan: subphase 6e — specialized prepared-INSERT execute (dispatch bypass)

Phase: redo-recovery (project B) — subphase 6e (SQLite write-parity, lever 1 of 3)
Status: in-progress
Effort: **max** (executor hot path, shared crate, correctness-critical with fallback)

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
