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

## Architecture decision: SQLite-WAL pure write-ahead (Option A — LOCKED 2026-05-21)

**DECISION (user, 2026-05-21 — "the most robust"): Option A, SQLite's proven
write-ahead WAL model.** Page writes go to the **WAL as page frames first**; the main
DB file is updated only by a (background) **checkpoint**. A commit is durable once its
frames + a commit marker are fsync'd in the WAL — *nothing in the main file is required
for durability*. Recovery replays committed frames. This is SQLite's WAL mode
(`research/sqlite/src/wal.c`: `sqlite3WalFrames`, `walFindFrame`, `sqlite3WalCheckpoint`).

Considered alternatives (rejected): **B** = keep mmap-direct writes, capture the
txn's dirty pages as frames at commit (needs per-txn dirty-page tracking — none today);
**C** = logical replay of committed records (page-allocation-determinism risk). Per the
project's robustness-first principle, **A is the proven, lowest-risk *end state*** even
though it is the **largest rework** (the storage read/write path becomes WAL-aware).
A also: dissolves every per-path blocker (everything is a page frame); needs **no
per-txn dirty tracking** (frames are appended as pages are written — true write-ahead);
removes the per-commit main-file fsync entirely; gives torn-page safety for free (a
frame is a whole page → likely retires `doublewrite`).

**Idempotence:** the existing `PageHeader.lsn` — checkpoint/recovery apply a frame only
if `frame.lsn > on_disk_page.lsn`. The existing logical WAL records stay for UNDO/MVCC
(hybrid: **undo from logical records, redo from page frames**).

**Reads become WAL-aware:** `read_page` must consult the WAL frame index (latest frame
for the page) before the main file — SQLite's `walFindFrame` + wal-index (`-shm`).
This is the crux of the rework and the main source of risk/perf-sensitivity.

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

## Subphase breakdown (Option A — SQLite-WAL write-ahead)

> The earlier "Design" subsections above predate the A decision (they sketched the
> lighter per-commit-capture model B); the authoritative high-level design is now the
> A architecture block. Detailed design lands in each per-subphase spec/plan.

1. ✅ **DONE — Fault-injection crash harness + T0** (commits `89f3bd1d`, `21fc3060`):
   the power-loss simulator + the failing hole-characterization test.
2. **WAL page-frame format + writer/reader** — define a page frame
   `{ page_id, lsn, commit-marker, page_bytes, crc }`, a frame appender + reader, and
   the **wal-index** (page_id → latest committed frame) for reads. Mirror SQLite
   `wal.c`; the page-frame WAL may be a distinct file/section.
3. **Write path → write-ahead (the crux)** — `write_page` appends a frame (stamping
   `PageHeader.lsn` = frame LSN) instead of touching the main file; `read_page`
   consults the wal-index first (`walFindFrame`), then the main file. Main perf/risk
   surface; folds in the old "maintain pageLSN" step.
4. **Commit** — commit = (frames already appended) + commit marker + WAL fsync; drop
   the per-commit main-file `storage.flush()`. Defines the durable commit boundary.
5. **Recovery** — on open, scan WAL frames, rebuild the wal-index up to the last
   committed frame so reads see committed data; ignore uncommitted frames (reconcile
   with the existing logical UNDO). **T0 flips GREEN here** (the proof REDO works).
6. **Checkpoint** — copy committed frames → main file (pageLSN guard), fsync main,
   reset/truncate the WAL. Reconcile with / likely retire `doublewrite`.
7. **Full crash suite** (T1–T7) + perf A/B (autocommit win) + docs + memory.

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
