# Spec: Index-Only Scans / Covering Indexes (Phase 6.13)

## What to build

An **index-only scan** reads all needed column values directly from the B-Tree
index entry instead of fetching the full heap row. This eliminates the most
expensive part of an index scan: decoding the heap row (which may involve
many columns and variable-length TEXT fields).

Reference: PostgreSQL `check_index_only` (`indxpath.c:2227`) detects plan-time
coverage; `nodeIndexonlyscan.c` handles execution with visibility map. InnoDB
appends PK columns to every secondary index leaf entry.

---

## Scope

### In scope (Phase 6.13)

1. **`decode_index_key`** — reverse of `encode_index_key`: deserialize encoded
   key bytes back to `Vec<Value>` without knowing the original types (the type
   tag in each byte prefix makes the encoding self-delimiting).

2. **`INCLUDE (col1, col2)` syntax** — parsed and stored in `IndexDef.include_columns`
   for catalog persistence. NOT stored in B-Tree leaves yet (deferred — requires
   B-Tree architecture change to variable-length entries).

3. **Key-column coverage detection** — at plan time, check if all columns needed
   by the query (SELECT list + WHERE columns not already used by the index key)
   are covered by the index **key columns**. If yes → use index-only scan path.

4. **Index-only executor path** — when covered:
   - Decode `Vec<Value>` from the B-Tree key bytes (using `decode_index_key`)
   - Still verify MVCC visibility via the heap slot header (read header only —
     NOT the full row encoding)
   - Return the decoded key values directly, skipping full heap row decode

### ⚠️ DEFERRED

- **INCLUDE column storage in B-Tree leaves** → Phase 6.15
  Requires rearchitecting `LeafNodePage` from fixed `[u8; 64]` key slots to
  variable-length entries (same scope as MVCC on secondary indexes, Phase 6.14).
  Until then, INCLUDE columns are catalog metadata only.

---

## `decode_index_key` — the core primitive

The encoding in `key_encoding.rs` is **self-delimiting**: each value starts with
a 1-byte type tag that determines how many bytes follow.

```rust
/// Decodes N values from an encoded key byte slice.
///
/// The encoding is self-delimiting — each value starts with a type tag:
/// 0x00=Null, 0x01=Bool(1B), 0x02=Int(8B), 0x03=BigInt(8B), 0x04=Real(8B),
/// 0x05=Decimal(17B), 0x06=Date(8B), 0x07=Timestamp(8B),
/// 0x08=Text(NUL-term), 0x09=Bytes(NUL-term), 0x0A=Uuid(16B)
///
/// Returns `Ok((values, bytes_consumed))`.
pub fn decode_index_key(
    key: &[u8],
    n_values: usize,
) -> Result<(Vec<Value>, usize), DbError>
```

Decode logic per tag:
- `0x00` → `Value::Null`; consumed = 1
- `0x01` → `Value::Bool(byte != 0)`; consumed = 2
- `0x02` → `Value::Int(...)`: un-sign-flip 8 BE bytes → i64 → i32; consumed = 9
- `0x03` → `Value::BigInt(...)`: un-sign-flip 8 BE bytes → i64; consumed = 9
- `0x04` → `Value::Real(...)`: reverse `encode_f64` → f64; consumed = 9
- `0x05` → `Value::Decimal(...)`: scale byte + un-sign-flip 16 BE bytes → i128; consumed = 18
- `0x06` → `Value::Date(...)`: un-sign-flip 8 BE bytes → i32; consumed = 9
- `0x07` → `Value::Timestamp(...)`: un-sign-flip 8 BE bytes → i64; consumed = 9
- `0x08` → `Value::Text(...)`: NUL-terminated with 0xFF escape; consumed = len+1
- `0x09` → `Value::Bytes(...)`: NUL-terminated with 0xFF escape; consumed = len+1
- `0x0A` → `Value::Uuid([u8;16])`: 16 raw bytes; consumed = 17

**Roundtrip guarantee:** `decode_index_key(encode_index_key(&values), values.len())` == `values`
(excluding NaN floats which encode to 0 and cannot round-trip).

---

## Coverage check

