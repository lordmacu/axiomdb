# Plan: Index-Only Scans / Covering Indexes (Phase 6.13)

## Files to create / modify

### Create
- `crates/axiomdb-sql/tests/integration_index_only.rs` — integration tests

### Modify
- `crates/axiomdb-catalog/src/schema.rs` — `IndexDef.include_columns: Vec<u16>`
- `crates/axiomdb-sql/src/ast.rs` — `CreateIndexStmt.include_columns: Vec<String>`
- `crates/axiomdb-sql/src/parser/ddl.rs` — parse `INCLUDE (col1, col2)`
- `crates/axiomdb-sql/src/key_encoding.rs` — add `decode_index_key`
- `crates/axiomdb-storage/src/heap_chain.rs` — add `is_slot_visible` (header-only check)
- `crates/axiomdb-sql/src/planner.rs` — add `AccessMethod::IndexOnlyScan`, `index_covers_query`
- `crates/axiomdb-sql/src/executor.rs` — handle `IndexOnlyScan` path, bootstrap `include_columns`, pass `select_col_idxs` to planner

---

## Algorithm / Data structures

### `decode_index_key(key: &[u8], n_values: usize) -> Result<(Vec<Value>, usize)>`

```rust
// Tag constants (same as encode_value)
const TAG_NULL: u8 = 0x00;
const TAG_BOOL: u8 = 0x01;
const TAG_INT: u8 = 0x02;   // 8 BE bytes, sign-flipped
const TAG_BIGINT: u8 = 0x03; // 8 BE bytes, sign-flipped
const TAG_REAL: u8 = 0x04;  // 8 BE bytes, encode_f64
const TAG_DECIMAL: u8 = 0x05; // 1 scale + 16 BE bytes, sign-flipped
const TAG_DATE: u8 = 0x06;  // 8 BE bytes, sign-flipped
const TAG_TIMESTAMP: u8 = 0x07; // 8 BE bytes, sign-flipped
const TAG_TEXT: u8 = 0x08;  // NUL-terminated, 0xFF escape
const TAG_BYTES: u8 = 0x09; // NUL-terminated, 0xFF escape
const TAG_UUID: u8 = 0x0A;  // 16 raw bytes

fn decode_value(key: &[u8], pos: usize) -> Result<(Value, usize), DbError> {
    if pos >= key.len() { return Err(ParseError("key truncated")); }
    match key[pos] {
        TAG_NULL => Ok((Value::Null, pos + 1)),
        TAG_BOOL => Ok((Value::Bool(key[pos+1] != 0), pos + 2)),
        TAG_INT => {
            let u = u64::from_be_bytes(key[pos+1..pos+9]);
            let n = (u ^ (i64::MIN as u64)) as i64 as i32;
            Ok((Value::Int(n), pos + 9))
        }
        TAG_BIGINT => {
            let u = u64::from_be_bytes(key[pos+1..pos+9]);
            let n = (u ^ (i64::MIN as u64)) as i64;
            Ok((Value::BigInt(n), pos + 9))
        }
        TAG_REAL => {
            let bytes: [u8;8] = key[pos+1..pos+9].try_into().unwrap();
            Ok((Value::Real(decode_f64(bytes)), pos + 9))
        }
        TAG_DECIMAL => {
            let scale = key[pos+1];
            let u = u128::from_be_bytes(key[pos+2..pos+18].try_into().unwrap());
            let m = (u ^ (i128::MIN as u128)) as i128;
            Ok((Value::Decimal(m, scale), pos + 18))
        }
        TAG_DATE => {
            let u = u64::from_be_bytes(key[pos+1..pos+9]);
            let n = (u ^ (i64::MIN as u64)) as i64 as i32;
            Ok((Value::Date(n), pos + 9))
        }
        TAG_TIMESTAMP => {
            let u = u64::from_be_bytes(key[pos+1..pos+9]);
            let n = (u ^ (i64::MIN as u64)) as i64;
            Ok((Value::Timestamp(n), pos + 9))
        }
        TAG_TEXT => {
            let (s, end) = decode_bytes_nul(&key[pos+1..])?;
            Ok((Value::Text(String::from_utf8(s)?), pos + 1 + end))
        }
        TAG_BYTES => {
            let (b, end) = decode_bytes_nul(&key[pos+1..])?;
            Ok((Value::Bytes(b), pos + 1 + end))
        }
        TAG_UUID => {
            let mut u = [0u8;16];
            u.copy_from_slice(&key[pos+1..pos+17]);
            Ok((Value::Uuid(u), pos + 17))
        }
        tag => Err(ParseError(format!("unknown key tag: 0x{tag:02x}")))
    }
}

pub fn decode_index_key(key: &[u8], n_values: usize) -> Result<(Vec<Value>, usize), DbError> {
    let mut values = Vec::with_capacity(n_values);
    let mut pos = 0;
    for _ in 0..n_values {
        let (v, new_pos) = decode_value(key, pos)?;
        values.push(v);
        pos = new_pos;
    }
    Ok((values, pos))
}

fn decode_f64(bytes: [u8;8]) -> f64 {
    let u = u64::from_be_bytes(bytes);
    let bits = if u & (1u64 << 63) != 0 { u ^ (1u64 << 63) } else { !u };
    f64::from_bits(bits)
}

fn decode_bytes_nul(src: &[u8]) -> Result<(Vec<u8>, usize), DbError> {
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        if i >= src.len() { return Err(ParseError("unterminated key bytes")); }
        if src[i] == 0xFF && i+1 < src.len() && src[i+1] == 0x00 {
            out.push(0x00); i += 2;
        } else if src[i] == 0x00 {
            return Ok((out, i + 1));  // +1 for the terminator
        } else {
            out.push(src[i]); i += 1;
        }
    }
}
```

