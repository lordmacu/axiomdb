# Spec: subphase 6b — frame checkpoint (frames → main file, recycle the log)

Phase: redo-recovery (project B) — subphase 6b (second of the 6a/6b/6c split)
Task: a **frame checkpoint** that applies committed page frames to the main DB file
(pageLSN-guarded), fsyncs it, advances the checkpoint LSN, and recycles the frame log —
bounding the log and making the main file the durable home of checkpointed pages.
Status: approved
Effort: **max** (data-mover; checkpoint-vs-writers concurrency; recovery interaction).

## Context

After subphase 5, recovery REDOes committed frames; after 6a, the frame log's durable
prefix is gap-free under concurrent appends. Writes are still **dual-write** (main file +
frame log) with the per-commit main-file flush as the durability net (redo opt-in). The
frame log therefore grows without bound and nothing recycles it. This subphase adds the
**checkpoint**: the only thing that will move frames → main once 6c makes writes
frame-only. It is the prerequisite for 6c (frame-only + dropping the per-commit flush).

Reference: SQLite `sqlite3WalCheckpoint`/`walCheckpoint` (`research/sqlite/src/wal.c`) —
sort+dedup the latest frame per page, **fsync the WAL before copying** (barrier), copy
each page's latest frame into the db file, **fsync the db after**, then reset/truncate the
WAL. Existing in-house: `Checkpointer::checkpoint` (`crates/axiomdb-wal/src/checkpoint.rs`,
the *logical*-WAL checkpoint: `storage.flush()` + a `Checkpoint` entry + checkpoint LSN in
the meta page) and `MmapStorage::redo_committed_frames` (subphase 5 — already applies
committed frames to main with the `frame.lsn > page.lsn` guard).

