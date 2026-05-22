# Spec: subphase 6c — the frame-only switch (REDO becomes the production durability path)

Phase: redo-recovery (project B) — subphase 6c (last of the 6a/6b/6c split)
Task: flip writes to **frame-only** and remove the per-commit main-file `flush()`, so a
commit is durable via the WAL/frame fsync alone — the autocommit ~2-3× win and the real
durability-model change.
Status: approved
Effort: **max** (production durability switch; checkpoint-vs-reads concurrency; data-loss surface).

## Context

Subphases 5/6a/6b built and proved the pieces: REDO recovery (T0 green), the gap-free
contiguous durable prefix, and the frame checkpoint (apply→fsync→recycle). All are
**additive** — production still dual-writes (main file + frame log) and fsyncs the main
file at commit via `ensure_database_roots` (`crates/axiomdb-sql/.../catalog/writer.rs`,
the load-bearing per-commit flush). This subphase removes that net: writes become
**frame-only**, the per-commit flush is dropped, and the checkpoint becomes load-bearing.

The win (memory `project_insert_perf.md`): the per-commit `storage.flush()` (a `sync_all`
of the main file, ~12 ms on APFS) is the dominant autocommit cost; replacing it with a
WAL/frame fsync is the ~2-3× autocommit improvement.

Reference: SQLite WAL mode steady state — `walFindFrame` (read path: wal-index → WAL →
db file) and `walCheckpoint` (drain frames → db, reset WAL); `journal_mode=WAL` is a
per-database mode, not a compile-time flag.

## Goal

With redo enabled, a committed write is durable once its frames are fsync'd (no per-commit
main-file flush); the main file is updated only by the checkpoint; reads stay correct
before and after a checkpoint and across a crash.

## Non-goals

- New crash tests T1–T7 + the full perf A/B harness write-up — **subphase 7** (this
  subphase keeps T0 green + a focused autocommit A/B sanity check + the existing guard test).
- Retiring `doublewrite` — **subphase 7** decision.
- Background checkpoint scheduler tuning — a size + clean-shutdown trigger is enough here.
- Logical-WAL checkpoint-LSN trimming — recovery already tolerates re-scanning; out of scope.

## Behavior

### 1. Enable redo on the production open path

`MmapStorage::create`/`open` enable the page-frame redo log (`<db>.wf`) when the database's
durability config selects it (see Open question: config vs hard switch). When enabled,
`write_page` is frame-only and the per-commit flush is dropped.

### 2. Frame-only `write_page`

