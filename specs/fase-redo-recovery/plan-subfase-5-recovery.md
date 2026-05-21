# Plan: subphase 5 — recovery (REDO) → T0 GREEN

Phase: redo-recovery (project B) — subphase 5
Task: on open, REDO committed page frames so a committed-but-unflushed write survives power loss
Spec: specs/fase-redo-recovery/spec-subfase-5-recovery.md
Status: draft

## Summary

Add the **REDO pass** to crash recovery. Today `CrashRecovery::recover`
(recovery.rs:160) is UNDO-only. Subphases 2–4 built the durable physical frame log
(page images stamped with `txn_id` + a page `lsn`, fsync'd at commit by
`commit_durable`). This subphase makes recovery, on open, rebuild the committed-frame
view and write each committed frame back to its page when the frame is newer than the
on-disk page (`frame.lsn > page.lsn`). After this, T0 (`integration_redo_recovery.rs`)
flips from RED to GREEN.

Order of the steps is bottom-up so every commit compiles and tests pass: first the
two small primitives the REDO needs (a `Page` LSN accessor + a way to iterate the
`WalIndex`), then the trait method (default no-op), then the two real implementations
(`MmapStorage`, then the `FaultInjectionStorage` that T0 drives), then the `recover`
integration, and finally T0 itself. The hybrid model is unchanged: **UNDO logical
(uncommitted), REDO physical (committed)**; the nexus is `txn_id ∈ committed`.

### Research grounding (research/ — port, don't copy)

- **SQLite `wal.c`** (`walIndexRecover`, `walIteratorInit`/`walCheckpoint`): recovery
  scans frames and stops at the first bad checksum (contiguous valid prefix), and the
  checkpoint keeps the **latest frame per page**. We already have both:
  `FrameLog::scan` (wal_frame.rs:275, stops at torn tail) and
  `FrameLog::build_index` (wal_frame.rs:312, latest-committed-per-page). Adaptation:
  SQLite detects a commit via the frame header's `nTruncate`; AxiomDB is multi-writer,
  so each frame self-identifies with `txn_id` and recovery supplies the committed
  predicate from the **logical** WAL.
- **PostgreSQL pageLSN idempotence** (`xlogutils.c:420`: apply only when
  `record_lsn > PageGetLSN`, i.e. skip on `<=`). Our guard `frame.lsn > page.lsn`
  (strict `>`) matches Postgres and is correct: a frame is a *full page image* whose
  embedded `PageHeader.lsn` (offset 24, outside the checksum-covered body) becomes the
  page's lsn once applied — so a re-run sees `frame.lsn == page.lsn` and skips. No
  separate `PageSetLSN` is needed. (InnoDB uses `>=`; we deliberately pick Postgres's
  `>` — fewer redundant applies on a recovery re-entry.)

## Dependencies

Must be done first:
- [x] spec-subfase-5-recovery approved
- [x] subphase 4 (per-frame `txn_id`, `commit_durable`, `build_index(predicate)`)

Blocks (until this plan is done):
- [ ] subphase 6 (drop per-commit flush, frame-only reads, contiguous prefix, checkpoint)

## Affected files

New files:
- `crates/axiomdb-storage/src/txn_stamp.rs` — shared thread-local txn stamp
  (extracted from mmap.rs so `MmapStorage` and `FaultInjectionStorage` share it)

Modified files:
- `crates/axiomdb-storage/src/page.rs` — `pub(crate) const LSN_OFFSET` + `lsn()`/`set_lsn()`
- `crates/axiomdb-storage/src/mmap.rs` — use `crate::page::LSN_OFFSET`, delegate the
  txn stamp to `txn_stamp`, add `redo_committed_frames`
- `crates/axiomdb-storage/src/wal_frame.rs` — `WalIndex::frames()`
- `crates/axiomdb-storage/src/engine.rs` — `redo_committed_frames` trait method (default no-op)
- `crates/axiomdb-storage/src/fault_injection.rs` — durable frame log + redo impl
- `crates/axiomdb-storage/src/lib.rs` — `mod txn_stamp;`
- `crates/axiomdb-wal/src/recovery.rs` — collect committed set, call REDO after UNDO,
  add `redone_pages` to `RecoveryResult`
