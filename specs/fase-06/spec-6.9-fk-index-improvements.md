# Spec: FK + Index Improvements (Phase 6.9)

## What to build

Three targeted improvements that resolve deferred items from Phases 6.5/6.6 and 6.3:

1. **PK B-Tree population on INSERT** — every INSERT now maintains the primary key
   B-Tree index, enabling O(log n) FK parent lookup and PK uniqueness enforcement
   via the index.

2. **FK auto-index with composite key** — FK indexes store `(fk_val | RecordId)`
   as the B-Tree key, making every entry globally unique. Enables O(log n)
   RESTRICT violation detection and efficient range scans for CASCADE/SET NULL.

3. **Composite index planner** — `WHERE col1 = v1 AND col2 = v2` uses index
   `(col1, col2)` for a composite equality scan instead of falling back to
   full table scan.

Reference: PostgreSQL treats PK and secondary indexes identically in
`ExecInsertIndexTuples`; InnoDB appends PK columns to secondary index entries
(`row0row.cc`); PostgreSQL's `indxpath.c` matches WHERE clauses column-by-column
to build composite index scan keys.

---

## Task A — PK B-Tree population on INSERT

### Current behavior (gap)
`insert_into_indexes` and `delete_from_indexes` filter `!i.is_primary`.
PK B-Tree indexes are created empty and never populated.
FK parent lookup in `check_fk_child_insert` always falls back to O(n) full scan.

### Target behavior
All indexes — including primary — are maintained on every INSERT/UPDATE/DELETE.
`check_fk_child_insert` uses the PK B-Tree for O(log n) parent existence check.

### Changes

**`index_maintenance.rs`:**
- Remove `filter(|(_, i)| !i.is_primary ...)` from both `insert_into_indexes`
  and `delete_from_indexes`. Primary indexes are now maintained identically to
  secondary indexes.
- Note: for PK indexes, `idx.is_unique = true` → the uniqueness check in
  `insert_into_indexes` also fires. This enforces PK uniqueness at the B-Tree
  level (correct, and actually improves UniqueViolation error messages for PKs
  to show the real index name).

**`fk_enforcement.rs` in `check_fk_child_insert`:**
- Remove the `if is_primary { full scan fallback }` branch.
- After Task A, PK indexes are populated → `BTree::lookup_in` works for all
  index types.
- Bloom filter is now populated for PK indexes → the `bloom.might_exist()`
  shortcut can be re-enabled for all index types.

### Acceptance criteria
- [ ] INSERT maintains PK B-Tree index (key = encoded PK column value, rid = row RecordId)
- [ ] Second INSERT with same PK value → `UniqueViolation` (B-Tree uniqueness check)
- [ ] FK parent lookup uses B-Tree (no more full scan path in `check_fk_child_insert`)
- [ ] Pre-existing PK B-Tree pages from Phase 6.5 (empty, not populated) still open
  correctly (empty B-Tree is valid; next INSERT populates it)

---

## Task B — FK auto-index with composite key `(fk_val | RecordId)`

### Current behavior (gap)
FK auto-indexes are disabled (`fk_index_id = 0`) because non-unique B-Trees
cannot store multiple entries with the same key. FK enforcement uses full table
scans for all operations — RESTRICT, CASCADE, SET NULL.

### Target behavior
FK auto-indexes use composite keys: `encode_index_key(&[fk_val]) ++ encode_rid(rid)`
(10 appended bytes). Every entry is globally unique even with duplicate fk_val.

- **RESTRICT check**: `BTree::range_in(lo, hi)` with prefix bounds → O(log n)
- **CASCADE/SET NULL**: same range scan returns all child RecordIds → O(log n + k)

### Composite key format

```
FK auto-index entry key:
  [encode_index_key(&[fk_val])]   ← variable length, order-preserving
  [page_id: u64 LE]               ← 8 bytes  ┐  encode_rid(rid)
  [slot_id: u16 LE]               ← 2 bytes  ┘  total 10 bytes

FK auto-index entry value (stored RecordId): same rid
```

### Prefix range scan for a given fk_val

To find all children with `fk_col = fk_val`:
```
let prefix = encode_index_key(&[fk_val])?;
let lo = prefix.clone() + [0u8; 10]  // smallest RecordId suffix
let hi = prefix.clone() + [0xFF; 10] // largest RecordId suffix

let children = BTree::range_in(storage, fk_index_root, Some(&lo), Some(&hi))?;
// children: Vec<(RecordId, key_bytes)> — RecordId = child row location
```

