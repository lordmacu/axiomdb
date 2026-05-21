# Spec: subphase 4 — commit boundary (frame log becomes durably correct)

Phase: redo-recovery (project B) — subphase 4
Task: make each frame carry its writing `txn_id`, and make the frame log durable at
the commit boundary, so subphase 5's recovery can REDO committed txns and discard
in-flight ones — under **multi-writer**.
Status: approved
Effort: **max** (durability contract; data-loss surface).

## Context

Subphase 3 wired `read_page`/`write_page` to the page-frame log (opt-in, dual-write).
Frames are appended *during* `write_page` (true write-ahead, no per-txn dirty tracking
— that was the reason Option A beat Option B). But under **multi-writer** (Phase 40)
frames from different txns interleave in the log, and today every frame carries
`commit_marker = 0`. SQLite's WAL gets away with a single end-of-txn commit marker
because it is **single-writer** (all frames between two commits belong to one txn);
AxiomDB is not. So recovery cannot tell which interleaved frames belong to a committed
txn vs an in-flight one. This subphase fixes that by stamping each frame with its
**`txn_id`** and making the frame log **durable at commit**.

## Goal

Every frame records the `txn_id` that wrote it, and a commit makes the frame log
durable (fsync) before the WAL commit record — so recovery (subphase 5) can rebuild
exact committed page state by keeping frames whose `txn_id` committed.

## Non-goals (explicit re-sequencing — changed from the original checkpoint)

The original checkpoint put "drop the per-commit flush + frame-only writes" in
subphase 4. That is **moved out** because doing it before recovery exists would break
reopen (empty index ⇒ `read_page` falls to a main file that no longer has the data ⇒
data loss). New ordering:

- **This subphase (4)** is **additive**: keeps dual-write + the per-commit
  `storage.flush()` (the safety net). **T0 stays RED** (recovery is subphase 5).
- **Subphase 5** — recovery rebuilds the wal-index on open from committed frames →
  **T0 flips GREEN**.
- **Subphase 6** — only now: drop the per-commit flush, frame-only writes, and add the
  **contiguous durable prefix** (`link_buf`/`WALInsertLock`) — load-bearing only once
  the main file is no longer authoritative — plus checkpoint (frames → main).
- **Subphase 7** — full crash suite + perf A/B.

Also out of scope here: changing MVCC/undo; the `read_page` path (unchanged).

## Behavior

### Frame format change: `txn_id` replaces `commit_marker`

The per-frame `commit_marker: u32` (0 / committing-txn-id-on-last-frame) becomes
`txn_id: u64` (the txn that wrote *this* frame; `0` = non-transactional write, e.g.
bootstrap/recovery). The frame header grows 32 → **36 bytes** (the frame log is
subphase-2 infra with no production data, so a format bump is free).

```rust
pub struct FrameRef {
    pub page_id: u64,
    pub lsn: u64,
    pub txn_id: u64,     // was: commit_marker: u32
    pub offset: u64,
}
// FrameLog::append(&self, page_id, lsn, txn_id, page) -> Result<u64, DbError>
```

`build_index` no longer infers "committed" from a marker; it takes a **committed
predicate** (supplied by recovery from the logical WAL's `Commit` entries):

```rust
// Latest committed frame per page; a frame counts only if is_committed(txn_id).
pub fn build_index(&self, is_committed: &dyn Fn(u64) -> bool) -> Result<WalIndex, DbError>;
```

(The live `WalIndex::record` path is unchanged — it indexes every appended frame for
in-session reads, committed or not.)

### Threading the txn_id to `write_page` (the multi-writer crux)

`write_page` does not receive a `txn_id`, and a *global* `AtomicU64` would be wrong
(two concurrent txns would clobber it). Because a statement executes **synchronously
end-to-end on one thread** (`Db::execute_query` is sync; the B-tree/storage write path
has no `await`/`spawn_blocking`/rayon between the executor and `write_page`), the
correct, lock-free mechanism is a **thread-local current txn**:

```rust
// StorageEngine (default no-op, like the unused set_current_snapshot):
fn set_current_txn(&self, _txn_id: u64) {}

// MmapStorage: a module thread_local! { static CURRENT_TXN: Cell<u64> }.
// set_current_txn stores it; write_page reads it for the frame's txn_id.
```

The executor sets it once per statement (where the `ConnectionTxn` is in scope) before
the storage writes, and resets it to 0 at statement end. Two concurrent statements run
on two threads ⇒ two independent thread-locals ⇒ no clobber.

**Robustness (no silent failure).** The thread-local is correct only while the write
path stays single-threaded-synchronous between `set_current_txn` and `write_page`. To
keep that invariant from breaking silently (e.g. a future `rayon`/`spawn_blocking` in a
bulk-write path would run `write_page` on a worker thread with `CURRENT_TXN == 0`,
stamping the wrong id and corrupting recovery):

- **Panic-safe reset:** the executor sets/clears the txn via an **RAII guard** (reset to
  0 on drop, including on error/panic unwind), never a bare set + manual reset.
- **Threading test (not a universal assert):** `write_page` cannot tell a transactional
  write from a system write (both arrive as `&Page`), and system writes legitimately
  carry `txn_id == 0`, so a universal `debug_assert(txn != 0)` would false-positive.
  Instead an integration test asserts that a real txn's frames carry its id.
- **Documented invariant:** "no `spawn_blocking`/rayon between `set_current_txn` and the
  txn's `write_page`s; if a write path is ever parallelized, the `txn_id` must be passed
  explicitly there." (Breaking it would silently stamp `txn_id = 0`.)

### Commit boundary: fsync the frame log (write-ahead order)

A commit must make the txn's frames durable **before** the logical `Commit` record
(write-ahead: data before the commit record). New `StorageEngine` method:

```rust
fn sync_frame_log(&self) -> Result<(), DbError>;   // default no-op; MmapStorage fsyncs
```

The executor calls `storage.sync_frame_log()` at the commit boundary, ordered before
`TxnManager::commit` (which appends + fsyncs the `Commit` record). In subphase 4 this
is an *extra* fsync on top of the still-present WAL fsync + conditional main flush
(transient overhead; the win lands in subphase 6 when the main flush is removed).

### Semantics

- Precondition: `set_current_txn(txn_id)` was called on this thread before the txn's
  `write_page`s; `txn_id != 0` for transactional writes.
- Postcondition (commit): all frames appended by the txn are fsync'd to the frame log
  before the `Commit` record is durable.
- Invariant: a frame's `txn_id` equals the logical txn that produced it; recovery treats
  a frame as committed iff that `txn_id` has a `Commit` in the logical WAL.
- Invariant (unchanged): main file stays authoritative (dual-write + flush) ⇒ no
  observable durability change yet; T0 RED.

### Error cases

| Input | Expected error | Notes |
|-------|----------------|-------|
| frame log append/fsync I/O failure | `DbError` via `classify_io` | propagated at the commit boundary |
| `write_page` with `CURRENT_TXN == 0` while redo enabled | none — frame stamped `txn_id=0` | non-transactional/system write; recovery convention defined in subphase 5 |

## Edge cases

- [ ] Two concurrent txns writing different pages → each frame carries the correct
      `txn_id` (thread-local isolation). Concurrency test with 2+ threads.
- [ ] RAII guard resets `CURRENT_TXN` to 0 at statement end AND on error/panic unwind
      (no stale `txn_id` leaks to the next statement on the same thread).
- [ ] Integration test asserts a real txn's frames carry its `txn_id` (threading works
      end-to-end through the executor).
