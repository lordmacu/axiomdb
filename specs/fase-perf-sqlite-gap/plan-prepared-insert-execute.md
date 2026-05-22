# Plan: specialized prepared-INSERT execute (VDBE compile-once model)

Phase: perf-sqlite-gap — Parity lever #1 (executor scaffolding)
Spec: specs/fase-perf-sqlite-gap/spec-prepared-insert-execute.md
Status: in-progress

> **Progress:** Step 1 DONE (1a `948f8552` + 1b `eba98b64`). 1a = `PreparedInsertPlan` +
> `try_prepare_insert_plan` (eligibility) + `catalog_epoch()` accessor + 5 tests. 1b =
> `execute_prepared_insert` (replicates the dispatch INSERT arm via the existing inner fns,
> skipping the execute_with_ctx_locked/dispatch_ctx scaffolding) + `is_current` epoch
> recheck + a fast-hit counter + `PreparedStatement.insert_plan` routing (embedded) + the
> **differential-equivalence test** (fast vs generic → byte-identical, fast path exercised)
> + autocommit-fallback test. Embedded 126/126, clippy+fmt clean. **The path is wired +
> proven-equivalent but NOT yet faster** (1b only skips the outer wrapping; the real win is
> Step 5). **NEXT = Steps 2-4** (atomicity / DDL-fallback / eligibility tests — lock
> correctness) → **Step 5** (optimize the inner loop: reuse `ExecutionContext`, value
> template instead of the `substitute_params` clone, skip the per-row epoch invalidate = the
> actual win) → **Step 6** (measure via `--diagnose-prepared-insert`).

## Summary

Correctness-first TDD: first stand up the new path (eligibility + cached plan + routing) doing
the **correct** thing (reusing `resolve_insert_target` + `execute_insert_ctx_with_resolved`) with
a **differential-equivalence** test (prepared-fast vs generic → byte-identical) + a fast-path-hit
counter — this may not be faster yet. Then lock atomicity, epoch-fallback, and eligibility with
tests. ONLY THEN optimize the inner loop (skip `ExecutionContext` rebuild, the full-AST
`substitute_params` clone via a value template, the per-row epoch invalidate) — each optimization
guarded by the green differential + atomicity tests. Measure last. This order means the risky
speed work happens under a proven equivalence net (the 6e burn was a correctness regression).

## Dependencies

Must be done first:
- [x] spec-prepared-insert-execute approved
- [x] the 6e split (`resolve_insert_target` + `execute_insert_ctx_with_resolved`) — exists

Blocks:
- [ ] full insert_batch parity (pairs with the B-tree apply lever — next sprint task)

## Affected files

New:
- `crates/axiomdb-sql/src/executor/prepared_insert.rs` — `PreparedInsertPlan`,
  `try_prepare_insert_plan`, `execute_prepared_insert`.
- `crates/axiomdb-sql/tests/integration_prepared_insert.rs` — differential + atomicity +
  eligibility + epoch-fallback tests.

Modified:
- `crates/axiomdb-sql/src/executor/mod.rs` — wire the module + re-exports.
- `crates/axiomdb-sql/src/executor/{exec_dispatch.rs, insert_heap_ctx.rs, insert_clustered_ctx.rs}`
  — make the inner seams reachable (visibility) without duplicating logic.
- `crates/axiomdb-sql/src/session.rs` — `pub fn catalog_epoch(&self) -> u64` accessor (if absent);
  fast-path-hit counter (diagnostic).
- `crates/axiomdb-embedded/src/lib.rs` — `PreparedStatement.insert_plan` + route `execute()`.

## Step 1 — path + cached plan + differential equivalence (correctness scaffold)

**Goal:** an eligible prepared INSERT routes through the new path and produces byte-identical
results to the generic path. No speedup required yet — establish the equivalence net.
**Files:** `prepared_insert.rs` (new), `session.rs` (catalog_epoch accessor + hit counter),
embedded `lib.rs` (routing), `tests/integration_prepared_insert.rs`.
**Approach:** TDD — differential test first.

