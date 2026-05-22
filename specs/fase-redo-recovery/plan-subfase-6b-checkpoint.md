# Plan: subphase 6b — frame checkpoint (frames → main, recycle the log)

Phase: redo-recovery (project B) — subphase 6b
Spec: specs/fase-redo-recovery/spec-subfase-6b-checkpoint.md
Status: done

## Summary

Build the frame checkpoint bottom-up: first `FrameLog::recycle` (reset the log to a fresh
header after a checkpoint), then extract the shared frame-apply into
`MmapStorage::apply_committed_frames` with **grow-on-redo** (resolving the subphase-5
DEFERRED) **+ a freelist reload** (so a redone bitmap page doesn't leave the in-memory
freelist stale), then the `StorageEngine::checkpoint_frames` trait method + the
`MmapStorage` impl (exclusive `checkpoint_lock` write-guard → apply → fsync main → recycle;
the frame append takes the read-guard), then the `FaultInjectionStorage` impl + a
concurrent-writers test, then close. Concurrency model **A** (exclusive guard) per the spec.

Additive: dual-write + the per-commit flush stay, so the apply is usually a pageLSN no-op
in production — the new behavior (recycle, grow-on-redo) is exercised via a **stale-main**
scenario (clobber the main page, checkpoint → restored + log reset).

### Plan-level decisions (spec open questions resolved)