### New `IndexDef` field: `is_fk_index: bool`

Stored in the existing `flags` byte as **bit 2** (`0x04`). Backward-compatible:
pre-6.9 rows have bit 2 = 0 → `is_fk_index = false`.

```rust
// Updated flags encoding:
if self.is_unique   { flags |= 0x01; }
if self.is_primary  { flags |= 0x02; }
if self.is_fk_index { flags |= 0x04; }  // NEW Phase 6.9
```

When `is_fk_index = true`, `insert_into_indexes` and `delete_from_indexes`
build the composite key by appending `encode_rid(rid)` to the normal
`encode_index_key` output.

### Changes

**`schema.rs`:**
- Add `is_fk_index: bool` to `IndexDef`
- Update `to_bytes()` to set flag bit 2
- Update `from_bytes()` to read flag bit 2

**`executor.rs` in `persist_fk_constraint`:**
- Re-enable FK auto-index creation (was `fk_index_id = 0`)
- Build the index with composite keys during table scan:
  for each row, key = `encode_index_key(&[fk_val]) ++ encode_rid(rid)`
- Create `IndexDef` with `is_fk_index: true, is_unique: false, is_primary: false`
- Store the resulting `fk_index_id` in `FkDef.fk_index_id` (non-zero now)

**`index_maintenance.rs`:**
- In `insert_into_indexes`: if `idx.is_fk_index`, build composite key and skip
  the uniqueness check (FK indexes are never unique at the fk_val level).
- In `delete_from_indexes`: if `idx.is_fk_index`, build composite key for deletion.

**`fk_enforcement.rs`:**
- `enforce_fk_on_parent_delete` — for RESTRICT: use `BTree::range_in(lo, hi)`
  on the FK index to check existence in O(log n); no fallback to full scan.
- For CASCADE/SET NULL: use same range scan, collect RecordIds, then read rows
  from heap for secondary index maintenance (row values still needed for index keys).

### Acceptance criteria
- [ ] FK auto-index created with composite key format (`is_fk_index = true`)
- [ ] INSERT child row: FK index entry has composite key `fk_val | RecordId`
- [ ] INSERT two rows with same fk_val: both entries in FK index (no DuplicateKey)
- [ ] DELETE parent with RESTRICT: B-Tree range scan used (O(log n), not full scan)
- [ ] DELETE parent with CASCADE: range scan finds all children; all deleted
- [ ] DELETE parent with SET NULL: range scan finds all children; fk_col set to NULL
- [ ] `is_fk_index = false` on pre-6.9 rows (bit 2 = 0 in old bytes)
- [ ] `fk_index_id != 0` after FK creation (auto-index actually created)

---

## Task C — Composite index planner

### Current behavior (gap)
`plan_select` only matches single-column WHERE conditions. `WHERE a = 1 AND b = 2`
with index `(a, b)` falls back to full table scan.

### Target behavior
`WHERE col1 = v1 AND col2 = v2` uses index `(col1, col2)` with a composite
equality range scan. `WHERE col1 = v1 AND col2 = v2 AND col3 > v3` uses the
index for the equality prefix and scans from `(v1, v2, v3)` onwards.

### Algorithm (based on PostgreSQL `indxpath.c` column-by-column approach)

```
plan_composite(where, indexes, columns):
  eq_conditions = collect_eq_conditions(where)
  // eq_conditions: Vec<(col_name, Value)>
  // Only atomic `col = literal` or `literal = col` clauses from AND-tree

  for each index in indexes (non-primary, ≥ 2 columns, no partial predicate):
    key_parts: Vec<Value> = []

    for each idx_col in index.columns (in order):
      col_name = columns[idx_col.col_idx].name
      if eq_conditions.contains(col_name):
        key_parts.push(eq_conditions[col_name])
      else:
        break  // stop at first unmatched column

    if key_parts.len() >= 2:  // matched at least 2 columns
      composite_key = encode_index_key(&key_parts)?
      // Use IndexRange lo=hi=composite_key (handles both unique and non-unique)
      return IndexRange { index_def: index, lo: Some(composite_key), hi: Some(composite_key) }

  return None  // no composite match found
```

