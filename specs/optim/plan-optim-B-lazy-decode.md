# Plan: Lazy Column Decode (optim-B)

## Files to create/modify

| File | Action | What changes |
|---|---|---|
| `crates/axiomdb-types/src/codec.rs` | modify | Add `pub fn decode_row_masked()` after `decode_row`. Add 7 unit tests. |
| `crates/axiomdb-sql/src/table.rs` | modify | Add `column_mask: Option<&[bool]>` to `scan_table`. Dispatch to `decode_row_masked` when `Some`. |
| `crates/axiomdb-sql/src/executor.rs` | modify | Add `fn build_column_mask()` + `fn collect_column_refs()`. Update `execute_select_ctx` (single-table) and `execute_delete_ctx` (WHERE path). Add `None` to all 10 other `scan_table` call sites. |
| `crates/axiomdb-sql/tests/integration_table.rs` | modify | Add `None` to all `scan_table` call sites in tests. |
| `crates/axiomdb-sql/tests/integration_lazy_decode.rs` | create | 4 integration tests for masked behavior. |

---

## Algorithm for `decode_row_masked`

```text
pub fn decode_row_masked(
    bytes: &[u8],
    schema: &[DataType],
    mask: &[bool],
) -> Result<Vec<Value>, DbError>

1. Guard: mask.len() != schema.len() → Err(DbError::TypeMismatch)
2. blen = bitmap_len(schema.len())
   if bytes.len() < blen → Err(DbError::ParseError)
   bitmap = &bytes[0..blen]
   pos = blen
3. For i in 0..schema.len():
   a. is_null = is_null_bit(bitmap, i)
   b. want    = mask[i]

   if is_null:
       // NULL has no wire bytes — pos unchanged regardless of want
       values.push(Value::Null)
       continue

   if want:
       // Decode normally (identical to decode_row)
       match schema[i] { ... same arms ... }
       values.push(decoded_value)
   else:
       // Skip: advance pos without allocating
       match schema[i] {
           Bool               => { ensure_bytes(bytes, pos, 1)?;  pos += 1; }
           Int | Date         => { ensure_bytes(bytes, pos, 4)?;  pos += 4; }
           BigInt | Real | Timestamp
                              => { ensure_bytes(bytes, pos, 8)?;  pos += 8; }
           Decimal            => { ensure_bytes(bytes, pos, 17)?; pos += 17; }
           Uuid               => { ensure_bytes(bytes, pos, 16)?; pos += 16; }
           Text | Bytes       => {
               let len = read_u24(bytes, pos)?;
               pos += 3;
               ensure_bytes(bytes, pos, len)?;
               pos += len;         // skip payload — no copy, no String
           }
       }
       values.push(Value::Null)   // placeholder
4. return Ok(values)
```

**Critical invariant**: `ensure_bytes` is called before every `pos` advance, even for
skipped columns. Corrupt input still returns `DbError::ParseError`.

**NULL + false mask**: when `is_null` is true AND `mask[i]` is false → push `Value::Null`
and do NOT advance `pos`. NULL columns have no wire bytes regardless of mask.

---

## `scan_table` dispatch

```rust
let values = match column_mask {
    None => decode_row(&bytes, &col_types)?,
    Some(mask) => {
        if mask.iter().all(|&b| b) {
            decode_row(&bytes, &col_types)?   // all-true → no mask overhead
        } else {
            decode_row_masked(&bytes, &col_types, mask)?
        }
    }
};
```

---

## `build_column_mask` and `collect_column_refs`

```text
fn build_column_mask(n_cols: usize, exprs: &[&Expr]) -> Vec<bool>:
    mask = vec![false; n_cols]
    for each expr in exprs:
        collect_column_refs(expr, &mut mask)
    return mask

fn collect_column_refs(expr: &Expr, mask: &mut Vec<bool>):
    match expr:
        Expr::Column { col_idx, .. }           → if col_idx < mask.len() { mask[col_idx] = true }
        Expr::Literal | Expr::OuterColumn | Expr::Param
                                                → (no-op)
        Expr::UnaryOp { operand }              → recurse(operand)
        Expr::BinaryOp { left, right }         → recurse(left); recurse(right)
        Expr::IsNull { expr }                  → recurse(expr)
        Expr::Between { expr, low, high }      → recurse(expr); recurse(low); recurse(high)
        Expr::Like { expr, pattern }           → recurse(expr); recurse(pattern)
        Expr::In { expr, list }                → recurse(expr); for e in list: recurse(e)
        Expr::Function { args }                → for a in args: recurse(a)
        Expr::Case { operand, when_thens, else_result }
                                                → recurse opt operand; for (w,t): recurse w, t;
                                                  recurse opt else_result
        Expr::Cast { expr }                    → recurse(expr)
        Expr::Subquery | Expr::InSubquery | Expr::Exists
                                                → (no-op — inner scope, different row)
```

---

## How `execute_select_ctx` uses the mask

```text
// After resolving the table, before scan_table:
let n_cols = resolved.columns.len();

let has_wildcard = stmt.columns.iter().any(|item| matches!(
    item, SelectItem::Wildcard | SelectItem::QualifiedWildcard(_)
));

let column_mask: Option<Vec<bool>> = if has_wildcard {
    None   // SELECT * → decode_row() directly
} else {
    let mut expr_ptrs: Vec<&Expr> = Vec::new();
    for item in &stmt.columns {
        if let SelectItem::Expr { expr, .. } = item { expr_ptrs.push(expr); }
    }
    if let Some(ref wc) = stmt.where_clause { expr_ptrs.push(wc); }
    for ob in &stmt.order_by  { expr_ptrs.push(&ob.expr); }
    for gb in &stmt.group_by  { expr_ptrs.push(gb); }
    if let Some(ref hav) = stmt.having { expr_ptrs.push(hav); }

    let mask = build_column_mask(n_cols, &expr_ptrs);
    if mask.iter().all(|&b| b) { None } else { Some(mask) }
};

let rows = TableEngine::scan_table(
    storage, &resolved.def, &resolved.columns, snap,
    column_mask.as_deref(),
)?;
```

