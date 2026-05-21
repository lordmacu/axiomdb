# Spec: redo-recovery — physical page-image REDO so commit relies on WAL fsync (project B)

Phase: redo-recovery (a.k.a. "project B" — deferred-checkpoint / SQLite WAL-frame model)
Task: Implement REDO crash recovery so a COMMIT is durable once the WAL is fsynced,
removing the load-bearing per-commit main-file `storage.flush()`. This both (a) makes
writes much faster (esp. autocommit: per-row fsync → none) and (b) closes a likely
latent data-loss-on-power-loss hole.
Status: draft (architecture) — crash-tests-first; implementation goes on branch
`fase-redo-recovery`, NOT main (concurrent json work on main).
Effort: **max** (durability contract; data-loss risk).

## Context

Crash recovery today is **UNDO-only**. `CrashRecovery::recover`
([`recovery.rs:160`](crates/axiomdb-wal/src/recovery.rs:160)) scans the WAL forward
but only timelines ops for txns still in `active_txns` (uncommitted at crash) and
rolls them back; it **never REDOes committed txns** (recovery.rs ~422-425). REDO was
explicitly deferred — `EntryType::PageWrite`'s `page_bytes` are
*"preserved for future REDO support (Phase 3.8b)"*
([`entry.rs`](crates/axiomdb-wal/src/entry.rs)).

`TxnManager::commit` ([`txn_begin_commit.rs:91`](crates/axiomdb-wal/src/txn_begin_commit.rs:91))
fsyncs only the **WAL** (`commit_data_sync`, [`writer.rs:303`](crates/axiomdb-wal/src/writer.rs:303);
plus a group-commit pipeline). Data pages reach the main file only via
`Checkpointer::checkpoint` ([`checkpoint.rs:53`](crates/axiomdb-wal/src/checkpoint.rs:53))
or the **conditional** per-commit catalog flush: `apply_clustered_insert_rows`
calls `update_table_root` → `CatalogWriter::new` → `ensure_database_roots` →
`storage.flush()` **only when the table root changed**
([`insert_clustered.rs:537`](crates/axiomdb-sql/src/executor/insert_clustered.rs:537)).

**Likely latent durability hole (high-confidence from code, not yet reproduced):**
a committed INSERT that does NOT change the root (the common case after the first
row) gets no eager flush; its page is durable only via OS mmap writeback. A power
loss before the next checkpoint can lose it, and there is no REDO to rebuild it. The
existing `test_dirty_open_truncates_unlogged_tables_only` does NOT cover this — its
single insert changes the root (so it flushes), and `mark_database_dirty_open`
([`integration_open_integrity.rs:47`](crates/axiomdb-network/tests/integration_open_integrity.rs:47))
only flips the clean-shutdown flag (synthetic; the `Database` drop already flushed).

**Existing scaffolding we build on:**
- Per-page LSN field: `PageHeader.lsn` ([`page.rs:76`](crates/axiomdb-storage/src/page.rs:76))
  exists but is **unmaintained** (always 0). → idempotence groundwork present, **no
  64-byte-header format change needed**.
- Page-image WAL precedent: `EntryType::PageWrite` already logs full `page_bytes`.
- `checkpoint.rs` (flush + checkpoint LSN), `doublewrite.rs` (torn-page repair),
  group-commit pipeline (`fsync_pipeline.rs`), `freelist.rs` (bitmap page).

## Goal

After a crash (power loss), reopening the DB **REDOes every committed transaction**
from the last checkpoint forward, reconstructing exact page state — so commit needs
only the WAL fsync (already done), and the per-commit main-file flush is removed.
UNLOGGED tables remain non-durable (truncated on dirty open, unchanged).

## Non-goals

- **Logical/operation REDO (ARIES replay).** See architecture decision — we use
  physical page-image redo instead.
- **Changing MVCC undo / rollback.** The existing logical undo records stay as-is
  (undo from logical; redo from physical page frames — a deliberate hybrid).
- **Removing the doublewrite buffer** in this phase (evaluate later — WAL page
  frames may make it redundant; keep it until proven so).
- **The contained insert micro-opts** (codec, insertCellFast — already shown marginal).
- **On-disk page-format change.** `PageHeader.lsn` already exists.

## Architecture decision: PHYSICAL page-image REDO (not logical)

We log a **full page image** (per dirty page, per commit) to the WAL and REDO by
writing those bytes back. Rationale:

1. **It dissolves the hard blockers uniformly.** Logical redo would have to solve,
   per-path: B-tree split page-allocation determinism (clustered), index records
   (none exist today — would need rebuild-from-base), overflow/TOAST chains, and
   free-list reconstruction. Physical redo treats *everything as a page* — clustered
   leaves, index pages, overflow pages, catalog heap, AND the free-list bitmap are
   all just pages whose images replay identically. The per-path work vanishes.
2. **Idempotence is trivial with the existing `pageLSN`.** REDO applies a frame only
   if `frame.lsn > on_disk_page.lsn` — safe to replay any number of times (double
   crash). ARIES-standard, and the field already exists.
3. **It is our embedded reference's model.** SQLite's WAL is page-frame based; the
   `PageWrite.page_bytes` were already reserved for exactly this.