- `crates/axiomdb-wal/tests/integration_redo_recovery.rs` — T0 un-ignore → green
- `docs-site/src/internals/wal.md` — recovery REDO section (close step)

## Invariant (the load-bearing one)

A frame's page bytes carry the page's `lsn` at byte offset 24 (`LSN_OFFSET`), which is
**outside** the checksum-covered body (`[HEADER_SIZE..]`). Therefore writing a frame's
full bytes to a page sets that page's lsn to the frame's lsn *atomically with* the
data, and the CRC stays valid. Both the write path (stamp lsn into the page) and the
redo guard (read the page's lsn, compare to the frame's) must use this same offset.

---

## Step 1 — `Page` LSN accessor + centralize `LSN_OFFSET`

**Goal:** one authoritative `LSN_OFFSET` and typed `lsn()`/`set_lsn()` so both storages
and the redo guard read/write the page LSN the same way.
**Files:** `page.rs` (add), `mmap.rs` (use the shared const).
**Approach:** TDD — assert the round-trip and the checksum invariant first.

### Test to add

```rust
// crates/axiomdb-storage/src/page.rs  (#[cfg(test)] mod)
#[test]
fn lsn_roundtrips_and_does_not_disturb_checksum() {
    let mut p = Page::new(PageType::Data, 7);
    p.body_mut()[0] = 0xAB;
    p.update_checksum();
    p.verify_checksum().unwrap();
    assert_eq!(p.lsn(), 0);
    p.set_lsn(42);
    assert_eq!(p.lsn(), 42);
    // lsn lives in the header, outside the checksum-covered body.
    p.verify_checksum().expect("set_lsn must not invalidate the checksum");
}
```

### Implementation outline

```rust
// page.rs — near the other header offsets / accessors
/// Byte offset of `PageHeader.lsn`: magic(8)+page_type(1)+flags(1)+item_count(2)
/// +checksum(4)+page_id(8) = 24. The checksum (offset 12) covers only the body
/// [HEADER_SIZE..], so stamping the lsn never invalidates it.
pub(crate) const LSN_OFFSET: usize = 24;

impl Page {
    pub fn lsn(&self) -> u64 {
        u64::from_le_bytes(self.as_bytes()[LSN_OFFSET..LSN_OFFSET + 8].try_into().unwrap())
    }
    pub fn set_lsn(&mut self, lsn: u64) {
        self.as_bytes_mut()[LSN_OFFSET..LSN_OFFSET + 8].copy_from_slice(&lsn.to_le_bytes());
    }
}
```

In `mmap.rs`: delete the local `const LSN_OFFSET` (mmap.rs:48) and its comment, and
change the one use in `write_page_inner` (mmap.rs:501) to `crate::page::LSN_OFFSET`.

### Verification

```bash
./tools/vm.sh test -p axiomdb-storage page
./tools/vm.sh clippy -p axiomdb-storage
```

### Commit

```
feat(redo-recovery): Page::lsn/set_lsn + centralized LSN_OFFSET (subphase 5 step 1)
```

---

## Step 2 — `WalIndex::frames()` iterator

**Goal:** let `redo_committed_frames` walk every page's latest-committed frame.
**Files:** `wal_frame.rs`.

### Test to add

```rust
// wal_frame.rs tests
#[test]
fn frames_returns_every_indexed_page() {
    let (_d, path) = tmp("frames.wf");
    let log = FrameLog::create(&path).unwrap();
    log.append(2, 1, 7, &page(0x10)).unwrap();
    log.append(3, 2, 7, &page(0x20)).unwrap();
    log.append(2, 3, 8, &page(0x11)).unwrap(); // page 2 latest = lsn 3
    let idx = log.build_index(&|_| true).unwrap();
    let mut got: Vec<(u64, u64)> = idx.frames().iter().map(|f| (f.page_id, f.lsn)).collect();
    got.sort_unstable();
    assert_eq!(got, vec![(2, 3), (3, 2)]);
}
```

