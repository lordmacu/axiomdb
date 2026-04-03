# Plan: 39.22 — UPDATE in-place zero-alloc patch

## Files to create/modify

| File | Change |
|---|---|
| `crates/axiomdb-storage/src/clustered_leaf.rs` | Add `patch_field_in_place()`, `update_row_header_in_place()`, `cell_row_data_abs_off()` |
| `crates/axiomdb-wal/src/clustered.rs` | Change `FieldDelta.old_bytes/new_bytes: Vec<u8>` → `[u8; 8]`; update encode/decode |
| `crates/axiomdb-wal/src/recovery.rs` | Trivial: `delta.old_bytes` → `&delta.old_bytes[..delta.size as usize]` |
| `crates/axiomdb-sql/src/executor/update.rs` | Restructure `PatchInfo` + Phase 2 of `fused_clustered_scan_patch()` |

---

## Algorithm

### New primitives in `clustered_leaf.rs`

```rust
/// Returns the absolute byte offset (within page.as_bytes()) where row_data
/// starts for the cell at logical index `cell_idx`, plus the key length.
///
/// row_data_abs_off = HEADER_SIZE + body_off + CELL_META_SIZE + ROW_HEADER_SIZE + key_len
pub fn cell_row_data_abs_off(
    page: &Page,
    cell_idx: usize,
) -> Result<(usize, usize), DbError> {
    let body_off = cell_ptr_at(page, cell_idx as u16) as usize;
    let abs = HEADER_SIZE + body_off;
    let b = page.as_bytes();
    if abs + CELL_META_SIZE > PAGE_SIZE {
        return Err(DbError::Other("clustered_leaf: cell index out of bounds".into()));
    }
    let key_len = u16::from_le_bytes([b[abs], b[abs + 1]]) as usize;
    let row_data_abs = abs + CELL_META_SIZE + ROW_HEADER_SIZE + key_len;
    Ok((row_data_abs, key_len))
}

/// Patch `new_bytes.len()` bytes at absolute page offset `field_abs_off`.
///
/// Caller must ensure `field_abs_off` points within the row_data region of an
/// inline (non-overflow) cell, and that `new_bytes.len()` equals the fixed
/// encoded size of the column type being updated.
///
/// This is the AxiomDB equivalent of InnoDB's `mtr->memcpy(block, rec + off, buf, len)`.
pub fn patch_field_in_place(
    page: &mut Page,
    field_abs_off: usize,
    new_bytes: &[u8],
) -> Result<(), DbError> {
    let end = field_abs_off + new_bytes.len();
    if end > PAGE_SIZE {
        return Err(DbError::Other(format!(
            "patch_field_in_place: field [{field_abs_off}..{end}) exceeds PAGE_SIZE {PAGE_SIZE}"
        )));
    }
    page.as_bytes_mut()[field_abs_off..end].copy_from_slice(new_bytes);
    Ok(())
}

/// Write a RowHeader at the cell's header slot (HEADER_SIZE + body_off + CELL_META_SIZE).
///
/// Serializes manually to a [u8; ROW_HEADER_SIZE] stack buffer before the
/// copy_from_slice to avoid UB from unaligned pointer casts (cells are not
/// guaranteed 8-byte aligned in the page body).
pub fn update_row_header_in_place(
    page: &mut Page,
    body_off: u16,
    new_header: &RowHeader,
) -> Result<(), DbError> {
    let hdr_abs = HEADER_SIZE + body_off as usize + CELL_META_SIZE;
    if hdr_abs + ROW_HEADER_SIZE > PAGE_SIZE {
        return Err(DbError::Other("update_row_header_in_place: header exceeds PAGE_SIZE".into()));
    }
    // Serialize manually — cannot use bytemuck::bytes_of due to alignment.
    let mut buf = [0u8; ROW_HEADER_SIZE]; // ROW_HEADER_SIZE = 24
    buf[0..8].copy_from_slice(&new_header.txn_id_created.to_le_bytes());
    buf[8..16].copy_from_slice(&new_header.txn_id_deleted.to_le_bytes());
    buf[16..20].copy_from_slice(&new_header.row_version.to_le_bytes());
    buf[20..24].copy_from_slice(&new_header._flags.to_le_bytes());
    page.as_bytes_mut()[hdr_abs..hdr_abs + ROW_HEADER_SIZE].copy_from_slice(&buf);
    Ok(())
}
```