---

## How `execute_delete_ctx` uses the mask

Only the WHERE path — the no-WHERE path already uses `scan_rids_visible` (no decode).

```text
let n_cols = resolved.columns.len();
let mask_vec = if let Some(ref wc) = stmt.where_clause {
    build_column_mask(n_cols, &[wc])
} else {
    unreachable!("no-WHERE exits before this point")
};
let column_mask: Option<&[bool]> =
    if mask_vec.iter().all(|&b| b) { None } else { Some(&mask_vec) };

let rows = TableEngine::scan_table(
    storage, &resolved.def, &schema_cols, snap, column_mask,
)?;
```

---

## Implementation steps (ordered, verifiable)

### Step 1 — `decode_row_masked` in `codec.rs`

Add after `decode_row`. Wire-size table for skip logic:

| DataType | Wire bytes |
|---|---|
| Bool | 1 |
| Int, Date | 4 |
| BigInt, Real, Timestamp | 8 |
| Decimal | 17 |
| Uuid | 16 |
| Text, Bytes | 3 (len prefix) + len |

Checkpoint: `cargo test -p axiomdb-types` passes.

### Step 2 — `scan_table` signature in `table.rs`

Add `column_mask: Option<&[bool]>` as fifth parameter. Update dispatch. No internal callers to fix.

Checkpoint: `cargo check -p axiomdb-sql 2>&1 | grep "scan_table"` — lists all sites to fix.

### Step 3 — Fix all non-target call sites (`None`)

All `scan_table` calls in `executor.rs` NOT in `execute_select_ctx` (single-table) and NOT in `execute_delete_ctx` (WHERE path) get `None` appended. Fix integration test call sites in `integration_table.rs`.

Checkpoint: `cargo test --workspace` passes with all 1165+ tests.

### Step 4 — `build_column_mask` + `collect_column_refs` in `executor.rs`

Add both as module-level private functions. Update `execute_select_ctx` and `execute_delete_ctx`.

Checkpoint: `cargo test --workspace` + `cargo clippy -p axiomdb-sql -- -D warnings`.

### Step 5 — Integration tests

Create `crates/axiomdb-sql/tests/integration_lazy_decode.rs`:
- `test_select_projection_skips_unused_columns`
- `test_delete_with_where_mask`
- `test_select_star_uses_full_decode`
- `test_single_column_select_large_text_table`

Checkpoint: `cargo test --workspace`.

---

## Tests to write

### Unit tests — `codec.rs`

1. `test_masked_all_false_mixed_types` — all-false mask → all `Value::Null`, correct length
2. `test_masked_all_true_matches_decode_row` — all-true mask == `decode_row` output
3. `test_masked_mixed_fixed_and_variable` — select cols 0+2 from `[Int, Text, Int, Text]`
4. `test_masked_null_column_false_mask` — NULL+mask=false → `Value::Null`, pos not advanced
5. `test_masked_null_column_true_mask` — NULL+mask=true → `Value::Null` (same as decode_row)
6. `test_masked_len_mismatch_returns_type_mismatch` — `mask.len() != schema.len()`
7. `test_masked_truncated_skipped_column_returns_parse_error` — truncated bytes for skipped col

### Integration tests — `integration_lazy_decode.rs`

1. `test_select_projection_skips_unused_columns` — 6 columns, SELECT name+age WHERE age>5
2. `test_delete_with_where_mask` — DELETE WHERE id=5 → 1 affected, 9 remaining
3. `test_select_star_uses_full_decode` — SELECT * returns all non-null values
4. `test_single_column_select_large_text_table` — SELECT id FROM t WHERE id=5 (5 large text cols skipped)

---

## Anti-patterns to avoid

- **DO NOT** modify `decode_row()` — unchanged, same signature and behavior.
- **DO NOT** skip `ensure_bytes` for masked-out columns — corrupt input must still error.
- **DO NOT** advance `pos` for NULL columns (bitmap bit set) even when `mask[i] = false`.
- **DO NOT** apply mask to `execute_update_ctx` — UPDATE must decode all columns to re-encode.
- **DO NOT** apply mask to the JOIN path — deferred (composite schema).
- **DO NOT** recurse into `Expr::Subquery` / `Expr::InSubquery` / `Expr::Exists` in `collect_column_refs`.
- **DO NOT** pass all-true mask as `Some(mask)` — use the `all(|&b| b)` check to pass `None`.

---

## Risks

| Risk | Mitigation |
|---|---|
| Off-by-one in `col_idx` guard | `debug_assert!(col_idx < mask.len())` in `collect_column_refs` |
| Borrow order in `execute_select_ctx` (`stmt` moved) | Mask computed from refs → owned `Vec<bool>` → `stmt` fields used normally |
| Aggregates see `Value::Null` for skipped columns | Correct: SQL aggregates ignore NULLs; mask only skips columns not in GROUP BY / SELECT |
| 10+ call sites need `None` mechanically | Compiler catches every missing site after step 2 |
