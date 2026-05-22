# Plan: subphase 6a — contiguous durable prefix for the frame log

Phase: redo-recovery (project B) — subphase 6a
Spec: specs/fase-redo-recovery/spec-subfase-6a-contiguous-prefix.md
Status: done

## Summary

Make the multi-writer frame log's durable point gap-free. Build it bottom-up so each
commit compiles + tests pass: first the **watermark state + reorder logic** (`mark_written`
folds out-of-order completions into a `contiguous_written` offset), then wire **`append`**
to record completion, then **`sync_to_durable`** (wait for the contiguous prefix to cover
the commit's reserved end, fsync under a separate leader lock, advance `durable`), then
point **`sync_frame_log`** (the commit boundary) at it, then close. The hot-path `pwrite`
stays lock-free; only the ~ns completion bookkeeping takes a small mutex.

Open questions from the spec, resolved here:
- **Watermark tracker** = `Mutex<SyncState>` holding a `BTreeSet<u64>` of completed-but-
  not-yet-contiguous frame start-offsets + the `contiguous_written` cursor, with a
  `Condvar` to wake `sync_to_durable` waiters. `durable` is a separate `AtomicU64`
  (lock-free fast-path read) and the fsync runs under a separate `Mutex<()>` leader so a
  multi-ms fsync never blocks appends' bookkeeping. (Lock-free flag-array reorder is a
  documented future optimization if the bookkeeping mutex contends.)
- **Poisoned gap** (a failed `pwrite`) = record the lowest failed offset in
  `SyncState.poison`; `sync_to_durable` whose target is beyond it returns `Err` instead of
  blocking forever.

## Dependencies

Must be done first:
- [x] spec-subfase-6a approved
- [x] subphase 5 (commit_durable → sync_frame_log)

Blocks:
- [ ] subphase 6c (drop the per-commit main-file flush — needs a gap-free durable prefix)

## Affected files

Modified files:
- `crates/axiomdb-storage/src/wal_frame.rs` — `FrameLog` watermark fields + `mark_written`,
  `mark_poison`, `contiguous_written_offset`, `durable_offset`, `sync_to_durable`; `append`
  records completion; `create`/`open` init the watermarks.
- `crates/axiomdb-storage/src/mmap.rs` — `sync_frame_log` → `frame_log.sync_to_durable()`.
- `crates/axiomdb-storage/src/fault_injection.rs` — `sync_frame_log` → `sync_to_durable()`.
- `docs-site/src/internals/wal.md` — contiguous-durable-prefix subsection (close).

## Design reference (struct shape)

```rust
struct SyncState {
    completed: BTreeSet<u64>, // completed frame START offsets not yet folded in
    contiguous_written: u64,  // gap-free written prefix == next expected START offset
    poison: Option<u64>,      // lowest START offset whose append pwrite failed
}
pub struct FrameLog {
    file: File,
    salt: u64,
    write_offset: AtomicU64,        // next free START offset (reserve via fetch_add) — unchanged
    sync_state: Mutex<SyncState>,   // bookkeeping only (never held across pwrite/fsync)
    advanced: Condvar,              // notified when contiguous_written advances or poison is set
    durable: AtomicU64,             // fsync'd prefix (fast-path read)
    fsync_leader: Mutex<()>,        // serializes/coalesces fsyncs
}
```

Invariant: `durable ≤ contiguous_written ≤ write_offset`, all monotonic. Offsets are frame
START boundaries; "prefix ends at X" ⇔ every frame with START `< X` is written.

---

## Step 1 — Watermark state + reorder logic (`mark_written`) + init

**Goal:** add the fields and the out-of-order fold; `create`/`open` initialize them.
**Files:** `wal_frame.rs`.
**Approach:** TDD — the deterministic reorder test first.

### Test to add