In the redo-on branch of `write_page_inner`, **stop the dual-write `pwrite` to the main
file**: stamp the page LSN, append the frame, record the live `wal_index`, cache the
stamped page. The main file is no longer touched on the write path — only by the checkpoint
(and by recovery's apply on open). `read_page` already consults pool → `wal_index` → main
(SQLite `walFindFrame` order, subphase 3), so an un-checkpointed page is served from its
frame.

### 3. Drop the per-commit main-file flush

`ensure_database_roots`'s `storage.flush()` is removed (or skipped) when redo is enabled —
the commit's durability now comes from `commit_durable` → `sync_frame_log` →
`sync_to_durable` (frame fsync, gap-free). UNLOGGED tables remain non-durable (unchanged).

### 4. Checkpoint reconciles the live `wal_index`

When `checkpoint_frames` **recycles** the log (full recycle — only when no in-flight frame
remains, see #5), it must also **clear the live `wal_index`**: the recycled log's offsets
are now invalid, and the applied pages live in the (just-fsync'd) main file, so reads must
fall through to main. The apply→fsync-main happens **before** the recycle+clear, so main is
current when the wal_index is cleared.

### 5. In-flight-safe recycle

Once frame-only, an uncommitted txn's pages live **only** in the frame log. The checkpoint
must not truncate them. So the recycle is conditional: **recycle (truncate + clear
wal_index) only when every frame in the log belongs to a committed txn**; otherwise apply
the committed frames to main but **leave the log** (the in-flight tail stays; a later
checkpoint recycles once those txns finish). This replaces 6b's unconditional recycle
(which was safe only because dual-write kept the main current).

### 6. Checkpoint trigger

- **Size:** after a commit, if the frame log exceeds a configured size, run a checkpoint
  (inline in 6c; background thread later). The committed-txn predicate comes from the
  `TxnManager` (committed set / `max_committed` + active set).
- **Clean shutdown:** checkpoint on `Database` close so a clean reopen starts with an
  empty (or small) log.

### Semantics

- Precondition (commit): the txn's frames are appended; `commit_durable` fsyncs the frame
  log's contiguous durable prefix covering them.
- Postcondition (commit): the commit survives a power loss with **no** main-file flush —
  recovery REDOes the committed frames into the main file on the next open.
- Invariant: a committed page is readable at all times — from the frame (live `wal_index`)
  before a checkpoint, from the main file after a checkpoint or after recovery.
- Invariant (UNLOGGED): UNLOGGED-table pages are still truncated on a dirty open.

## Edge cases

- [ ] **T0 still green**: committed insert survives power loss (now via the production path).
- [ ] **Autocommit A/B**: per-row insert shows the ~2-3× improvement vs the per-commit flush.
- [ ] **Read of an un-checkpointed committed page**: served from its frame (wal_index).
- [ ] **Read of a checkpointed page after recycle**: served from the main file (wal_index cleared).
- [ ] **Read racing a checkpoint's recycle+clear** (the hard one — see Open questions).
- [ ] **In-flight txn at checkpoint**: its frames are NOT truncated; recycle is skipped.
- [ ] **Crash after commit, before any checkpoint**: recover REDOes the committed frames → main.
- [ ] **UNLOGGED tables**: still truncated on a dirty open (`test_dirty_open_truncates_unlogged_tables_only`).
- [ ] **Clean shutdown + reopen**: checkpoint drained the log; reopen is clean, no REDO needed.

## Performance budget

| Operation | Target | Reference |
|-----------|--------|-----------|
| autocommit single-row INSERT | **~2-3× faster** than pre-6c (per-commit fsync gone) | `project_insert_perf.md` (the ~12 ms `ensure_database_roots` fsync) |
| read_page (warm / cold) | no regression vs pre-6c | subphase-3 read path |
| checkpoint pause (commits racing it) | bounded (exclusive guard; ~the main fsync) | 6b |

Measure with `cargo build --release -p axiomdb-bench-comparison` then
`target/release/axiomdb_bench --compare --rows 10000` (autocommit A/B is the signal).

## Dependencies

- Depends on: subphase 5 (recovery REDO), 6a (gap-free durable prefix), 6b (checkpoint).
- Blocks: subphase 7 (full crash suite + doublewrite retirement decision).

## Open questions (resolve before approving)

- [x] **Enable mechanism — RESOLVED → config mode** (user-confirmed 2026-05-21). A
      durability **config mode** on `DbConfig` (like SQLite `journal_mode=WAL`) — principled
      (not a feature-flag shim), lets the autocommit A/B compare on/off, safe rollout for a
      data-loss switch; defaults **on** once the A/B + suite pass.
- [x] **Checkpoint-vs-reads race — RESOLVED → B** (user-confirmed 2026-05-21): lock-free
      reads that **fall back to the main file** if a frame read fails its CRC / is beyond
      EOF (the checkpoint applies to main *before* recycling, so main is current at the
      moment the frame becomes unreadable). No read-path lock; gated by a targeted
      read-vs-checkpoint concurrency test.
- [ ] **Per-commit-flush removal scope** (plan-time verification, not a design fork): grep
      the commit path for any `storage.flush()` besides `ensure_database_roots` and confirm
      each is gated on redo. Resolved in the plan.

## Done criteria

- [ ] Redo enabled on the production open path (config mode); `write_page` frame-only;
      per-commit `ensure_database_roots` flush dropped when redo is on.
- [ ] Checkpoint clears the live `wal_index` on a full recycle; recycle is in-flight-safe
      (skipped when an uncommitted frame remains).
- [ ] T0 green via the production path; `test_dirty_open_truncates_unlogged_tables_only`
      green; a read-vs-checkpoint concurrency test green; a frame-only crash test green.
- [ ] Autocommit A/B shows the win; no read regression.
- [ ] `./tools/vm.sh test --workspace` + clippy + fmt clean; docs (`wal.md`,
      `user-guide/features/transactions.md`, `performance.md`) + memory updated.

## References

- `crates/axiomdb-storage/src/mmap.rs` (`write_page_inner` redo branch, `read_page`,
  `checkpoint_frames`, `wal_index`), `wal_frame.rs` (`recycle`).
- `crates/axiomdb-catalog/src/bootstrap.rs` (`CatalogBootstrap::ensure_database_roots` —
  the per-commit `storage.flush()`, called from `CatalogWriter::new`, writer.rs:154; the
  catalog is its own crate now — memory `project_insert_perf.md` saying `axiomdb-sql/catalog`
  is STALE);
  `crates/axiomdb-storage/src/config.rs` (`DbConfig`, `WalDurabilityPolicy`).
- `crates/axiomdb-network/tests/integration_open_integrity.rs`
  (`test_dirty_open_truncates_unlogged_tables_only` — the gating guarantee).
- `specs/fase-redo-recovery/spec-subfase-{5,6a,6b}-*.md`.
- External: SQLite `wal.c` `walFindFrame` / `walCheckpoint`; `journal_mode=WAL`.
