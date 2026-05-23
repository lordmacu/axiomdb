# Plan: single-fsync commit — commit marker in the frame log

Phase: redo-recovery (project B)
Task: 1 fsync per durable commit under frame-only redo
Spec: specs/fase-redo-recovery/spec-single-fsync-commit.md
Status: in-progress (Step 1)

## Summary

Make the frame log carry the commit decision so frame-only recovery is self-sufficient
and a Strict commit needs one fsync. Order: first make the frame log able to hold a
**compact commit marker** alongside full page frames (variable stride) and survive a torn
tail (T1, the foundation + the subtle gap-free-prefix change); then teach recovery to
derive the committed set from markers under frame-only while leaving redo=off on the
logical-WAL predicate (T2, gated by the crash suite); then drop the second fsync on the
commit path (T3); then measure the FULL-autocommit win and confirm no NORMAL/read/batch
regression + docs (T4). TDD throughout; the crash suite `integration_redo_crash_suite.rs`
T1–T7 is the gate from T2 on.

## Dependencies

Must be done first:
- [x] spec-single-fsync-commit approved
- [x] frame-only redo (6c), gap-free prefix (6a), crash suite, IntegrityChecker exist

Blocks: nothing.

## Affected files

Modified:
- `crates/axiomdb-storage/src/wal_frame.rs` — marker format + VERSION bump, `append_commit_marker`,
  variable-stride `scan`/append reservation/`sync_to_durable` bookkeeping, marker-derived
  committed set, recycle awareness.
- `crates/axiomdb-storage/src/mmap.rs` — `StorageEngine::append_commit_marker` impl + the
  marker-derived committed-set surface; `sync_frame_log` unchanged (one fsync).
- `crates/axiomdb-storage/src/engine.rs` (or wherever `StorageEngine` is defined) — trait
  methods `append_commit_marker` (default no-op) + `committed_txns_from_frames`.
- `crates/axiomdb-wal/src/txn_begin_commit.rs` — `commit_durable` frame-only path: append
  marker → one `sync_frame_log` → `commit` with logical WAL `flush_no_sync` (skip
  `commit_data_sync`).
- `crates/axiomdb-wal/src/recovery.rs` — frame-only sources the committed set from markers;
  redo=off keeps the logical-WAL scan.

New tests:
- `crates/axiomdb-storage/tests/integration_frame_marker.rs` — marker round-trip, torn tail,
  variable-stride durable prefix.
- assertions added to `crates/axiomdb-wal/tests/integration_redo_crash_suite.rs` (T1–T7 under
  the marker predicate) + a one-fsync-per-commit test.

---

## Step 1 — Compact commit-marker record + variable-stride scan

**Goal:** the frame log can append + read back a 36-byte commit marker interleaved with
full page frames; VERSION bumped; a torn marker ends the valid prefix.
**Files:** `wal_frame.rs`, new `tests/integration_frame_marker.rs`.
**Approach:** TDD.

### Test to add
```rust
// integration_frame_marker.rs
#[test]
fn commit_marker_round_trips_between_page_frames() {
    let log = FrameLog::create(&path).unwrap();
    log.append(7, 1, 100, &page_bytes()).unwrap();        // page frame
    log.append_commit_marker(/*txn_id*/100, /*lsn*/2).unwrap(); // compact marker
    log.append(8, 3, 101, &page_bytes()).unwrap();
    let frames = log.scan().unwrap();
    // scan yields the two page frames + one marker, in order, by their actual lengths
    assert_eq!(frames.page_frames().count(), 2);
    assert_eq!(frames.markers(), &[(100u64, 2u64)]);
}

#[test]
fn torn_commit_marker_ends_valid_prefix() {
    // write a page frame + a marker, then corrupt the marker's crc → scan stops before it
}
```

### Implementation outline
```rust
// wal_frame.rs
const VERSION: u32 = 2;                       // was 1; gates the variable-stride reader
const COMMIT_MARKER: u64 = u64::MAX;          // sentinel page_id ⇒ compact marker record

// A scanned record is one of:
enum FrameRecord { Page(FrameRef), Commit { txn_id: u64, lsn: u64, offset: u64 } }

pub fn append_commit_marker(&self, txn_id: u64, lsn: u64) -> Result<u64, DbError> {
    // header: page_id=COMMIT_MARKER, lsn, txn_id, salt, crc over [0..32]
    // reserve a COMPACT slot: write_offset.fetch_add(FRAME_HDR_SIZE as u64)
    // single pwrite of the 36-byte header; mark_written(offset, FRAME_HDR_SIZE)
}

// scan(): read 36-byte header at the cursor; if page_id==COMMIT_MARKER → marker,
// advance FRAME_HDR_SIZE; else read the page, advance FRAME_SIZE. CRC-validate each;
// stop at the first salt-mismatch or crc-fail (torn tail).
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-storage --test integration_frame_marker
./tools/vm.sh test -p axiomdb-storage   # whole storage suite stays green
```
### Commit
```
feat(fase-redo-recovery): compact commit-marker record + variable-stride frame scan

Step 1 of specs/fase-redo-recovery/plan-single-fsync-commit.md
```

