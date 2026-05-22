# Plan: non-truncating frame-log recycle (WAL-reuse)

Phase: redo-recovery (project B) — Lever 2 / Task 3, step T1
Spec: specs/fase-redo-recovery/spec-frame-log-reuse-recycle.md
Status: done

> **DONE (2026-05-22).** Step 1 `bdb0e6ac` (RecycleMode + non-truncating recycle +
> migrate callers/tests), steps 2-4 `135c8fef` (reuse-correctness + crash-idempotence
> tests + integration verify). storage 376/376, axiomdb-wal 216/216, network crash
> guardian green, clippy --workspace + fmt clean. **Next sprint task: T2** (chunked
> pre-growth) — option B agreed in brainstorm.

## Summary

Make `FrameLog::recycle` reuse the file in place by default. The implementation is
small — gate the existing `set_len(FILE_HDR_SIZE)` behind a new `RecycleMode::Truncate`
and keep the fresh-salt header rewrite + watermark reset for both modes — so the bulk of
this plan is the **test matrix** that proves reuse is correct and crash-safe. Order:
(1) introduce the API + behavior + migrate callers/tests + basic size/salt tests;
(2) lock the reuse-correctness invariants (round-trip, no stale-frame resurrection, torn
write, reopen); (3) prove crash-mid-recycle is an idempotent no-op at the engine level;
(4) integrate-verify against the spec's done criteria. Behavior details live in the spec.

## Dependencies

Must be done first:
- [x] spec-frame-log-reuse-recycle approved
- [x] 6f checkpoint trigger (the recycle caller) — done

Blocks (until this plan is done):
- [ ] Task 3 / T2 (chunked pre-growth — shares the file-size bookkeeping)
- [ ] Task 3 / T3 (truncate-on-shutdown — uses `RecycleMode::Truncate`)
- [ ] Task 3 / T4 (warmed batch benchmark — the win this enables)

## Affected files

Modified:
- `crates/axiomdb-storage/src/wal_frame.rs` — `RecycleMode` enum (pub) + `recycle(&self, mode)`;
  reuse/truncate tests in the `#[cfg(test)] mod tests`.
- `crates/axiomdb-storage/src/mmap.rs` — caller `checkpoint_frames` (~1070) → `recycle(RecycleMode::Reuse)`;
  crash-mid-recycle idempotence test in the test module.
- `crates/axiomdb-storage/src/fault_injection.rs` — IF its `checkpoint_frames` calls `frame_log.recycle()`,
  update to the new signature (verify in Step 1).
- `crates/axiomdb-storage/src/lib.rs` — re-export `RecycleMode` if `FrameLog`/`FrameRef` are re-exported (they are).

No new files (tests extend existing module test blocks, matching the codebase convention).

---

## Step 1 — `RecycleMode` + `recycle(mode)`: API, behavior, caller + test migration

**Goal:** introduce the mode, make `Reuse` skip the truncate, keep all existing tests green.
**Files:** `wal_frame.rs`, `mmap.rs`, `fault_injection.rs` (if needed), `lib.rs`.
**Approach:** TDD — write the basic distinction test first, then the minimal impl, then fix
the broken callers/tests.

### Test to add (`wal_frame.rs` tests)

```rust
#[test]
fn recycle_reuse_keeps_size_truncate_shrinks() {
    let (_dir, path) = tmp_wf();              // existing helper pattern
    let log = FrameLog::create(&path).unwrap();
    let salt0 = log.salt();
    // append a few frames so the file is > header
    for i in 0..4 { append_dummy_frame(&log, i); }   // helper: salt-stamped frame
    let grown = file_len(&path);
    assert!(grown > FILE_HDR_SIZE);

    // Reuse: file size preserved, scan empty, fresh salt.
    log.recycle(RecycleMode::Reuse).unwrap();
    assert_eq!(file_len(&path), grown, "reuse keeps the allocated blocks");
    assert!(log.scan().unwrap().is_empty(), "fresh salt ⇒ old frames stale");
    assert_ne!(log.salt(), salt0, "fresh salt on recycle");

    // Truncate: file shrinks to the header.
    for i in 0..4 { append_dummy_frame(&log, i); }
    log.recycle(RecycleMode::Truncate).unwrap();
    assert_eq!(file_len(&path), FILE_HDR_SIZE, "truncate reclaims disk");
    assert!(log.scan().unwrap().is_empty());
}
```

### Implementation outline (`wal_frame.rs`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecycleMode { Reuse, Truncate }

