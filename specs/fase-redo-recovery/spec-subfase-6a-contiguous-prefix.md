# Spec: subphase 6a — contiguous durable prefix for the multi-writer frame log

Phase: redo-recovery (project B) — subphase 6a (first of the 6a/6b/6c split)
Task: make the page-frame log's durable point **gap-free** under concurrent lock-free
appends, so a committed frame is never lost behind a hole after a crash.
Status: approved
Effort: **max** (concurrency + durability; data-loss surface).

## Context

`FrameLog::append` (`crates/axiomdb-storage/src/wal_frame.rs`) is **fully lock-free**:
it reserves a fixed-size slot with `write_offset.fetch_add(FRAME_SIZE)` and then
`pwrite`s the frame at that offset — two appenders write disjoint regions with no lock.
`scan()` reads frames from the start and **stops at the first frame whose salt ≠ the run
or whose CRC fails** (the valid prefix = the contiguous run of good frames).

This is safe today only because durability still comes from the per-commit main-file
`flush()` (frames are not yet the durability source — redo is opt-in/additive through
subphase 5). Subphase 6c will make frames the **sole** durability mechanism. Before that
switch, this subphase fixes a latent multi-writer hole.

**The hole (gap behind a written frame).** Two appenders interleave:

```
thread A: offset = fetch_add → O_A          (reserved, not yet pwritten)
thread B: offset = fetch_add → O_B = O_A+F  ; pwrite(O_B) done ; commit fsyncs
*** power loss *** (A never pwrote O_A)
on reopen: scan() reaches O_A → bytes are zeros → salt/CRC fail → STOP.
           B's frame at O_B is beyond the gap → EXCLUDED → committed data lost.
```

`ConcurrentWalWriter` (the logical WAL) does **not** have this hole: its LSN reservation
is lock-free but the actual write+fsync runs under a `Mutex<WriterState>` leader, so bytes
land in order. The frame log traded that for a fully-lock-free `pwrite` (subphase 3, by
explicit user preference for scalability) — which is why it needs an explicit
contiguous-prefix mechanism.

