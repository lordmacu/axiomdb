# Spec: subphase 6d — frame-log fast path

Phase: redo-recovery (project B) — subphase 6d (perf tail of the frame-only switch)
Task: make write-ahead frame-only a REAL insert win (matching SQLite WAL+NORMAL), not the
6c wash.
Status: approved
Effort: **max** (durability semantics change + mmap'd growing log; data-loss surface).

## Context

6c wired frame-only redo into production (gated, default off) and **validated it is
crash-safe** (committed rows survive a crash via REDO; T0 + the frame-only crash test green).
But the A/B showed **no perf win**: dropping the ~16 ms per-commit `ensure_database_roots`
flush was offset by the frame-only write path. T1 (a `sample` profile of a clean 199 K-row
frame-only run, built with symbols) pinned the overhead to `MmapStorage::write_page_inner`'s
frame branch (`mmap.rs:585-610`): **`frame_log.append` (2 `pwrite`s per frame to a `.wf`
file that grows) + `frame_crc` + a 16 KB copy + (now removed) a redundant `Page::from_bytes`
re-CRC**, plus the per-commit frame **fsync** (`commit_durable` → `sync_to_durable`). Micro
CPU trims are <1 % each. This subphase removes the two structural costs the way SQLite WAL
does: **defer the fsync to checkpoint at `synchronous=NORMAL`** and **mmap the frame log so
writes are `memcpy`, not `pwrite`**.

## Goal

With frame-only redo on, a steady-state insert is meaningfully faster than the redo-off
baseline (the per-commit main flush stays dropped AND the frame log is cheap to write +
fsynced only at checkpoint under NORMAL), while crash recovery stays correct.

## Non-goals

- New durability *guarantee* beyond SQLite-NORMAL/FULL parity — NORMAL keeps the "lose the
  last unsynced txn(s) on power loss" relaxation; not adding group-commit pipelining here.
- Background (async) checkpoint thread — an **inline** size + clean-shutdown trigger is enough
  (a background scheduler is a later tuning subphase).
- Changing the `.wf` on-disk format — same 36 B frame header + 16 KB page; only the *access
  method* (mmap vs pwrite) changes. Bases with a pwrite-written `.wf` open unchanged.
- The >200 K-row single-txn safety-valve panic — separate pre-existing bug (own task).
- Multi-`.wf` / WAL segmentation — out of scope.

## Behavior

### 1. Configurable frame durability (SQLite `synchronous` model)

`commit_durable` becomes **policy-aware** (reads the effective `WalDurabilityPolicy` — the
per-txn `durability_override` or the instance default, same source the WAL commit already
uses):

- **Strict** → `sync_frame_log()` per commit (fsync the frame log's contiguous durable
  prefix BEFORE the WAL Commit record — today's behavior, full durability).
- **Normal / Off** → **do NOT** fsync the frame log per commit. The frames are written
  (memcpy/append + `mark_written`) and visible to readers via the `wal_index`, but the fsync
  is **deferred to the checkpoint** (SQLite `wal.c:2170-2180`: under NORMAL all WAL fsyncs
  occur in `walCheckpoint`).

Recovery already only REDOes frames for txns whose WAL Commit record is durable; under NORMAL
both the WAL and the frame log lose their unsynced tail consistently on power loss, so REDO
never sees a committed txn with missing frames.

### 2. mmap-backed frame log (cheap writes)

`FrameLog` switches its append + read path from `pwrite`/`pread` (`write_all_at` /
`read_exact_at`) to an **mmap'd** region:

- **append**: reserve the slot via the existing atomic `write_offset.fetch_add` (lock-free,
  unchanged), then `memcpy` the 36 B header + 16 KB page straight into the mapped region (no
  syscall, no per-write file extension). `mark_written` (6a contiguous-written prefix) and the
  `salt`/recycle (6b) are unchanged.
- **growth**: the `.wf` is pre-allocated/extended in large chunks (e.g. ≥ a few MB) and the
  mapping is grown under a dedicated grow lock (`ftruncate` + remap); the lock-free append
  fast path only takes the grow lock when its reserved slot would exceed the mapped length.
- **read** (`read_page_if_for`): read the header+page directly from the mapped region (verify
  `page_id` + `salt` + `crc`, same 6c contract; on mismatch fall back to main) — no `pread`.
- **durability**: `sync_to_durable` becomes an `msync` (+ the existing contiguous-prefix wait
  + `durable_offset` advance + fsync-leader). Called per-commit only at Strict; at the
  checkpoint otherwise.

> Fallback: if mmap-of-a-growing-log proves too risky under concurrent append, fall back to
> **batched per-commit writes** (one `pwritev` of a txn's contiguous frame range) — preserves
> the win on writes but not on the read path. Decided in `/plan-task` after a confirm-measure.

### 3. Checkpoint trigger + checkpoint fsync (subsumes 6c step 6)

- **Trigger**: after a commit, if the frame log's written size exceeds a configured threshold
  (`DbConfig`, default e.g. 64 MB), run `checkpoint_frames` inline; also on clean `Database`
  close. The committed-txn predicate comes from the `TxnManager` (committed set / max_committed
  + active set), as in 6b/6c.
- **Checkpoint order (walCheckpoint)**: fsync (msync) the frame log FIRST (make the frames
  durable), THEN apply committed frames to the main file (pageLSN-guarded, grow-on-redo),
  fsync main, then in-flight-safe recycle (only when every frame is committed) + clear the
  live `wal_index` (6c). This is where the NORMAL-deferred fsync actually happens.

### 4. CPU trims (already / marginal)

- `Page::from_bytes_unchecked` for the post-write cache insert (`mmap.rs:609`) — **done** (the
  LSN stamp is in the header, outside the checksum body, so the body checksum is still valid).
- Combining the 2 append writes is moot once mmap'd (single memcpy).

### Public API (signatures that change)

```rust
// txn_begin_commit.rs — commit_durable consults the policy instead of always fsyncing.
// (No signature change; internal: skip storage.sync_frame_log() unless Strict.)
impl TxnManager { pub fn commit_durable(&self, conn_txn: ConnectionTxn, storage: &dyn StorageEngine) -> Result<Option<TxnId>, DbError>; }

// wal_frame.rs — FrameLog internals switch to mmap; public method shapes unchanged:
impl FrameLog {
    pub fn append(&self, page_id: u64, lsn: u64, txn_id: u64, page: &[u8; PAGE_SIZE]) -> Result<u64, DbError>; // memcpy now
    pub fn sync_to_durable(&self) -> Result<(), DbError>;          // msync now
    pub fn read_page_if_for(&self, offset: u64, page_id: u64) -> Result<Option<Box<[u8; PAGE_SIZE]>>, DbError>; // from map
    pub fn written_size(&self) -> u64;                              // NEW: for the size trigger
}

// config.rs — checkpoint size threshold.
pub struct DbConfig { /* … */ pub checkpoint_frame_bytes: Option<u64> } // resolved default e.g. 64 MiB

// engine.rs — a trigger hook the commit path can call (default no-op).
trait StorageEngine { fn maybe_checkpoint(&self, is_committed: &dyn Fn(u64)->bool) -> Result<(), DbError> { Ok(()) } }
```

### Semantics

- Precondition (commit, frame-only): the txn's frames are appended (memcpy'd) + `mark_written`.
- Postcondition (Strict): the frames covering the commit are fsync'd before the WAL Commit
  record (durable across power loss).
- Postcondition (Normal): the commit is durable across a **process** crash (frames are in the
  OS page cache via the shared mapping + WAL); a **power** loss may lose the last unsynced
  txn(s) — exactly SQLite WAL+NORMAL.
- Invariant: a committed page is readable at all times — from its frame (mapped, via wal_index)
  before a checkpoint, from the main file after a checkpoint/recovery.
- Invariant: the checkpoint fsyncs the frame log before applying to main and before recycle.

### Error cases

| Input | Expected error |
|-------|----------------|
| msync / fsync failure at commit (Strict) or checkpoint | `DbError::Io` (poison the prefix; a waiter past it errors, not deadlocks — 6a) |
| frame append would exceed map and grow fails | `DbError::Io` |
| stale wal_index offset after recycle | not an error — `read_page_if_for` returns `None` → read from main (6c) |

## Edge cases

- [ ] Power loss at NORMAL after commit, before checkpoint → reopen REDOes the fsync'd-tail
      committed frames; the unsynced tail is lost (documented NORMAL semantics).
- [ ] Crash at Strict after commit → no loss (frames fsync'd per commit).
- [ ] mmap remap (growth) racing concurrent lock-free appends → grow lock; appends whose slot
      is within the current map don't block.
- [ ] Read racing a checkpoint recycle → `read_page_if_for` salt/crc mismatch → fall back to
      main (main made current before recycle) (6c).
- [ ] Recycle re-salts + resets the mapping; a stale mapped offset never validates.
- [ ] Existing pwrite-written `.wf` opened by the mmap path → read identically (same format).
- [ ] Checkpoint trigger fires mid-large-batch (size threshold) → bounded log; no data loss.
- [ ] Clean shutdown → checkpoint drains; clean reopen needs no REDO.
- [ ] `test_dirty_open_truncates_unlogged_tables_only` stays green.

## On-disk format

Unchanged. `.wf` = file header + frames `[36 B header (page_id, lsn, txn_id, salt, crc) || 16 KB page]`.
Only the access method changes (mmap vs pwrite). Compatibility: a `.wf` written by 6a–6c
(pwrite) is byte-identical and read correctly by the mmap path.

## Performance budget

| Operation | Target | Reference |
|-----------|--------|-----------|
| insert_batch 50K COMMIT (frame-only) | ~63 ms → **~47 ms (deferred fsync alone, ~+22 %)** → ~32 ms (+ mmap, ~+50 %) | 6c A/B (`project_insert_perf.md`) |
| insert autocommit (frame-only) | large win — NORMAL drops the per-row frame fsync | 6c A/B |
| read_page warm/cold | no regression vs 6c | 6c read path |
| checkpoint pause | bounded (exclusive guard + one msync+main fsync) | 6b |

A/B: `cargo build --release -p axiomdb-bench-comparison` then
`AXIOMDB_BENCH_REDO=1 … --diagnose-prepared-insert` (50K) vs the redo-off baseline; medians.

## Dependencies

- Depends on: 6c (frame-only switch wired + validated), 6a (contiguous prefix), 6b (checkpoint
  + recycle), 5 (REDO recovery).
- Blocks: subphase 7 (full crash suite T1–T7 + doublewrite retirement + flip default ON).

## Open questions

- [ ] **mmap vs batched-pwrite for lever 1** — resolve in `/plan-task` after measuring the
      deferred-fsync-only A/B: if deferred fsync alone hits the budget, mmap may be deferrable;
      if the frame writes still dominate, mmap (or pwritev batch) is required. (Brainstorm
      leaned mmap as optimal; confirm with data first.)
- [ ] **Checkpoint default threshold** — 64 MiB? tune with the autocommit A/B.
- [ ] **Strict + deferred** interaction with the existing fsync pipeline (FsyncPipeline) — keep
      the frame fsync separate from the WAL pipeline (it already is via commit_durable).

## Done criteria

- [ ] `commit_durable` defers the frame fsync at NORMAL (per-commit fsync only at Strict).
- [ ] Frame log writes are cheap (mmap memcpy, or batched pwritev fallback); A/B shows
      insert_batch + autocommit measurably faster (> macOS noise) than the redo-off baseline.
- [ ] Checkpoint trigger (size + clean shutdown) bounds the log; checkpoint fsyncs frames
      before applying to main.
- [ ] No read regression (`--compare` reads within noise of 6c).
- [ ] T0 green; `test_dirty_open_truncates_unlogged_tables_only` green; the 6c frame-only
      crash test green; a NORMAL-deferred crash+checkpoint recovery test green.
- [ ] `./tools/vm.sh test --workspace` + clippy + fmt clean.
- [ ] Default flip ON (gated by subphase 7's full crash suite) OR documented as the remaining
      gate; docs (`wal.md`, `transactions.md`, `performance.md`) + memory updated.

## References

- `research/sqlite/src/wal.c` — `sqlite3WalFrames`/`walWriteToLog` (write path), `walCheckpoint`
  (`2170-2180`: NORMAL fsyncs only at checkpoint), `walIndexAppend`.
- `crates/axiomdb-storage/src/wal_frame.rs` (`FrameLog::append`/`sync_to_durable`/`recycle`/
  `read_page_if_for`), `mmap.rs` (`write_page_inner` frame branch 585-610, `read_page`,
  `checkpoint_frames`), `txn_begin_commit.rs` (`commit_durable`), `config.rs`
  (`WalDurabilityPolicy`, `RedoMode`).
- `specs/fase-redo-recovery/spec-subfase-6c-frame-only.md` (the switch this accelerates).