### Test to add
```rust
// integration_prepared_insert.rs
// Insert the same rows two ways into two identical tables and assert identical final state.
#[test]
fn prepared_fast_and_generic_produce_identical_state() {
    // table A: db.prepare("INSERT ... VALUES (?,?,?)") + execute per row (fast path)
    // table B: db.execute("INSERT ... VALUES (lit,lit,lit)") per row (generic)
    // assert SELECT * FROM A == SELECT * FROM B (ordered), and affected counts match.
    // assert the fast-path-hit counter advanced for A (eligible) and not for B.
}
```

### Implementation outline
```rust
// prepared_insert.rs
pub struct PreparedInsertPlan { table_id, is_clustered, /* resolved bits */, catalog_epoch_at_build, value_template, eligible: bool }
// Eligibility (single-table INSERT VALUES; no ON CONFLICT/REPLACE/ODKU/RETURNING/SELECT-source/triggers).
pub fn try_prepare_insert_plan(analyzed, storage, txn, ctx) -> Option<PreparedInsertPlan> { /* resolve once, build template */ }
// v1: CORRECT but not-yet-optimized — recheck epoch; on miss return Ok(None); else bind params
// into the template and route through the SAME execute_insert_ctx_with_resolved + trigger/epoch
// handling the generic Insert arm uses (exec_dispatch.rs:67-95). Bump the hit counter.
pub fn execute_prepared_insert(plan, params, storage, txn, bloom, ctx) -> Result<Option<QueryResult>, DbError> { ... }
```
```rust
// embedded lib.rs PreparedStatement::execute: if let Some(plan) = &self.insert_plan {
//   match execute_prepared_insert(plan, params, &*db.storage, &*db.txn, &db.bloom, &mut db.session)? {
//     Some(r) => return Ok(r), None => { /* fall through to generic */ } } }
// insert_plan built lazily on first execute (avoids prepare-time txn context).
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-sql integration_prepared_insert
./tools/vm.sh test -p axiomdb-embedded
```

### Commit
`feat(perf-sqlite-gap): prepared-INSERT fast path scaffold + differential test`

---

## Step 2 — statement atomicity (the 6e burn guard)

**Goal:** an error mid-statement leaves NO partial staged rows / deferred-FK / notify residue —
identical to the generic path, in autocommit AND explicit txn.
**Files:** tests + (if needed) the atomicity wrapper seam in `prepared_insert.rs`.
**Approach:** TDD — write failing atomicity tests, then ensure the path reuses the generic
per-statement cleanup (do NOT hand-roll).

### Test to add
```rust
// PK-dup mid multi-row execute, NOT NULL, CHECK violation — both txn modes.
#[test] fn fast_path_pk_dup_is_statement_atomic_autocommit() { /* table unchanged after the error */ }
#[test] fn fast_path_constraint_error_is_statement_atomic_in_txn() { /* prior rows in the txn survive; the failed stmt's rows do not */ }
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-sql integration_prepared_insert
```

### Commit
`test(perf-sqlite-gap): prepared-INSERT statement-atomicity (both txn modes)`

---

## Step 3 — epoch revalidation + DDL fallback

**Goal:** a DDL between executes flips `catalog_epoch` → the plan is stale → `execute_prepared_insert`
returns `Ok(None)` → caller falls back to the generic path → correct + plan rebuilt.
**Files:** `prepared_insert.rs` (epoch recheck), tests.

### Test to add
```rust
#[test] fn ddl_between_executes_falls_back_and_stays_correct() {
    // prepare INSERT; execute once (fast); ALTER TABLE ADD COLUMN; execute again ⇒
    // fast-path-hit counter does NOT advance on the 2nd; result is correct for the new schema.
}
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-sql integration_prepared_insert
```

### Commit
`feat(perf-sqlite-gap): prepared-INSERT epoch revalidation + DDL fallback`

---

## Step 4 — eligibility matrix → generic fallback