```rust
/// Returns `true` if `index` covers all columns needed to answer the query.
///
/// "Needed" = SELECT columns + WHERE columns not already encoded in the key.
/// For Phase 6.13, only KEY columns count (INCLUDE columns are not yet stored
/// in B-Tree leaves). True INCLUDE coverage is Phase 6.15.
pub fn index_covers_query(
    index: &IndexDef,
    select_col_idxs: &[u16],   // columns in SELECT output
    where_col_idxs: &[u16],    // columns referenced in WHERE (beyond the key)
) -> bool {
    let key_cols: HashSet<u16> = index.columns.iter().map(|c| c.col_idx).collect();
    select_col_idxs.iter().chain(where_col_idxs.iter())
        .all(|col| key_cols.contains(col))
}
```

---

## MVCC check (required even for index-only scans)

A B-Tree key entry has a `RecordId`. The heap slot at that RecordId holds the
MVCC visibility fields (`txn_id_created`, `txn_id_deleted`). We must check
these for correctness.

**Optimization:** read ONLY the slot header (first `TUPLE_HEADER_SIZE` bytes),
NOT the full row payload. This is much cheaper than `decode_row`.

```rust
/// Checks if the heap slot at `rid` is visible to `snapshot` without
/// decoding the full row. Returns `true` if visible.
pub fn is_visible_quick(
    storage: &dyn StorageEngine,
    rid: RecordId,
    snapshot: TransactionSnapshot,
) -> Result<bool, DbError>
```

Internal: reads the heap page, checks the slot header's `txn_id_created` and
`txn_id_deleted` fields against `snapshot.snapshot_id`. Returns false if the
slot has been deleted by a committed transaction visible to the snapshot.

---

## New `AccessMethod` variant

```rust
/// Index-only scan: all needed columns are in the index key.
/// Values are decoded from key bytes; heap is checked for MVCC only.
IndexOnlyScan {
    index_def: IndexDef,
    /// Pre-encoded key (same as IndexLookup/IndexRange).
    key_lo: Vec<u8>,
    key_hi: Option<Vec<u8>>,   // None = point lookup (lo = hi)
    /// Column indices to extract from the decoded key (in key column order).
    key_col_positions: Vec<usize>,  // positions within index.columns for each needed col
}
```

---

## `IndexDef.include_columns` (catalog-only, Phase 6.13)

```rust
pub struct IndexDef {
    // ... existing fields ...
    /// Columns stored as extra data in the index leaf (Phase 6.13 catalog;
    /// B-Tree storage planned for Phase 6.15).
    /// `col_idx` values reference the table's column list.
    pub include_columns: Vec<u16>,
}
```

On-disk: appended after `fillfactor` byte:
```
[is_fk_index: 1 byte]   ← already present
[include_len: 1 byte]   ← NEW: number of INCLUDE columns (0 = none)
[include_col_idxs: include_len × 2 bytes LE]  ← NEW
```

Backward-compatible: old rows end before this section → `include_columns = vec![]`.

---

## Parser: `INCLUDE (col1, col2)` syntax

```sql
CREATE INDEX idx_user_name ON users(user_id) INCLUDE (name, email);
CREATE INDEX idx_email ON users(email) INCLUDE (name);
```

Added after the column list but before `WHERE` and `WITH`:
```
CREATE [UNIQUE] INDEX ... ON table(cols)
  [INCLUDE (col1, col2, ...)]
  [WHERE predicate]
  [WITH (fillfactor=N)]
```

---

## Planner integration

In `plan_select`, before returning `IndexLookup` or `IndexRange`, check if all
SELECT columns (and any remaining WHERE columns not in the key) are in the key:

```rust
if index_covers_query(idx, &select_col_idxs, &remaining_where_cols) {
    return AccessMethod::IndexOnlyScan {
        index_def: idx.clone(),
        key_lo: key.clone(),
        key_hi: None,          // for IndexLookup
        key_col_positions: ...,
    };
}
```

`select_col_idxs` comes from analyzing the SELECT target list. The planner
receives this from the executor as an additional parameter (added to `plan_select`).

---

## Executor integration

When `AccessMethod::IndexOnlyScan` is chosen:

