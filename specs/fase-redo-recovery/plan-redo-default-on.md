# Plan: redo-default-on

Phase: redo-recovery (project B) — final rollout
Task: Lever B — flip the embedded `Db` default to frame-only redo, gated by the
crash suite (T1–T7) + a no-regression A/B.
Spec: specs/fase-redo-recovery/spec-redo-default-on.md
Status: draft

## Summary

Validate frame-only redo with the full crash suite (T1–T7 on the
`FaultInjectionStorage` power-loss harness), confirm no read/batch regression
with a clean interleaved A/B, THEN flip the embedded default to
`RedoMode::FrameOnly` and fix any test that depended on the old (Off) durability
shape. The mechanism is already built (subphases 3–6f); each T-test is a
*validation* that passes if recovery is correct and turns RED (→ stop + fix the
mechanism) only if it finds a latent bug. The flip is scoped to embedded; the
server stays opt-in (its checkpointer wiring, 6f step 6, is out of scope).

## Dependencies

Must be done first:
- [x] subphases 3–6f built; `FaultInjectionStorage`; T0 green; embedded
      checkpointer wiring (6f step 5).

Blocks:
- [ ] server default-on (needs 6f step 6 — not here).

## Affected files

New:
- `crates/axiomdb-wal/tests/integration_redo_crash_suite.rs` — T1–T7 (reuses the
  T0 pattern from `integration_redo_recovery.rs`).

Modified:
- `crates/axiomdb-embedded/src/lib.rs` — open path defaults redo → FrameOnly
  when unset (Step 9).
- `crates/axiomdb-storage/src/config.rs` — only if we choose the `resolved_redo`
  route; lean toward NOT touching it (keep Off as the struct default, default in
  the embedded layer so the server stays Off).
- `benches/comparison/axiomdb_bench/src/main.rs` — explicit-Off path for the A/B
  once the embedded default flips (Step 8/9).
- `docs-site/src/user-guide/features/transactions.md`, `internals/wal.md` (Step 10).

## Crash-test pattern (from T0, reuse verbatim)

```
let mut storage = FaultInjectionStorage::new();
storage.enable_redo_log(&dir.join("x.wf")).unwrap();
// durable baseline: alloc + write_page + flush()
// write + commit: TxnManager::create, begin, set_current_txn(txn_id),
//   mutate page, write_page (VOLATILE), record_*(...), commit_durable(conn,&storage)
storage.simulate_power_loss();                       // volatile writes lost
let (_txn, result) = TxnManager::open_with_recovery(&mut storage, &wal_path)?;
// assert data restored + result.redone_pages
let r2 = CrashRecovery::recover(&mut storage, &wal_path)?;  // idempotence: redone == 0
```

## Step 1 — T1: redo of each page type

**Goal:** every committed page type survives power loss + recovery.
**Files:** `integration_redo_crash_suite.rs` (new).
**Approach:** one test per page type, T0 pattern. Heap is T0 (reference); add:
clustered leaf, clustered internal (force a split so an internal frame exists),
secondary index (B-tree), overflow chain (large row), free-list/meta (alloc that
dirties page 1).

### Tests
```rust
fn t1_clustered_leaf_insert_survives_power_loss() { ... }
fn t1_clustered_internal_split_survives_power_loss() { ... } // many rows → split
fn t1_secondary_index_insert_survives_power_loss() { ... }
fn t1_overflow_row_survives_power_loss() { ... }            // row > inline budget
fn t1_freelist_meta_alloc_survives_power_loss() { ... }
```
### Verification
```bash
./tools/vm.sh test -p axiomdb-wal --test integration_redo_crash_suite t1
```
Expect green (mechanism built). RED ⇒ STOP, fix recovery for that page type.

### Commit
`test(fase-redo-recovery): T1 redo-per-page-type crash tests`

---

## Step 2 — T2: idempotent replay

**Goal:** crash *during* recovery → second recovery is identical / no-op.
**Approach:** after a recover, simulate_power_loss again before any flush, recover
again; assert state identical + `redone_pages == 0` on the stabilized pass.
Extends the inline idempotence check already in T0.

### Verification / Commit
`./tools/vm.sh test -p axiomdb-wal --test integration_redo_crash_suite t2` ·
`test(fase-redo-recovery): T2 idempotent-replay crash test`

---

## Step 3 — T3: partial checkpoint

**Goal:** crash mid-checkpoint → redo from the previous checkpoint LSN; no
double-apply, all committed data present.
**Approach:** commit some frames, run `checkpoint_frames` to a point, crash before
the recycle completes (or with frames still pending), recover; assert all
committed rows present + idempotent. Reuse `checkpoint_frames` + the 6b grow/
recycle paths.

### Commit
`test(fase-redo-recovery): T3 partial-checkpoint crash test`

---

## Step 4 — T4: torn page repaired by frame replay

