# Plan: carry the resolved table in the prepared statement

Phase: perf-sqlite-gap — insert/execute hot-path
Task: skip per-execute `resolve_table_cached` by caching the resolved table in `PreparedInsertPlan`
Spec: specs/fase-perf-sqlite-gap/spec-carry-resolved-table.md
Status: in-progress — Step 1 (verify validator) DONE. write_commit_seq bumps at COMMIT on
data/DDL txns (txn_begin_commit.rs:228/275/286), STABLE within an open txn (no commit in the
batch row loop) — confirmed by the existing test `read_only_select_does_not_advance_ln`
(integration_scenario_correctness.rs: reads don't bump, write-commit does). catalog_epoch += 1
in invalidate_all (session.rs:1650, DDL). Validator correct + stable-within-batch (the win
case); autocommit bumps per commit (no win there, expected). NEXT: Step 2 (the struct field +
validator) → Steps 3-4 (build@prepare + wire/skip) → Step 5 (DDL-invalidation GATE, the crux).

## Summary

The clean perf profile found the bulk insert re-resolves the table per row (per `stmt.execute`):
~3 String allocs + a SipHash HashMap lookup in `resolve_table_cached`, for the same table. Carry
the resolved `Arc<ResolvedTable>` in `PreparedInsertPlan` (built once at `Db::prepare`), validate
per execute with a cheap 2-scalar compare (`catalog_epoch` + `write_commit_seq`, both DDL-bumped),
and skip `resolve_table_cached` when current; fall back to today's path (unchanged) on a miss.
Order: verify the validator (Step 1) → add the cached field + validator (2) → build at prepare (3)
→ wire into execute + skip the resolve (4) → the DDL-invalidation correctness gate (5) → bench A/B
+ close (6). The fallback stays bit-identical so epoch-miss / DDL workloads never regress.

## Dependencies
- [x] spec-carry-resolved-table approved
- [x] catalog_epoch / write_commit_seq / is_table_epoch_current exist; PreparedInsertPlan exists
Blocks: nothing.

## ✅ Correctness VERIFIED (2026-05-23) — caching Arc<ResolvedTable> is safe; v1 = no-secondary-index tables

The risky intellectual work is DONE. Findings (read the code):
- `resolve_insert_target` returns `Arc<ResolvedTable>` (insert_heap_ctx.rs:24); it calls
  `resolve_table_cached` + `search_path[i].clone()` — THE per-row String allocs + SipHash. Arc ⇒
  cheap to cache/clone.
- `ResolvedTable { def, columns, indexes, constraints, foreign_keys }` (catalog/resolver.rs) = the
  SCHEMA. `execute_insert_ctx_with_resolved` reads `resolved.def.id/.table_name/.is_clustered()/
  .columns` — it does **NOT** read a root from `resolved`. The live clustered root comes from
  `clustered_root_for_conn` (per-txn, Attack 14). So the cached resolve can't go stale on the
  clustered root. Schema only changes on DDL → caught by the validator. ✅ safe.
- SECONDARY index roots DO mutate per insert (index_maintenance.rs: AtomicU64 + update_index_root).
  If the insert read them from cached `resolved.indexes`, splits would make them stale. So **v1
  GATES the cache to tables with NO secondary indexes** (`resolved.indexes.is_empty()`) — the bench
  qualifies (`bench_users` has only `PRIMARY KEY (id)`, no secondary). Indexed tables fall back to
  the per-row resolve (correct, unoptimized) — a follow-on can verify index-root liveness.
- Resolve at PREPARE: `resolve_table_cached(storage, txn, ctx, None, &s.table)` (Optional conn → works
  outside a txn). execute is `&self` (no &mut) → eager cache at prepare; on epoch/seq-miss the existing
  fallback (generic path) handles it (no re-cache needed for v1 — degrades to per-row resolve after DDL,
  which is rare + correct).

### Turn-key implementation recipe (mechanical now)
1. `PreparedInsertPlan { catalog_epoch_at_build, write_seq_at_build: u64, resolved: Arc<ResolvedTable> }`
   (ensure ResolvedTable: Debug or drop derive). `resolved_if_current(ctx, txn) -> Option<&Arc<ResolvedTable>>`
   = epoch && write_seq match.
2. `try_prepare_insert_plan(analyzed, ctx, storage, txn)`: eligible INSERT → resolve_table_cached(…, None, &s.table);
   if `resolved.indexes.is_empty()` → Some(plan{resolved, epoch, seq}); else None.
3. `Db::prepare` (lib.rs): pass `&*self.storage, &self.txn` to try_prepare_insert_plan.
4. `execute` (lib.rs ~709): fast arm uses `resolved_if_current(&db.session,&db.txn)` → pass
   `Some(rt.clone())` to execute_prepared_insert.
5. `execute_prepared_insert(stmt, cached: Option<Arc<ResolvedTable>>, …)`:
   `let resolved = match cached { Some(rt)=>rt, None=>resolve_insert_target(&stmt,&exec_ctx,&mut conn,ctx)? };`
6. `resolve_table_cached` (shared.rs): add a `RESOLVE_CALLS` AtomicU64 + getter (test asserts the fast
   path → resolve calls == 1 per batch, not N).

## Affected files
Modified:
- `crates/axiomdb-sql/src/executor/prepared_insert.rs` — `PreparedInsertPlan` gains `resolved` +
  `write_seq_at_build` + `resolved_if_current`; a `resolve-skipped` diagnostic counter.
- `crates/axiomdb-sql/src/executor/shared.rs` — a diagnostic counter in `resolve_table_cached`
  (assert the fast path doesn't call it); the fallback logic UNCHANGED.
- `crates/axiomdb-embedded/src/lib.rs` — build the plan with the resolve at `Db::prepare`; `execute`
  uses `resolved_if_current` → pass the Arc to `execute_prepared_insert` (skip resolve) or fall back.
- `crates/axiomdb-sql/src/executor/prepared_insert.rs` — `execute_prepared_insert` takes the
  resolved table (skips its internal resolve).
New tests:
- `crates/axiomdb-embedded/tests/…` — DDL-between-executes (same + cross-session), txn modes.

---

## Step 1 — Verify the validator (write_commit_seq + catalog_epoch bump semantics)

**Goal:** confirm `(catalog_epoch, write_commit_seq)` is a correct + stable-within-txn validator.
**Files:** read `txn_begin_commit.rs` (the `write_commit_seq` fetch_add), `session.rs` (catalog_epoch
bump = `invalidate_all`); add a focused test.
**Approach:** TDD — a test that pins the behavior.

### Test to add
```rust
// a DDL bumps the validator; a data insert WITHIN an open txn does not (so the batch row loop is stable)
#[test]
fn validator_bumps_on_ddl_not_within_open_txn() {
    // BEGIN; capture (epoch, write_seq); INSERT a few rows; assert (epoch, write_seq) UNCHANGED.
    // COMMIT; CREATE/ALTER (DDL); assert the validator CHANGED.
}
```
### Verification
```bash
./tools/vm.sh test -p axiomdb-sql validator_bumps
```
Documents whether write_commit_seq bumps on all-commits vs catalog-only (affects autocommit win;
correctness holds either way). **GATE:** if a data insert inside the open txn bumps it, the batch
win still holds (no commit during the loop) — but record it.
### Commit
`test(perf-sqlite-gap): pin (catalog_epoch, write_commit_seq) validator semantics — Step 1`

---

## Step 2 — `PreparedInsertPlan` carries the resolve + the validator

**Goal:** the plan stores the resolved table + both stamps + a cheap `resolved_if_current`.
**Files:** `prepared_insert.rs`.

### Test to add
```rust
#[test]
fn resolved_if_current_some_then_none_after_bump() {
    let plan = /* build with epoch=E, seq=S, resolved=Arc */;
    assert!(plan.resolved_if_current(&ctx_at(E, S), &txn).is_some());
    assert!(plan.resolved_if_current(&ctx_at(E + 1, S), &txn).is_none()); // epoch bump
    assert!(plan.resolved_if_current(&ctx_at(E, S + 1), &txn).is_none()); // write_seq bump
}
```
### Implementation outline
```rust
pub struct PreparedInsertPlan {
    catalog_epoch_at_build: u64,
    write_seq_at_build: u64,                 // NEW
    resolved: std::sync::Arc<ResolvedTable>, // NEW (ensure ResolvedTable: Debug, or drop derive(Debug))
}
impl PreparedInsertPlan {
    pub fn resolved_if_current(&self, ctx: &SessionContext, txn: &TxnManager)
        -> Option<&Arc<ResolvedTable>> {
        (self.catalog_epoch_at_build == ctx.catalog_epoch()
            && self.write_seq_at_build == txn.write_commit_seq())
        .then_some(&self.resolved)
    }
}
```
### Verification
```bash
./tools/vm.sh test -p axiomdb-sql resolved_if_current
```
### Commit
`feat(perf-sqlite-gap): PreparedInsertPlan carries the resolved table + 2-scalar validator — Step 2`

---

## Step 3 — Build the plan (with the resolve) eagerly at `Db::prepare`

**Goal:** `Db::prepare` resolves the table once and stamps (epoch, write_seq).
**Files:** `lib.rs` (`Db::prepare`), `prepared_insert.rs` (`try_prepare_insert_plan` gains the resolve).

### Test to add
```rust
#[test]
fn prepare_resolves_table_once() {
    // db.prepare("INSERT INTO t VALUES (?, …)") → the PreparedStatement's insert_plan is Some
    // with a resolved table whose def.id == t's id.
}
```
### Implementation outline
- `try_prepare_insert_plan(analyzed, ctx, storage, txn, conn_txn)` now also calls
  `resolve_table_cached` once, storing `resolved` + `catalog_epoch()` + `write_commit_seq()`.
### Verification
```bash
./tools/vm.sh test -p axiomdb-embedded prepare_resolves_table_once
```
### Commit
`feat(perf-sqlite-gap): resolve the table once at prepare (stamp epoch+seq) — Step 3`

---

## Step 4 — `execute` reuses the cached resolve, skips `resolve_table_cached`

**Goal:** on the epoch-current fast path, pass the cached Arc to `execute_prepared_insert` and do
NOT call `resolve_table_cached`; on a miss, fall back to today's path + rebuild the plan.
**Files:** `lib.rs` (`execute`), `prepared_insert.rs` (`execute_prepared_insert` takes the resolved
table), `shared.rs` (diagnostic counter).

### Test to add
```rust
#[test]
fn fast_execute_skips_resolve_and_matches_generic() {
    // counter of resolve_table_cached calls; run N prepared executes in one txn;
    // assert resolve calls == 1 (the prepare), not N; and the inserted rows == the generic path.
}
```
### Implementation outline
- `execute`: `match self.insert_plan.resolved_if_current(&db.session, &db.txn) { Some(rt) =>
  execute_prepared_insert(ins, rt.clone(), …) , None => { generic path; rebuild plan } }`.
- `execute_prepared_insert(stmt, resolved: Arc<ResolvedTable>, …)` uses `resolved` instead of
  resolving internally.
- `shared.rs`: `RESOLVE_CALLS` atomic + getter (diagnostic, like `prepared_insert_fast_hits`).
### Verification
```bash
./tools/vm.sh test -p axiomdb-embedded fast_execute_skips_resolve
./tools/vm.sh test -p axiomdb-sql
```
### Commit
`feat(perf-sqlite-gap): execute reuses the cached resolve, skips resolve_table_cached — Step 4`

---

## Step 5 — Correctness GATE: DDL invalidation (the crux — Attack 2 reverted twice)

**Goal:** any DDL between executes on a live prepared stmt forces a re-resolve; no stale resolve.
**Files:** new embedded tests.

### Tests to add
```rust
// same-session: prepare INSERT; execute; ALTER TABLE add column; execute → new schema (or error if shape changed)
// same-session: prepare; execute; DROP TABLE; execute → TableNotFound
// cross-session: conn2 ALTER+COMMIT; conn1's next execute re-resolves (write_commit_seq bump)
// autocommit vs explicit BEGIN..COMMIT both correct
// table dropped + recreated (new id) → re-resolve
```
### Verification
```bash
./tools/vm.sh test -p axiomdb-embedded ddl_invalidates_prepared_resolve
./tools/vm.sh test --workspace   # no regression anywhere
```
### Commit
`test(perf-sqlite-gap): DDL-between-executes invalidates the cached resolve — Step 5 (gate)`

---

## Step 6 — Bench A/B + perf re-profile + close

**Goal:** confirm ≥+8% bulk insert + the per-row resolve allocs/SipHash gone; no read regression.
**Files:** docs.

### Verification (Lima)
```bash
# A/B: insert_batch before/after (interleaved or vs the prior commit); ≥+8%
# perf re-profile (CARGO_PROFILE_RELEASE_STRIP=false): the per-row resolve String allocs + SipHash gone
./tools/vm.sh test --workspace && ./tools/vm.sh clippy && ./tools/vm.sh fmt-check
```
### Done-criteria check (from the spec)
- [ ] epoch-current execute does NOT call resolve_table_cached (counter)
- [ ] DDL between executes re-resolves (same + cross-session)
- [ ] reads/point_lookup ±2%; bulk insert ≥+8%
- [ ] workspace nextest + clippy + fmt clean; rustdoc
### Final commit
`perf(perf-sqlite-gap): carry resolved table in prepared stmt (skip per-row resolve) — ≥+8% bulk insert`

---

## Risk register
| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Stale resolve after DDL → wrong schema/corruption | medium | the Step 5 gate (same+cross-session DDL); validator = catalog_epoch + write_commit_seq (both DDL-bumped) |
| write_commit_seq bumps within the txn → no win | low | Step 1 verifies; the batch is one txn (no commit in the loop) → stable |
| ResolvedTable not Debug (derive break) | low | drop derive(Debug) or impl manually (Step 2) |
| Regression on epoch-miss / SELECT | low | fallback path bit-identical; SELECT untouched (out of scope) |

## Rollback plan
Each step is an isolated commit; `git reset --hard <before the bad step>`. The fallback path is
never changed, so a rollback can't corrupt — it just reverts to per-row resolve.

## Estimated effort
Total ~2-3 días. Step 1 (verify) low ~0.5d; Steps 2-3 (plan field + build) medium ~0.5d; Step 4
(wire + skip) high ~0.5-1d; Step 5 (DDL gate) high ~0.5d; Step 6 (bench+close) ~0.5d.
Per-step effort: 1 low · 2 medium · 3 medium · 4 high · 5 high · 6 low-medium.