- [ ] Same txn writes many pages → all frames carry that `txn_id`.
- [ ] Autocommit statement → its own `txn_id` set/reset per statement.
- [ ] `txn_id = 0` (no current txn) → frame stamped 0 (bootstrap/system write).
- [ ] `build_index` with a predicate that excludes a txn → its frames are skipped.
- [ ] Log disabled → `set_current_txn`/`sync_frame_log` are no-ops; behavior unchanged.
- [ ] commit fsync ordering: frame log fsync precedes the WAL `Commit` fsync.

## On-disk format

Frame header (was 32 B): `page_id(8) lsn(8) txn_id(8) salt(8) frame_crc(4)` = **36 B**
(`commit_marker(4)` removed, `txn_id(8)` added). `frame_crc` still covers the header
sans crc ++ the page. `FILE_HDR`/page layout unchanged. No main-file format change.

## Performance budget

| Operation | Subphase-4 cost | Note |
|-----------|-----------------|------|
| `write_page` enabled | +1 thread-local read | negligible |
| commit (enabled) | +1 frame-log fsync | transient; removed-net in subphase 6 (replaces the main `sync_all`) |

No read regression. Disabled path unchanged.

## Dependencies

- Depends on: subphase 3 (frame log in storage, lock-free append, live index).
- Blocks: subphase 5 (recovery uses `txn_id` + the committed predicate → T0 green).

## Open questions

- [ ] Exact executor insertion points for `set_current_txn` / `sync_frame_log` (one
      central statement-execution wrapper vs per-DML). Resolve in `/plan-task`.
- [ ] `txn_id = 0` recovery convention (always-apply vs never) — finalize in subphase 5;
      subphase 4 only stamps it.

## Done criteria

- [ ] Frame carries `txn_id` (header 36 B); `FrameLog::append` takes `txn_id`;
      `FrameRef.txn_id`; `build_index(is_committed)` filters by it. Frame tests updated.
- [ ] `set_current_txn` (thread-local) + `sync_frame_log` on `StorageEngine`/`MmapStorage`.
- [ ] Executor threads the txn_id and fsyncs the frame log at the commit boundary
      (frame log before the WAL `Commit` record).
- [ ] Concurrency test: 2+ threads, frames carry correct per-txn ids.
- [ ] Additive: dual-write + per-commit flush intact; **T0 still RED**.
- [ ] `./tools/vm.sh test --workspace` green; clippy + fmt clean; docs-site/wal.md +
      memory updated.

## References

- `specs/fase-redo-recovery/spec-subfase-3-write-ahead.md` (subphase 3).
- External: `research/sqlite/src/wal.c` `sqlite3WalFrames` (single-writer commit marker
  — the contrast that forces our per-frame `txn_id`); PostgreSQL `WALInsertLock` /
  MySQL 8.0 `link_buf` (subphase 6 contiguous prefix).
- `crates/axiomdb-wal/src/txn_begin_commit.rs` `commit()` (L91), `recovery.rs`
  `recover()` (L160), `crates/axiomdb-storage/src/{wal_frame.rs,mmap.rs,engine.rs}`.