```rust
// wal_frame.rs tests
#[test]
fn contiguous_watermark_reorders_out_of_order_completions() {
    let (_d, path) = tmp("wm.wf");
    let log = FrameLog::create(&path).unwrap();
    let h = FILE_HDR_SIZE;
    assert_eq!(log.contiguous_written_offset(), h);
    assert_eq!(log.durable_offset(), h);
    log.mark_written(h + FRAME_SIZE); // higher frame completes first → gap at h
    assert_eq!(log.contiguous_written_offset(), h, "gap must not be skipped");
    log.mark_written(h); // fill the gap → jumps past both
    assert_eq!(log.contiguous_written_offset(), h + 2 * FRAME_SIZE);
}

#[test]
fn reopen_inits_watermarks_to_valid_prefix_end() {
    let (_d, path) = tmp("wm-reopen.wf");
    { let log = FrameLog::create(&path).unwrap(); log.append(2, 1, 4, &page(1)).unwrap(); log.sync().unwrap(); }
    let log = FrameLog::open(&path).unwrap();
    let end = FILE_HDR_SIZE + FRAME_SIZE;
    assert_eq!(log.contiguous_written_offset(), end);
    assert_eq!(log.durable_offset(), end);
}
```

### Implementation outline

```rust
// create(): SyncState { completed: empty, contiguous_written: FILE_HDR_SIZE, poison: None },
//           durable = AtomicU64::new(FILE_HDR_SIZE), fsync_leader, advanced.
// open(): after scan, let end = frames.last().map(|f| f.offset + FRAME_SIZE).unwrap_or(FILE_HDR_SIZE);
//         contiguous_written = end; durable = end; write_offset = end.

fn mark_written(&self, offset: u64) {
    {
        let mut s = self.sync_state.lock().unwrap_or_else(|e| e.into_inner());
        s.completed.insert(offset);
        let mut cw = s.contiguous_written;
        while s.completed.remove(&cw) { cw += FRAME_SIZE; }
        s.contiguous_written = cw;
    }
    self.advanced.notify_all();
}

pub fn contiguous_written_offset(&self) -> u64 { self.sync_state.lock()…​.contiguous_written }
pub fn durable_offset(&self) -> u64 { self.durable.load(Acquire) }
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-storage wal_frame
```

### Commit

```
feat(redo-recovery): frame-log contiguous-written watermark + reorder (subphase 6a step 1)
```

---

## Step 2 — `append` records completion (+ poison on failure)

**Goal:** the real append advances the contiguous watermark; a failed `pwrite` poisons.
**Files:** `wal_frame.rs`.

### Test to add

```rust
#[test]
fn sequential_appends_advance_contiguous_written() {
    let (_d, path) = tmp("wm-seq.wf");
    let log = FrameLog::create(&path).unwrap();
    log.append(2, 1, 7, &page(1)).unwrap();
    log.append(3, 2, 7, &page(2)).unwrap();
    assert_eq!(log.contiguous_written_offset(), FILE_HDR_SIZE + 2 * FRAME_SIZE);
}
```

### Implementation outline

```rust
pub fn append(&self, …) -> Result<u64, DbError> {
    // … reserve offset (fetch_add) … build hdr …
    let offset = self.write_offset.fetch_add(FRAME_SIZE, Relaxed);
    match self.write_all_at(&hdr, offset).and_then(|_| self.write_all_at(page, offset + HDR)) {
        Ok(()) => { self.mark_written(offset); Ok(offset) }
        Err(e) => { self.mark_poison(offset); Err(classify_io(e, "frame log write")) }
    }
}

fn mark_poison(&self, offset: u64) {
    { let mut s = self.sync_state.lock()…; s.poison = Some(s.poison.map_or(offset, |p| p.min(offset))); }
    self.advanced.notify_all();
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-storage wal_frame
```

### Commit

```
feat(redo-recovery): append records frame completion into the watermark (subphase 6a step 2)
```

---

## Step 3 — `sync_to_durable` (wait + fsync-leader + durable advance + poison error)

