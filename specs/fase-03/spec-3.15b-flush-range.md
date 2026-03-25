# Spec: 3.15b — Targeted `flush_range` For Dirty mmap Pages

These files were reviewed before writing this spec:
- `db.md`
- `docs/progreso.md`
- `specs/fase-03/spec-3.15-page-dirty-tracker.md`
- `crates/axiomdb-storage/src/dirty.rs`
- `crates/axiomdb-storage/src/mmap.rs`
- `crates/axiomdb-storage/src/page.rs`

## What to build (not how)

`MmapStorage::flush()` must stop flushing the entire mmap on every checkpoint or
commit-related storage flush. Instead, it must flush only the pages that are
known dirty, using contiguous page runs and targeted `flush_range` calls.

Semantics must remain identical to the current `flush()`:
- when `flush()` returns `Ok(())`, all dirty pages are durable
- dirty tracking is cleared only after a successful flush
- if flush fails, dirty tracking remains intact

The freelist page (page 1) must be included whenever `freelist_dirty` is set.

## Inputs / Outputs

- Input:
  - dirty page IDs tracked by `PageDirtyTracker`
  - `freelist_dirty` boolean
- Output:
  - successful durable flush of only the required mmap ranges
- Errors:
  - any `flush_range` failure propagates as `DbError::Io`
  - dirty state remains uncleared on failure

## Use cases

1. No dirty pages.
   `flush()` returns success without flushing the whole file.

2. One dirty page.
   `flush()` flushes exactly one page-sized range.

3. Three contiguous dirty pages.
   `flush()` emits one coalesced range, not three independent flushes.

4. Two separated dirty runs.
   `flush()` emits two ranges.

5. Only freelist changed.
   `flush()` serializes page 1 and flushes only page 1.

6. Failure on the second range.
   `flush()` returns error and keeps the dirty tracker unchanged.

## Acceptance criteria

- [ ] `flush()` no longer uses full-file `mmap.flush()` on the normal success path
- [ ] Dirty page IDs are coalesced into contiguous runs before flushing
- [ ] A single dirty run becomes one `flush_range` call
- [ ] Multiple separated runs become multiple `flush_range` calls
- [ ] `freelist_dirty` causes page 1 to be flushed even if it is not already in the tracker
- [ ] Dirty tracking is cleared only after all targeted flushes succeed
- [ ] If any targeted flush fails, dirty tracking is preserved
- [ ] Existing `dirty_page_count()` semantics stay correct

## Out of scope

- Background async writeback
- Linux-specific `sync_file_range` writeback scheduling
- Dynamic heuristics based on database size or dirty coverage
- Checkpoint policy / frequency

## Dependencies

- `crates/axiomdb-storage/src/dirty.rs` — dirty page tracking
- `crates/axiomdb-storage/src/mmap.rs` — mmap-backed storage flush path
- `crates/axiomdb-storage/src/page.rs` — page size and fixed offsets

## ⚠️ DEFERRED

- Linux-specific background writeback (`sync_file_range`) once profiling proves it is worthwhile
- Adaptive “many scattered pages -> full flush” heuristics if benchmarks justify them
