# Plan: subphase 6d — frame-log fast path

Phase: redo-recovery (project B) — subphase 6d
Spec: specs/fase-redo-recovery/spec-subfase-6d-frame-log-fast-path.md
Status: in-progress

## Summary

Turn the 6c frame-only write-ahead from a perf wash into a real insert win, matching SQLite
WAL+NORMAL. Order is **by risk/confidence**: first the **deferred fsync** at NORMAL (the safe
~+22 % win — frame-only already dropped the 16 ms main flush; deferring the per-commit frame
fsync drops ~15 ms more), which needs the **checkpoint to own the fsync** (Step 1) before
`commit_durable` may skip it (Step 2). Then a **confirm-measure** (Step 3) decides whether the
**mmap'd frame log** (Step 4, the ~+25 % more on writes) is required or deferrable. Crash tests
for the NORMAL-deferred semantics (Step 5) and the final A/B + close (Step 6) gate it. Each
step compiles, tests, and commits cleanly; the switch only matters with redo on (default off),
so nothing changes for the default path until the deliberate flip in Step 6 / subphase 7.

## Dependencies

Must be done first:
- [x] spec-subfase-6d approved
- [x] subphases 5, 6a, 6b, 6c (REDO recovery, contiguous prefix, checkpoint+recycle, the switch)

Blocks:
- [ ] subphase 7 (full crash suite T1–T7, doublewrite retirement, flip default ON)

## Affected files

Modified:
- `crates/axiomdb-storage/src/wal_frame.rs` — `FrameLog`: `written_size()`; (Step 4) mmap
  append/read/msync.
- `crates/axiomdb-storage/src/mmap.rs` — `checkpoint_frames` fsyncs the frame log first;
  `maybe_checkpoint`; (Step 4) FrameLog wiring.
- `crates/axiomdb-storage/src/fault_injection.rs` — mirror checkpoint frame-fsync.
- `crates/axiomdb-storage/src/engine.rs` — `maybe_checkpoint` trait method (default no-op);
  `written_frame_bytes()` accessor if needed.
- `crates/axiomdb-storage/src/config.rs` — `checkpoint_frame_bytes` (resolved default).
- `crates/axiomdb-wal/src/txn_begin_commit.rs` — `commit_durable` policy-aware (defer at NORMAL).
- `crates/axiomdb-network/src/mysql/shared_db.rs`, `crates/axiomdb-embedded/src/lib.rs` —
  call `maybe_checkpoint` after commit + checkpoint on clean close.
- `benches/comparison/axiomdb_bench/src/main.rs` — A/B knob already present (`AXIOMDB_BENCH_REDO`).
- Tests: `crates/axiomdb-storage/tests/*`, `crates/axiomdb-network/tests/integration_open_integrity.rs`.

---

## Step 1 — Checkpoint owns the frame fsync + checkpoint trigger

**Goal:** the checkpoint fsyncs the frame log (before applying to main, walCheckpoint order),
and a size + clean-shutdown trigger runs it. This gives the deferred fsync (Step 2) a home.
**Files:** `mmap.rs` + `fault_injection.rs` (`checkpoint_frames`), `wal_frame.rs`
(`written_size`), `engine.rs` (`maybe_checkpoint` default no-op + `frame_bytes` accessor),
`config.rs` (`checkpoint_frame_bytes`), `shared_db.rs`/`embedded` (call sites + clean close).
**Approach:** TDD.

### Tests to add
```rust
// storage: checkpoint_frames now syncs the frame log before apply (observable via
// FaultInjectionStorage: the frame log's durable_offset advances to the contiguous prefix
// before main is written).
#[test] fn checkpoint_fsyncs_frame_log_before_applying_to_main() { /* ... */ }

// trigger: after enough commits to exceed checkpoint_frame_bytes, written_size drops.
#[test] fn size_trigger_runs_checkpoint_and_shrinks_the_log() { /* ... */ }

// clean shutdown drains: reopen needs no REDO (redone_pages == 0).
#[test] fn clean_close_checkpoints_so_reopen_has_no_redo() { /* ... */ }
```

