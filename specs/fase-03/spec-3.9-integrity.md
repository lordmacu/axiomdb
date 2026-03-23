# Spec: 3.9 — Post-recovery Integrity Check

## What to build (not how)

A structural integrity checker for heap pages that runs after crash recovery
to verify the database is consistent before accepting connections.

Without the catalog (Phase 3.11+), index-vs-heap cross-checks are impossible
(we don't know which B+ Trees exist). This subfase provides heap-level
structural verification and post-recovery MVCC consistency checks.

## Scope

**In scope:**
1. Structural check per heap page: slot array vs tuple area consistency.
2. MVCC post-recovery check: no alive row has `txn_id_created > max_committed`
   (would mean crash recovery left uncommitted data visible).
3. Stale-delete detection: alive row with `txn_id_deleted <= max_committed`
   (visible row whose deleter committed — should be invisible, might indicate
   a recovery gap; logged as warning, not error).
4. `IntegrityReport` with violations, counts, and summary.

**Deferred (needs catalog — Phase 3.12+):**
- Index → Heap: for each B+ Tree RecordId, verify the heap slot is alive.
- Heap → Index: every alive row has a corresponding B+ Tree entry.

## Integrity violations

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Severity { Error, Warning }

#[derive(Debug, Clone)]
pub enum IntegrityViolation {
    /// Alive slot has txn_id_created > max_committed.
    /// Indicates crash recovery failed to undo an uncommitted INSERT.
    UncommittedAliveRow {
        page_id: u64, slot_id: u16,
        txn_id_created: u64, max_committed: u64,
    },

    /// Alive slot has txn_id_deleted > 0 AND txn_id_deleted <= max_committed.
    /// The deleter committed but the row is still alive — recovery gap or VACUUM needed.
    StaleDeletedRow {
        page_id: u64, slot_id: u16,
        txn_id_deleted: u64, max_committed: u64,
    },

    /// Two alive slots have overlapping byte ranges in the page body.
    /// Indicates structural corruption of the slot array or tuple packing.
    SlotAreaOverlap {
        page_id: u64, slot_a: u16, slot_b: u16,
    },

    /// Page free_start >= free_end (slot array extends into tuple area).
    FreeSpaceInconsistent {
        page_id: u64, free_start: u16, free_end: u16,
    },

    /// A slot's offset points into the page header region (< HEADER_SIZE),
    /// which is structurally impossible for a valid tuple.
    InvalidSlotOffset {
        page_id: u64, slot_id: u16, offset: u16,
    },
}

impl IntegrityViolation {
    pub fn severity(&self) -> Severity {
        match self {
            Self::StaleDeletedRow { .. } => Severity::Warning,
            _ => Severity::Error,
        }
    }
}
```

## Inputs / Outputs

### `IntegrityChecker::check_page(page, max_committed) -> Vec<IntegrityViolation>`
- Input: a single heap page reference + max_committed TxnId
- Output: all violations found on that page (empty = clean)
- Pure function, no I/O, no state

### `IntegrityChecker::check_all_data_pages(storage, max_committed) -> IntegrityReport`
- Iterates all pages from 2 to `storage.page_count()`.
- Skips non-Data pages (PageType check via `body()[0]` heuristic — see note).
- Calls `check_page` on each Data page.
- Returns `IntegrityReport`.

Note: without a catalog we can't reliably distinguish Data pages from Index pages.
We skip pages where `body()[0] == 0 || body()[0] == 1` (those are likely B+ Tree
nodes with is_leaf byte). We only check pages where the byte pattern suggests
a heap page (free_start and free_end in header fields look like valid heap pointers).
In Phase 3.12, this will be replaced by catalog-driven page type lookup.

### `IntegrityReport`
```rust
pub struct IntegrityReport {
    pub pages_checked: u64,
    pub slots_checked: u64,
    pub errors: Vec<IntegrityViolation>,    // severity == Error
    pub warnings: Vec<IntegrityViolation>,  // severity == Warning
}
impl IntegrityReport {
    pub fn is_clean(&self) -> bool { self.errors.is_empty() }
    pub fn summary(&self) -> String { /* "OK: N pages, M slots" or "ERRORS: ..." */ }
}
```

### `IntegrityChecker::post_recovery_check(storage, recovery_result) -> IntegrityReport`
- Convenience wrapper: checks all data pages using `recovery_result.max_committed`.
- Called by `CrashRecovery::recover` at the end (optional, controlled by flag).

## Use cases

1. **Clean database**: all slots have committed txn_ids → no violations. ✓
2. **Recovery missed an uncommitted INSERT**: slot alive with txn_id_created > max_committed
   → `UncommittedAliveRow` error. ✓
3. **Stale deleted row** (VACUUM pending): slot alive, txn_id_deleted <= max_committed
   → `StaleDeletedRow` warning. ✓ (normal pre-VACUUM state)
4. **Structural corruption** (bit flip, partial write): free_start >= free_end
   → `FreeSpaceInconsistent` error. ✓
5. **Empty database** (no data pages): returns `IntegrityReport { pages_checked: 0, ... }`. ✓
6. **Mixed violations**: multiple violations on different pages → all reported. ✓

## Acceptance criteria

- [ ] `check_page` returns `UncommittedAliveRow` for a slot with txn_id_created > max_committed
- [ ] `check_page` returns `StaleDeletedRow` (warning) for a slot with txn_id_deleted <= max_committed
- [ ] `check_page` returns `FreeSpaceInconsistent` when free_start >= free_end
- [ ] `check_page` returns `InvalidSlotOffset` for a slot with offset < HEADER_SIZE
- [ ] `check_page` returns empty for a clean page (all committed, no overlap)
- [ ] `is_clean()` returns true iff no errors (warnings don't block)
- [ ] `post_recovery_check` returns clean report for a recovered-clean database
- [ ] `post_recovery_check` detects an uncommitted row that survived recovery
- [ ] No `unwrap()` in src/

## Out of scope

- Index → Heap cross-check (needs catalog)
- Heap → Index reverse check (needs catalog)
- WAL checksum verification (WalReader already does this on scan)
- B+ Tree structural check (out of scope — no Tree reference available here)
- Auto-repair of violations (report only, no mutation)

## Dependencies

- `axiomdb-storage`: `StorageEngine`, `Page`, `PageType`, `HEADER_SIZE`, heap types
- `axiomdb-core`: `TxnId`, `DbError`
- 3.8 `RecoveryResult` (used in `post_recovery_check`) — imported from axiomdb-wal
  → to avoid circular dep, `post_recovery_check` takes `max_committed: TxnId` directly,
  not the full RecoveryResult