### `FieldDelta` change in `clustered.rs`

```rust
// BEFORE
pub struct FieldDelta {
    pub offset: u16,
    pub size: u8,
    pub old_bytes: Vec<u8>,
    pub new_bytes: Vec<u8>,
}

// AFTER
pub struct FieldDelta {
    pub offset: u16,
    pub size: u8,
    pub old_bytes: [u8; 8],
    pub new_bytes: [u8; 8],
}
```

Update `encode_old_value` / `encode_new_value`:
```rust
// Before: buf.extend_from_slice(&delta.old_bytes);
// After:
buf.extend_from_slice(&delta.old_bytes[..delta.size as usize]);
```

Update `decode_patch_value` — build FieldDelta from decoded bytes:
```rust
let mut old_arr = [0u8; 8];
old_arr[..size as usize].copy_from_slice(&bytes[cursor..end]);
fields.push((offset, size, old_arr));
// Return (u16, u8, [u8; 8]) triples — update the return type signature
```

Update `ClusteredFieldPatchEntry::from_wal_values`:
```rust
field_deltas.push(FieldDelta {
    offset: old_offset,
    size: old_size,
    old_bytes: /* [u8;8] from old_arr */,
    new_bytes: /* [u8;8] from new_arr */,
});
```

### Recovery handler in `recovery.rs`

Line ~528:
```rust
// Before:
restored_row_data[start..end].copy_from_slice(&delta.old_bytes);
// After:
restored_row_data[start..end].copy_from_slice(&delta.old_bytes[..delta.size as usize]);
```

### `fused_clustered_scan_patch` restructuring in `update.rs`

#### Phase 1 — PatchInfo struct change

```rust
struct PatchInfo {
    idx: usize,
    body_off: u16,                         // NEW: body-relative cell offset
    old_header: axiomdb_storage::heap::RowHeader,
    total_row_len: usize,
    overflow_first_page: Option<u64>,
    key: Vec<u8>,
    changed_fields: Vec<(usize, Value)>,
    // REMOVED: local_row_data: Vec<u8>
}
```

Population (Phase 1 loop):
```rust
patches.push(PatchInfo {
    idx,
    body_off: clustered_leaf::cell_ptr_at(&page, idx as u16),  // NEW
    old_header: cell.row_header,
    total_row_len: cell.total_row_len,
    overflow_first_page: cell.overflow_first_page,
    key: cell.key.to_vec(),
    changed_fields,
    // REMOVED: local_row_data: cell.row_data.to_vec(),
});
```

#### Phase 2 — apply patches