### `is_slot_visible(storage, page_id, slot_id, snapshot) -> Result<bool>`

```rust
// Read only the RowHeader (24 bytes) — skip the full row payload.
pub fn is_slot_visible(
    storage: &dyn StorageEngine,
    page_id: u64,
    slot_id: u16,
    snap: TransactionSnapshot,
) -> Result<bool, DbError> {
    let page = storage.read_page(page_id)?;
    match read_tuple(&page, slot_id)? {
        None => Ok(false),                          // slot empty / deleted
        Some((header, _data)) => Ok(header.is_visible(&snap)),
    }
}
```

Note: `read_tuple` returns `(&RowHeader, &[u8])` — the header IS already parsed
by the existing function; we just ignore `_data`. Zero extra cost vs a full read.

### `IndexDef.include_columns` on-disk extension

Appended after `is_fk_index` byte (flags byte already present):
```
[...existing format...]
[is_fk_index bit in flags]  ← already at flags bit 2
[include_len: 1 byte]       ← NEW: 0 = no INCLUDE columns
[include_col_idxs: include_len × 2 bytes LE u16]  ← NEW
```

`from_bytes`: after reading `is_fk_index` from flags, if `bytes.len() > consumed`:
read `include_len: u8`, then `include_len` u16 values.

### `plan_select` changes

New parameter: `select_col_idxs: &[u16]` — column indices needed in the output.

New `AccessMethod` variant in the returned enum:
```rust
IndexOnlyScan {
    index_def: IndexDef,
    lo: Vec<u8>,
    hi: Option<Vec<u8>>,            // None = point lookup
    n_key_cols: usize,               // number of columns in index key
    needed_key_positions: Vec<usize>, // which key-column positions to include in output
}
```

Coverage check function (added to `planner.rs`):
```rust
fn index_covers_query(
    index: &IndexDef,
    select_col_idxs: &[u16],
) -> bool {
    let key_cols: std::collections::HashSet<u16> =
        index.columns.iter().map(|c| c.col_idx).collect();
    select_col_idxs.iter().all(|col| key_cols.contains(col))
}
```

Rules 0, 1, 2 gain a post-check: after finding an index, if
`index_covers_query(idx, select_col_idxs)` → promote to `IndexOnlyScan`.

---

## Implementation phases

### Phase 1 — Catalog + AST + Parser (IndexDef.include_columns)

**Step 1.1** — `schema.rs`:
Add `pub include_columns: Vec<u16>` to `IndexDef`.
Update `to_bytes()`: after `is_fk_index` flag byte (already in flags), append:
`[include_len: u8][col_idx: u16 LE] × include_len`.
Update `from_bytes()`: read them if bytes remain after current position.
Update all `IndexDef { ... }` literals with `include_columns: vec![]`.