4. **Torn-page safety comes for free.** A frame is the whole page; replaying it
   repairs a torn page (may let us retire doublewrite later).

Cost: more WAL bytes than logical (page images), but only DIRTY pages per commit,
written sequentially. Acceptable and SQLite-proven. The existing logical records are
kept for UNDO/MVCC (hybrid log).

## Design

### WAL: page-image frames
At commit, for each page the txn dirtied, append a frame `{ page_id, lsn, page_bytes,
crc }`. Reuse/extend the `PageWrite` machinery. Frames carry the commit's LSN.
(Batch them; the group-commit pipeline already coalesces fsyncs.)

### Maintain `PageHeader.lsn`
On every page write, stamp `header.lsn = <WAL LSN of the change>` before checksum.
Existing pages have `lsn=0` → treated as "older than any frame" → safely redone from
the last checkpoint.

### REDO replay (recovery.rs)
New forward pass after the scan: for each frame of a COMMITTED txn with
`lsn > checkpoint_lsn`, read the on-disk page; if `frame.lsn > page.lsn`, write
`frame.page_bytes` (grow the file if `page_id` beyond EOF). The free-list bitmap is
itself a page → its frames replay → free-list state is reconstructed. No separate
allocation log needed.

### Commit path
Drop the per-commit `ensure_database_roots` `storage.flush()`; commit = append
frames + logical records + Commit entry, then WAL fsync (existing `commit_data_sync`
/ group commit). Catalog root updates ride the same page frames.

### Checkpoint
Unchanged in spirit (flush dirty pages to main file, advance checkpoint LSN, recycle
WAL) but now it is the ONLY thing that moves pages to the main file. Tune frequency
(time/size based). Reconcile ordering with doublewrite.

### Fault-injection crash harness (test infra — built FIRST)
Because mmap `MAP_SHARED` dirty pages live in the kernel page cache and **survive
even SIGKILL** (only real power loss/panic loses them), the existing synthetic
dirty-open cannot test REDO. Build a test storage wrapper (or a `StorageEngine`
shim) that records which writes were `fsync`'d and, on "simulated power loss",
presents only the fsynced state to a fresh open — discarding un-fsynced page writes
while keeping the fsynced WAL. This is the foundation of every crash test below.

## Crash-test plan (crash-tests-first)

- **T0 — characterize the hole (write first, expect RED today):** commit a
  non-root-changing INSERT, simulate power loss (no checkpoint, drop un-fsynced
  pages), reopen → assert the row is present. Today: FAILS (proves the latent hole).
  After REDO: PASSES. This becomes the headline regression test.
- **T1** redo of each page type (heap, clustered leaf+internal, index, overflow,
  catalog) after power loss.
- **T2** idempotent replay: crash again *during* recovery → second recovery still
  correct (pageLSN guard).
- **T3** partial checkpoint: crash mid-checkpoint → redo from previous checkpoint LSN.
- **T4** torn page (single page half-written) repaired by frame replay (+ doublewrite).
- **T5** UNLOGGED tables still truncated on dirty open; clean reopen preserves them.
- **T6** uncommitted txn at crash is still UNDONE (no regression).
- **T7** soak: randomized op streams + random crash points vs an oracle.

## Subphase breakdown

1. **Fault-injection crash harness + T0** — the simulator + the failing
   hole-characterization test. (Proves the problem; foundation for all tests.)
2. **Maintain `PageHeader.lsn`** on writes (no format change). Unit-verify.
3. **WAL page-frame logging** for committed dirty pages (extend PageWrite).
4. **REDO replay pass** in `recovery.rs` (committed frames forward, pageLSN guard,
   file grow). T1/T2 green.
5. **Drop per-commit main-file flush** + checkpoint integration + recycle. T3/T4.
6. **Full crash-test suite** (T5–T7) + perf validation (autocommit/insert_batch A/B)
   + docs + memory.

## Risk register

- **Data loss / corruption** — the whole point; mitigated by crash-tests-first and
  the pageLSN idempotence guard. Every subphase gated on the relevant crash tests.
- **Hybrid log coherence** — logical (undo) + physical (redo) must agree; the Commit
  entry is the single source of "committed". Document the ordering invariant.
- **WAL volume** — page images grow the WAL; mitigated by checkpoint frequency +
  group commit. Measure.
- **doublewrite overlap** — keep it until frame-replay torn-page repair is proven;
  then consider retiring (separate decision).
- **Concurrency** — checkpoint vs live writers vs recovery; reuse existing page-latch
  + the checkpoint ordering invariant in checkpoint.rs.

## Done criteria

- T0–T7 green on the fault-injection harness (real simulated power loss).
- Per-commit main-file `storage.flush()` removed; commit durable via WAL fsync only.
- Existing suites green (UNDO unchanged; UNLOGGED behavior unchanged).
- Perf: autocommit insert A/B shows the big win (per-row fsync gone); no read regression.
- Lima `nextest --workspace` + clippy + fmt clean; docs (wal.md, benchmarks.md) +
  memory updated.

## Open questions (resolve in per-subphase plans)

- Frame granularity: per-dirty-page at commit vs a redo segment per txn.
- Whether to retire doublewrite once frame-replay covers torn pages.
- Checkpoint trigger policy (size vs time vs WAL length).
