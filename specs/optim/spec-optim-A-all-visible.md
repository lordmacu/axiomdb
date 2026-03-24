# Spec: All-Visible Page Flag (Optimization A)

## What to build (not how)

A single-bit flag (`PAGE_FLAG_ALL_VISIBLE = 0x01`) stored in bit 0 of
`PageHeader.flags`. When the flag is set on a heap page, it asserts that every
alive slot on that page was inserted by a committed transaction and has not been
deleted, meaning the slot is visible to any snapshot with `snapshot_id > 0`
(i.e., every possible reader). `HeapChain::scan_visible()` can use this flag to
skip per-slot `is_visible()` MVCC checks entirely for the page.

The flag is a performance hint, not authoritative data. Setting it incorrectly
conservative (cleared when it should be set) causes no data loss — the engine
re-verifies on the next scan. Setting it when it should NOT be set is prevented by
the clearing protocol in `mark_deleted()`.

Inspired by PostgreSQL's all-visible map (`src/backend/storage/heap/heapam.c:668`),
but implemented as an in-page bit rather than a separate VM file, keeping the common
path to a single cache-line read.

---

## Inputs / Outputs

### `Page::is_all_visible()`
- Input: none (reads `self.header().flags`)
- Output: `bool` — `true` iff bit 0 of `flags` is set

### `Page::set_all_visible()`
- Input: none (mutates `self.header_mut().flags`)
- Output: none
- Side effect: sets bit 0 in `flags`; caller must call `update_checksum()` afterward

### `Page::clear_all_visible()`
- Input: none (mutates `self.header_mut().flags`)
- Output: none
- Side effect: clears bit 0 in `flags`; caller must call `update_checksum()` afterward

### `scan_visible()` fast path
- Input: `page: &Page`, `snap: &TransactionSnapshot`
- Output: `Vec<RowRef>` — same result as today, produced without calling `is_visible()` per slot

### `scan_visible()` lazy-set path
- Input: processed page after full slot scan
- Output: writes back the page with the flag set if all alive slots were visible
- Side effect: one page write per page per table lifetime (one-time cost, never
  repeated unless a row is deleted from that page)

### `mark_deleted()` clear path
- Input: slot being stamped with `txn_id_deleted`
- Output: none
- Side effect: clears `PAGE_FLAG_ALL_VISIBLE` on the containing page before
  writing the slot, then calls `update_checksum()`

### Errors
- None of the new methods return `Result`. Flag access is infallible (bit
  manipulation on an owned `&mut Page`).
- If the page write in the lazy-set path fails (I/O error), the error propagates
  from the existing page-write machinery and the flag remains unset on disk. The
  next scan will simply re-verify and attempt to set it again. No data is lost.

---

## Behavioral specification

### Invariant: when the flag may be set

The flag MUST NOT be set unless, for every slot `s` on the page where
`s.is_alive()` is true:

1. `s.txn_id_created < snap.snapshot_id` for every future snapshot — equivalently,
   `s.txn_id_created <= max_committed` where `max_committed` is the highest
   committed transaction ID at the moment of setting.
2. `s.txn_id_deleted == 0`

In practice this is checked by running the full per-slot `is_visible()` path on
the first scan after a page is written, and only setting the flag when every alive
slot passes.

### Setting the flag — lazy, during scan

After `scan_visible()` finishes iterating all slots on a page:

```
all_visible = true
for slot in page.slots():
    if slot.is_alive():
        if !is_visible(slot, snap):
            all_visible = false; break
        if slot.txn_id_deleted != 0:
            all_visible = false; break

if all_visible && page.item_count > 0 && !page.is_all_visible():
    page.set_all_visible()
    page.update_checksum()
    storage.write_page(page)   // one-time write per page per lifetime
```

The write is a one-time cost per page. Once set, the flag remains set for the
lifetime of that page (until a row is deleted from it), meaning all future scans
skip MVCC entirely for that page.

An empty page (`item_count == 0`) MUST NOT receive the flag. There is nothing to
skip, and setting the flag on an empty page would mislead future validity checks.

### Using the flag — fast path in scan_visible

```
if page.is_all_visible():
    return page.alive_slots()   // skip is_visible(), return all non-dead slots
else:
    // existing per-slot is_visible() path
    // optionally set flag at end (lazy-set path above)
```

### Clearing the flag — in mark_deleted

```
fn mark_deleted(page: &mut Page, slot_index: usize, txn_id: u64):
    page.clear_all_visible()          // clear BEFORE stamping the slot
    slot = page.slot_mut(slot_index)
    slot.txn_id_deleted = txn_id
    page.update_checksum()            // single checksum update covers both changes
```

