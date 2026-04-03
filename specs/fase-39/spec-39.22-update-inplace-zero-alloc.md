# Spec: 39.22 — UPDATE in-place zero-alloc patch

## What to build (not how)

Eliminate the two full-row heap allocations that occur per matched row inside
`fused_clustered_scan_patch()`, replacing them with direct byte writes to the
page mutable buffer — mirroring InnoDB's `btr_cur_upd_rec_in_place()` strategy.

Also eliminate the two `Vec<u8>` per-field-delta heap allocations in `FieldDelta`
by using inline `[u8; 8]` arrays (fixed-size fields are at most 8 bytes).

The combined effect is zero heap allocations per matched row for the fixed-size
UPDATE fast path, except for the PK key bytes needed for WAL (typically 4 bytes
for INT primary keys).

### Root cause

`fused_clustered_scan_patch()` (update.rs:565–842) produces **five heap allocations
per matched row** in its current form:

| Allocation | Site | Size |
|---|---|---|
| `local_row_data: cell.row_data.to_vec()` | PatchInfo construction (line ~734) | full row ~150–500 B |
| `patched_data = patch.local_row_data.clone()` | Phase 2 start (line ~745) | full row ~150–500 B |
| `encode_cell_image()` inside `rewrite_cell_same_key_with_overflow` | Phase 2 write (line ~784) | full cell ~200 B |
| `FieldDelta.old_bytes: Vec<u8>` | per changed field (line ~768) | max 8 B |
| `FieldDelta.new_bytes: Vec<u8>` | per changed field (line ~769) | max 8 B |

For `UPDATE bench_users SET score = score + 1.0` on 25 000 rows this is
~125 000 heap allocations totalling ~20–25 MB of heap traffic, causing the
measured 380K r/s vs MariaDB 1 017K r/s (0.37× ratio).

### Target state after this subfase

- `patch_field_in_place()` — new primitive in `clustered_leaf.rs`: writes `N` bytes
  directly into `page.as_bytes_mut()` at the computed absolute offset of the field
  within the cell. Zero allocations. For non-overflow cells only.
- `update_row_header_in_place()` — new primitive in `clustered_leaf.rs`: writes 24
  RowHeader bytes directly into the page buffer at the cell's header slot.
  Handles the alignment issue (cells are not guaranteed 8-byte aligned) by
  serializing to a `[u8; 24]` stack buffer before the `copy_from_slice`.
- Modified `PatchInfo`: remove `local_row_data: Vec<u8>`, add `body_off: u16`.
- Modified Phase 2 loop in `fused_clustered_scan_patch()`:
  1. Read phase (immutable page borrow): compute `row_data_abs_off` from `body_off`,
     call `compute_field_location_runtime()` using a slice into `page.as_bytes()`,
     capture old bytes into `[u8; 8]` stack buffers. Drop borrow.
  2. Write phase (mutable page borrow): call `patch_field_in_place()` and
     `update_row_header_in_place()` for each cell. One `page.as_bytes_mut()`
     call per cell, not per field. No intermediate Vec.
- `FieldDelta.old_bytes` and `FieldDelta.new_bytes` changed from `Vec<u8>` to
  `[u8; 8]`. The `size: u8` field already present in `FieldDelta` determines how
  many bytes are valid. Update all callers: `encode_old_value`, `encode_new_value`,
  `decode_patch_value` (use `&old_bytes[..size as usize]`), and recovery handler
  (`copy_from_slice(&delta.old_bytes[..delta.size as usize])`).
- Overflow-backed rows (where `overflow_first_page.is_some()`) are **not eligible**
  for the in-place path. They fall through to the existing
  `rewrite_cell_same_key_with_overflow` path unchanged.
- MAYBE_NOP: after encoding the new field value to `[u8; 8]`, compare with the
  current bytes read from the page before patching. If identical (no-op assignment
  like `SET score = score * 1.0`), skip that field's patch and WAL delta entirely
  (the `new_val != values[col_pos]` guard already catches expression-level no-ops;
  this new guard catches numeric near-identity cases that survived expression
  comparison due to floating-point equality).

### On-disk and WAL compatibility

The **on-disk page format is unchanged**: the cell bytes written via
`patch_field_in_place()` are identical to the bytes that would have been written
via `rewrite_cell_same_key_with_overflow()`. Only the write path changes.

The **WAL format is unchanged**: `ClusteredFieldPatchEntry` serialization
(`encode_old_value`, `encode_new_value`) produces the same byte stream —
`[RowHeader:24][num_fields:1][offset:2][size:1][bytes:N]...` — regardless of
whether `FieldDelta.old_bytes` is a `Vec<u8>` or `[u8; 8]`.