### Implementation outline

```rust
// impl WalIndex
/// Snapshot of every page's recorded frame (recovery REDO; not a hot path).
pub fn frames(&self) -> Vec<FrameRef> {
    self.shards.iter()
        .flat_map(|s| s.lock().unwrap_or_else(|e| e.into_inner()).values().copied().collect::<Vec<_>>())
        .collect()
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-storage wal_frame
```

### Commit

```
feat(redo-recovery): WalIndex::frames() snapshot for REDO (subphase 5 step 2)
```

---

## Step 3 — `StorageEngine::redo_committed_frames` trait method (default no-op)

**Goal:** the recovery-callable entry point; backends without a redo log do nothing.
**Files:** `engine.rs`.

### Test to add

```rust
// engine.rs tests
#[test]
fn redo_is_noop_without_a_frame_log() {
    use crate::MemoryStorage;
    let storage = MemoryStorage::new();
    assert_eq!(storage.redo_committed_frames(&|_| true).unwrap(), 0);
}
```

### Implementation outline

```rust
// trait StorageEngine — after sync_frame_log (engine.rs:119)
/// REDO: apply every committed frame to its page so committed data survives a crash.
/// For each page with a committed frame (latest wins), if `frame.lsn > page.lsn`
/// write the frame's bytes to the page directly (no new frame appended).
/// `is_committed(txn_id)` selects committed frames. Returns the pages redone.
/// Default: no-op (backend without a redo log). Called once, on open, after UNDO.
fn redo_committed_frames(&self, is_committed: &dyn Fn(u64) -> bool) -> Result<usize, DbError> {
    let _ = is_committed;
    Ok(0)
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-storage engine
```

### Commit

```
feat(redo-recovery): StorageEngine::redo_committed_frames default no-op (subphase 5 step 3)
```

---

## Step 4 — `MmapStorage::redo_committed_frames`

**Goal:** the real on-disk REDO: build the committed index, and for each frame newer
than the on-disk page, `pwrite` the frame bytes + invalidate the buffer pool (NOT
`write_page`, which would append a second frame).
**Files:** `mmap.rs` (method inside `impl StorageEngine for MmapStorage`, before its
close at mmap.rs:1004).
**Approach:** TDD via a **real reopen** (the `.wf` persists across instances; `flock`
releases on drop). In-process power-loss can't be simulated on `MmapStorage`
(MAP_SHARED survives SIGKILL — see fault_injection.rs module doc), so we manufacture
the "frame ahead of main file" divergence with two sessions.

### Test to add

```rust
// mmap.rs tests
#[test]
fn redo_restores_a_page_whose_main_file_write_was_lost() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("redo.db");

    // Session A (redo ON): write ROW under committed txn 5 → frame ROW@lsn N durable.
    let page_id;
    let row_byte = 0xC7u8;
    {
        let mut s = MmapStorage::create(&db).unwrap();
        s.enable_redo_log(&db).unwrap();
        page_id = s.alloc_page(PageType::Data).unwrap();
        s.set_current_txn(5);
        let mut p = Page::new(PageType::Data, page_id);
        p.body_mut()[0] = row_byte;
        p.update_checksum();
        s.write_page(page_id, &p).unwrap();
        s.sync_frame_log().unwrap(); // frame durable in <db>.wf
    }
    // Session B (redo OFF): clobber the main-file page with an empty lsn-0 image,
    // modelling the lost (un-fsync'd) main-file write. The .wf is untouched.
    {
        let s = MmapStorage::open(&db).unwrap();
        s.write_page(page_id, &Page::new(PageType::Data, page_id)).unwrap();
        s.flush().unwrap();
        assert_eq!(s.read_page(page_id).unwrap().body()[0], 0, "main file lost the row");
    }
    // Session C (redo ON): REDO restores the row from the frame, idempotently.
    {
        let mut s = MmapStorage::open(&db).unwrap();
        s.enable_redo_log(&db).unwrap();
        assert_eq!(s.redo_committed_frames(&|t| t == 5).unwrap(), 1, "one page redone");
        assert_eq!(s.read_page(page_id).unwrap().body()[0], row_byte, "row restored");
        assert_eq!(s.redo_committed_frames(&|t| t == 5).unwrap(), 0, "idempotent re-run");
    }
}
```