- **Checkpoint-LSN coordination:** `checkpoint_frames` is the **storage-side** op only
  (apply + fsync main + recycle log). It does NOT advance the logical-WAL checkpoint LSN.
  Recovery stays correct: after a recycle the frame log is empty ⇒ frame REDO is a no-op
  and the main file is current (we fsync'd it); the logical UNDO still runs from the
  existing `checkpoint_lsn` (just re-scans more — harmless). Tighter logical-WAL trimming
  is a 6c/7 optimization.
- **Trigger policy → DEFERRED to 6c.** 6b delivers the checkpoint *mechanism* (trait +
  impls), exercised in tests with an explicit committed predicate. The production trigger
  (size / clean-shutdown / auto) needs the `TxnManager` committed-set + `Database` wiring
  and only *matters* once 6c makes writes frame-only (the log can't be recycled lazily
  before then because dual-write keeps the main current). Spec listed it; scoping it to 6c
  keeps 6b inside `axiomdb-storage`.
- **In-flight frames vs recycle → safe in 6b, DEFERRED hardening to 6c.** 6b recycles the
  *whole* log under the exclusive guard; safe because dual-write + the checkpoint's
  `flush()` put every page (committed or in-flight) in the main file before the truncate.
  Once 6c removes dual-write, an in-flight txn's frames live ONLY in the log, so 6c must
  preserve them (quiesce in-flight txns, or recycle only a fully-committed prefix). Marked
  ⚠️ DEFERRED below.

## Dependencies

Must be done first:
- [x] spec-subfase-6b approved (model A)
- [x] subphase 5 (`redo_committed_frames` apply) + 6a (`durable_offset`)

Blocks:
- [ ] subphase 6c (frame-only writes need the checkpoint to drain the log)

## Affected files

Modified files:
- `crates/axiomdb-storage/src/wal_frame.rs` — `salt: AtomicU64` + `FrameLog::recycle`.
- `crates/axiomdb-storage/src/engine.rs` — `checkpoint_frames` trait method (default no-op).
- `crates/axiomdb-storage/src/mmap.rs` — `apply_committed_frames` (grow-on-redo + freelist
  reload), `redo_committed_frames` delegates, `checkpoint_lock: RwLock<()>`, append takes
  the read-guard, `checkpoint_frames` impl.
- `crates/axiomdb-storage/src/fault_injection.rs` — `checkpoint_frames` impl + recycle.
- `docs-site/src/internals/wal.md` — checkpoint subsection (close).

---

## Step 1 — `FrameLog::recycle` (+ `salt: AtomicU64`)

**Goal:** reset the log to a fresh, empty header after a checkpoint.
**Files:** `wal_frame.rs`.
**Approach:** TDD. `salt` becomes `AtomicU64` so `recycle(&self)` can swap it (the field is
read on every `append`/`scan`; recycle runs under the caller's exclusive guard).

### Test to add

```rust
#[test]
fn recycle_resets_to_empty_with_a_fresh_salt() {
    let (_d, path) = tmp("recycle.wf");
    let log = FrameLog::create(&path).unwrap();
    let old_salt = log.salt();
    log.append(2, 1, 7, &page(1)).unwrap();
    log.sync_to_durable().unwrap();
    log.recycle().unwrap();
    assert_ne!(log.salt(), old_salt, "fresh salt invalidates any stale tail");
    assert_eq!(log.scan().unwrap().len(), 0, "log is empty after recycle");
    assert_eq!(log.contiguous_written_offset(), FILE_HDR_SIZE);
    assert_eq!(log.durable_offset(), FILE_HDR_SIZE);
    // New appends start fresh and are scannable.
    log.append(3, 2, 8, &page(2)).unwrap();
    assert_eq!(log.scan().unwrap().len(), 1);
}
```

### Implementation outline

```rust
// salt: AtomicU64 (create/open store it; append/scan/salt() load it).
/// Reset the log after a checkpoint: rewrite the header with a FRESH salt, truncate to
/// FILE_HDR_SIZE, fsync, and reset the offset watermarks. Caller MUST hold the exclusive
/// checkpoint guard (no concurrent appends). Does not touch any external LSN counter.
pub fn recycle(&self) -> Result<(), DbError> {
    let salt = fresh_salt();
    // write header bytes (magic|version|page_size|salt|hcrc) like create(), then:
    self.file.set_len(FILE_HDR_SIZE)?;          // drop all frames
    self.file.write_all_at(&hdr, 0)?;
    self.file.sync_all()?;
    self.salt.store(salt, Ordering::Release);
    self.write_offset.store(FILE_HDR_SIZE, Ordering::Release);
    self.durable.store(FILE_HDR_SIZE, Ordering::Release);
    let mut s = self.sync_state.lock()…; s.completed.clear();
    s.contiguous_written = FILE_HDR_SIZE; s.poison = None;
    Ok(())
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-storage wal_frame
```

### Commit

```
feat(redo-recovery): FrameLog::recycle + atomic salt (subphase 6b step 1)
```

---

## Step 2 — `MmapStorage::apply_committed_frames` (grow-on-redo + freelist reload)

**Goal:** one shared apply used by both recovery REDO and the checkpoint; restore pages
beyond EOF and keep the in-memory freelist consistent with a redone bitmap.
**Files:** `mmap.rs`.

### Test to add

```rust
#[test]
fn redo_grows_the_file_for_a_committed_frame_beyond_eof() {
    // Session A (redo on): alloc page, write ROW under txn 5, sync frame log, note page_id.
    //   Then SHRINK the main file below page_id (set_len) to model a lost post-flush alloc.
    // Session C (reopen, redo on): redo_committed_frames(|t| t==5) grows the file and
    //   restores ROW; read_page(page_id) returns ROW; page_count covers page_id.
    // (Construct via a #[cfg(test)] truncate-main hook or a second redo-off session that
    //  set_len's the file; assert restored + no PageNotFound.)
}
```

### Implementation outline

```rust
fn apply_committed_frames(&self, is_committed: &dyn Fn(u64) -> bool) -> Result<usize, DbError> {
    let Some(frame_log) = &self.frame_log else { return Ok(0); };
    let index = frame_log.build_index(is_committed)?;
    let mut touched_bitmap = false;
    let mut applied = 0usize;
    for frame in index.frames() {
        // grow-on-redo: a committed frame for a page past EOF (alloc'd after last flush).
        if frame.page_id >= self.page_count.load(Ordering::Acquire) {
            let _g = self.grow_lock.lock()…;
            let cur = self.page_count.load(Ordering::Acquire);
            if frame.page_id >= cur { self.do_grow(frame.page_id + 1 - cur)?; }
        }
        let page_lsn = read raw lsn at frame.page_id (LSN_OFFSET);  // PageNotFound→0
        if frame.lsn > page_lsn {
            let bytes = frame_log.read_page_at(frame.offset)?;
            self.pwrite_bytes(frame.page_id * PAGE_SIZE as u64, &bytes[..])?;
            self.buffer_pool.invalidate(frame.page_id);
            if frame.page_id == FREELIST_PAGE_ID { touched_bitmap = true; }
            applied += 1;
        }
    }
    // A redone bitmap page makes the in-memory freelist stale → reload it from page 1.
    if touched_bitmap { self.reload_freelist_from_disk()?; }
    Ok(applied)
}

fn redo_committed_frames(&self, is_committed) -> Result<usize, DbError> {
    self.apply_committed_frames(is_committed)   // now grows + reloads freelist
}
```

`reload_freelist_from_disk`: read page 1, `FreeList::from_bytes(body, page_count)`, swap
into `self.freelist`. (FREELIST_PAGE_ID = 1.)

> ⚠️ NOTE: enabling grow + freelist-reload in `redo_committed_frames` strictly improves
> recovery (resolves the subphase-5 DEFERRED absent-page case); existing redo/T0 tests use
> in-range pages so they are unaffected.

### Verification

```bash
./tools/vm.sh test -p axiomdb-storage mmap
./tools/vm.sh test -p axiomdb-wal t0_committed_heap_insert_survives_power_loss
```

### Commit

```
feat(redo-recovery): apply_committed_frames with grow-on-redo + freelist reload (subphase 6b step 2)
```

---

## Step 3 — `checkpoint_frames` trait + `MmapStorage` impl (exclusive guard)

**Goal:** the checkpoint: under an exclusive guard, apply committed frames → main, fsync,
recycle the log. The frame append path takes the shared read-guard.
**Files:** `engine.rs` (trait default no-op), `mmap.rs`.

### Tests to add

```rust
#[test]
fn checkpoint_restores_stale_main_then_recycles_the_log() {
    // redo on: write ROW under txn 5, sync; clobber the main page to an older image
    // (second redo-off session set_len/pwrite, like the subphase-5 reopen test);
    // reopen redo on: checkpoint_frames(|t| t==5) → main page == ROW AND the frame log is
    // empty (a fresh FrameLog::open scan returns 0); a second checkpoint applies 0.
}
```

### Implementation outline

```rust
// engine.rs
fn checkpoint_frames(&self, is_committed: &dyn Fn(u64) -> bool) -> Result<usize, DbError> {
    let _ = is_committed; Ok(0)
}

// mmap.rs: new field `checkpoint_lock: RwLock<()>`.
// write_page_inner (redo-on branch): hold `let _ck = self.checkpoint_lock.read()…;` around
//   the frame append + index record (so a checkpoint's recycle can't race an append).
fn checkpoint_frames(&self, is_committed) -> Result<usize, DbError> {
    let Some(frame_log) = &self.frame_log else { return Ok(0); };
    let _ckpt = self.checkpoint_lock.write()…;        // exclusive: drains in-flight appends
    let applied = self.apply_committed_frames(is_committed)?;
    self.flush()?;                                     // fsync main (applied pages durable)
    frame_log.recycle()?;                             // reset the log (fresh salt)
    Ok(applied)
}
```

Ordering invariant (spec): apply → fsync main → recycle. A crash before `flush` leaves the
main unchanged (frames replay); after `flush` before `recycle` the frames replay
idempotently; after `recycle` the log is empty and the main is current.

### Verification

```bash
./tools/vm.sh test -p axiomdb-storage mmap
```

### Commit

```
feat(redo-recovery): MmapStorage::checkpoint_frames (exclusive guard) (subphase 6b step 3)
```

---

## Step 4 — `FaultInjectionStorage::checkpoint_frames` + concurrent writers

**Goal:** the test-vehicle checkpoint (apply into current+durable, recycle the log) and a
concurrency test for the guard.
**Files:** `fault_injection.rs`.

### Tests to add

```rust
#[test]
fn checkpoint_applies_committed_then_recycles() {
    // enable_redo; write ROW txn5; sync; simulate_power_loss (data reverts, frame survives);
    // checkpoint_frames(|t|t==5) → read_page == ROW (applied into current+durable) AND the
    // frame log is empty afterward (frame_log_has_committed == false).
}

#[test]
fn checkpoint_excludes_concurrent_writers_safely() {
    // 8 threads append+commit while one thread checkpoints; no panic/deadlock; afterward
    // every committed page is either in the main layer or (post-recycle) consistently gone-
    // from-log-but-present-in-data. (Assert no committed row is lost.)
}
```

### Implementation outline

```rust
// FaultInjectionStorage gets a checkpoint guard (RwLock<()>); write_page_inner (redo on)
// takes read; checkpoint_frames takes write, applies build_index into BOTH layers
// (ensure_capacity already grows), then frame_log.recycle().
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-storage fault_injection
```

### Commit

```
feat(redo-recovery): FaultInjectionStorage::checkpoint_frames + concurrency test (subphase 6b step 4)
```

---

## Step 5 — Close

### Verification against spec Done criteria

- [ ] `checkpoint_frames` (default no-op) + Mmap + FaultInjection impls per the ordering invariant
- [ ] grow-on-redo + freelist reload; log recycle (fresh salt, frame LSN preserved)
- [ ] stale-main apply+recycle, grow-on-redo, idempotent, concurrent-writers tests green
- [ ] existing recovery + T0 + 6a suites green

```bash
./tools/vm.sh test --workspace
limactl shell axiomdb -- bash -c "source ~/.cargo/env && CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo clippy -p axiomdb-storage -p axiomdb-wal -- -D warnings 2>&1"
limactl shell axiomdb -- bash -c "source ~/.cargo/env && cargo fmt --check -p axiomdb-storage -p axiomdb-wal 2>&1"
```

### Docs + memory

- `docs-site/src/internals/wal.md` — checkpoint subsection (apply→fsync→recycle invariant,
  grow-on-redo, exclusive guard; `callout-design` citing SQLite `walCheckpoint`).
- `memory/project_insert_perf.md` + `docs/checkpoint-redo-recovery.md` — 6b done, 6c next
  (trigger wiring + in-flight-frame-safe recycle land in 6c).

### Final commit

```
feat(redo-recovery): close subphase 6b — frame checkpoint
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Redone bitmap page leaves the in-memory freelist stale | medium | reload freelist from page 1 after apply (step 2) |
| Recycle races a concurrent append | medium | exclusive `checkpoint_lock` write-guard; append takes read |
| Read-guard on the append path adds hot-path cost | low | shared lock (parallel appends); negligible vs the 16 KB pwrite |
| `frame_lsn` reset on recycle would break the pageLSN guard | low (avoided) | recycle resets file offsets + salt only; `frame_lsn` (in MmapStorage) is untouched |
| ⚠️ In-flight frames lost by full recycle once dual-write is gone | n/a in 6b | safe here (dual-write + checkpoint fsync put all pages in main); 6c must preserve in-flight frames — **DEFERRED to 6c** |
| Grow during recovery vs the freelist bitmap | medium | apply the bitmap frame + reload freelist; do_grow under grow_lock (uncontended at open) |

## Rollback plan

Each step is an isolated commit. To abandon: `git reset --hard <commit before step 1>`.
Additive (redo opt-in; dual-write + per-commit flush remain), so a partial landing changes
no production behavior.

## Estimated effort

Total: ~1 day. Step 1 ~45min, step 2 ~1.5h (grow + freelist reload), step 3 ~1.5h (guard +
checkpoint), step 4 ~1h, step 5 ~1h. Implementation effort: **max** (data-mover +
concurrency + freelist consistency).