No WAL version bump required. Recovery works unchanged except for the trivial
`old_bytes` slice syntax update.

---

## Inputs / Outputs

### `patch_field_in_place`

```
Input:
  page:          &mut Page          — the clustered leaf page to modify
  field_abs_off: usize              — absolute byte offset within page.as_bytes()
                                     where the field bytes start
  new_bytes:     &[u8]              — new encoded bytes (len ≤ 8)

Output:
  Ok(())  — field patched in-place
  Err(DbError::Other)  — field_abs_off + new_bytes.len() > PAGE_SIZE (out of bounds)

Preconditions (caller's responsibility):
  - The cell has no overflow (total_row_len == local inline len).
  - field_abs_off points within the row_data region of the cell (after RowHeader).
  - new_bytes.len() equals the fixed encoded size of the column type.
```

### `update_row_header_in_place`

```
Input:
  page:        &mut Page    — the clustered leaf page to modify
  body_off:    u16          — body-relative offset of the cell (from cell ptr array)
  new_header:  &RowHeader   — the updated RowHeader to write

Output:
  Ok(())  — RowHeader written at HEADER_SIZE + body_off + CELL_META_SIZE
  Err(DbError::Other)  — offset exceeds page bounds

Preconditions:
  - body_off is a valid cell pointer from cell_ptr_at().
  - The page type is ClusteredLeaf.
```

### `fused_clustered_scan_patch` (modified)

Same external signature as today (no change). Internal behavior changes:
- No `local_row_data: cell.row_data.to_vec()` allocation.
- No `patched_data = patch.local_row_data.clone()` allocation.
- No `encode_cell_image()` via `rewrite_cell_same_key_with_overflow` for inline cells.
- Overflow cells (less than 1% of typical workloads) still use the existing
  `rewrite_cell_same_key_with_overflow` path.

### `FieldDelta` (modified fields only)

```rust
// Before
pub struct FieldDelta {
    pub offset: u16,
    pub size: u8,
    pub old_bytes: Vec<u8>,
    pub new_bytes: Vec<u8>,
}

// After
pub struct FieldDelta {
    pub offset: u16,
    pub size: u8,
    pub old_bytes: [u8; 8],
    pub new_bytes: [u8; 8],
}
```

---

## Use cases

### 1. Happy path — UPDATE fixed-size column, all rows inline

```sql
UPDATE bench_users SET score = score + 1.0 WHERE active = TRUE;
-- 25 000 rows match, all inline (no overflow), score is REAL (8 bytes)
```

- All 25 000 rows go through `patch_field_in_place` + `update_row_header_in_place`.
- Zero `local_row_data` / `patched_data` allocations.
- FieldDelta carries `old_bytes: [f64 old], new_bytes: [f64 new]` inline.
- Page checksum updated once per leaf after all cells on that leaf are patched.

### 2. Mixed schema — TEXT columns before the target column

```sql
UPDATE bench_users SET age = age + 1 WHERE id = 42;
-- bench_users: id INT, name TEXT, age INT, active BOOL, score REAL, email TEXT
-- age is at column 2; name TEXT precedes it → needs runtime scan of row bytes
```

- `compute_field_location_runtime(col_types, 2, bitmap, Some(row_data_slice))` scans
  past the `[u24 len][payload]` of `name` to find `age`'s byte offset.
- Row data slice is `&page.as_bytes()[row_data_abs_off..]` — **no clone**.
- `patch_field_in_place` writes 4 new bytes directly to the page.

### 3. Overflow-backed row — falls back to existing path

```sql
UPDATE large_text_table SET counter = counter + 1 WHERE id = 7;
-- Row is overflow-backed (total_row_len > local inline limit)
```

- `overflow_first_page.is_some()` detected in Phase 1.
- PatchInfo records `overflow: true`.
- Phase 2 falls through to `rewrite_cell_same_key_with_overflow` unchanged.
- No regression in correctness or allocation count for this case.

### 4. MAYBE_NOP — float assignment that evaluates to identical bytes

```sql
UPDATE t SET score = score + 0.0;
-- Mathematically a no-op, but the expression `score + 0.0` might produce the
-- same IEEE 754 bytes as the original value.
```

- After computing `new_val = score + 0.0 = score`, the expression equality check
  `new_val != values[col_pos]` already catches this (both are `Value::Real(x)`).
- No `changed_fields` entry pushed → no patch → no WAL delta.
- Zero extra work after the WHERE match.

### 5. Rollback after UPDATE (recovery path)

```sql
BEGIN;
UPDATE bench_users SET score = score + 1.0 WHERE active = TRUE;
-- connection dies
```