### Implementation outline

```rust
// impl StorageEngine for MmapStorage
fn redo_committed_frames(&self, is_committed: &dyn Fn(u64) -> bool) -> Result<usize, DbError> {
    let Some(frame_log) = &self.frame_log else { return Ok(0); };
    let index = frame_log.build_index(is_committed)?;
    let mut redone = 0usize;
    for frame in index.frames() {
        // On-disk page lsn (raw, no checksum verify). Absent page → out of scope here
        // (file growth on redo is checkpoint territory, subphase 6) → skip.
        let page_lsn = match self.read_page_raw(frame.page_id) {
            Ok(bytes) => u64::from_le_bytes(
                bytes[crate::page::LSN_OFFSET..crate::page::LSN_OFFSET + 8].try_into().unwrap()),
            Err(DbError::PageNotFound { .. }) => continue,
            Err(e) => return Err(e),
        };
        if frame.lsn > page_lsn {
            let page_bytes = frame_log.read_page_at(frame.offset)?;
            self.pwrite_bytes(frame.page_id * PAGE_SIZE as u64, page_bytes.as_slice())?;
            self.buffer_pool.invalidate(frame.page_id);
            redone += 1;
        }
    }
    Ok(redone)
}
```

> ⚠️ DEFERRED — a committed frame for a page that no longer exists in the main file
> (allocated after the last flush, then lost) needs file growth to restore. That is
> the checkpoint's job → subphase 6. Tracked in progreso.md.

### Verification

```bash
./tools/vm.sh test -p axiomdb-storage mmap
./tools/vm.sh clippy -p axiomdb-storage
```

### Commit

```
feat(redo-recovery): MmapStorage::redo_committed_frames (subphase 5 step 4)
```

---

## Step 5 — `FaultInjectionStorage`: shared txn stamp + durable frame log plumbing

**Goal:** give the test storage the same write-ahead plumbing as `MmapStorage` so T0
can drive it: a real `FrameLog` that survives `simulate_power_loss`, a per-txn stamp,
and `sync_frame_log`. No REDO yet (next step).
**Files:** new `txn_stamp.rs`, `lib.rs`, `mmap.rs` (delegate stamp), `fault_injection.rs`.

### Test to add

```rust
// fault_injection.rs tests
#[test]
fn frame_log_survives_power_loss_while_data_page_reverts() {
    let dir = tempfile::tempdir().unwrap();
    let mut storage = FaultInjectionStorage::new();
    storage.enable_redo_log(&dir.path().join("fi.wf")).unwrap();

    let id = storage.alloc_page(PageType::Data).unwrap();
    storage.write_page(id, &data_page(id, 0xAA)).unwrap(); // baseline, txn 0
    storage.flush().unwrap();

    storage.set_current_txn(5);
    storage.write_page(id, &data_page(id, 0xBB)).unwrap();  // committed-row write
    storage.sync_frame_log().unwrap();
    storage.set_current_txn(0);

    storage.simulate_power_loss();
    // Data page reverted to the durable baseline …
    assert_eq!(storage.read_page(id).unwrap().body()[0], 0xAA);
    // … but the fsync'd frame for txn 5 is still in the (separate) frame log.
    assert!(storage.frame_log_has_committed(id, &|t| t == 5));
}
```

(`frame_log_has_committed` is a tiny `#[cfg(test)]`/diagnostic helper:
`build_index(pred).latest(page_id).is_some()`.)

### Implementation outline

```rust
// txn_stamp.rs (new) — the thread-local moved out of mmap.rs verbatim
thread_local! { static CURRENT_TXN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
pub(crate) fn set(txn_id: u64) { CURRENT_TXN.with(|c| c.set(txn_id)); }
pub(crate) fn get() -> u64 { CURRENT_TXN.with(|c| c.get()) }
```

