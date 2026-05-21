# Plan: subphase 4 — commit boundary

Phase: redo-recovery (project B) — subphase 4
Task: per-frame txn_id + durable frame log at commit
Spec: specs/fase-redo-recovery/spec-subfase-4-commit-boundary.md
Status: draft

## Summary

Four steps, each green, **T0 stays RED** (recovery is subphase 5). 4a changes the frame
format (`commit_marker u32` → `txn_id u64`) in storage and keeps every call site
compiling by passing `txn_id = 0` for now. 4b adds the thread-local `current_txn` +
`sync_frame_log` and makes `write_page` stamp the real `txn_id` (+ a `debug_assert`).
4c threads the txn through the executor with a panic-safe RAII guard and routes the
frame-log fsync through `TxnManager::commit(conn, storage)` (centralized, write-ahead
order). 4d verifies the workspace + docs + memory. Additive throughout (dual-write +
per-commit flush intact).

## Dependencies

- [x] spec-subfase-4-commit-boundary approved
- [x] subphase 3 done (frame log in storage, live index, lock-free append)
- Blocks: subphase 5 (recovery uses txn_id + committed predicate → T0 green)

## Affected files

Modified:
- `crates/axiomdb-storage/src/wal_frame.rs` — `FrameRef.txn_id`, `append(txn_id)`,
  `scan`, `build_index(is_committed)`; header 32→36 B; update 8 frame tests
- `crates/axiomdb-storage/src/engine.rs` — `set_current_txn` (already stubbed?),
  `sync_frame_log` trait methods
- `crates/axiomdb-storage/src/mmap.rs` — thread-local `CURRENT_TXN`, `set_current_txn`,
  `sync_frame_log`, `write_page` stamps txn_id + `debug_assert`
- `crates/axiomdb-wal/src/txn_begin_commit.rs` — `commit(conn, storage)` fsyncs frame
  log before the WAL `Commit` record
- `crates/axiomdb-sql/src/executor/exec_with_ctx.rs` (+ other commit sites:
  exec_entry.rs, sequence_runtime.rs, mod.rs, cron_runtime.rs) — RAII txn guard +
  pass `storage` to `commit`
- `docs-site/src/internals/wal.md`, memory (4d)

---

## Step 4a — Frame carries txn_id (format change)

**Goal:** each frame stores `txn_id: u64`; `build_index` takes a committed predicate.
Every call site compiles (write_page passes `0` for now).
**Files:** `wal_frame.rs` (+ the `mmap.rs` append call site → `0` placeholder).

### Tests (update existing 8 in wal_frame.rs)
```rust
// append/scan round-trip carries txn_id:
let off = log.append(2, 1, /*txn_id*/ 7, &page(1)).unwrap();
assert_eq!(log.scan().unwrap()[0].txn_id, 7);

// build_index with a committed predicate:
#[test]
fn build_index_keeps_only_committed_txns() {
    // frames for txn 7 (committed) and txn 9 (not) on different pages;
    let idx = log.build_index(&|t| t == 7).unwrap();
    assert!(idx.latest(page_of_7).is_some());
    assert!(idx.latest(page_of_9).is_none());
}
```

### Implementation outline
```rust
// header: page_id(8) lsn(8) txn_id(8) salt(8) frame_crc(4) = 36 B
const FRAME_HDR_SIZE: usize = 36;
const FRAME_HDR_CRC_PREFIX: usize = 32;
pub struct FrameRef { pub page_id: u64, pub lsn: u64, pub txn_id: u64, pub offset: u64 }
pub fn append(&self, page_id, lsn, txn_id, page) -> Result<u64, DbError> { ... }
pub fn build_index(&self, is_committed: &dyn Fn(u64) -> bool) -> Result<WalIndex, DbError> {
    // index latest frame per page among frames where is_committed(txn_id)
}
// mmap.rs write_page: append(page_id, lsn, /*txn_id*/ 0, &buf) — placeholder, 4b fixes
```
Note: `last_commit_lsn` semantics drop (no marker); recovery (subphase 5) drives
"committed" via the predicate. Remove `commit_marker` from `WalIndex::record` callers.

### Verification
```bash
./tools/vm.sh test -p axiomdb-storage wal_frame
./tools/vm.sh clippy -p axiomdb-storage -- -D warnings
```

### Commit
```
feat(redo-recovery): frame carries txn_id; build_index takes a committed predicate (4a)
```

---

## Step 4b — thread-local current_txn + sync_frame_log + stamp

**Goal:** `write_page` stamps the real `txn_id` from a thread-local; add
`sync_frame_log`; `debug_assert` catches a lost txn_id.
**Files:** `engine.rs` (trait), `mmap.rs`.

### Tests
```rust
#[test]
fn write_stamps_current_txn_into_frame() {
    let mut s = MmapStorage::create(&path).unwrap(); s.enable_redo_log(&path).unwrap();
    s.set_current_txn(42);
    let pid = s.alloc_page(PageType::Data).unwrap();
    let mut p = Page::new(PageType::Data, pid); p.update_checksum();
    s.write_page(pid, &p).unwrap();
    // diag accessor: latest frame's txn_id == 42
    assert_eq!(s.frame_txn_id(pid), Some(42));
    s.set_current_txn(0);
}
#[test]
fn sync_frame_log_is_noop_when_disabled() { /* MemoryStorage / disabled MmapStorage */ }
```