- WAL contains `ClusteredFieldPatchEntry` entries with `old_bytes: [u8; 8]` (inline).
- Recovery reads `ClusteredFieldPatchEntry::from_wal_values()` — format unchanged.
- `RecoveryOp::ClusteredFieldPatchRestore` applies `delta.old_bytes[..delta.size]` to
  restore the old value — trivial `copy_from_slice` change.

### 6. Multiple changed columns per row

```sql
UPDATE t SET a = a + 1, b = b * 2 WHERE id < 1000;
-- a: INT (col 1), b: BIGINT (col 3), both fixed-size
```

- Phase 1 produces `changed_fields = [(col_pos_a, new_a), (col_pos_b, new_b)]`.
- Phase 2 read: computes two `FieldLocation` entries, captures two old `[u8; 8]` bufs.
- Phase 2 write: calls `patch_field_in_place` twice on the same page borrow block.
- `field_patches` Vec has 2 entries, each 36 bytes (usize + usize + [u8;8] + [u8;8]).

### 7. Variable-length column in SET — falls back to slow path at query routing level

```sql
UPDATE t SET name = 'new name' WHERE id = 5;
-- name is TEXT (variable-length)
```

- `execute_clustered_update` checks assignment targets before routing.
- TEXT column in SET → not all changed fields are fixed-size → falls back to
  `execute_clustered_update` slow path (unchanged), not `fused_clustered_scan_patch`.
- This subfase does not change the routing logic — `fused_clustered_scan_patch` is
  only called when all SET columns are fixed-size (pre-existing gate).

---

## Acceptance criteria

- [ ] `patch_field_in_place()` exists in `clustered_leaf.rs`, has unit tests covering:
  - writes bytes at the correct absolute offset
  - out-of-bounds offset returns `Err`
  - correctness verified by reading the page bytes back after the write
- [ ] `update_row_header_in_place()` exists in `clustered_leaf.rs`, has unit tests:
  - writes txn_id_created, txn_id_deleted, row_version, _flags at the correct offset
  - reads back with `read_cell` and verifies all four fields match the new header
  - alignment-safe: no panics from unaligned reads (handled by manual serialization)
- [ ] `FieldDelta.old_bytes` and `FieldDelta.new_bytes` are `[u8; 8]` (not `Vec<u8>`)
- [ ] `ClusteredFieldPatchEntry::encode_old_value()`, `encode_new_value()`, and
  `decode_patch_value()` use `&old_bytes[..size as usize]` (slice from inline array)
- [ ] `ClusteredFieldPatchEntry` roundtrip test still passes: old WAL byte layout is
  identical, only the in-memory representation changed
- [ ] Recovery handler `RecoveryOp::ClusteredFieldPatchRestore` uses
  `&delta.old_bytes[..delta.size as usize]` — all recovery tests still pass
- [ ] `PatchInfo` struct has no `local_row_data: Vec<u8>` field
- [ ] `fused_clustered_scan_patch()` Phase 2 contains no `patched_data = ...clone()`
- [ ] `fused_clustered_scan_patch()` Phase 2 contains no call to
  `rewrite_cell_same_key_with_overflow` for inline (non-overflow) cells
- [ ] Overflow cells still use `rewrite_cell_same_key_with_overflow` (regression test)
- [ ] All existing clustered UPDATE integration tests pass:
  - `crates/axiomdb-sql/tests/integration_clustered_create_table.rs`
  - `crates/axiomdb-sql/tests/integration_clustered_rebuild.rs`
  - `crates/axiomdb-sql/tests/integration_clustered_vacuum.rs`
- [ ] All existing WAL clustered recovery integration tests pass:
  - `crates/axiomdb-wal/tests/integration_clustered_recovery.rs`
- [ ] Wire-test scenario `[39.22 update in-place]`:
  - `UPDATE bench_users SET score = score + 1.0 WHERE active = TRUE` returns
    affected rows > 0 and a subsequent SELECT verifies score values increased
  - `BEGIN; UPDATE ...; ROLLBACK;` verifies score reverts to original value
  - `UPDATE bench_users SET score = 0.0 WHERE id = 1` followed by SELECT id=1
    verifies score is 0.0
- [ ] Performance benchmark:
  - `UPDATE bench_users SET score = score + 1.0 WHERE active = TRUE` (25 000 rows)
    achieves ≥ 1 000 000 r/s (≥ MariaDB 1 017K r/s baseline)
  - Target: **1.5× MariaDB** = 1 500 000 r/s
  - Maximum acceptable: 800 000 r/s (≥ MariaDB)

---

## Out of scope

- Variable-length (Text/Bytes) field in-place patching — requires row relocation when
  new length differs; deferred to future subfase.
- Overflow-backed row in-place patching — the overflow chain might hold the target
  field; requires overflow-aware offset computation; deferred.