```rust
// fault_injection.rs — new fields + plumbing
pub struct FaultInjectionStorage {
    state: RwLock<State>,
    page_locks: crate::page_lock::PageLockTable,
    frame_log: Option<FrameLog>,     // set once by enable_redo_log(&mut self)
    frame_lsn: AtomicU64,            // 0 while redo disabled
}

impl FaultInjectionStorage {
    pub fn enable_redo_log(&mut self, path: &Path) -> Result<(), DbError> {
        self.frame_log = Some(FrameLog::create(path)?);
        self.frame_lsn.store(1, Ordering::Relaxed);
        Ok(())
    }
}

// write_page_inner: when frame_log is Some, stamp lsn + append a frame (mirror mmap).
fn write_page_inner(&self, page_id: u64, page: &Page) -> Result<(), DbError> {
    page.verify_checksum()?;
    if let Some(frame_log) = &self.frame_log {
        let lsn = self.frame_lsn.fetch_add(1, Ordering::Relaxed);
        let mut stamped = page.clone();
        stamped.set_lsn(lsn);
        let txn_id = crate::txn_stamp::get();
        frame_log.append(page_id, lsn, txn_id, stamped.as_bytes())?;
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        state.current.ensure_capacity(page_id);
        let idx = page_id as usize;
        state.current.pages[idx] = stamped;
        state.current.allocated[idx] = true;
    } else {
        // unchanged existing path
    }
    Ok(())
}

impl StorageEngine for FaultInjectionStorage {
    fn set_current_txn(&self, txn_id: u64) { crate::txn_stamp::set(txn_id); }
    fn current_txn(&self) -> u64 { crate::txn_stamp::get() }
    fn sync_frame_log(&self) -> Result<(), DbError> {
        match &self.frame_log { Some(fl) => fl.sync(), None => Ok(()) }
    }
    // … existing methods …
}
```

`mmap.rs`: replace the local `thread_local! CURRENT_TXN` (mmap.rs:50-57) and its uses
(`write_page_inner` + the `set_current_txn`/`current_txn` impls) with
`crate::txn_stamp::{set,get}`. `simulate_power_loss` is unchanged — it reverts only
`state`, never the frame log.

### Verification

```bash
./tools/vm.sh test -p axiomdb-storage fault_injection
./tools/vm.sh test -p axiomdb-storage mmap   # txn-stamp extraction regression
```

### Commit

```
feat(redo-recovery): FaultInjectionStorage durable frame log + shared txn stamp (subphase 5 step 5)
```

---

## Step 6 — `FaultInjectionStorage::redo_committed_frames` + idempotence

**Goal:** the REDO on the test storage — write each committed, newer frame into **both**
the `current` and `durable` layers (so the redone state is itself durable). This is the
algorithm T0 exercises.
**Files:** `fault_injection.rs`.

### Test to add

```rust
// fault_injection.rs tests
#[test]
fn redo_restores_committed_row_after_power_loss_idempotently() {
    let dir = tempfile::tempdir().unwrap();
    let mut storage = FaultInjectionStorage::new();
    storage.enable_redo_log(&dir.path().join("fi.wf")).unwrap();

    let id = storage.alloc_page(PageType::Data).unwrap();
    storage.write_page(id, &data_page(id, 0xAA)).unwrap(); // baseline txn 0
    storage.flush().unwrap();

    storage.set_current_txn(5);
    storage.write_page(id, &data_page(id, 0xBB)).unwrap();  // committed row
    storage.sync_frame_log().unwrap();
    storage.set_current_txn(0);
    storage.simulate_power_loss();
    assert_eq!(storage.read_page(id).unwrap().body()[0], 0xAA, "row lost on crash");

    assert_eq!(storage.redo_committed_frames(&|t| t == 5).unwrap(), 1);
    assert_eq!(storage.read_page(id).unwrap().body()[0], 0xBB, "row restored by REDO");
    // crash-during-recovery: a second pass is a no-op (pageLSN guard).
    assert_eq!(storage.redo_committed_frames(&|t| t == 5).unwrap(), 0);
    // an uncommitted txn's frame is never applied.
    assert_eq!(storage.redo_committed_frames(&|_| false).unwrap(), 0);
}
```