**Why `IndexRange` not `IndexLookup` for composite?**
For a non-unique composite index `(user_id, status)`, multiple rows may match
`user_id = 5 AND status = 'active'`. `IndexLookup` only returns one row via
`BTree::lookup_in`. `IndexRange { lo: Some(key), hi: Some(key) }` returns all
matching rows via `BTree::range_in`. Both unique and non-unique indexes work
correctly with this approach.

### `collect_eq_conditions` helper

```rust
/// Collects all top-level `col = literal` conditions from an AND-tree.
/// Returns a map from column name to the literal value.
fn collect_eq_conditions(expr: &Expr) -> Vec<(&str, Value)>
```

Recursively decomposes `BinaryOp(And, left, right)` into leaves, then applies
`extract_eq_col_literal` to each leaf.

### Rule ordering in `plan_select`

```
Rule 0 (NEW): composite eq via AND → IndexRange lo=hi
Rule 1 (unchanged): single col eq → IndexLookup
Rule 2 (unchanged): range → IndexRange
Rule 3: Scan (fallback)
```

Rule 0 runs first. If it finds a composite match, it takes precedence over Rule 1
(which would only use the first column). If no composite match, Rules 1 and 2
apply as before.

**Partial index constraint:** composite lookup only uses indexes where
`idx.predicate.is_none()` OR where the composite predicate is implied by the
query WHERE. For Phase 6.9, skip partial indexes in composite rule (conservative).

### Acceptance criteria
- [ ] `WHERE a = 1 AND b = 2` uses index `(a, b)` → IndexRange with composite key
- [ ] `WHERE b = 2 AND a = 1` (reversed order) uses index `(a, b)` → same (order-invariant)
- [ ] `WHERE a = 1` with index `(a, b)` → IndexLookup (Rule 1, unchanged)
- [ ] `WHERE a = 1 AND c = 3` with index `(a, b)` → Scan (b is unmatched prefix breaker)
  Wait — actually: Rule 0 tries to match `a = 1` (col 0 ✅) then `b` (not in WHERE ❌)
  → key_parts = [1] → only 1 column → Rule 0 does NOT activate → falls to Rule 1 → `a=1` IndexLookup ✅
- [ ] `WHERE a = 1 AND b = 2 AND c = 3` with index `(a, b, c)` → composite key with 3 parts
- [ ] Multiple rows returned for non-unique composite index match
- [ ] Partial indexes skipped in composite rule

---

## Use cases

### 1. PK FK validation (O(log n) after Task A)
```sql
CREATE TABLE users (id INT PRIMARY KEY);
CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id));
INSERT INTO users VALUES (1);
INSERT INTO orders VALUES (10, 1);    -- ✅ B-Tree lookup: users PK index has id=1
INSERT INTO orders VALUES (20, 999);  -- ❌ B-Tree lookup: id=999 absent
```

### 2. FK cascade via composite index (Task B)
```sql
-- 1000 orders for user 1
DELETE FROM users WHERE id = 1;  -- CASCADE: range_in finds all 1000 orders in O(log n + 1000)
```

### 3. Composite index planner (Task C)
```sql
CREATE INDEX idx ON orders (user_id, status);
SELECT * FROM orders WHERE user_id = 5 AND status = 'active';
-- Plan: IndexRange { lo: encode(5, 'active'), hi: encode(5, 'active') }
-- Reads only matching rows, not entire table
```

---

## Out of scope (Phase 6.10+)

- ON UPDATE CASCADE / SET NULL → Phase 6.10
- Composite FKs (multi-column REFERENCES) → Phase 6.10
- Composite index range scan: `WHERE a = 1 AND b > 5` → Phase 6.10
- Bloom filter for FK composite index → Phase 6.10

## ⚠️ DEFERRED
- ON UPDATE CASCADE/SET NULL → Phase 6.10
- Composite FKs → Phase 6.10
- Range suffix in composite index: `WHERE a = 1 AND b > 5` → Phase 6.10

---

## Dependencies

- Phase 6.1–6.3: `IndexDef`, `BTree::insert_in`, `BTree::range_in`, `encode_index_key`
- Phase 6.4: `BloomRegistry` — now populated for PK indexes after Task A
- Phase 6.5/6.6: FK enforcement infrastructure (`fk_enforcement.rs`, `FkDef`)
- `encode_rid(rid) -> [u8; 10]` — exists in `axiomdb-index/src/page_layout.rs:335`