---

## Step 2 — Variable-stride gap-free durable prefix

**Goal:** `sync_to_durable`'s gap-free prefix advances correctly when records have
different sizes (the subtle correctness point: the fold needs each record's length).
**Files:** `wal_frame.rs` (`SyncState`, `mark_written`, the fold in `sync_to_durable`).
**Approach:** TDD.

### Test to add
```rust
#[test]
fn durable_prefix_handles_mixed_record_sizes() {
    // append page(16420) @A, marker(36) @B=A+16420, page(16420) @C=B+36
    // complete them OUT OF ORDER (C, A, B); after each, the contiguous durable prefix
    // must only cover a gap-free run, computed from the stored record LENGTHS:
    //   after C: prefix=A (gap at A)
    //   after A: prefix=B (A done, len known → next start B)
    //   after B: prefix=C+16420 (all gap-free)
}
```

### Implementation outline
```rust
// SyncState.completed: BTreeSet<u64> → BTreeMap<u64, u64>  (start → end = start+record_len)
fn mark_written(&self, offset: u64, record_len: u64) { /* insert (offset, offset+record_len) */ }
// fold: while completed has an entry starting at contiguous_written, advance
// contiguous_written to its end and remove it. (Length comes from the map, not FRAME_SIZE.)
```
Update both `append` and `append_commit_marker` to call `mark_written(offset, len)`; update
`mark_poison` similarly. The lock-free reservation already works for variable sizes
(`fetch_add(actual_len)` yields disjoint regions).

### Verification
```bash
./tools/vm.sh test -p axiomdb-storage --test integration_frame_marker
./tools/vm.sh test -p axiomdb-storage
```
### Commit
```
feat(fase-redo-recovery): variable-stride gap-free durable prefix (length-aware fold)

Step 2 of specs/fase-redo-recovery/plan-single-fsync-commit.md
```

---

## Step 3 — Marker-derived committed set (build_index reads markers)

**Goal:** the frame log can report the committed txn_id set from its own markers; redo=off
unaffected.
**Files:** `wal_frame.rs` (`build_index`, new `committed_txns_from_markers`), `mmap.rs`.

### Test to add
```rust
#[test]
fn build_index_uses_marker_committed_set() {
    // append frames for txn 100 (committed via marker) and txn 200 (no marker);
    // committed_txns_from_markers() == {100}; build_index(|t| set.contains(&t))
    // includes 100's latest frame, excludes 200's.
}
```

### Implementation outline
```rust
pub fn committed_txns_from_markers(&self) -> Result<BTreeSet<u64>, DbError> {
    // scan(): collect Commit{txn_id} records within the valid prefix
}
// reuse last_commit_lsn: set it to the max marker lsn so recycle/checkpoint can bound
// marker lifetime (markers older than the applied checkpoint LSN are recyclable).
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-storage --test integration_frame_marker
```
### Commit
```
feat(fase-redo-recovery): marker-derived committed set for build_index

Step 3 of specs/fase-redo-recovery/plan-single-fsync-commit.md
```

---

## Step 4 — Frame-only recovery sources committed-ness from markers

**Goal:** under frame-only, recovery builds the committed predicate from frame-log markers,
not the logical-WAL scan; redo=off keeps the logical-WAL scan. Crash suite T1–T7 green.
**Files:** `crates/axiomdb-wal/src/recovery.rs`, `mmap.rs`/`engine.rs` (surface
`committed_txns_from_frames`), `wal_frame.rs` (old-format fallback).

### Test to add
- Run the existing `integration_redo_crash_suite.rs` T1–T7 with markers as the source of
  truth (the suite already crashes + recovers; assert recovered state == oracle).
- Add: a v1 (no-marker) frame log recovers via the logical-WAL fallback (compat).

### Implementation outline
```rust
// recovery.rs frame-only branch: committed = storage.committed_txns_from_frames()?
//   (falls back to the logical-WAL scan when the log is v1 / has no markers)
// redo=off branch: unchanged (logical-WAL begun/ended scan, recovery.rs:155).
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-wal --test integration_redo_crash_suite
./tools/vm.sh test -p axiomdb-wal     # incl. clustered_recovery (redo=off path)
```
### Commit
```
feat(fase-redo-recovery): frame-only recovery committed-ness from frame markers

Step 4 of specs/fase-redo-recovery/plan-single-fsync-commit.md
```

---

## Step 5 — commit_durable: one fsync under frame-only

**Goal:** Strict commit under frame-only appends the marker, fsyncs the frame log ONCE,
and does NOT call `commit_data_sync`; the logical WAL is `flush_no_sync`.
**Files:** `crates/axiomdb-wal/src/txn_begin_commit.rs`, `engine.rs`/`mmap.rs`
(`StorageEngine::append_commit_marker`).

### Test to add
```rust
#[test]
fn frame_only_strict_commit_does_one_fsync() {
    // open frame-only, Strict; do one autocommit insert; read the fsync counter
    // (io_stats or a test FrameLog fsync counter) → exactly 1 fsync on the commit path.
}
```

### Implementation outline
```rust
// StorageEngine: fn append_commit_marker(&self, txn_id: u64) -> Result<(), DbError> { default no-op }
// MmapStorage::append_commit_marker: lsn = frame_lsn.fetch_add(1); frame_log.append_commit_marker(txn_id, lsn)

// commit_durable (frame-only, Strict):
//   storage.append_commit_marker(conn_txn.txn_id)?;   // after the txn's data frames
//   storage.sync_frame_log()?;                         // ONE fsync: data + marker
//   self.commit(conn_txn)?;                            // appends logical Commit, flush_no_sync
// commit(): under frame-only Strict, replace commit_data_sync() with flush_no_sync().
//   (redo=off Strict KEEPS commit_data_sync — gate on storage.frame_log_active().)
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-wal --test integration_redo_crash_suite   # T1–T7 still green
./tools/vm.sh test -p axiomdb-wal
# IntegrityChecker sweep after autocommit/batch/random/crash workloads
```
### Commit
```
feat(fase-redo-recovery): one fsync per Strict commit under frame-only (drop 2nd fsync)

Step 5 of specs/fase-redo-recovery/plan-single-fsync-commit.md
```

---

## Step 6 — Bench + docs (verify budget, no regression)

**Goal:** confirm FULL autocommit ≥ +80% and NORMAL/reads/batch within ±2%; document.
**Files:** bench harness (interleaved FULL OFF/ON A/B, macOS native), `docs-site/.../internals/wal.md`.

### Verification (macOS native — fsync-sensitive; Lima masks it)
```bash
cargo build -p axiomdb-bench-comparison --release          # macOS native (bench exception)
# interleaved FULL autocommit OFF(2-fsync)/ON(1-fsync) A/B, medians:
./target/release/axiomdb_bench --scenario insert_autocommit --rows 50000 --diagnose-1fsync-ab
# regression guard (Lima or macOS): --compare NORMAL writes + reads within ±2%
./tools/vm.sh test --workspace                              # close: full suite green
./tools/vm.sh clippy && ./tools/vm.sh fmt-check
```

### Verification against spec done-criteria
- [ ] frame-only recovery: no logical-WAL read (redo=off still uses it)
- [ ] exactly one fsync/commit under frame-only (Step 5 test)
- [ ] crash suite T1–T7 green via markers
- [ ] IntegrityChecker clean after autocommit/batch/random/crash
- [ ] redo=off suites green
- [ ] FULL autocommit ≥ +80%; NORMAL/batch/reads within ±2%
- [ ] workspace nextest + clippy + fmt clean
- [ ] internals/wal.md updated

### Final commit
```
feat(fase-redo-recovery): single-fsync commit (commit marker in frame log)

Implements specs/fase-redo-recovery/spec-single-fsync-commit.md
Plan: specs/fase-redo-recovery/plan-single-fsync-commit.md
FULL autocommit ~2× (1 fsync vs 2); NORMAL/reads/batch unchanged; T1–T7 green.
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Variable-stride gap-free prefix bug → stranded/over-advanced durable prefix | medium | Step 2 dedicated mixed-size out-of-order test before any commit-path change |
| Marker predicate wrong → apply uncommitted / drop committed frame | medium | crash suite T1–T7 + IntegrityChecker the gate from Step 4; start with the suite's existing oracle |
| redo=off regression | low | every commit/recovery branch gated on `frame_log_active()`; redo=off suites in Step 4/5 |
| NORMAL/throughput regression (hard rule) | low | compact 36B marker (no 16KB volume); marker append is one tiny pwrite; A/B in Step 6 |
| Old v1 frame log can't recover | low | VERSION gate + logical-WAL fallback (Step 4 compat test) |

## Rollback plan
1. Each step is an isolated commit; `git reset --hard <commit before the bad step>`.
2. Or branch `abandoned/single-fsync-commit-<date>`; set spec status back to `approved`
   with a note on what failed.
3. The redo=off path is never touched, so a rollback can't affect the non-frame-only mode.

## Estimated effort
Total: ~3–5 días. Steps 1–2 (frame log variable-stride) ~1.5d; Steps 3–4 (recovery) ~1.5d
(crash-suite-gated); Step 5 (commit path) ~0.5d; Step 6 (bench+docs) ~0.5d.