```rust
for patch in &patches {
    // Overflow rows: fall back to old path (unchanged).
    if patch.overflow_first_page.is_some() {
        // Re-read cell to get row_data, then call rewrite_cell_same_key_with_overflow.
        // (existing slow path, preserved without change)
        ...
        continue;
    }

    // === FAST PATH: inline cells — zero alloc per row ===

    // ── Read phase (immutable page borrow) ──
    let (row_data_abs_off, key_len_in_page) = {
        let b = page.as_bytes();
        let abs = HEADER_SIZE + patch.body_off as usize;
        let key_len_local = u16::from_le_bytes([b[abs], b[abs + 1]]) as usize;
        let rda = abs + CELL_META_SIZE + ROW_HEADER_SIZE + key_len_local;
        (rda, key_len_local)
    };

    // Inner block: hold immutable borrow, compute all field locations + old bytes.
    // field_patches: Vec<(field_abs_off, size, old_buf:[u8;8], new_buf:[u8;8])>
    let (field_patches, any_changed) = {
        let b = page.as_bytes();
        let row_data_slice = &b[row_data_abs_off..];
        let bitmap_len = col_types.len().div_ceil(8);
        let bitmap = &row_data_slice[..bitmap_len];

        let mut fps: Vec<(usize, usize, [u8; 8], [u8; 8])> =
            Vec::with_capacity(patch.changed_fields.len());
        let mut changed = false;

        for (col_pos, new_val) in &patch.changed_fields {
            let loc = match axiomdb_types::field_patch::compute_field_location_runtime(
                col_types, *col_pos, bitmap, Some(row_data_slice),
            ) {
                Some(l) => l,
                None => continue, // NULL target or variable-length: skip
            };

            // Encode new value.
            let new_encoded: [u8; 8] = axiomdb_types::field_patch::encode_value_fixed(
                new_val, loc.data_type,
            )?;

            // Capture old bytes from page.
            let field_abs = row_data_abs_off + loc.offset;
            let mut old_buf = [0u8; 8];
            old_buf[..loc.size].copy_from_slice(&b[field_abs..field_abs + loc.size]);

            // MAYBE_NOP: if byte-identical, skip this field.
            if old_buf[..loc.size] == new_encoded[..loc.size] {
                continue;
            }

            fps.push((field_abs, loc.size, old_buf, new_encoded));
            changed = true;
        }
        (fps, changed)
    }; // immutable borrow dropped

    if !any_changed {
        continue;
    }

    // Build new RowHeader before write phase.
    let new_header = axiomdb_storage::heap::RowHeader {
        txn_id_created: txn_id,
        txn_id_deleted: 0,
        row_version: patch.old_header.row_version.wrapping_add(1),
        _flags: patch.old_header._flags,
    };

    // ── Write phase (mutable page borrow) ──
    {
        for (field_abs, size, _, new_buf) in &field_patches {
            clustered_leaf::patch_field_in_place(&mut page, *field_abs, &new_buf[..*size])?;
        }
        clustered_leaf::update_row_header_in_place(&mut page, patch.body_off, &new_header)?;
    }

    page_dirty = true;
    patched += 1;

    // Build WAL delta (no Vec<u8> per field — FieldDelta now has [u8;8] inline).
    let field_deltas: Vec<axiomdb_wal::FieldDelta> = field_patches
        .iter()
        .map(|(field_abs, size, old_buf, new_buf)| {
            let field_offset_in_row = field_abs - row_data_abs_off;
            axiomdb_wal::FieldDelta {
                offset: field_offset_in_row as u16,
                size: *size as u8,
                old_bytes: *old_buf,
                new_bytes: *new_buf,
            }
        })
        .collect();

    wal_patches.push(axiomdb_wal::ClusteredFieldPatchEntry {
        key: patch.key.clone(),
        old_header: patch.old_header,
        new_header,
        old_row_data: Vec::new(), // not needed; WAL uses field deltas
        field_deltas,
    });
}
```

Note: `encode_value_fixed` is a new public function in `field_patch.rs` that encodes
a `Value` to `[u8; 8]` without the `write_field` mutable borrow. It re-uses the
private `encode_field_value` logic — just expose it with the right signature.

---

## Implementation phases

### Step 1 — New primitives in `clustered_leaf.rs`

1. Add `cell_row_data_abs_off()` — pure computation, no mutation.
2. Add `patch_field_in_place()` — mutable page write, bounds check.
3. Add `update_row_header_in_place()` — mutable page write, manual RowHeader serialize.
4. Write unit tests (see Tests section).
5. Run: `cargo test -p axiomdb-storage`.

### Step 2 — `FieldDelta` change in `axiomdb-wal`

1. Change `old_bytes: Vec<u8>` → `[u8; 8]`, `new_bytes: Vec<u8>` → `[u8; 8]`.
2. Update `encode_old_value` / `encode_new_value`: `&delta.old_bytes[..delta.size as usize]`.
3. Update `decode_patch_value` return type: use `[u8; 8]` with `copy_from_slice`.
4. Update `from_wal_values`: build `FieldDelta` from `[u8; 8]` fields.
5. Existing roundtrip test must still pass.
6. Run: `cargo test -p axiomdb-wal`.

### Step 3 — Recovery handler update (trivial)

1. In `recovery.rs` line ~528: `&delta.old_bytes` → `&delta.old_bytes[..delta.size as usize]`.
2. Run: `cargo test -p axiomdb-wal` (includes recovery tests).