- Batch WAL format (`ClusteredFieldPatchBatchV2`) — the current per-row WAL
  approach already produces compact field deltas. A batch format (one WAL entry per
  leaf instead of per row) is a further optimization deferred to Phase 40.
- NULL-transition field patching (NULL → value or value → NULL) — changes the
  null bitmap which may shift all subsequent field offsets; deferred.
  The existing gating in `compute_field_location_runtime` already returns `None` for
  NULL target columns; NULL transitions simply fall back to the slow path.
- PK-change UPDATE — different code path, not touched here.
- Secondary index maintenance — already excluded from `fused_clustered_scan_patch`
  (the pre-existing routing gate prevents its use when secondary keys change).

---

## Dependencies

- `crates/axiomdb-types/src/field_patch.rs` — `compute_field_location_runtime()`,
  `fixed_encoded_size()`, `write_field()` — already exist, used unchanged.
- `crates/axiomdb-types/src/codec.rs` — row codec format (null bitmap + field bytes)
  — already understood; no changes needed here.
- `crates/axiomdb-storage/src/clustered_leaf.rs` — location for new primitives;
  must export `patch_field_in_place()` and `update_row_header_in_place()`.
- `crates/axiomdb-wal/src/clustered.rs` — `FieldDelta` struct and encoding;
  `FieldDelta.old_bytes/new_bytes` type change + encode/decode updates.
- `crates/axiomdb-wal/src/recovery.rs` — trivial `[..delta.size as usize]` slice.
- `crates/axiomdb-sql/src/executor/update.rs` — main change site;
  `fused_clustered_scan_patch` Phase 1 and Phase 2 restructured.
- `crates/axiomdb-storage/src/clustered_leaf.rs` constants referenced:
  - `HEADER_SIZE` = 64 (page header)
  - `CELL_META_SIZE` = 6 (key_len:u16 + total_row_len:u32)
  - `ROW_HEADER_SIZE` = 24 (RowHeader struct)

### Key layout derivation

For a cell at `body_off` (body-relative, from cell pointer array):

```
page.as_bytes() absolute layout:
  [HEADER_SIZE + body_off]              : key_len (u16 LE)
  [HEADER_SIZE + body_off + 2]          : total_row_len (u32 LE)
  [HEADER_SIZE + body_off + 6]          : RowHeader (24 bytes)
  [HEADER_SIZE + body_off + 30]         : key bytes (key_len bytes)
  [HEADER_SIZE + body_off + 30 + key_len] : row_data bytes (local inline portion)

  row_data_abs_off = HEADER_SIZE + body_off + CELL_META_SIZE + ROW_HEADER_SIZE + key_len
                   = HEADER_SIZE + body_off + 6 + 24 + key_len
                   = HEADER_SIZE + body_off + 30 + key_len

  field_abs_off = row_data_abs_off + field_location.offset
```

### Overflow detection

A cell is overflow-backed iff `total_row_len > local inline len`, where:
```
local_inline_len = local_row_len(key_len, total_row_len)
```
`local_row_len` is defined in `clustered_leaf.rs`. The `CellRef.overflow_first_page`
field is `Some(_)` for overflow rows and `None` for inline rows. Only inline rows
are eligible for the new fast path.

---

## Reference: InnoDB `btr_cur_upd_rec_in_place` (storage/innobase/btr/btr0cur.cc)

InnoDB's equivalent is at `btr0cur.cc:2961–3115`. Key points that informed this design:

1. **Direct buffer write**: InnoDB calls `mtr->memcpy(block, rec + field_start, new_data, len)`
   where `rec` is a raw pointer into the buffer pool page. AxiomDB uses
   `page.as_bytes_mut()[field_abs_off..].copy_from_slice(new_bytes)` — same principle.

2. **Alignment**: InnoDB's records are also not guaranteed 8-byte aligned; it uses
   `mach_write_to_N` helpers that do byte-by-byte stores. AxiomDB uses manual
   serialization for RowHeader (same fix).

3. **Undo log before data write**: InnoDB writes the undo log entry *before* modifying
   the page. AxiomDB captures old bytes *before* calling `patch_field_in_place` (in
   the read phase), so WAL is built with correct old bytes before the page write
   (`storage.write_page()` happens after all patches + WAL batch).

4. **MAYBE_NOP (`UPD_NODE_NO_ORD_CHANGE`)**: InnoDB skips all work when the new value
   is byte-identical to the old. AxiomDB: `new_val != values[col_pos]` gate at
   expression level (already exists), plus the field-level byte comparison (new in
   this subfase).

5. **No full-row re-encode**: InnoDB never re-encodes the record — it only touches
   the changed bytes. AxiomDB's new path does the same: only the `N` bytes at
   `field_abs_off` are written, leaving the rest of the page untouched.