### Implementation outline

```rust
// impl StorageEngine for FaultInjectionStorage
fn redo_committed_frames(&self, is_committed: &dyn Fn(u64) -> bool) -> Result<usize, DbError> {
    let Some(frame_log) = &self.frame_log else { return Ok(0); };
    let index = frame_log.build_index(is_committed)?;
    let mut redone = 0usize;
    let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
    for frame in index.frames() {
        let idx = frame.page_id as usize;
        let page_lsn = if idx < state.current.pages.len() && state.current.allocated[idx] {
            state.current.pages[idx].lsn()
        } else { 0 };
        if frame.lsn > page_lsn {
            let page = Page::from_bytes(*frame_log.read_page_at(frame.offset)?)?;
            for layer in [&mut state.current, &mut state.durable] {
                layer.ensure_capacity(frame.page_id);
                layer.pages[idx] = page.clone();
                layer.allocated[idx] = true;
            }
            redone += 1;
        }
    }
    Ok(redone)
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-storage fault_injection
```

### Commit

```
feat(redo-recovery): FaultInjectionStorage::redo_committed_frames + idempotence (subphase 5 step 6)
```

---

## Step 7 — `recover` integration (committed set + REDO after UNDO)

**Goal:** recovery collects the committed-txn set during the forward scan and, after the
logical UNDO pass, calls `storage.redo_committed_frames`. Add `redone_pages` to
`RecoveryResult` for observability/tests.
**Files:** `recovery.rs`.

### Test to add

```rust
// recovery.rs tests — REDO call is harmless for a no-redo backend (no regression)
#[test]
fn recover_reports_zero_redone_without_a_frame_log() {
    let (_dir, wal) = temp_setup();
    let mut storage = MemoryStorage::new();
    let page_id = fresh_data_page(&mut storage);
    let mgr = TxnManager::create(&wal).unwrap();
    let mut conn = mgr.begin().unwrap();
    let mut page = Page::from_bytes(*storage.read_page(page_id).unwrap().as_bytes()).unwrap();
    let slot = insert_tuple(&mut page, b"x", conn.txn_id).unwrap();
    storage.write_page(page_id, &page).unwrap();
    mgr.record_insert(&mut conn, 1, b"k", b"x", page_id, slot).unwrap();
    drop(mgr); // crash, uncommitted
    let r = CrashRecovery::recover(&mut storage, &wal).unwrap();
    assert_eq!(r.undone_txns, 1);
    assert_eq!(r.redone_pages, 0); // MemoryStorage has no frame log
}
```

(The headline committed+REDO assertion is T0 in step 8.)

### Implementation outline

```rust
// recover() — add alongside max_committed / active_txns (recovery.rs:169-170)
let mut committed: HashSet<u64> = HashSet::new();
// in the EntryType::Commit arm (recovery.rs:190):
committed.insert(entry.txn_id);

// after the UNDO loop closes (recovery.rs:542), before storage.flush() (recovery.rs:545):
let redone_pages = storage.redo_committed_frames(&|t| committed.contains(&t))?;

// RecoveryResult: add `pub redone_pages: usize;` and set it in the returned struct.
```

Order is UNDO (logical, uncommitted) → REDO (physical, committed) → `flush()` (makes
both durable). For T0 there are no uncommitted txns so order is moot; the general
same-page interaction is a subphase-7 case.

### Verification

```bash
./tools/vm.sh test -p axiomdb-wal recovery
```

### Commit

```
feat(redo-recovery): recover REDOes committed frames after UNDO (subphase 5 step 7)
```

---

## Step 8 — T0 GREEN (un-ignore)

**Goal:** flip the headline test. Enable the redo log, stamp the txn, commit durably,
remove `#[ignore]`. Add the idempotent-recover assertion.
**Files:** `crates/axiomdb-wal/tests/integration_redo_recovery.rs`.