### Step 4 — `fused_clustered_scan_patch` restructuring

1. Add `encode_value_fixed()` public function to `field_patch.rs` (expose `encode_field_value` logic).
2. Change `PatchInfo`: remove `local_row_data`, add `body_off: u16`.
3. Restructure Phase 2 per algorithm above:
   - Overflow path: re-read cell row_data from page, call `rewrite_cell_same_key_with_overflow`.
   - Inline path: read phase → write phase → WAL.
4. Remove dead code: `local_row_data: cell.row_data.to_vec()` and `patched_data = ...clone()`.
5. Run: `cargo test -p axiomdb-sql`.
6. Run: `cargo test -p axiomdb-wal` (recovery integration).
7. Run: `tools/wire-test.py` (pre-flight: pkill + cargo build + rm stale binary).

### Step 5 — Benchmark and closing gate

1. Run `local_bench.py` — verify UPDATE ≥ 1 000 000 r/s.
2. Run `cargo test --workspace` — full clean sweep.
3. Run `cargo clippy --workspace -- -D warnings`.
4. Run `cargo fmt --check`.

---

## Tests to write

### Unit tests in `clustered_leaf.rs`

```
test_patch_field_in_place_basic:
  - create a mock Page with a known cell (fixed data)
  - call patch_field_in_place at the row_data field offset
  - assert page bytes at that offset equal new bytes
  - assert surrounding bytes unchanged

test_patch_field_in_place_out_of_bounds:
  - field_abs_off = PAGE_SIZE - 2, new_bytes.len() = 4
  - expect Err

test_update_row_header_in_place_roundtrip:
  - init_clustered_leaf, insert a cell with known RowHeader
  - call update_row_header_in_place with new header values
  - read_cell, verify row_header fields match new header exactly
  - verify key and row_data unchanged

test_update_row_header_in_place_alignment_safe:
  - verify no panic on unaligned body_off (every possible body_off % 8 value)
```

### Integration tests in `axiomdb-sql/tests/`

New file: `integration_clustered_update_inplace.rs`

```
test_update_inplace_single_column:
  CREATE TABLE t (id INT NOT NULL, score REAL NOT NULL, PRIMARY KEY(id)) USING CLUSTERED;
  INSERT INTO t VALUES (1, 1.0), (2, 2.0), (3, 3.0);
  UPDATE t SET score = score + 1.0;
  SELECT id, score FROM t ORDER BY id;
  -- Expected: (1, 2.0), (2, 3.0), (3, 4.0)

test_update_inplace_mixed_schema:
  -- Schema with TEXT before target column
  CREATE TABLE t2 (id INT NOT NULL, name TEXT NOT NULL, score REAL NOT NULL, PRIMARY KEY(id)) USING CLUSTERED;
  INSERT INTO t2 VALUES (1, 'alice', 10.0), (2, 'bob', 20.0);
  UPDATE t2 SET score = score * 2.0;
  SELECT id, score FROM t2 ORDER BY id;
  -- Expected: (1, 20.0), (2, 40.0)

test_update_inplace_rollback:
  BEGIN;
  UPDATE t SET score = score + 100.0;
  ROLLBACK;
  SELECT score FROM t WHERE id = 1;
  -- Expected: 2.0 (original, not 102.0)

test_update_inplace_where_clause:
  UPDATE t SET score = 0.0 WHERE id = 2;
  SELECT score FROM t WHERE id = 2;
  -- Expected: 0.0
  SELECT score FROM t WHERE id = 1;
  -- Expected: unchanged (2.0)

test_update_noop_same_value:
  UPDATE t SET score = score + 0.0;
  -- All rows still have their current scores (MAYBE_NOP)
  SELECT score FROM t WHERE id = 1;
  -- Expected: 2.0 (unchanged)

test_update_inplace_crash_recovery:
  -- Write, crash-simulate (drop engine), reopen
  let dir = tempfile::tempdir();
  let eng = Engine::open(dir.path())?;
  eng.execute("CREATE TABLE cr (id INT NOT NULL, v INT NOT NULL, PRIMARY KEY(id)) USING CLUSTERED")?;
  eng.execute("INSERT INTO cr VALUES (1, 10)")?;
  eng.execute("BEGIN")?;
  eng.execute("UPDATE cr SET v = 99")?;
  drop(eng); // crash before COMMIT
  let eng2 = Engine::open(dir.path())?;
  let rows = eng2.execute("SELECT v FROM cr WHERE id = 1")?;
  assert_eq!(rows[0].get_int("v"), 10); // old value restored by crash recovery
```