**Step 1.2** — `ast.rs`:
Add `pub include_columns: Vec<String>` to `CreateIndexStmt`.

**Step 1.3** — `parser/ddl.rs` in `parse_create_index`:
After the column list `RParen`, before `WHERE`, add:
```rust
let include_columns = if p.eat(&Token::Include) {
    p.expect(&Token::LParen)?;
    let mut cols = vec![p.parse_identifier()?];
    while p.eat(&Token::Comma) { cols.push(p.parse_identifier()?); }
    p.expect(&Token::RParen)?;
    cols
} else { vec![] };
```
Add `Token::Include` to lexer.

**Step 1.4** — `executor.rs` in `execute_create_index`:
When persisting `IndexDef`, resolve `stmt.include_columns` to `col_idx` values:
```rust
let include_col_idxs: Vec<u16> = stmt.include_columns.iter()
    .map(|name| col_defs.iter().find(|c| &c.name == name).map(|c| c.col_idx)
         .ok_or_else(|| DbError::ColumnNotFound { name: name.clone(), table: ... }))
    .collect::<Result<_,_>>()?;
// Add include_columns: include_col_idxs to IndexDef
```

**Verify:** `cargo build --workspace` clean.

---

### Phase 2 — `decode_index_key` + `is_slot_visible`

**Step 2.1** — `key_encoding.rs`:
Add `decode_index_key`, `decode_value`, `decode_f64`, `decode_bytes_nul` private helpers.
Add unit tests verifying roundtrip for every Value variant including edge cases.

**Step 2.2** — `heap_chain.rs`:
Add `pub fn is_slot_visible(storage, page_id, slot_id, snap) -> Result<bool>`.
Uses existing `read_tuple` which returns `(&RowHeader, _)`.

**Verify:** `cargo test -p axiomdb-sql -p axiomdb-storage` — roundtrip tests pass.

---

### Phase 3 — Planner: coverage detection + `IndexOnlyScan`

**Step 3.1** — `planner.rs`:
Add `IndexOnlyScan` variant to `AccessMethod`.
Add `index_covers_query(index, select_col_idxs) -> bool`.
Update `plan_select` signature to add `select_col_idxs: &[u16]`.
After each Rule (0, 1, 2) finds an index candidate, check coverage:
```rust
if !select_col_idxs.is_empty() && index_covers_query(idx, select_col_idxs) {
    // Build needed_key_positions: for each select col, find its position in index.columns
    let positions = select_col_idxs.iter().map(|col_idx| {
        idx.columns.iter().position(|c| c.col_idx == *col_idx).unwrap()
    }).collect();
    return AccessMethod::IndexOnlyScan {
        index_def: idx.clone(), lo: key.clone(), hi: None,
        n_key_cols: idx.columns.len(), needed_key_positions: positions
    };
}
// else: return IndexLookup / IndexRange as before
```

For composite Rule 0: same check, but for the range case `lo == hi`.

**Step 3.2** — Update all `plan_select` call sites (executor.rs lines ~588 and ~1666):
Load `select_col_idxs` before calling planner:
```rust
let select_col_idxs: Vec<u16> = collect_select_col_idxs(&stmt);
let access_method = plan_select(..., &select_col_idxs, ...);
```