### Implementation outline
```rust
// engine.rs trait (defaults no-op):
fn set_current_txn(&self, _txn_id: u64) {}
fn sync_frame_log(&self) -> Result<(), DbError> { Ok(()) }

// mmap.rs:
thread_local! { static CURRENT_TXN: Cell<u64> = const { Cell::new(0) }; }
fn set_current_txn(&self, txn_id: u64) { CURRENT_TXN.with(|c| c.set(txn_id)); }
fn sync_frame_log(&self) -> Result<(), DbError> {
    if let Some(fl) = &self.frame_log { fl.sync()?; } Ok(())
}
// write_page_inner (enabled branch): let txn = CURRENT_TXN.with(|c| c.get());
//   debug_assert!(txn != 0 || is_system_write, "redo write with no current txn");
//   fl.append(page_id, lsn, txn, &buf)
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-storage
./tools/vm.sh clippy -p axiomdb-storage --all-targets -- -D warnings
```

### Commit
```
feat(redo-recovery): thread-local current_txn stamp + sync_frame_log (4b)
```

---

## Step 4c — Executor threads the txn + commit fsyncs the frame log

**Goal:** RAII guard sets/resets `current_txn` per statement (panic-safe); the frame
log is fsync'd inside `commit(conn, storage)` before the WAL `Commit` record.
**Files:** `txn_begin_commit.rs` (commit signature), executor commit sites.

### Tests (wire-level / integration)
```rust
// integration: a committed INSERT leaves a frame stamped with its txn_id and the
// frame log is fsync'd at commit (observe via a storage diag counter or io_stats).
// Plus: existing suites stay green (additive).
```

### Implementation outline
```rust
// txn_begin_commit.rs: commit takes storage, fsyncs frames BEFORE the Commit record.
pub fn commit(&self, mut conn_txn: ConnectionTxn, storage: &dyn StorageEngine)
    -> Result<Option<TxnId>, DbError> {
    storage.sync_frame_log()?;                 // write-ahead: data (frames) durable first
    // ... existing: append Commit entry + commit_data_sync()/flush_no_sync() ...
}

// executor (exec_with_ctx.rs ~L20 begin, ~L474 commit) + other commit sites:
struct TxnScope<'a> { s: &'a dyn StorageEngine }
impl Drop for TxnScope<'_> { fn drop(&mut self) { self.s.set_current_txn(0); } }
// after begin: storage.set_current_txn(conn.txn_id); let _scope = TxnScope { s: storage };
// commit: txn.commit(conn, storage)   // was txn.commit(conn)
```
Update ALL `txn.commit(conn)` call sites (exec_entry, sequence_runtime, mod,
cron_runtime) to pass `storage`.

### Verification
```bash
./tools/vm.sh test -p axiomdb-sql -p axiomdb-wal
./tools/vm.sh clippy -p axiomdb-sql -p axiomdb-wal -- -D warnings
```

### Commit
```
feat(redo-recovery): executor threads txn_id + commit fsyncs the frame log (4c)
```

---

## Step 4d — Verify, docs, memory

### Against spec done-criteria
- [ ] frame has txn_id; build_index(predicate); set_current_txn + sync_frame_log
- [ ] RAII guard resets on error/panic; threading integration test asserts per-txn ids
- [ ] concurrency test: 2+ threads stamp correct per-txn ids
- [ ] additive: dual-write + per-commit flush intact; **T0 still RED**

```bash
./tools/vm.sh test --workspace
./tools/vm.sh clippy --workspace -- -D warnings   # (note: pre-existing array_codec warning is not ours)
./tools/vm.sh fmt-check
./tools/vm.sh test -p axiomdb-wal --run-ignored all t0_committed_heap_insert_survives_power_loss  # expect RED
```

### Docs / memory
- `docs-site/src/internals/wal.md`: subphase-4 section (per-frame txn_id, why multi-writer
  forces it vs SQLite single-writer; commit fsync write-ahead order).
- Update `docs/checkpoint-redo-recovery.md` + `memory/project_insert_perf.md`.

### Final commit
```
feat(redo-recovery): complete subphase 4 commit boundary

Implements specs/fase-redo-recovery/spec-subfase-4-commit-boundary.md
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| thread-local broken by a parallelized write path | low now | `debug_assert` + documented invariant; no rayon in the write path today |
| `commit(conn, storage)` signature churns many call sites | medium | mechanical; compiler finds them all (5 files) |
| build_index semantics change breaks subphase-3 frame tests | low | rewrite the 8 tests in 4a (predicate-based) |
| extra commit fsync hurts perf | expected/transient | net-removed in subphase 6 (replaces the main `sync_all`) |

## Rollback plan

Each step is its own commit. Abandon: `git reset --hard <commit before 4a>` on
`fase-redo-recovery`. No main-file format change; existing dbs unaffected (frame log
only active when enabled).

## Estimated effort

Total: ~1.5–2 days. 4a: 2h · 4b: 2h · 4c: 3–4h (executor surface) · 4d: 1.5h.
Effort level **max** (durability contract).