**Research grounding (`research/postgres/src/backend/access/transam/xlog.c`):** PostgreSQL
separates two monotonic watermarks — `LogwrtResult.Write` (contiguously written) and
`.Flush` (fsync'd) — and `XLogFlush(lsn)` first calls `WaitXLogInsertionsToFinish(lsn)`
(wait for every in-flight insertion below `lsn` to complete, so it never flushes past a
gap), then fsyncs, then advances `.Flush`; it returns only once `.Flush ≥ lsn`. We port
this Write/Flush separation to fixed-size frame offsets.

## Goal

When a commit's frame-log sync returns Ok, every frame reserved before that call is
**contiguously written and fsync'd** — so a crash can never leave a committed frame behind
a hole in the valid-prefix scan.

## Non-goals

- Dropping the per-commit main-file flush / frame-only writes / enabling redo in
  production — **subphase 6c** (this subphase is additive; redo stays opt-in).
- Checkpoint (frames → main, recycle the log) — **subphase 6b**.
- Changing `scan()` / `build_index()` / recovery semantics — they already stop at the
  first gap; this subphase guarantees no *committed* frame sits beyond that gap.
- A fully-lock-free reorder watermark micro-optimization — the plan picks the simplest
  correct mechanism; lock-free refinement is a follow-up only if it contends.
- On-disk frame format change — none (the durable prefix is in-memory runtime state,
  recomputed by `scan()` on open).

## Behavior

### New `FrameLog` state + API

Two in-memory monotonic watermarks (offsets into the log file), mirroring PG Write/Flush:

```rust
impl FrameLog {
    /// Highest offset W such that EVERY frame slot in [FILE_HDR_SIZE, W) has finished
    /// its pwrite — i.e. the gap-free written prefix. Advances as concurrent appends
    /// complete (out-of-order completions are reordered into this contiguous point).
    pub fn contiguous_written_offset(&self) -> u64;

    /// Highest offset that has been fsync'd (≤ contiguous_written_offset). The durable
    /// point: a crash preserves exactly the frames in [FILE_HDR_SIZE, durable).
    pub fn durable_offset(&self) -> u64;

    /// Make every frame reserved before this call durable, gap-free:
    /// 1. snapshot `target = write_offset` (end of the last reserved frame),
    /// 2. wait until `contiguous_written_offset() >= target` (all in-flight appends
    ///    below `target` have finished their pwrite — no hole),
    /// 3. fsync the file (`sync_data`),
    /// 4. advance `durable_offset` to at least `target`.
    /// Returns once `durable_offset() >= target`. Idempotent / coalesces concurrent
    /// callers (a later caller whose `target` is already ≤ durable returns immediately).
    pub fn sync_to_durable(&self) -> Result<(), DbError>;
}
```

`append` is unchanged on its hot path (lock-free reserve + `pwrite`), but on completion it
**records that its slot finished** so `contiguous_written_offset` can advance over the
contiguous run. The exact tracker (a lock-free CAS watermark vs a `Mutex` over a small
"completed but not yet contiguous" set guarding **only the bookkeeping**, never the 16 KB
`pwrite` or the fsync) is a plan decision; the append's I/O must stay concurrent.

### `sync()` → `sync_to_durable`

`MmapStorage::sync_frame_log` and `FaultInjectionStorage::sync_frame_log` (called by
`TxnManager::commit_durable` at the commit boundary) now call `sync_to_durable()` instead
of the plain `sync()` (`file.sync_data()`). The plain `sync()` may stay as a private
primitive used inside `sync_to_durable`.

### Semantics

- **Precondition:** the committing thread appended its frames (via `write_page`) before
  calling the commit path; those frames have offsets `< write_offset` at sync time.
- **Postcondition (the invariant):** after `sync_to_durable()` returns Ok, for every
  offset `o < target`, the frame slot at `o` is fully written and fsync'd. Therefore
  `scan()` after a crash returns a valid prefix that **includes every frame reserved
  before any returned commit** — no committed frame is lost behind a gap.
- **Invariant (monotonic):** `durable_offset ≤ contiguous_written_offset ≤ write_offset`,
  and all three are monotonically non-decreasing within a run.
- **Harmlessly over-syncs:** `target` is the whole reserved end, so an uncommitted txn's
  in-flight frame below `target` is also fsync'd. That is safe — recovery's
  `build_index(committed)` already excludes frames whose txn never committed.

### Error cases

| Input | Expected | Notes |
|-------|----------|-------|
| a frame `pwrite` fails (I/O error) | the failing `append` returns `Err`; its slot stays unwritten (a permanent gap) | the writing txn must abort; `sync_to_durable` targeting beyond the gap must surface an error rather than block forever (plan: poisoned-gap detection / bounded wait) |
| `sync_to_durable` with no appends since open | `Ok(())` immediately (`target == durable`) | no-op |
| fsync (`sync_data`) fails | `Err(DbError)` propagated; `durable_offset` not advanced | commit fails |

## Edge cases

- [ ] **Gap behind a written frame:** reserve O_A then O_B; complete O_B first; a
      `sync_to_durable` whose `target > O_A` must NOT advance `durable`/return until O_A is
      written. (Deterministic test via a split reserve/write hook.)
- [ ] **Out-of-order completion fills the gap:** once O_A completes,
      `contiguous_written_offset` jumps past both O_A and O_B in one step.
- [ ] **Concurrent commits (group):** N threads append + `sync_to_durable` concurrently;
      each returns only after its own `target` is durable; no committed frame is lost on a
      crash at any interleaving.
- [ ] **Single-writer (no concurrency):** behaves exactly like the old `sync()` — append,
      `sync_to_durable` fsyncs immediately (`contiguous_written == write_offset` already).
- [ ] **Crash with an in-flight (un-synced) append beyond `durable`:** the un-synced frame
      may be lost — that is correct (its commit had not returned).
- [ ] **Reopen recomputes watermarks:** `FrameLog::open` sets `contiguous_written` and
      `durable` to the end of the valid prefix from `scan()` (no persisted watermark).

## Performance budget

`append` hot path: no new I/O, no lock around the `pwrite` (only ~ns bookkeeping). The
commit-boundary `sync_to_durable` adds at most a brief wait for in-flight appends below
`target` to finish their `pwrite` (microseconds — the writes are already in progress),
then the same single fsync as today. No change to `read_page`/`write_page` throughput.
This subphase is additive (redo opt-in) → no production durability/perf change yet; the
A/B autocommit win is realized in 6c.

## Dependencies

- Depends on: subphase 5 (frame log durable at commit via `commit_durable`/`sync_frame_log`).
- Blocks: subphase 6c (cannot safely drop the per-commit main-file flush until the frame
  log's durable prefix is gap-free under concurrency). Independent of 6b (checkpoint).

## Open questions

- [ ] Watermark tracker data structure (plan decision): lock-free CAS advance over a
      completed-slots view vs a `Mutex`-guarded `BTreeSet<u64>` of completed offsets that
      guards only the bookkeeping. Recommendation: start with the bookkeeping-`Mutex`
      (simplest correct; the `pwrite` stays concurrent), revisit if it contends.
- [ ] Poisoned-gap handling on a failed `pwrite` (bounded wait + error vs treat the slot
      as a permanent torn tail). Resolve in the plan.

## Done criteria

- [ ] `FrameLog`: `contiguous_written_offset()`, `durable_offset()`, `sync_to_durable()`
      implemented; `append` records completion to advance the contiguous watermark.
- [ ] `sync_frame_log` (Mmap + FaultInjection) calls `sync_to_durable`.
- [ ] Deterministic gap test green (durable prefix never advances past an unwritten slot).
- [ ] Concurrent-commit crash test green: N threads append+commit, simulated crash at
      randomized points, `scan()` after crash includes every frame whose commit returned.
- [ ] Single-writer + reopen behavior unchanged; existing `wal_frame` + recovery + T0
      suites green (additive — no regression).
- [ ] `./tools/vm.sh test -p axiomdb-storage wal_frame` + clippy (storage) + fmt clean;
      docs-site `wal.md` (contiguous-prefix subsection) + memory updated.

## References

- `crates/axiomdb-storage/src/wal_frame.rs` (`append`, `scan`, `sync`, `write_offset`).
- `crates/axiomdb-wal/src/concurrent_writer.rs` (leader-flush model + `flushed_lsn`
  watermark — the in-house precedent we mirror at the watermark level).
- `crates/axiomdb-wal/src/txn_begin_commit.rs` (`commit_durable` → `sync_frame_log`).
- External: PostgreSQL `xlog.c` — `LogwrtResult` (Write/Flush), `WaitXLogInsertionsToFinish`,
  `XLogFlush` (`research/postgres/src/backend/access/transam/xlog.c`).