### WAL roundtrip test (in `clustered.rs`)

```
test_field_delta_inline_roundtrip:
  let patch = ClusteredFieldPatchEntry {
      key: b"pk".to_vec(),
      old_header: row_header(7, 0, 3),
      new_header: row_header(8, 0, 4),
      old_row_data: Vec::new(),
      field_deltas: vec![
          FieldDelta { offset: 5, size: 8, old_bytes: [1,2,3,4,5,6,7,8], new_bytes: [9,10,11,12,13,14,15,16] },
          FieldDelta { offset: 1, size: 1, old_bytes: [0,0,0,0,0,0,0,0], new_bytes: [1,0,0,0,0,0,0,0] },
      ],
  };
  let decoded = ClusteredFieldPatchEntry::from_wal_values(
      &patch.key,
      &patch.encode_old_value(),
      &patch.encode_new_value(),
  ).unwrap();
  assert_eq!(decoded.field_deltas[0].old_bytes[..8], [1,2,3,4,5,6,7,8]);
  assert_eq!(decoded.field_deltas[0].new_bytes[..8], [9,10,11,12,13,14,15,16]);
  assert_eq!(decoded.field_deltas[1].old_bytes[0], 0);
  assert_eq!(decoded.field_deltas[1].new_bytes[0], 1);
```

---

## Anti-patterns to avoid

- **DO NOT** call `cell.row_data.to_vec()` in `PatchInfo`. This is the root cause
  of the performance regression. The new path reads row_data as a slice from the
  page's immutable borrow.
- **DO NOT** call `encode_cell_image()` or `rewrite_cell_same_key_with_overflow()`
  for inline cells. These rebuild the entire cell image. The new path writes only
  the changed bytes.
- **DO NOT** use `bytemuck::bytes_of(new_header)` for writing RowHeader to the page.
  Cells are not guaranteed 8-byte aligned; `bytemuck::bytes_of` requires alignment.
  Always serialize RowHeader manually with `to_le_bytes()` writes.
- **DO NOT** hold both an immutable and mutable borrow of `page` simultaneously.
  The read phase (immutable) and write phase (mutable) must be in separate scopes.
- **DO NOT** change the WAL serialization byte layout. The `encode_old_value` /
  `encode_new_value` output must be byte-for-byte identical to the current format.
  Only the in-memory representation of `old_bytes`/`new_bytes` changes.
- **DO NOT** patch overflow cells with the new primitives. Overflow cells may have
  the target field in the overflow chain (not on the leaf page). Always check
  `overflow_first_page.is_some()` and use the slow path for those.

---

## Risks

| Risk | Mitigation |
|---|---|
| Borrow checker rejects read+write phases in the same loop | Separate into two distinct block scopes (existing Rust pattern; see `std::collections::HashMap::entry` design) |
| `body_off` stored in PatchInfo becomes stale if the page is reorganized between Phase 1 and Phase 2 | Pages are not reorganized between phases within the same leaf iteration — the `patches` Vec is local and Phase 2 runs immediately after Phase 1 on the same `page` variable. No other code touches the page in between. |
| Recovery test for field patch now uses `[u8;8]` — old WAL files with `Vec<u8>` layout | WAL serialization format is unchanged (byte stream identical). Only in-memory representation changes. Old WAL files decode identically. |
| `encode_value_fixed` returns wrong bytes for Int/Bool coercion | Mirror `encode_field_value` exactly; add unit test for all 6 fixed types |
| Overflow cell handling breaks after PatchInfo struct change | Add regression test: `test_update_inplace_overflow_fallback` — create a table with rows large enough to be overflow-backed, run UPDATE, verify correctness |
