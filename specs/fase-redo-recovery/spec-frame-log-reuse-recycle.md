# Spec: non-truncating frame-log recycle (WAL-reuse)

Phase: redo-recovery (project B) — Lever 2 / **Task 3, step T1** (frame-log file reuse)
Task: make `FrameLog::recycle` reuse the file's allocated blocks instead of truncating
Status: approved

## Context

Under `RedoMode::FrameOnly`, every page write appends a frame to `<db>.wf`
(`crates/axiomdb-storage/src/wal_frame.rs`), and the 6f background checkpointer
(`MmapStorage::checkpoint_frames`, mmap.rs:1043) periodically applies committed frames to
the main file and **recycles** the log. Today `FrameLog::recycle` (wal_frame.rs:445)
`set_len(FILE_HDR_SIZE)`s the file — it **frees every block**, so the next batch re-grows
the file from zero (the measured per-batch block-allocation cost; redo-on batch is a wash
because of it). SQLite's WAL avoids this: on checkpoint-restart it **bumps the salt and does
not truncate** (`research/sqlite/src/wal.c`, default PERSIST mode), so blocks stay allocated
and the next write overwrites in place. T1 ports that.

## Goal

`FrameLog::recycle` reuses the file in place by default (bump salt + reset watermarks,
**keep the file size**), with an explicit truncating mode for disk reclaim.

## Non-goals

- **Chunked pre-growth** of the file — Task 3 / T2.
- **Truncate-on-shutdown wiring** (routing the final/shutdown checkpoint to the truncating
  mode) — Task 3 / T3 (T1 only *provides* the truncating mode).
- **The warmed/sustained batch benchmark** that demonstrates the steady-state win — Task 3 / T4.
- **Defense-in-depth zeroing** of the first stale frame on recycle — not needed (crash safety
  comes from idempotent redo, see Semantics); explicitly rejected to avoid extra I/O.
- **A second on-disk header copy** to survive a torn 32-byte header write — pre-existing
  single-header limitation, unchanged by T1 (out of scope; possible future hardening).

## Behavior

### Public API

```rust
/// How a frame-log recycle treats the file's already-allocated blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecycleMode {
    /// Keep the file at its current (high-water) size: bump the salt + reset the
    /// watermarks only. Old frames become stale via salt mismatch and the next append
    /// overwrites them in place (SQLite PERSIST WAL-reuse). The runtime default — avoids
    /// freeing + re-allocating blocks every checkpoint cycle.
    Reuse,
    /// As `Reuse`, then truncate the file to the header to reclaim disk (SQLite TRUNCATE).
    /// For the shutdown / final checkpoint so the log is small at rest.
    Truncate,
}

impl FrameLog {
    /// Recycle the log after a checkpoint. Caller MUST hold the exclusive checkpoint
    /// guard (no concurrent appends). Does NOT touch the engine's monotonic frame-LSN
    /// counter (a post-recycle frame must still out-rank the on-disk page lsn).
    pub fn recycle(&self, mode: RecycleMode) -> Result<(), DbError>;
}
```

`MmapStorage::checkpoint_frames` calls `frame_log.recycle(RecycleMode::Reuse)` (runtime).
The `Truncate` mode has no production caller in T1 (T3 wires the shutdown path).

### Semantics

- **Precondition:** the caller holds the exclusive checkpoint write-guard
  (`checkpoint_lock.write()`), so no append runs concurrently — same as today. By the
  checkpoint contract, every live frame has already been applied to the main file **and the
  main file has been fsync'd** before `recycle` is called.
- **Postcondition (`Reuse`):** a **fresh, unique** salt is written to the file header and
  fsync'd; `write_offset`, `contiguous_written`, `durable` are all reset to `FILE_HDR_SIZE`;
  `completed` is cleared; `poison` is cleared; **the file length is unchanged**;
  `scan()` returns an empty list (the first on-disk frame now carries the previous salt).