pub fn recycle(&self, mode: RecycleMode) -> Result<(), DbError> {
    let salt = fresh_salt();
    let mut hdr = [0u8; FILE_HDR_SIZE as usize];
    // ... build header (magic/version/page_size/salt/header_crc) — unchanged ...
    if mode == RecycleMode::Truncate {
        self.file.set_len(FILE_HDR_SIZE).map_err(|e| classify_io(e, "frame log recycle truncate"))?;
    }
    // Rewrite the header with the FRESH salt in BOTH modes — this is what makes the
    // leftover frames stale (Reuse) / matches the truncated file (Truncate).
    self.file.write_all_at(&hdr, 0).map_err(|e| classify_io(e, "frame log recycle header"))?;
    self.file.sync_all().map_err(|e| classify_io(e, "frame log recycle sync"))?;
    self.salt.store(salt, Ordering::Release);
    self.write_offset.store(FILE_HDR_SIZE, Ordering::Release);
    self.durable.store(FILE_HDR_SIZE, Ordering::Release);
    let mut s = self.sync_state.lock().unwrap_or_else(|e| e.into_inner());
    s.completed.clear();
    s.contiguous_written = FILE_HDR_SIZE;
    s.poison = None;
    Ok(())
}
```

Then fix the breakage from the signature change:
- `mmap.rs` `checkpoint_frames`: `frame_log.recycle()` → `frame_log.recycle(RecycleMode::Reuse)`.
- `fault_injection.rs`: if its `checkpoint_frames` recycles, same update (verify by grep first).
- `wal_frame.rs` `recycle_resets_to_empty_with_a_fresh_salt` (≈805): **read it** — if it asserts the
  file shrinks, retarget to `RecycleMode::Truncate`; if it only asserts scan-empty + fresh salt,
  pass `Truncate` (preserves its original intent) and rely on the new test above for `Reuse`.
- `lib.rs`: add `RecycleMode` to the `pub use wal_frame::{...}` re-export.

### Verification

```bash
./tools/vm.sh test -p axiomdb-storage wal_frame
./tools/vm.sh test -p axiomdb-storage checkpoint   # the 6b/6f tests still green
```

### Commit

```
feat(redo-recovery): T1 step 1 — RecycleMode + non-truncating frame-log recycle

Step 1 of specs/fase-redo-recovery/plan-frame-log-reuse-recycle.md
```

---

## Step 2 — Lock the reuse-correctness invariants

**Goal:** prove a reused log never serves stale data, across cycles and partial writes.
**Files:** `wal_frame.rs` tests.
**Approach:** TDD — these tests validate Step 1's behavior; each should pass once written
(if any fails, fix the impl before moving on — they ARE the correctness contract).

### Tests to add (`wal_frame.rs`)

```rust
#[test]
fn recycle_reuse_uses_a_fresh_salt_each_cycle() {
    // two Reuse recycles ⇒ three distinct salts.
}

#[test]
fn reuse_roundtrip_scan_sees_only_new_frames() {
    // append N, recycle(Reuse), scan empty + size unchanged, append M (M<N),
    // scan == M (stops at the first old-salt frame at H + M*FRAME_SIZE).
}

#[test]
fn reuse_does_not_resurrect_stale_frames_across_shrinking_cycles() {
    // append 10 → recycle → append 3 → recycle → append 5 ⇒ scan == 5;
    // none of the 10/3 leftover frames ever reappear (unique salts bound the prefix).
}

#[test]
fn torn_write_after_reuse_stops_scan_at_crc_not_leftover() {
    // after reuse + 2 good frames, corrupt frame 2's bytes (crc fail) ⇒ scan == 1,
    // even though a fully-written leftover old frame sits beyond it.
}