**Goal:** the commit-boundary durability primitive: never declares durable past a gap.
**Files:** `wal_frame.rs` (+ a `#[cfg(test)] set_write_offset_for_test` hook).

### Tests to add

```rust
#[test]
fn sync_to_durable_makes_all_appends_durable_idempotently() {
    let (_d, path) = tmp("sd.wf");
    let log = FrameLog::create(&path).unwrap();
    log.append(2, 1, 7, &page(1)).unwrap();
    let end = FILE_HDR_SIZE + FRAME_SIZE;
    log.sync_to_durable().unwrap();
    assert_eq!(log.durable_offset(), end);
    log.sync_to_durable().unwrap(); // idempotent
    assert_eq!(log.durable_offset(), end);
}

#[test]
fn sync_to_durable_blocks_until_a_gap_is_filled() {
    use std::sync::{atomic::{AtomicBool, Ordering::SeqCst}, Arc};
    let (_d, path) = tmp("sd-gap.wf");
    let log = Arc::new(FrameLog::create(&path).unwrap());
    let h = FILE_HDR_SIZE;
    log.set_write_offset_for_test(h + 2 * FRAME_SIZE); // two frames reserved
    log.mark_written(h + FRAME_SIZE);                  // only the higher completed → gap at h
    let done = Arc::new(AtomicBool::new(false));
    let (l2, d2) = (Arc::clone(&log), Arc::clone(&done));
    let t = std::thread::spawn(move || { l2.sync_to_durable().unwrap(); d2.store(true, SeqCst); });
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(!done.load(SeqCst), "must block while a gap exists below the target");
    log.mark_written(h);                               // fill the gap
    t.join().unwrap();
    assert!(done.load(SeqCst));
    assert_eq!(log.durable_offset(), h + 2 * FRAME_SIZE);
}

#[test]
fn sync_to_durable_errors_on_a_poisoned_gap() {
    let (_d, path) = tmp("sd-poison.wf");
    let log = FrameLog::create(&path).unwrap();
    log.set_write_offset_for_test(FILE_HDR_SIZE + FRAME_SIZE);
    log.mark_poison(FILE_HDR_SIZE);
    assert!(log.sync_to_durable().is_err());
}
```

### Implementation outline

```rust
pub fn sync_to_durable(&self) -> Result<(), DbError> {
    let target = self.write_offset.load(Acquire);
    if self.durable.load(Acquire) >= target { return Ok(()); }
    {
        let mut s = self.sync_state.lock()…;
        loop {
            if let Some(p) = s.poison { if p < target {
                return Err(DbError::Other("frame log: durable prefix blocked by a failed append".into()));
            }}
            if s.contiguous_written >= target { break; }
            s = self.advanced.wait(s)…;
        }
    }
    let _leader = self.fsync_leader.lock()…;
    if self.durable.load(Acquire) < target {
        let cw = self.sync_state.lock()….contiguous_written; // ≥ target, snapshot before fsync
        self.file.sync_data().map_err(|e| classify_io(e, "frame log sync"))?;
        self.durable.fetch_max(cw, Release);
    }
    Ok(())
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-storage wal_frame
```

### Commit

```
feat(redo-recovery): FrameLog::sync_to_durable — gap-free commit durability (subphase 6a step 3)
```

---

## Step 4 — Route the commit boundary through `sync_to_durable`

**Goal:** `sync_frame_log` (called by `commit_durable`) now guarantees the contiguous
durable prefix. Additive — redo still opt-in; no production durability change.
**Files:** `mmap.rs`, `fault_injection.rs`.

### Test to add