`collect_select_col_idxs`: walk `stmt.columns` (the SELECT list after analysis),
collect `col_idx` from `SelectItem::Column { col_idx }` items. Returns `vec![]` for
`SELECT *` (wildcard — don't use index-only scan since all columns needed).

**Verify:** Planner tests pass. No regression on existing tests.

---

### Phase 4 — Executor: `IndexOnlyScan` path

**Step 4.1** — `executor.rs` in `execute_select_ctx` (single-table path):
Add handling for `AccessMethod::IndexOnlyScan`:

```rust
AccessMethod::IndexOnlyScan { index_def, lo, hi, n_key_cols, needed_key_positions } => {
    let pairs = BTree::range_in(storage, index_def.root_page_id,
                                Some(&lo), hi.as_deref())?;
    let mut rows = Vec::new();
    for (rid, key_bytes) in pairs {
        // MVCC check — read slot header only, no full row decode
        if !HeapChain::is_slot_visible(storage, rid.page_id, rid.slot_id, snap)? {
            continue;
        }
        // Decode all key columns
        let (key_values, _) = decode_index_key(&key_bytes, n_key_cols)?;
        // Project to needed columns
        let row_values: Vec<Value> = needed_key_positions.iter()
            .map(|&pos| key_values[pos].clone()).collect();
        rows.push((rid, row_values));
    }
    rows
}
```

Note: the `row_values` here only contain the `select_col_idxs` columns.
The rest of the executor (WHERE filter, projection) operates on these values.
Column indices in WHERE conditions that reference key columns need to be remapped
from table `col_idx` to the projected position — handle this by passing a full
`column_mask` to the scan that maps projected positions back.

**Simpler approach:** project ALL key columns first, then apply the existing
WHERE filter (which uses `col_idx`), then project to the SELECT list.
```rust
// Project full key (all n_key_cols values)
let (all_key_values, _) = decode_index_key(&key_bytes, n_key_cols)?;
// Reconstruct a pseudo-row by placing key values at their col_idx positions
let mut pseudo_row = vec![Value::Null; max_col_idx + 1];
for (col_def, val) in index_def.columns.iter().zip(all_key_values.iter()) {
    pseudo_row[col_def.col_idx as usize] = val.clone();
}
// Apply WHERE filter using existing eval() on pseudo_row
// Then project to SELECT list
```

This avoids any col_idx remapping complexity and reuses the existing filter logic.

---

### Phase 5 — Tests

**Integration tests** (`tests/integration_index_only.rs`):

```rust
// decode_index_key roundtrip
fn test_roundtrip_null()
fn test_roundtrip_int()
fn test_roundtrip_bigint()
fn test_roundtrip_real()
fn test_roundtrip_text()
fn test_roundtrip_composite_key()
fn test_roundtrip_with_null_in_composite()

// Coverage detection
fn test_planner_uses_index_only_for_single_col_select()
fn test_planner_uses_index_only_for_count_star()
fn test_planner_falls_back_when_not_covered()
fn test_planner_uses_index_only_for_composite_covered()

// Correctness
fn test_index_only_returns_same_results_as_heap_scan()
fn test_index_only_respects_mvcc_deleted_rows()
fn test_index_only_respects_mvcc_uncommitted_rows()

// INCLUDE catalog (no B-Tree storage yet)
fn test_include_columns_persisted_in_catalog()
fn test_include_columns_backward_compat_old_rows()
fn test_create_index_with_include_compiles()
```

---

## Anti-patterns to avoid

- **DO NOT** return values without MVCC check. Even if the key is in the B-Tree,
  the row might be deleted or uncommitted. `is_slot_visible` is mandatory.
- **DO NOT** decode more columns than needed in the hot path. The pseudo-row
  approach fills ALL key column positions — but key count is small (1-3 typically).
- **DO NOT** use `IndexOnlyScan` for `SELECT *` (wildcard). `collect_select_col_idxs`
  returns `vec![]` for wildcards → no index-only path activated.
- **DO NOT** assume `decode_index_key` succeeds on invalid bytes. Use `?` and
  surface the error — B-Tree corruption would produce parse errors, which should
  be loud.
- **DO NOT** skip the INCLUDE column storage regression: ensure old `IndexDef`
  rows with no include section deserialize to `include_columns = vec![]`.

## Risks

| Risk | Mitigation |
|------|-----------|
| `decode_f64` doesn't exactly invert `encode_f64` | Add dedicated roundtrip tests for ±∞, subnormals, -0.0 edge cases |
| `select_col_idxs` wrong when subqueries / aliases involved | Only activate `IndexOnlyScan` when all select items are plain `Column` refs (no expressions); conservative fallback to IndexLookup |
| CoW B-Tree root change after DELETE makes key unreadable | `is_slot_visible` reads the heap page at `rid`, not the B-Tree key — heap slot is stable; the key was valid when range_in returned it |
| `is_slot_visible` reads a freed heap page | Same CoW staleness issue as before; mitigated by `ctx.invalidate_all()` on DML with root changes (already in place from Phase 6.7) |