- **Postcondition (`Truncate`):** as `Reuse`, and the file is truncated to `FILE_HDR_SIZE`.
- **Invariant:** each recycle uses a salt distinct from every prior run's salt
  (`fresh_salt()` = wall-clock nanos XOR a process-local counter), so a frame written in any
  earlier cycle fails the salt check and bounds the valid prefix.

**Crash-safety argument (why `Reuse` is as safe as `Truncate`).** Correctness does not
depend on what `recycle` does — it depends on the checkpoint ordering
(`apply committed frames → fsync main → recycle`). The main file is durable with every
committed frame applied *before* `recycle` runs. So:

- Crash **before** the header rewrite lands ⇒ on reopen the header still holds the old salt,
  so `scan` sees the old frames as "valid" and recovery REDOes them — but each is applied to
  a main page whose `lsn` already equals the frame's lsn, so the strict-`>` pageLSN guard
  skips it (idempotent no-op). Main is correct.
- Crash **after** the header rewrite lands (new salt) ⇒ `scan` stops at the first frame
  (old salt) → empty log → recovery REDOes nothing. Main is correct.

Either way the database is consistent; the only difference is whether recovery re-scans some
already-applied frames (a harmless no-op). `recycle` is therefore a best-effort
log-bounding operation, not a durability-critical one.

### Error cases

| Input / situation | Expected error | Notes |
|---|---|---|
| header `pwrite` fails | `DbError` via `classify_io(_, "frame log recycle header")` | file unchanged-ish; caller propagates |
| header `fsync` fails | `DbError` via `classify_io(_, "frame log recycle sync")` | salt change may not be durable → safe (idempotent redo) |
| `set_len` fails (`Truncate` only) | `DbError` via `classify_io(_, "frame log recycle truncate")` | reuse path never calls `set_len` |

## Edge cases

- [ ] **Reuse round-trip:** write N frames → `recycle(Reuse)` → file length unchanged, `scan()`
  empty, salt changed; then append M<N frames → `scan()` returns exactly M (stops at the first
  old-salt frame at offset `FILE_HDR_SIZE + M*FRAME_SIZE`).
- [ ] **Fresh salt per recycle:** salt after recycle ≠ salt before (and ≠ across two recycles).
- [ ] **Stale frames never resurrect:** after several `Reuse` cycles with shrinking write
  counts, `scan` always stops at the current cycle's contiguous-write boundary (unique salts).
- [ ] **Torn new write after reuse:** a torn frame in the new prefix ends `scan` at the crc
  failure, never at a leftover old frame beyond it.
- [ ] **Reopen after reuse + partial writes:** `FrameLog::open` (re-scans to end, wal_frame.rs:287)
  recovers exactly the new valid prefix; watermarks set to its end.
- [ ] **Crash mid-recycle (header not yet rewritten):** reopen → old frames visible → recovery
  REDO is idempotent (pageLSN strict-`>`) → main unchanged/correct.
- [ ] **Empty-log recycle:** no frames present → salt bumped, watermarks at `FILE_HDR_SIZE`,
  file length unchanged (`Reuse`) / already minimal (`Truncate`); a follow-up `scan` is empty.
- [ ] **`Truncate` mode:** file length drops to `FILE_HDR_SIZE`.
- [ ] **No in-flight frames at recycle:** `checkpoint_frames` only recycles when every frame is
  committed (existing guard, mmap.rs:1069), so the watermark reset to `FILE_HDR_SIZE` never
  drops an uncommitted in-flight frame.

## On-disk format (if applicable)

Unchanged. The 32-byte file header (`magic | version | page_size | salt | header_crc | _pad`)
and the 36-byte frame header are identical to subphase 2/6b. The only behavioral change:
**after a `Reuse` recycle the file retains stale frames beyond the valid prefix** (previous
salt). `scan` already terminates the valid prefix at the first salt mismatch (wal_frame.rs:540),
so readers/recovery are unaffected. Compatibility: a `.wf` written by the old (always-truncate)
recycle is still readable; a `.wf` left large by a `Reuse` recycle is readable by any version
whose `scan` honors the salt boundary (all current versions do).