```
1. BTree::range_in(fk_index_root, Some(&lo), hi.as_deref())
   → Vec<(RecordId, key_bytes)>

2. For each (rid, key_bytes):
   a. is_visible_quick(storage, rid, snapshot)?
      → false: skip (row deleted/not yet committed)
   b. decode_index_key(&key_bytes, index.columns.len())
      → Vec<Value> (full key values)
   c. Project to needed columns via key_col_positions
   d. Apply remaining WHERE conditions (if any) to projected values
   e. Emit row

3. Return QueryResult::Rows with decoded values
```

The existing `IndexLookup` and `IndexRange` paths are **unchanged** — they still
read the full heap row. `IndexOnlyScan` is a new optimized path.

---

## Use cases

### 1. Single-column equality (most common)
```sql
CREATE UNIQUE INDEX uq_email ON users(email);
SELECT email FROM users WHERE email = 'alice@x.com';
-- Coverage: SELECT has [email], WHERE has [email] → both in key ✅
-- Plan: IndexOnlyScan — decode email from key bytes, skip heap row decode
```

### 2. Composite key, all columns needed
```sql
CREATE INDEX idx_status_user ON orders(user_id, status);
SELECT user_id, status FROM orders WHERE user_id = 5 AND status = 'active';
-- Coverage: SELECT has [user_id, status], both in composite key ✅
-- Plan: IndexOnlyScan — decode both columns from key
```

### 3. Partial coverage — falls back to IndexLookup
```sql
CREATE INDEX idx_user ON orders(user_id);
SELECT user_id, total FROM orders WHERE user_id = 5;
-- Coverage: SELECT has [user_id, total]. `total` NOT in key ❌
-- Plan: IndexLookup (existing path) — still reads heap row
```

### 4. INCLUDE column (catalog-only in 6.13, effective in 6.15)
```sql
CREATE INDEX idx_user_cover ON orders(user_id) INCLUDE (total);
-- Phase 6.13: INCLUDE stored in catalog, NOT in B-Tree leaf
-- Plan: falls back to IndexLookup (total not in key)
-- Phase 6.15: IndexOnlyScan with total decoded from leaf data
```

### 5. COUNT with index key
```sql
SELECT COUNT(*) FROM orders WHERE user_id = 5;
-- Coverage: COUNT(*) needs no columns → trivially covered ✅
-- Plan: IndexOnlyScan — just count RIDs, MVCC check only
```

---

## Acceptance criteria

- [ ] `decode_index_key(encode_index_key(&values), n)` roundtrips correctly for all Value types
- [ ] `INCLUDE (cols)` syntax parses and persists `include_columns` in catalog
- [ ] Pre-6.13 indexes open without error (`include_columns = vec![]`)
- [ ] `index_covers_query` returns true when all SELECT cols are in index key cols
- [ ] `index_covers_query` returns false when any SELECT col is missing from key
- [ ] `is_visible_quick` correctly skips deleted/invisible rows
- [ ] `IndexOnlyScan` path decodes correct values for all Value types
- [ ] `IndexOnlyScan` respects MVCC — deleted rows not returned
- [ ] `IndexOnlyScan` returns same results as `IndexLookup` for same query
- [ ] `IndexLookup`/`IndexRange` behavior unchanged (zero regression)
- [ ] `COUNT(*)` query on indexed table uses IndexOnlyScan
- [ ] Integration tests for all acceptance criteria

---

## Out of scope

- INCLUDE column storage in B-Tree leaves → Phase 6.15
- All-visible bitmap per page (PG visibility map optimization) → Phase 7+
- Covering indexes for JOIN conditions → Phase 6.15
- Statistics integration (NDV for index-only scan cost) → Phase 6.15

---

## Dependencies

- Phase 6.1–6.3: `IndexDef`, `encode_index_key`, `BTree::range_in`
- Phase 6.7: `IndexDef.predicate` (same backward-compat extension pattern)
- Phase 6.8: `IndexDef.fillfactor` (same)
- `key_encoding.rs`: `encode_value` logic (to write the inverse `decode_value`)
- `HeapChain::read_row` (to adapt for header-only visibility check)