**Goal:** a single half-written page is repaired by redo (+ doublewrite); no
post-recovery checksum mismatch.
**Approach:** use the fault-injection torn-write hook (or write a corrupt/partial
page image) for a committed page, then recover; assert the frame replay restores
a checksum-valid page with the committed bytes.

### Commit
`test(fase-redo-recovery): T4 torn-page repair crash test`

---

## Step 5 — T5: UNLOGGED truncate / logged survive (under frame-only)

**Goal:** the `integration_open_integrity` guarantee holds with frame-only redo:
logged tables survive a dirty open, UNLOGGED truncate.
**Approach:** mirror `integration_open_integrity::test_dirty_open_truncates_
unlogged_tables_only` but with redo = FrameOnly; assert logged rows survive +
unlogged truncated.

### Commit
`test(fase-redo-recovery): T5 unlogged-truncate under frame-only`

---

## Step 6 — T6: uncommitted txn still UNDONE

**Goal:** redo coexists with logical undo — an uncommitted txn at crash is gone.
**Approach:** begin + write (volatile + frame) but DO NOT commit_durable; crash;
recover; assert the row is absent (UNDO) and committed neighbors present (REDO).

### Commit
`test(fase-redo-recovery): T6 uncommitted-undo no-regression crash test`

---

## Step 7 — T7: bounded soak vs oracle

**Goal:** randomized op streams + random crash points match an in-memory oracle.
**Approach:** seeded RNG (≤ ~200 seeds for CI). Per seed: random insert/update/
delete across heap + clustered + secondary into both the engine (frame-only) and
a `BTreeMap` oracle; at a random point, `simulate_power_loss`; recover; assert the
recovered committed state == oracle's committed state. Deterministic per seed.

### Verification / Commit
`./tools/vm.sh test -p axiomdb-wal --test integration_redo_crash_suite t7` ·
`test(fase-redo-recovery): T7 bounded crash-soak vs oracle`

---

## Step 8 — No-regression A/B (measure, no code beyond a bench Off-override)

**Goal:** confirm the flip won't regress batch/reads.
**Approach:** macOS-native interleaved A/B (redo OFF vs ON), document numbers:
- autocommit: ON ≥ OFF (✓ ~1.56×).
- insert_batch 50K: ON within ±5% (wash).
- full_scan / point_lookup / select_where / range_scan: ON within ±5%.
Add an explicit `redo=Off` bench path so the A/B still works after Step 9 flips
the default (knob inversion).

### Commit
`bench(fase-redo-recovery): redo on/off A/B + explicit-Off override`

---

## Step 9 — Flip the embedded default + fix fallout

**Goal:** embedded `Db` defaults to `RedoMode::FrameOnly`; server unchanged.
**Approach:** in `axiomdb-embedded` open path, when `config.redo` is unset
default it to `FrameOnly` (NOT in `config.rs::resolved_redo`, so the server's
DbConfig stays Off). Then run the FULL workspace — any test that depended on the
Off shape (like the append-split update tests depended on half-full leaves) gets
fixed here. Verify the server path still resolves Off (no unbounded log).

### Verification
```bash
./tools/vm.sh test --workspace --no-fail-fast   # fix every fallout
./tools/vm.sh clippy ; ./tools/vm.sh fmt-check
```
### Commit
`feat(fase-redo-recovery): default embedded Db to frame-only redo`

---

## Step 10 — Close

- docs-site `user-guide/features/transactions.md` (default is frame-only redo;
  opt-out `redo = "off"`) + `internals/wal.md` (recovery is the live default).
- `memory/project_insert_perf.md` + `project_state.md`: autocommit ~1.56× win
  banked + data-loss hole closed by default (embedded).
- `cargo nextest run --workspace` green; clippy + fmt clean.
- `docs/fase-redo-recovery.md` handoff update + `docs/progreso.md`.

### Final commit
`feat(fase-redo-recovery): complete frame-only redo default rollout (lever B)`

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| A T-test finds a real recovery bug | medium | that's the point — STOP, fix the mechanism, then continue (don't weaken the test) |
| Flip breaks many existing tests | medium | Step 9 isolates the flip + a full --no-fail-fast pass to fix fallout in one place |
| Server accidentally flipped → unbounded log | low | default in the embedded layer only; assert server resolves Off |
| MmapStorage power-loss not simulatable in-proc | known | T1–T7 use FaultInjectionStorage + a real reopen for the mmap apply branch |
| T7 soak flaky/slow | medium | seeded + bounded (≤200), deterministic per seed |

## Rollback plan

Steps 1–8 are additive (tests + bench) — safe. Step 9 (the flip) is the only
behavior change; revert that single commit to return to opt-in if needed.

## Estimated effort

Impl max (durability validation). Steps 1–7 ~1–1.5 days (crash tests), Step 8
~1h, Step 9 ~0.5–1 day (flip + fallout), Step 10 ~2h.