**Additive subtlety:** while dual-write is on, `write_page` stamps `page.lsn = frame.lsn`
on the main file, so the checkpoint's *apply* is almost always a no-op (`frame.lsn ==
page.lsn` ⇒ skipped). The genuinely new, risky parts are **(a) recycling the log** (reset
without losing committed-but-un-applied frames or concurrent appends) and **(b) the
grow-on-redo** case deferred from subphase 5. Both are exercised via a stale-main scenario
(clobber the main page, checkpoint → restored) rather than the dual-write happy path.

## Goal

Provide a checkpoint that makes the main file reflect all committed frames up to a point,
fsyncs it, advances the checkpoint LSN, and recycles the frame log — so the log stays
bounded and recovery after a checkpoint has nothing (or little) to REDO.

## Non-goals

- Frame-only writes / dropping the per-commit main-file flush — **subphase 6c** (this
  subphase stays additive: dual-write + per-commit flush remain; redo opt-in).
- A background checkpoint thread / automatic scheduling tuning — this subphase exposes a
  **size/clean-shutdown/manual trigger**; a dedicated background scheduler is later.
- Retiring `doublewrite` — keep it (torn-page repair) until frame-replay is proven; a
  separate decision (subphase 7).
- Changing recovery's REDO/UNDO semantics — only the checkpoint LSN start point shifts.
- On-disk page/frame format change — none (checkpoint LSN already lives in the meta page).

## Behavior

### New `StorageEngine` method

```rust
/// CHECKPOINT: apply every committed frame to the main file (pageLSN-guarded, growing
/// the file for a page beyond EOF), fsync the main file, then recycle the frame log
/// (truncate to its header with a fresh salt; reset the offset watermarks — but NOT the
/// monotonic frame LSN counter). `is_committed(txn_id)` selects committed frames.
/// Returns the number of pages written to the main file. Default: no-op (no redo log).
///
/// Concurrency: takes an exclusive checkpoint guard that briefly excludes new frame
/// appends/commits while it snapshots the log, applies, fsyncs, and resets. Reads are
/// unaffected. Called by the checkpoint trigger, never on the per-write hot path.
fn checkpoint_frames(&self, is_committed: &dyn Fn(u64) -> bool) -> Result<usize, DbError> {
    let _ = is_committed;
    Ok(0)
}
```

`MmapStorage` (and `FaultInjectionStorage`, the test vehicle) implement it. The apply
reuses the subphase-5 logic (`build_index(is_committed)` → for each page, if
`frame.lsn > page.lsn` write the frame bytes to the page, **growing the main file** if the
page is beyond EOF), then fsyncs the main file, then recycles the log.

### Ordering invariant (MUST NOT be violated)

Mirrors `checkpoint.rs` + SQLite's barriers:

```text
1. acquire the exclusive checkpoint guard (new appends/commits wait)
2. snapshot the durable prefix D = frame_log.durable_offset()   (only fsync'd frames)
3. apply every committed frame in [HDR, D) to the main file (pageLSN guard; grow if needed)
4. storage.flush()      ← fsync the main file: applied pages are durable
5. advance checkpoint_lsn in the meta page (so recovery starts after this point)
6. storage.flush()      ← meta page durable
7. recycle the frame log: truncate to FILE_HDR_SIZE with a FRESH salt; reset
   write_offset / contiguous_written / durable = FILE_HDR_SIZE (keep the frame LSN counter)
8. release the guard
```

A crash between any two steps is safe: before step 4 the main file is unchanged (recovery
REDOes from the previous checkpoint); after step 4 but before step 7 the frames are still
in the log (recovery re-applies them idempotently — pageLSN guard); after step 7 the log
is empty and the main file + checkpoint LSN agree.

### Why keep the frame LSN counter monotonic across a recycle

The main file's `PageHeader.lsn` persists across the recycle. A frame appended *after* the
recycle must have `lsn >` the main page's lsn or the REDO/checkpoint guard would wrongly
skip it. So step 7 resets the file offsets + salt but **never** the `frame_lsn` counter —
it stays monotonic for the DB's lifetime (or at least strictly above any on-disk page lsn).

### Trigger policy

- **Size**: after a commit, if `frame_log` length (≈ `write_offset`) exceeds a threshold
  (config, default e.g. 64 MB of frames), schedule a checkpoint. In 6b run it inline on
  the committing thread when over threshold (a background thread is later).
- **Clean shutdown**: checkpoint on `Database` drop/close so a clean reopen has an empty log.
- **Manual**: a public `Database::checkpoint()` / the existing checkpoint entry point.

### Grow-on-redo (the subphase-5 DEFERRED case)

When a committed frame's `page_id` is ≥ the main file's page count (the page was allocated
after the last main-file flush and lost), the checkpoint **grows the main file** to cover
`page_id` before writing the frame bytes. The freelist bitmap is itself a page with frames,
so its committed frames replay too — no separate allocation log.

### Semantics

- Precondition: the frame log holds committed frames (their txns committed in the logical
  WAL); `is_committed` reflects that set.
- Postcondition: every page with a committed frame in the checkpointed prefix is in the
  main file and fsync'd; the checkpoint LSN is advanced; the frame log is reset (empty,
  fresh salt) so `scan` returns nothing until the next append.
- Invariant (idempotence): re-running the checkpoint (or a crash mid-checkpoint) re-applies
  only frames newer than the on-disk page (`frame.lsn > page.lsn`) — safe.

### Error cases

| Input | Expected | Notes |
|-------|----------|-------|
| main-file `flush` (fsync) fails | `Err`; do NOT recycle the log (frames remain) | log is the source of truth until main is durable |
| a committed frame for a page beyond EOF | grow the main file, then apply | grow-on-redo |
| checkpoint while another checkpoint runs | second waits on the guard, then no-ops (log already reset) | exclusive guard |
| redo log disabled | `Ok(0)` | default no-op |

## Edge cases

- [ ] **Stale-main apply + recycle**: write a committed frame, clobber the main page to an
      older lsn, checkpoint → main page restored AND log reset (scan empty afterward).
- [ ] **Grow-on-redo**: committed frame for a page beyond the main EOF → file grows, page applied.
- [ ] **Idempotent / crash mid-checkpoint**: apply twice → second is a pageLSN no-op; a
      crash before recycle → recovery re-applies, then a later checkpoint recycles.
- [ ] **Concurrent writers**: appends/commits during a checkpoint either complete before
      the guard or wait; no committed frame is dropped by the recycle (only the snapshotted
      prefix [HDR, D) is applied+truncated; frames beyond D, if any, are preserved/handled).
- [ ] **Uncommitted frames in the log at checkpoint**: excluded by `is_committed` — never
      applied to the main file.
- [ ] **Empty log / redo disabled**: `Ok(0)`, no main-file change.

## Open questions

- [x] **Checkpoint-vs-writers concurrency model — RESOLVED → A (exclusive checkpoint
      guard)** (user-confirmed 2026-05-21).
  - **A (recommended): exclusive checkpoint guard.** A checkpoint takes a lock that briefly
    blocks new frame appends + commits while it snapshots/applies/fsyncs/recycles. Simplest
    and obviously correct; checkpoints are infrequent so a short pause is acceptable
    (SQLite's TRUNCATE checkpoint similarly needs the write lock). Truncates the whole log.
  - **B: snapshot prefix + preserve the tail.** Apply+truncate only `[HDR, D)`; shift/keep
    frames appended after `D`. No writer pause, but needs log compaction/wrap-around (SQLite
    `-shm` reader marks) — much more complex. Defer to a later perf pass.
  Recommendation: **A** for 6b; revisit B in subphase 7 if the pause shows up in the perf A/B.
- [ ] Checkpoint-LSN coordination with the **logical** WAL: reuse `Checkpointer::checkpoint`
      for steps 5–6, or fold the meta-page LSN write into `checkpoint_frames`? (Plan decision.)
- [ ] Trigger threshold default + whether the inline (over-threshold) checkpoint is
      acceptable latency in 6b or must be deferred to a background thread now. (Plan.)

## Performance budget

Checkpoint is an infrequent, off-hot-path operation. The exclusive guard (Option A) adds a
bounded pause to commits that race a checkpoint (target: a checkpoint of a 64 MB log is
dominated by the main-file fsync, tens of ms). No change to `read_page`/`write_page`
throughput between checkpoints. Additive subphase ⇒ no production durability/perf change yet
(the autocommit win lands in 6c).

## Dependencies

- Depends on: subphase 5 (`redo_committed_frames` apply + pageLSN guard) and 6a
  (gap-free `durable_offset` — the checkpoint snapshots it).
- Blocks: subphase 6c (frame-only writes need a checkpoint to drain the log to main).

## Done criteria

- [ ] `StorageEngine::checkpoint_frames` (default no-op) + `MmapStorage` +
      `FaultInjectionStorage` impls, following the ordering invariant.
- [ ] Grow-on-redo: a committed frame beyond the main EOF grows the file and applies.
- [ ] Log recycle: after checkpoint the frame log is reset (fresh salt, empty scan), the
      frame LSN counter is preserved (monotonic), checkpoint LSN advanced.
- [ ] Stale-main apply + recycle test, grow-on-redo test, idempotent/crash-mid-checkpoint
      test, concurrent-writers test green; existing recovery + T0 + 6a suites green.
- [ ] `./tools/vm.sh test --workspace` + clippy (storage/wal) + fmt clean; docs-site
      (`wal.md` checkpoint subsection) + memory updated.

## References

- `crates/axiomdb-wal/src/checkpoint.rs` (`Checkpointer::checkpoint`, the ordering invariant).
- `crates/axiomdb-storage/src/{mmap.rs,wal_frame.rs,fault_injection.rs}`
  (`redo_committed_frames`, `build_index`, `read_page_at`, `durable_offset`, salt/reset).
- `specs/fase-redo-recovery/spec-subfase-{5-recovery,6a-contiguous-prefix}.md`.
- External: SQLite `wal.c` `walCheckpoint`/`walCheckpointOnePass`/`walRestartHdr`
  (`research/sqlite/src/wal.c`) — sort+dedup, two-phase fsync barrier, WAL reset.