## Performance budget

| Operation | Target | Notes |
|---|---|---|
| `recycle(Reuse)` | 1 header `pwrite` + 1 `fsync`, **no `set_len`** | O(1); removes the block free/re-alloc that re-grows the log next cycle |
| `recycle(Truncate)` | `Reuse` + 1 `set_len` | shutdown/disk-reclaim only |
| steady-state `insert_batch` (redo on) | no per-cycle log re-growth | the measurable win is T4 (warmed bench); T1 only *enables* it |

Reference: the redo-on batch wash (~+18ms/50K batch) is dominated by log re-growth after
truncate; T1 removes the truncate so steady-state batches reuse allocated blocks.

## Dependencies

- Depends on: 6f `checkpoint_frames` (the recycle caller), 6a contiguous-written/durable
  watermarks, the salt-based `scan`, the pageLSN strict-`>` redo guard (idempotence).
- Blocks: T2 (chunked pre-growth shares the file-size bookkeeping), T3 (truncate-on-shutdown
  uses `RecycleMode::Truncate`), T4 (the win it enables).

## Open questions

- [x] **API shape:** `RecycleMode` enum + `recycle(mode)` (chosen over two methods —
  one method, explicit intent, extensible). Caller `checkpoint_frames` passes `Reuse`.
- [x] **Defense-in-depth zeroing of the first stale frame?** No — idempotent redo makes it
  unnecessary; skip the extra I/O.
- [ ] **(plan-time)** Do any existing tests assert the post-recycle *file length* shrinks?
  If so, retarget them at `RecycleMode::Truncate` (the reuse default no longer shrinks).

## Done criteria

- [ ] `RecycleMode { Reuse, Truncate }` + `FrameLog::recycle(&self, mode)` exist; `checkpoint_frames`
  passes `RecycleMode::Reuse`.
- [ ] `Reuse` keeps the file length unchanged; `scan()` is empty immediately after; the salt is
  fresh; the next append writes at `FILE_HDR_SIZE` (in place).
- [ ] Test: fresh, distinct salt on each recycle.
- [ ] Test: reuse round-trip (write N → recycle(Reuse) → append M → scan == M).
- [ ] Test: stale frames beyond the new prefix never validate across multiple reuse cycles.
- [ ] Test: reopen (`FrameLog::open`) after a reuse-recycle + partial writes reads the right prefix.
- [ ] Test: crash-mid-recycle modelled (reopen with the old header) → recovery REDO is an
  idempotent no-op, main unchanged (via the existing 3-session reopen pattern / FaultInjection).
- [ ] Test: `Truncate` mode shrinks the file to `FILE_HDR_SIZE`.
- [ ] Existing recycle tests (`recycle_resets_to_empty_with_a_fresh_salt`, the 6b checkpoint
  tests) pass under the new API (retargeted to a mode where they assert file length).
- [ ] `cargo nextest run -p axiomdb-storage` + the wal/network suites green (Lima); `cargo clippy
  --workspace -- -D warnings` clean; `cargo fmt --check` clean.
- [ ] rustdoc on `RecycleMode` and the new `recycle` signature.

## References

- `crates/axiomdb-storage/src/wal_frame.rs` — `recycle` (445), `scan` (525), `open` (287), `fresh_salt` (582)
- `crates/axiomdb-storage/src/mmap.rs` — `checkpoint_frames` (1043, the recycle caller)
- `specs/fase-redo-recovery/spec-subfase-6b-checkpoint.md` — the checkpoint apply→fsync→recycle order
- `specs/fase-redo-recovery/plan-subfase-6f-frame-checkpoint-trigger.md` — the 6f trigger that drives recycle
- `research/sqlite/src/wal.c` — salt-increment-on-restart, PERSIST vs TRUNCATE WAL modes
- `docs/checkpoint-redo-recovery.md` / `memory/project_insert_perf.md` — the Task 3 framing + the ~10% honest estimate