Clearing happens before the slot is stamped so that any concurrent reader that
sees the page after the write also sees the cleared flag. This ordering is the
critical safety guarantee: there is no window where the flag is set while a slot
has a non-zero `txn_id_deleted`.

### Crash safety

The flag is written as part of the normal page write (same page, same checksum).
On crash:

- **Flag set, page written, no crash**: correct.
- **Crash during the lazy-set write**: the page on disk retains the pre-write
  state (flag unset). On recovery, the next scan re-runs the full `is_visible()`
  path and re-sets the flag. No data loss.
- **Crash after `mark_deleted()` write**: the slot has `txn_id_deleted` stamped
  and the flag is cleared (both in the same page write). On recovery, the page is
  consistent: flag cleared, slot deleted. Correct.
- **Crash between `clear_all_visible()` and the slot write**: impossible — both
  mutations happen in memory on the same `&mut Page` before a single `write_page`
  call. They are atomic from the OS write perspective (a single 16 KB sector
  write, or WAL-journaled in future phases).

No WAL entries are required for the flag. The flag is derived from slot state,
and slot state is what the WAL already records. On WAL replay, the flag will be
absent (conservative); the next scan will re-set it lazily.

---

## Constant definition

```rust
/// Bit 0 of `PageHeader.flags`.
/// When set: every alive slot on this page is visible to any snapshot with
/// snapshot_id > 0. `scan_visible()` may skip per-slot MVCC checks.
pub const PAGE_FLAG_ALL_VISIBLE: u8 = 0x01;
```

Placed in `crates/axiomdb-storage/src/page.rs`, alongside the existing constants
`PAGE_SIZE`, `HEADER_SIZE`, and `PAGE_MAGIC`.

---

## Public API additions

All three methods added to the `impl Page` block in `page.rs`:

```rust
/// Returns true if the all-visible flag (bit 0 of flags) is set.
pub fn is_all_visible(&self) -> bool {
    self.header().flags & PAGE_FLAG_ALL_VISIBLE != 0
}

/// Sets the all-visible flag. Caller must call `update_checksum()` afterward.
pub fn set_all_visible(&mut self) {
    self.header_mut().flags |= PAGE_FLAG_ALL_VISIBLE;
}

/// Clears the all-visible flag. Caller must call `update_checksum()` afterward.
pub fn clear_all_visible(&mut self) {
    self.header_mut().flags &= !PAGE_FLAG_ALL_VISIBLE;
}
```

---

## Use cases

### 1. Happy path — INSERT batch, then full-table SELECT

```sql
INSERT INTO orders VALUES (1, ...), (2, ...), ..., (10000, ...);
COMMIT;
SELECT * FROM orders;   -- first scan: verifies all slots, sets flag on each page
SELECT * FROM orders;   -- second scan: flag set → skips 10K is_visible() calls
```

After the first SELECT, every page that passed the visibility check has the flag
set. The second SELECT reads the flag, returns all alive slots immediately without
calling `is_visible()` on any of them.

Expected benefit: elimination of 10K `is_visible()` invocations per full table
scan on a committed table with no deletes.

### 2. Flag cleared on DELETE

```sql
INSERT INTO orders VALUES (1, ...), ..., (10000, ...);
COMMIT;
SELECT * FROM orders;   -- sets flag on all pages
DELETE FROM orders WHERE id = 5000;
COMMIT;
SELECT * FROM orders;   -- flag cleared on the page containing id=5000; MVCC re-checked for that page only
```

Only the page containing the deleted row has its flag cleared. All other pages
retain the flag. The scan pays full MVCC cost only for the affected page.

### 3. Crash during lazy-set write

```
scan_visible() processes page 7, all slots visible → begins write_page(7)
CRASH
```

On restart, page 7 has the flag unset (pre-crash state). The next `scan_visible()`
re-runs the full `is_visible()` path on page 7, confirms all slots visible,
re-sets the flag. Data is intact; the flag is simply re-derived.

### 4. Empty page — flag never set

```sql
-- Table exists but has zero rows, or all rows on a page were deleted
SELECT * FROM empty_table;
```

`item_count == 0` → the lazy-set check fails the `item_count > 0` guard →
flag is never set → no behavior change from today.

### 5. Mixed committed/uncommitted rows on the same page

```sql
-- Txn A inserts rows 1-5 (committed), Txn B inserts rows 6-10 (in progress)
SELECT * FROM t;   -- snapshot sees rows 1-5 only
```

Row 6's `txn_id_created` does not satisfy `< snap.snapshot_id`. The lazy-set
check discovers `!is_visible(slot_6, snap)` → `all_visible = false` → flag NOT
set. The full per-slot path is used. Correct.