### Implementation outline
```rust
// wal_frame.rs
impl FrameLog { pub fn written_size(&self) -> u64 { self.write_offset.load(Relaxed) } }

// mmap.rs checkpoint_frames: BEFORE apply_committed_frames(..),
//   if let Some(fl) = &self.frame_log { fl.sync_to_durable()?; }   // frames durable first
//   ... apply to main, fsync main, in-flight-safe recycle + wal_index.clear (6c) ...

// engine.rs
trait StorageEngine {
    fn maybe_checkpoint(&self, _is_committed: &dyn Fn(u64)->bool) -> Result<(), DbError> { Ok(()) }
}
// mmap.rs: maybe_checkpoint = if frame_log_active && written_size() >= threshold { checkpoint_frames(..) }

// shared_db.rs / embedded after a successful commit (frame-only): db.storage.maybe_checkpoint(&committed_pred)?;
// + on clean close: a final checkpoint_frames.
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-storage
./tools/vm.sh clippy -p axiomdb-storage
```

### Commit
```
feat(redo-recovery): checkpoint fsyncs the frame log + size/clean-shutdown trigger (6d step 1)
```

---

## Step 2 — `commit_durable` policy-aware (the safe ~+22 % win)

**Goal:** at NORMAL/Off, `commit_durable` does NOT fsync the frame log per commit (deferred to
the Step 1 checkpoint); at Strict it fsyncs per commit as today.
**Files:** `txn_begin_commit.rs`.
**Approach:** TDD + A/B.

### Tests to add
```rust
// Strict: commit_durable fsyncs the frame log (durable_offset advances). NORMAL: it does NOT
// (durable_offset unchanged until checkpoint). Use FaultInjectionStorage + durability_override.
#[test] fn commit_durable_defers_frame_fsync_at_normal_syncs_at_strict() { /* ... */ }

// Strict crash safety unchanged: committed row survives power loss with no checkpoint.
#[test] fn strict_frame_only_commit_survives_crash_without_checkpoint() { /* ... */ }
```

### Implementation outline
```rust
// txn_begin_commit.rs
pub fn commit_durable(&self, conn_txn, storage) -> Result<Option<TxnId>, DbError> {
    let policy = conn_txn.durability_override.unwrap_or(self.durability_policy);
    if policy == WalDurabilityPolicy::Strict {
        storage.sync_frame_log()?;          // per-commit fsync (today's behavior)
    }
    // Normal/Off: skip — the checkpoint (6d step 1) fsyncs the frame log.
    self.commit(conn_txn)
}
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-wal -p axiomdb-storage
# A/B (macOS): cargo build --release -p axiomdb-bench-comparison
#   AXIOMDB_BENCH_REDO=1 axiomdb_bench --scenario insert_batch --rows 50000 --diagnose-prepared-insert
#   vs the redo-off baseline — expect COMMIT ~63ms→~49ms, autocommit large win.
```

### Commit
```
feat(redo-recovery): defer frame fsync to checkpoint at NORMAL (6d step 2)
```

---

## Step 3 — Confirm-measure: decide mmap vs batch (resolves the spec open question)

**Goal:** with deferred fsync ON, re-measure; decide if the frame *writes* still dominate
(→ Step 4 mmap) or the budget is met (→ defer Step 4).
**Files:** none (measurement) — update the plan with the decision.
**Approach:** A/B + a `sample` of a sub-200K frame-only run with deferred fsync.

### Verification
```bash
# A/B redo-on (deferred) vs redo-off baseline, medians; sample the apply.
```
**Done:** decision recorded here (mmap required / deferred). If deferred-fsync alone hits the
~+22 % budget and writes are no longer the dominant apply cost, Step 4 may be deferred to a
later subphase; record the measured numbers either way.

### Commit
```
docs(redo-recovery): record 6d step-3 confirm-measure (mmap vs batch decision)
```

---

## Step 4 — mmap the frame log (the ~+25 % more on writes) — IF Step 3 requires it

**Goal:** `FrameLog` append = `memcpy` into a mapped region (no `pwrite` syscall / no
per-write file growth); reads from the map; `sync_to_durable` = `msync`.
**Files:** `wal_frame.rs` (+ `mmap.rs` wiring).
**Approach:** TDD; keep the lock-free atomic-offset append + 6a contiguous prefix + 6b salt/recycle.