### Edits (per spec)

```rust
// remove the #[ignore] attribute
let mut storage = FaultInjectionStorage::new();
storage.enable_redo_log(&dir.path().join("t0.wf")).unwrap(); // before any write

// inside the txn, before write_page:
storage.set_current_txn(conn.txn_id);
// … insert_tuple + write_page …
txn.commit_durable(conn, &storage).unwrap(); // was txn.commit(conn)
// (reset the stamp after the txn: storage.set_current_txn(0);)
```

After `simulate_power_loss` the precondition (row gone) holds; `open_with_recovery`
runs `recover` → REDO restores the row. Add: a second `CrashRecovery::recover` returns
`redone_pages == 0` (crash-during-recovery is a no-op).

### Verification

```bash
./tools/vm.sh test -p axiomdb-wal --run-ignored all t0_committed_heap_insert_survives_power_loss
./tools/vm.sh test -p axiomdb-wal t0_committed_heap_insert_survives_power_loss   # now runs without --run-ignored
```

### Commit

```
feat(redo-recovery): T0 green — committed insert survives power loss (subphase 5 step 8)
```

---

## Step 9 — Close (workspace, docs, memory)

**Goal:** verify against the spec's Done criteria and close the subphase.

### Verification against spec Done criteria

- [ ] `StorageEngine::redo_committed_frames` (default no-op) + `MmapStorage` + `FaultInjectionStorage` impls
- [ ] `FaultInjectionStorage` durable frame log survives `simulate_power_loss`
- [ ] `recover` collects the committed set and calls `redo_committed_frames`
- [ ] **T0 GREEN** (un-`#[ignore]`d)
- [ ] idempotent-replay test green; existing UNDO/dirty-open recovery tests green
- [ ] workspace tests + clippy (storage, wal) + fmt clean

```bash
./tools/vm.sh test --workspace
./tools/vm.sh clippy -p axiomdb-storage -p axiomdb-wal
./tools/vm.sh fmt-check
```

### Docs + memory

- `docs-site/src/internals/wal.md` — add the **recovery REDO** subsection (hybrid:
  UNDO logical + REDO physical; pageLSN guard; `callout-design` citing Postgres `>`
  and SQLite contiguous-prefix scan).
- `docs/progreso.md` — mark subphase 5 `[x]`, note the DEFERRED grow-on-redo → 6.
- `memory/project_insert_perf.md` + checkpoint — subphase 5 done, subphase 6 next.

### Final commit

```
feat(redo-recovery): close subphase 5 — REDO recovery, T0 green

Implements specs/fase-redo-recovery/spec-subfase-5-recovery.md
Plan: specs/fase-redo-recovery/plan-subfase-5-recovery.md
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| txn-stamp extraction breaks subphase-4 mmap behavior | low | Step 5 re-runs the mmap suite; the thread-local moves verbatim |
| `read_page_raw` errors on an absent page during Mmap REDO | low | Skip `PageNotFound` (grow-on-redo is DEFERRED to subphase 6) |
| Adding `redone_pages` to `RecoveryResult` breaks a constructor | low | Only one constructor (recovery.rs:547); field is additive |
| `Page::from_bytes` rejects a frame image | low | Frames are written by `write_page` (valid CRC; lsn is outside the body) |
| Holding `state.write()` across frame-log reads in FI REDO | none (perf) | Recovery is one-time open cost, not a hot path |

## Rollback plan

Each step is an isolated commit. To abandon: `git reset --hard <commit before step 1>`.
Steps 1–7 are additive (redo stays opt-in/off in production until subphase 6), so a
partial landing leaves the engine behaving exactly as today; only T0 (step 8) changes
an observable test outcome.

## Estimated effort

Total: ~1 day. Steps 1–3 ~30 min each; step 4 ~1h; step 5 ~1.5h (extraction + plumbing);
step 6 ~1h; step 7 ~45 min; step 8 ~45 min; step 9 (close) ~1h.

Implementation effort level: **max** (durability/recovery; data-loss surface) per spec.