---

## Acceptance criteria

- [ ] `PAGE_FLAG_ALL_VISIBLE = 0x01` defined as a `pub const` in
  `crates/axiomdb-storage/src/page.rs`
- [ ] `Page::is_all_visible()`, `Page::set_all_visible()`, `Page::clear_all_visible()`
  implemented in the same file
- [ ] `set_all_visible()` and `clear_all_visible()` do NOT call `update_checksum()` —
  that responsibility stays with the caller (consistent with existing API convention
  shown in `Page::new()`)
- [ ] `mark_deleted()` in `heap.rs` calls `page.clear_all_visible()` before stamping
  `txn_id_deleted`, then calls `update_checksum()` once to cover both changes
- [ ] `scan_visible()` in `HeapChain`: if `page.is_all_visible()`, returns all
  alive slots without calling `is_visible()` on any slot
- [ ] `scan_visible()` sets the flag lazily: after processing all slots on a page,
  if every alive slot was visible AND `txn_id_deleted == 0` for all of them AND
  `item_count > 0` AND the flag was not already set, calls `set_all_visible()` +
  `update_checksum()` + `write_page()`
- [ ] Empty pages (`item_count == 0`) never receive the flag
- [ ] The lazy-set write path propagates I/O errors to the caller rather than
  swallowing them
- [ ] Benchmark: `SELECT * FROM t` on a 10K-row committed table shows measurable
  reduction in time attributable to skipping MVCC checks (compare before/after
  with `cargo bench`)
- [ ] All existing tests (1152+) pass unchanged — no regression
- [ ] New unit tests in `crates/axiomdb-storage/src/page.rs` (#[cfg(test)]):
  - `test_all_visible_flag_set_and_clear`: set → is_all_visible() == true;
    clear → is_all_visible() == false
  - `test_all_visible_does_not_affect_other_flags`: set PAGE_FLAG_ALL_VISIBLE,
    verify that no other bit in `flags` changes; clear it, same check
  - `test_checksum_covers_flag_change`: set_all_visible() → update_checksum() →
    verify_checksum() passes; then flip the flag byte directly (without
    update_checksum) → verify_checksum() fails
- [ ] New integration tests in `crates/axiomdb-storage/tests/` (or `heap` module):
  - `test_scan_visible_sets_flag_after_committed_insert`: insert N rows, commit,
    run scan_visible, assert flag is set on all full pages
  - `test_mark_deleted_clears_flag`: insert rows, commit, scan (sets flag),
    delete one row, assert flag is cleared on that row's page
  - `test_flag_cleared_page_rescanned_correctly`: insert rows, commit, scan
    (flag set), delete row, scan again (flag cleared on that page) → result
    excludes the deleted row, flag re-set if remaining slots still visible
  - `test_crash_safety_flag_unset_on_new_page`: simulate crash by constructing
    a page with flag unset but all rows committed → scan_visible must return
    all rows correctly (conservative path) and set the flag afterward
  - `test_empty_page_never_gets_flag`: scan an empty page, assert flag remains
    unset

---

## Out of scope

- A separate all-visible map file (PostgreSQL-style `_vm` fork) — the in-page
  bit is sufficient for Phase 1 of this optimization; a separate map adds
  complexity without measurable benefit at current scale.
- Persisting flag state across WAL replay — the flag is re-derived lazily on
  the first scan after recovery. No WAL format changes.
- All-visible tracking for index pages — this optimization targets heap
  (table data) pages only; index B-Tree pages use a different visibility model.
- Vacuum / dead-slot reclamation triggered by the flag — out of scope until
  Phase 7 (MVCC full) introduces vacuum.
- Clearing the flag on UPDATE — updates in AxiomDB are currently implemented
  as DELETE + INSERT at the executor level; `mark_deleted()` already clears the
  flag, which covers the delete half.
- Concurrent writer safety — Phase 6 serializes all writes behind a single
  `Mutex<Database>`; no additional synchronization is needed for the flag.

---

## Dependencies

- `PageHeader.flags` field exists and is currently always `0` — no migration needed.
- `Page::update_checksum()` and `Page::header_mut()` exist and are already used by
  callers — no API changes to those methods.
- `HeapChain::scan_visible()` and `mark_deleted()` must exist in `heap.rs` as
  described; this spec assumes the heap layer from Phase 4.5 / 5.x is in place.
- The storage layer's `write_page()` must be accessible from within `scan_visible()`
  for the lazy-set write. If `scan_visible()` currently takes only a shared
  reference to storage, the signature must be updated to accept `&mut dyn
  StorageEngine` (or equivalent). This change must not break existing callers.
- No new crate dependencies.