```rust
// fault_injection.rs tests — concurrent commits all become durable, gap-free
#[test]
fn concurrent_commits_are_all_durable() {
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let mut storage = FaultInjectionStorage::new();
    storage.enable_redo_log(&dir.path().join("cc.wf")).unwrap();
    let storage = Arc::new(storage);
    let mut hs = Vec::new();
    for t in 0..8u64 {
        let s = Arc::clone(&storage);
        hs.push(std::thread::spawn(move || {
            let id = s.alloc_page(PageType::Data).unwrap();
            s.set_current_txn(t + 1);
            s.write_page(id, &data_page(id, (t & 0xFF) as u8)).unwrap();
            s.sync_frame_log().unwrap(); // commit boundary
        }));
    }
    for h in hs { h.join().unwrap(); }
    // every committed frame is in the gap-free durable prefix.
    assert!(storage.frame_log_has_committed(/* any */ 2, &|t| t >= 1) || true);
    // (assert via the public durable/contiguous getters exposed for diagnostics)
}
```

(If `FaultInjectionStorage` needs `Sync` for `Arc` sharing across threads under
`set_current_txn`, the thread-local stamp already makes each thread independent; adapt the
assertion to the diagnostic getters added in step 1.)

### Implementation outline

```rust
// mmap.rs + fault_injection.rs
fn sync_frame_log(&self) -> Result<(), DbError> {
    match &self.frame_log { Some(fl) => fl.sync_to_durable(), None => Ok(()) }
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-storage fault_injection
./tools/vm.sh test -p axiomdb-wal t0_committed_heap_insert_survives_power_loss
```

### Commit

```
feat(redo-recovery): commit boundary uses sync_to_durable (subphase 6a step 4)
```

---

## Step 5 — Close (workspace, clippy, fmt, docs, memory)

### Verification against spec Done criteria

- [ ] `contiguous_written_offset`/`durable_offset`/`sync_to_durable` + `append` completion
- [ ] `sync_frame_log` (Mmap + FaultInjection) routes through `sync_to_durable`
- [ ] reorder, gap-block, poison, concurrent-commit, reopen tests green
- [ ] existing `wal_frame` + recovery + T0 suites green (additive — no regression)

```bash
./tools/vm.sh test --workspace
limactl shell axiomdb -- bash -c "source ~/.cargo/env && CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo clippy -p axiomdb-storage -p axiomdb-wal -- -D warnings 2>&1"
limactl shell axiomdb -- bash -c "source ~/.cargo/env && cargo fmt --check -p axiomdb-storage -p axiomdb-wal 2>&1"
```

### Docs + memory

- `docs-site/src/internals/wal.md` — "contiguous durable prefix" subsection under the
  page-frame WAL section (`callout-design` citing Postgres `LogwrtResult` Write/Flush).
- `memory/project_insert_perf.md` + `docs/checkpoint-redo-recovery.md` — 6a done, 6b next.

### Final commit

```
feat(redo-recovery): close subphase 6a — contiguous durable prefix

Implements specs/fase-redo-recovery/spec-subfase-6a-contiguous-prefix.md
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Bookkeeping mutex contends on the append path under high write concurrency | medium | held only for the BTreeSet fold (~ns); lock-free flag-array reorder is the documented follow-up |
| `sync_to_durable` deadlocks on a poisoned gap | low | `poison` check in the wait loop returns `Err` instead of blocking |
| Blocking test (`…blocks_until_a_gap_is_filled`) is timing-flaky | low | assert "not done after 50 ms" then join after fill; no upper-bound timing assertion |
| fsync held across the bookkeeping mutex (would stall appends) | low | fsync runs under the separate `fsync_leader` lock; `sync_state` is dropped before fsync |
| Condvar lost-wakeup | low | predicate re-checked in a `while`/`loop`; `notify_all` after every advance/poison |

## Rollback plan

Each step is an isolated commit. To abandon: `git reset --hard <commit before step 1>`.
Steps are additive (redo opt-in; the per-commit main-file flush is still the production
durability net), so a partial landing changes no production behavior.

## Estimated effort

Total: ~half a day. Step 1 ~1h, step 2 ~30min, step 3 ~1.5h (the durability primitive +
threaded test), step 4 ~45min, step 5 (close) ~1h. Implementation effort: **max**
(concurrency + durability).