### Tests to add
```rust
#[test] fn mmap_frame_append_read_round_trip() { /* ... */ }
#[test] fn mmap_frame_log_grows_under_concurrent_appends() { /* 8 threads */ }
#[test] fn mmap_recycle_resalts_and_invalidates_stale_offsets() { /* ... */ }
#[test] fn pwrite_written_wf_opens_and_reads_via_mmap() { /* compat */ }
```

### Implementation outline
```rust
// wal_frame.rs: FrameLog holds an mmap (memmap2) over the .wf, pre-grown in chunks.
//   append: reserve offset (atomic), if offset+FRAME_SIZE > mapped_len -> grow under grow_lock
//           (ftruncate + remap), then copy header+page into &mut map[offset..]; mark_written.
//   read_page_if_for: read header+page from &map[offset..]; verify page_id+salt+crc.
//   sync_to_durable: msync(contiguous prefix); advance durable_offset.
//   recycle: re-salt + reset write_offset + truncate/remap to FILE_HDR_SIZE.
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-storage
# A/B: expect COMMIT toward ~32ms (~+50% over redo-off).
```

### Commit
```
feat(redo-recovery): mmap the frame log — memcpy appends, no per-frame pwrite (6d step 4)
```

---

## Step 5 — Crash tests: NORMAL-deferred semantics

**Goal:** prove the NORMAL durability contract + that nothing regressed.
**Files:** `crates/axiomdb-storage/tests/` and/or `axiomdb-network/tests/integration_open_integrity.rs`.

### Tests to add
```rust
// NORMAL + crash WITHOUT checkpoint: the last unsynced txn(s) may be lost, but a txn whose
// frames were fsync'd at a prior checkpoint survives.
#[test] fn normal_deferred_crash_keeps_checkpointed_loses_unsynced_tail() { /* ... */ }
// NORMAL + checkpoint + crash: everything survives.
#[test] fn normal_deferred_commit_then_checkpoint_survives_crash() { /* ... */ }
```
Plus: T0 green, the 6c frame-only crash test green, `test_dirty_open_truncates_unlogged_tables_only` green.

### Verification
```bash
./tools/vm.sh test -p axiomdb-storage -p axiomdb-wal -p axiomdb-network
```

### Commit
```
test(redo-recovery): NORMAL-deferred frame-only crash semantics (6d step 5)
```

---

## Step 6 — A/B final + flip default + close

**Goal:** confirm the win, decide the default, close.
**Verification against spec done criteria:**
- [ ] A/B (insert_batch + autocommit, redo-on vs redo-off baseline) measurably faster (> noise)
- [ ] no read regression (`--compare` reads within noise of 6c)
- [ ] T0 / guard / frame-only-crash / NORMAL-deferred-crash tests green
- [ ] `./tools/vm.sh test --workspace` + clippy + fmt clean
- [ ] flip default ON (gated by subphase 7's full crash suite) OR documented as the remaining gate
- [ ] docs (`wal.md`, `transactions.md`, `performance.md`) + memory updated

### Final commit
```
feat(redo-recovery): complete subphase 6d — frame-log fast path

Implements specs/fase-redo-recovery/spec-subfase-6d-frame-log-fast-path.md
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| mmap-grow racing concurrent lock-free appends | medium | grow lock; appends within the map don't block; fallback = pwritev batch |
| Deferred fsync widens the power-loss window at NORMAL | by design | documented SQLite-NORMAL semantics; Strict available per-session |
| Inline checkpoint pauses commits | low | bounded (exclusive guard, 6b); one msync + one main fsync |
| Step 4 (mmap) unneeded / risky | low | Step 3 confirm-measure decides; can ship Step 1-2 win alone |

## Rollback plan

Steps default the redo mode **off** (no production change). Reverting = `git reset` the step
commits or set redo off. Each step is an isolated commit; Step 1-2 (the safe win) can ship
without Step 4.

## Estimated effort

Total ~3-5 days (max). Step 1 ~half-day, Step 2 ~half-day + A/B, Step 3 ~1-2h, Step 4 ~1-2 days
(mmap, the risky one), Step 5 ~half-day, Step 6 ~half-day.
