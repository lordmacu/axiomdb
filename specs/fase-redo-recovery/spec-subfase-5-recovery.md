# Spec: subphase 5 — recovery (REDO) → T0 GREEN

Phase: redo-recovery (project B) — subphase 5
Task: on open, REDO committed page frames so a committed-but-unflushed write survives
a power loss. Flips T0 from RED to GREEN — the proof that REDO works.
Status: approved
Effort: **max** (durability/recovery; data-loss surface).

## Context

Recovery today is **UNDO-only** (`CrashRecovery::recover`, recovery.rs:160 — rolls
back uncommitted txns, never REDOes committed ones). Subphases 2–4 built the physical
frame log (page images stamped with `txn_id` + `PageHeader.lsn`) and made it durable at
commit (`commit_durable` fsyncs it). This subphase adds the **REDO pass**: on open,
reconstruct committed page state from the frame log. After this, a committed write whose
data page never reached the main file is restored from its frame.

T0 (`integration_redo_recovery.rs`) drives this with `FaultInjectionStorage` (power-loss
sim: only `flush`'d data survives). For T0 to recover via the **physical** frame log
(Option A), `FaultInjectionStorage` gains a **durable frame log** (the real `FrameLog`,
whose fsync'd file survives the simulated crash while volatile data-pages are reverted).

## Goal

`recover` REDOes every committed transaction's frames into page state on open, so
committed data survives power loss — T0 passes.

## Non-goals (later subphases)

- Dropping the per-commit `storage.flush()` + frame-only writes — **subphase 6**
  (this subphase keeps the dual-write/flush net; redo is still opt-in).
- Checkpoint (frames → main file, recycle WAL), contiguous-durable-prefix — **subphase 6**.
- Full crash suite T1–T7 + perf A/B — **subphase 7** (this subphase = T0 only, plus an
  idempotent-replay test).
- Changing the logical UNDO path (stays as-is; hybrid = undo logical, redo physical).

## Behavior

### New `StorageEngine` method

```rust
/// REDO: apply every committed frame to its page so committed data survives a crash.
/// For each page with a committed frame (latest wins), if `frame.lsn > page.lsn`
/// (idempotence guard) write the frame's bytes to the page directly — WITHOUT
/// re-appending a frame. `is_committed(txn_id)` selects committed frames (recovery
/// passes the logical WAL's committed-txn set). Returns the number of pages redone.
/// Default: no-op (backend without a redo log). Called once, on open, after UNDO.
fn redo_committed_frames(&self, is_committed: &dyn Fn(u64) -> bool) -> Result<usize, DbError> {
    let _ = is_committed;
    Ok(0)
}
```

`MmapStorage` and `FaultInjectionStorage` implement it:
- `frame_log.build_index(is_committed)` → latest committed frame per page.
- for each: read on-disk page; if `frame.lsn > page.lsn`, write the frame's page bytes
  **directly** (MmapStorage: `pwrite` + buffer-pool invalidate, NOT `write_page` which
  would append another frame; FaultInjectionStorage: write into both `current` and
  `durable` layers so the redone state persists).

### `FaultInjectionStorage` gains a durable frame log

- `enable_redo_log(db_path)` opens/creates a real `FrameLog` (tempdir file).
- `write_page` (redo on) appends a frame stamped with `current_txn` (like `MmapStorage`).
- `sync_frame_log` fsyncs it; `set_current_txn`/`current_txn` via the same thread-local.
- **`simulate_power_loss` reverts the data-page layers (current→durable) but does NOT
  touch the frame-log file** — the fsync'd frames survive, modeling real durability.

### `recover` integration (recovery.rs:160)

- During the forward scan, collect the **committed-txn set** (insert `entry.txn_id` on
  `EntryType::Commit`) — alongside the existing `active_txns`/`max_committed`.
- After the existing logical UNDO pass, call
  `storage.redo_committed_frames(&|t| committed.contains(&t))`.
- Order: **UNDO (logical, uncommitted) then REDO (physical, committed)** — for T0 there
  are no uncommitted txns, so order is moot here; the general committed+uncommitted
  same-page interaction is exercised in subphase 7.

### T0 changes (un-ignore → green)

`enable_redo_log` on the `FaultInjectionStorage`; `set_current_txn(conn.txn_id)` before
the page write; `commit_durable(conn, &storage)` instead of `commit`; remove `#[ignore]`.

### Semantics

- Precondition: the frame log holds the committed txn's fsync'd frames; data pages may
  be stale/lost.
- Postcondition: every page with a committed frame whose `lsn` exceeds the on-disk
  page's `lsn` reflects the frame's bytes; `read_page` returns committed data.
- Invariant (idempotence): re-running `redo_committed_frames` is a no-op for already-
  redone pages (`frame.lsn > page.lsn` is then false) — safe to crash mid-recovery.

### Error cases

| Input | Expected | Notes |
|-------|----------|-------|
| frame CRC fails on read during redo | `DbError` (propagated) | torn frame beyond the valid prefix is already excluded by `scan` |
| redo on a storage without a frame log | `Ok(0)` | default no-op |

## Edge cases

- [ ] **T0**: committed insert, power loss, recover → row restored (the headline).
- [ ] **Idempotent replay**: run `redo_committed_frames` twice → second is a no-op
      (pageLSN guard); models a crash *during* recovery.
- [ ] **Uncommitted txn at crash**: its frames are excluded (predicate false) and the
      logical UNDO still rolls it back — no regression (existing dirty-open test green).
- [ ] **Empty frame log / redo disabled**: `recover` behaves exactly as today.
- [ ] **Frame older than the page** (`frame.lsn <= page.lsn`): skipped (already applied).

## Performance budget

Recovery is a one-time open cost (not the hot path). REDO is O(committed frames); the
sharded `WalIndex` build is reused. No new lock on the live path. No read/write hot-path
change in this subphase.

## Dependencies

- Depends on: subphase 4 (per-frame `txn_id`, `commit_durable`, `build_index(predicate)`).
- Blocks: subphase 6 (safe to drop the per-commit flush once REDO is proven).

## Open questions

- [ ] Does `redo_committed_frames` also rebuild the live `WalIndex` for post-recovery
      reads, or is applying-to-pages enough? (Applying is enough for T0/subphase 5;
      revisit if subphase 6 frame-only reads need the live index after open.)

## Done criteria

- [ ] `StorageEngine::redo_committed_frames` (default no-op) + impls on `MmapStorage`
      and `FaultInjectionStorage`.
- [ ] `FaultInjectionStorage` has a durable frame log that survives `simulate_power_loss`.
- [ ] `recover` collects the committed-txn set and calls `redo_committed_frames`.
- [ ] **T0 is GREEN** (un-`#[ignore]`d) — committed insert survives power loss.
- [ ] Idempotent-replay test green; existing UNDO/dirty-open tests green (no regression).
- [ ] `./tools/vm.sh test --workspace` + clippy (storage/wal) + fmt clean; docs-site
      (`wal.md`) + memory updated.

## References

- `specs/fase-redo-recovery/spec-subfase-4-commit-boundary.md` (per-frame txn_id).
- `crates/axiomdb-wal/src/recovery.rs` `recover()` (L160), `tests/integration_redo_recovery.rs`
  (T0), `crates/axiomdb-storage/src/{fault_injection.rs,wal_frame.rs,mmap.rs,engine.rs}`.
- External: ARIES REDO (redo-all then undo-losers; idempotence via pageLSN) — we redo
  committed-only physically + undo uncommitted logically (hybrid).