**Goal:** ineligible INSERTs (ON CONFLICT/REPLACE/ODKU/RETURNING/SELECT-source/triggers/complex
exprs) never take the fast path; they fall back transparently.
**Files:** `try_prepare_insert_plan` eligibility, tests.

### Test to add
```rust
#[test] fn ineligible_inserts_use_generic_path() {
    // for each: prepare + execute ⇒ correct result AND fast-path-hit counter unchanged.
}
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-sql integration_prepared_insert
```

### Commit
`feat(perf-sqlite-gap): prepared-INSERT eligibility gating`

---

## Step 5 — optimize the inner loop (the actual win, under the green net)

**Goal:** cut the per-row scaffolding now that equivalence + atomicity + fallback are locked.
**Files:** `prepared_insert.rs` (+ minimal visibility changes in the executor).
**Approach:** apply optimizations one at a time, re-running the Step 1-4 tests after each:
1. Reuse one `ExecutionContext` across the prepared statement's executes (don't rebuild per call).
2. Bind params via the cached `value_template` instead of cloning + walking the full AST
   (`substitute_params`); only eval non-literal exprs.
3. Skip the per-row `invalidate_table_epoch_for_id` for staged inserts — do it once at flush/commit
   (the root only moves at commit; verify against the equivalence + a SELECT-after-INSERT-in-txn test).
4. Reuse the resolved table from the plan (skip the resolve probe) while epoch is current.

### Verification
```bash
./tools/vm.sh test -p axiomdb-sql                 # full sql crate (equivalence net)
./tools/vm.sh test -p axiomdb-embedded
# perf (macOS native, clean build — NO bench-timings):
cargo build --release -p axiomdb-bench-comparison
./target/release/axiomdb_bench --diagnose-prepared-insert --scenario insert_batch --rows 50000
```

### Commit
`perf(perf-sqlite-gap): prepared-INSERT inner-loop — skip per-row scaffolding`

---

## Step 6 — integration verification + measure + final

**Goal:** confirm the spec done-criteria + no cross-crate regression + record the measured win.

### Verification against spec done criteria
- [ ] API present; eligible→fast, ineligible/epoch-miss→generic.
- [ ] Differential equivalence + atomicity + DDL-fallback + eligibility tests green.
- [ ] `--diagnose-prepared-insert insert_batch 50000` per-row execute measurably lower vs the
      pre-Step-5 baseline; no regression on generic path or reads.
- [ ] `cargo nextest --workspace` + clippy + fmt clean (Lima).
- [ ] rustdoc on new public items.

### Final commit
`feat(perf-sqlite-gap): specialized prepared-INSERT execute (parity lever #1)`

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Atomicity divergence (the 6e burn) | **high** | Step 2 tests BEFORE optimizing; reuse the generic cleanup, never hand-roll; optimize only under green tests (Step 5) |
| `catalog_epoch` not actually DDL-only in some path | medium | Step 3 test; audit every `catalog_epoch += 1` site (only invalidate_all today) |
| Skipping per-row epoch invalidate breaks SELECT-after-INSERT-in-txn | medium | Step 5.3 keeps a SELECT-after-staged-INSERT equivalence test; if it breaks, keep the invalidate at flush |
| Win smaller than hoped (instrumentation inflated the estimate) | medium | measure at Step 5; if <~0.3µs, stop + document — the B-tree lever is the bigger remaining one |
| value_template misses an expr case → wrong bind | medium | differential test covers literal/param/mixed; ineligible-fallback covers complex exprs |

## Rollback plan

Each step is an isolated commit. Abandon: `git reset --hard <commit before step 1>`. The new
path is opt-in via `PreparedStatement.insert_plan`; setting it to `None` (or reverting the routing)
restores the generic path with zero behavior change (the on-disk format is untouched).

## Estimated effort

Total: ~2-3 days (impl max). Steps 1 ~0.5d (scaffold+differential), 2 ~0.5d (atomicity),
3-4 ~0.5d (epoch+eligibility), 5 ~1d (the optimization, careful), 6 ~0.5d (measure+integrate).