#[test]
fn reopen_after_reuse_reads_the_new_prefix() {
    // append N → recycle(Reuse) → append M → drop → FrameLog::open ⇒ scan == M,
    // write_offset/durable == H + M*FRAME_SIZE.
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-storage wal_frame
```

### Commit

```
test(redo-recovery): T1 step 2 — reuse-correctness invariants (salt/roundtrip/no-resurrection)

Step 2 of specs/fase-redo-recovery/plan-frame-log-reuse-recycle.md
```

---

## Step 3 — Crash-mid-recycle is an idempotent no-op (engine level)

**Goal:** prove the spec's crash-safety claim: a crash during a `Reuse` recycle leaves the
database consistent because the main file is already current and REDO is idempotent.
**Files:** `mmap.rs` tests (where the checkpoint/redo tests live; real-file 3-session pattern).
**Approach:** TDD — model "crash before the header rewrite" (old salt + old frames still
visible) and "crash after the header rewrite" (new salt → empty), assert REDO applies 0 and
the main page is correct in both.

### Tests to add (`mmap.rs`)

```rust
#[test]
fn crash_before_reuse_recycle_header_redoes_idempotently() {
    // Session A (frame-only redo): write a committed (txn 5) page, sync frame log,
    //   apply to main + flush (checkpoint_frames Reuse), so main is current.
    // Model "crash before the recycle header rewrite": the .wf still holds the old
    //   salt + the applied frames (i.e. do NOT recycle, or reopen the pre-recycle .wf).
    // Session B (reopen, frame-only): redo_committed_frames(|t| t==5) applies 0
    //   (pageLSN strict-> guard: main already has frame.lsn) and the page reads correct.
}

#[test]
fn after_reuse_recycle_reopen_log_is_empty_main_current() {
    // Session A: write committed page, checkpoint_frames(Reuse) ⇒ main current, log
    //   reused (salt bumped, scan empty, file size unchanged).
    // Session B: reopen ⇒ scan empty, redo applies 0, page reads correct.
}
```

> Note: this leans on the existing idempotence already shown by
> `redo_restores_a_page_whose_main_file_write_was_lost` (the "idempotent re-run is a no-op"
> assertion) — these tests make the *reuse* crash scenario explicit. No FaultInjection
> needed; the real-file reopen pattern (as in `checkpoint_restores_stale_main_then_recycles_the_log`)
> suffices.

### Verification

```bash
./tools/vm.sh test -p axiomdb-storage mmap
./tools/vm.sh test -p axiomdb-storage integration_checkpoint_trigger
```

### Commit

```
test(redo-recovery): T1 step 3 — crash-mid-recycle idempotence (reuse is crash-safe)

Step 3 of specs/fase-redo-recovery/plan-frame-log-reuse-recycle.md
```

---

## Step 4 — Integration verification + final

**Goal:** confirm the full spec is met and nothing cross-crate regressed.

### Verification against spec done criteria

- [ ] `RecycleMode { Reuse, Truncate }` + `recycle(&self, mode)`; `checkpoint_frames` passes `Reuse`.
- [ ] `Reuse` keeps file size; `scan` empty post-recycle; fresh salt; next append in place.
- [ ] Tests: fresh salt / round-trip / no-resurrection / torn-write / reopen / crash-idempotent / Truncate-shrinks.
- [ ] Existing recycle + 6b/6f checkpoint tests green under the new API.
- [ ] rustdoc on `RecycleMode` + the new `recycle` signature.

```bash
./tools/vm.sh test -p axiomdb-storage          # full storage crate
./tools/vm.sh test -p axiomdb-wal              # commit-path consumer
./tools/vm.sh test -p axiomdb-network integration_open_integrity   # server commit path
./tools/vm.sh clippy
./tools/vm.sh fmt-check                          # my files only
```

### Final commit

```
feat(redo-recovery): T1 — non-truncating frame-log recycle (WAL-reuse)

Implements specs/fase-redo-recovery/spec-frame-log-reuse-recycle.md
Plan: specs/fase-redo-recovery/plan-frame-log-reuse-recycle.md
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| An existing test asserts the file shrinks on recycle → breaks under `Reuse` | medium | Step 1 reads + retargets it to `Truncate`; the new test covers `Reuse` size-preserved |
| Stale frame validates after reuse (salt collision) | low | `fresh_salt()` is unique (nanos^counter); Step 2 asserts distinct salt per cycle |
| `recycle()` signature change ripples to an unseen caller | low | Step 1 greps all `recycle(` callers (mmap, fault_injection, tests) before compiling |
| Crash-mid-recycle modeling is unconvincing without a real crash hook | low | lean on the proven pageLSN idempotence; assert REDO applies 0 with main current |
| Torn 32-byte header on crash → reopen error | low | pre-existing (single-sector atomic); unchanged by T1; out of scope (future 2-header hardening) |

## Rollback plan

Each step is an isolated commit. To abandon: `git reset --hard <commit before step 1>`.
The `recycle` API change is the only non-test production change; reverting it restores the
always-truncate behavior with no data-format impact (the on-disk format is unchanged).

## Estimated effort

Total: ~1 day (impl high — crash-correctness validation, not code volume).
Per step: step 1 ~2h (API + migrate + basic tests), step 2 ~2h (invariant tests),
step 3 ~2h (crash idempotence tests), step 4 ~1h (integration verify + final).
