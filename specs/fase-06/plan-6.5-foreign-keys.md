# Plan: Foreign Key Constraints (Phase 6.5 + 6.6)

## Files to create / modify

### Create
- `crates/axiomdb-sql/src/fk_enforcement.rs` — FK check/cascade functions (keeps executor.rs clean)
- `crates/axiomdb-sql/tests/integration_fk.rs` — all FK integration tests

### Modify
- `crates/axiomdb-storage/src/meta.rs` — two new meta page offsets
- `crates/axiomdb-catalog/src/bootstrap.rs` — `CatalogPageIds.foreign_keys` + `ensure_fk_root()`
- `crates/axiomdb-catalog/src/schema.rs` — `FkDef` struct + `FkAction` enum + serde
- `crates/axiomdb-catalog/src/writer.rs` — `alloc_fk_id`, `create_foreign_key`, `drop_foreign_key`
- `crates/axiomdb-catalog/src/reader.rs` — `list_fk_constraints`, `list_fk_constraints_referencing`
- `crates/axiomdb-catalog/src/lib.rs` — re-export `FkDef`, `FkAction`
- `crates/axiomdb-catalog/src/resolver.rs` — add `foreign_keys: Vec<FkDef>` to `ResolvedTable`
- `crates/axiomdb-core/src/error.rs` — four new error variants + SQLSTATE mappings
- `crates/axiomdb-sql/src/executor.rs` — DDL + DML FK integration
- `crates/axiomdb-sql/src/lib.rs` — `pub mod fk_enforcement;`

---

## Algorithm / Data structure

### FkDef binary layout (26 bytes fixed + variable name)

```
offset  size  field
     0     4  fk_id:          u32 LE
     4     4  child_table_id: u32 LE
     8     2  child_col_idx:  u16 LE
    10     4  parent_table_id:u32 LE
    14     2  parent_col_idx: u16 LE
    16     1  on_delete:      u8  (FkAction)
    17     1  on_update:      u8  (FkAction)
    18     4  fk_index_id:    u32 LE (0 = user-provided index, not auto-created)
    22     4  name_len:       u32 LE
    26  name_len  name: UTF-8 bytes
```

`FkAction` encoding: 0=NoAction, 1=Restrict, 2=Cascade, 3=SetNull, 4=SetDefault

### FK enforcement algorithm — INSERT child

```
fn check_fk_child_insert(row, foreign_keys, storage, txn, bloom):
  if foreign_keys.is_empty() → return Ok  // fast path

  snap = txn.active_snapshot()
  reader = CatalogReader::new(storage, snap)

  for fk in foreign_keys:
    val = row[fk.child_col_idx]
    if val == Null → continue           // NULL exemption

    key = encode_index_key(&[val])?

    // Find parent's PK or UNIQUE index covering fk.parent_col_idx
    parent_indexes = reader.list_indexes(fk.parent_table_id)?
    parent_idx = parent_indexes
      .iter()
      .find(|i| (i.is_primary || i.is_unique)
             && i.columns.len() == 1
             && i.columns[0].col_idx == fk.parent_col_idx)
      .ok_or(ForeignKeyNoParentIndex)?

    // Bloom shortcut (parent index bloom)
    if !bloom.might_exist(parent_idx.index_id, &key):
      return Err(ForeignKeyViolation { table, column, value })

    // BTree lookup
    match BTree::lookup_in(storage, parent_idx.root_page_id, &key)?:
      Some(_) → continue                // parent exists, OK
      None    → return Err(ForeignKeyViolation { .. })
```

### FK enforcement algorithm — DELETE parent (with CASCADE/SET NULL)

```
fn enforce_fk_on_parent_delete(
    deleted_rows: &[(RecordId, Vec<Value>)],
    parent_table_id,
    storage, txn, bloom, depth,
) -> Result<(), DbError>:

  if depth > 10 → return Err(ForeignKeyCascadeDepth { limit: 10 })

  snap = txn.active_snapshot()
  reader = CatalogReader::new(storage, snap)
  fk_list = reader.list_fk_constraints_referencing(parent_table_id)?
  if fk_list.is_empty() → return Ok    // no FKs reference this table

  for fk in fk_list:
    child_def = reader.get_table_by_id(fk.child_table_id)?
    child_cols = reader.list_columns(fk.child_table_id)?
    child_indexes = reader.list_indexes(fk.child_table_id)?

    // Find the FK index on the child table (for O(log n) reverse lookup)
    fk_child_idx = child_indexes.iter()
      .find(|i| i.index_id == fk.fk_index_id
             || (i.columns.len() == 1 && i.columns[0].col_idx == fk.child_col_idx))
    // If no FK index: fall back to full scan of child table

    for (_, parent_row) in deleted_rows:
      parent_key_val = parent_row[fk.parent_col_idx]
      if parent_key_val == Null → continue  // no FK references NULL

      parent_key = encode_index_key(&[parent_key_val])?

      // Find child rows referencing this parent key
      child_rids: Vec<(RecordId, Vec<Value>)> = match fk_child_idx:
        Some(idx) → btree_range_lookup(storage, idx.root_page_id, &parent_key, child_cols, snap)
        None      → full_scan_filter(storage, child_def, child_cols, snap, fk.child_col_idx, parent_key_val)

      if child_rids.is_empty() → continue

      match fk.on_delete:
        NoAction | Restrict →
          return Err(ForeignKeyParentViolation { constraint: fk.name, .. })

        Cascade →
          // Delete children BEFORE parent (recursive)
          TableEngine::delete_rows_batch(storage, txn, child_def, child_rid_list)?
          delete_from_indexes(child_indexes, child_rows, storage, bloom)?
          // Recurse: children of children
          enforce_fk_on_parent_delete(&child_rows, fk.child_table_id, storage, txn, bloom, depth+1)?

        SetNull →
          check: child_cols[fk.child_col_idx].nullable == true
            else return Err(ForeignKeySetNullNotNullable)
          for (child_rid, mut child_row) in child_rows:
            child_row[fk.child_col_idx] = Value::Null
            new_rid = TableEngine::update_row(storage, txn, child_def, child_cols, child_rid, child_row)?
            // index maintenance: delete old FK key, no insert (NULL not indexed)
            delete_from_indexes(child_indexes, old_child_row, storage, bloom)?
            bloom.mark_dirty(fk_child_idx.index_id)

        SetDefault → Err(NotImplemented)
```

### Auto-index creation for FK column

```
fn create_fk_index_if_needed(
    child_table_def, child_col_idx, fk_name,
    storage, txn, bloom,
) → Result<u32 /* index_id, 0 if reused */, DbError>:

  // Check if an index already covers child_col_idx as leading column
  existing_indexes = CatalogReader::new(storage, snap).list_indexes(child_table_def.id)?
  if let Some(idx) = existing_indexes.iter().find(|i|
      !i.columns.is_empty() && i.columns[0].col_idx == child_col_idx):
    return Ok(0)  // reuse existing, fk_index_id = 0

  // None found → create index named "_fk_{fk_name}"
  // (same flow as execute_create_index but called inline)
  index_name = format!("_fk_{fk_name}")
  root_page_id = storage.alloc_page(PageType::Index)?
  // ... initialize leaf page ...
  let root_pid = AtomicU64::new(root_page_id)

  // Scan child table, insert into B-Tree
  rows = TableEngine::scan_table(storage, child_table_def, child_cols, snap, None)?
  for (rid, row_vals) in rows:
    val = row_vals[child_col_idx]
    if val == Null → continue
    key = encode_index_key(&[val])?
    BTree::insert_in(storage, &root_pid, &key, rid)?
    bloom.add(new_index_id, &key)  // populated after persist

  final_root = root_pid.load(Acquire)
  index_def = IndexDef { name: index_name, root_page_id: final_root,
                          is_unique: false, is_primary: false,
                          columns: [IndexColumnDef { col_idx: child_col_idx, order: Asc }] }
  new_index_id = CatalogWriter::new(storage, txn)?.create_index(index_def)?
  bloom.create(new_index_id, rows.len().max(1))
  // bloom.add() calls done during scan above (but index_id not known yet —
  // solution: collect bloom_keys during scan, add after persist)

  return Ok(new_index_id)
```

---

## Implementation phases

### Phase 1 — Catalog foundation

**Step 1.1** — `crates/axiomdb-storage/src/meta.rs`:
```rust
pub const CATALOG_FOREIGN_KEYS_ROOT_BODY_OFFSET: usize = 84;
pub const NEXT_FK_ID_BODY_OFFSET: usize = 92;
```
Add to imports everywhere that reads/writes meta.

**Step 1.2** — `crates/axiomdb-catalog/src/bootstrap.rs`:
- Add `pub foreign_keys: u64` to `CatalogPageIds`
- Update `page_ids()` to read offset 84
- Add `ensure_fk_root(storage) → Result<u64, DbError>` (same pattern as `ensure_constraints_root`)

**Step 1.3** — `crates/axiomdb-catalog/src/schema.rs`:
- Add `FkAction` enum with `#[repr(u8)]` and `TryFrom<u8>`
- Add `FkDef` struct with `to_bytes()` / `from_bytes()` methods
- Both must be `#[derive(Debug, Clone, PartialEq, Eq)]`

**Step 1.4** — `crates/axiomdb-catalog/src/writer.rs`:
- Add `pub const SYSTEM_TABLE_FOREIGN_KEYS: u32 = u32::MAX - 4;`
- Add `fn alloc_fk_id(storage: &mut dyn StorageEngine) → Result<u32, DbError>` (reads/increments NEXT_FK_ID_BODY_OFFSET)
- Add `pub fn create_foreign_key(&mut self, def: FkDef) → Result<u32, DbError>`
- Add `pub fn drop_foreign_key(&mut self, fk_id: u32) → Result<(), DbError>`

**Step 1.5** — `crates/axiomdb-catalog/src/reader.rs`:
- Add `pub fn list_fk_constraints(&mut self, table_id: u32) → Result<Vec<FkDef>, DbError>`
- Add `pub fn list_fk_constraints_referencing(&mut self, parent_table_id: u32) → Result<Vec<FkDef>, DbError>`
- Both: if `page_ids.foreign_keys == 0` → return `Ok(vec![])` (pre-6.5 DB)

**Step 1.6** — `crates/axiomdb-catalog/src/resolver.rs`:
- Add `pub foreign_keys: Vec<FkDef>` to `ResolvedTable`
- In `resolve_table()`: add `let foreign_keys = self.reader.list_fk_constraints(def.id)?;`

**Step 1.7** — `crates/axiomdb-core/src/error.rs`:
Add variants and SQLSTATE mappings:
```rust
ForeignKeyParentViolation { constraint: String, child_table: String, child_column: String }
  → "23503"
ForeignKeyCascadeDepth { limit: u32 }
  → "23503"
ForeignKeySetNullNotNullable { table: String, column: String }
  → "23000"
ForeignKeyNoParentIndex { table: String, column: String }
  → "42830"
```

**Step 1.8** — `crates/axiomdb-catalog/src/lib.rs`:
- Re-export `FkDef`, `FkAction`

**Verify:** `cargo test -p axiomdb-catalog` passes. `cargo test -p axiomdb-core` passes.

---

### Phase 2 — DDL: CREATE TABLE, ALTER TABLE

**Step 2.1** — `execute_create_table` in `executor.rs`:

After the column-creation loop, add FK processing:

```rust
// Collect FKs from inline column constraints
let mut fk_specs: Vec<FkSpec> = Vec::new();
for (col_idx, col_def) in stmt.columns.iter().enumerate() {
    for constraint in &col_def.constraints {
        if let ColumnConstraint::References { table, column, on_delete, on_update } = constraint {
            fk_specs.push(FkSpec {
                name: None,  // auto-named
                child_col_idx: col_idx as u16,
                ref_table: table.clone(),
                ref_column: column.clone(),
                on_delete: *on_delete,
                on_update: *on_update,
            });
        }
    }
}
// Collect FKs from table constraints
for tc in &stmt.table_constraints {
    if let TableConstraint::ForeignKey { name, columns, ref_table, ref_columns, on_delete, on_update } = tc {
        // single-column only (spec scope)
        if columns.len() != 1 {
            return Err(DbError::NotImplemented { feature: "composite FK — Phase 6.9".into() });
        }
        fk_specs.push(FkSpec { name: name.clone(), ... });
    }
}
// Process each FK spec
for spec in fk_specs {
    persist_fk_constraint(spec, table_id, &schema_cols, storage, txn, bloom)?;
}
```

`persist_fk_constraint` (new private fn):
1. Resolve parent table (use a fresh `make_resolver`)
2. Find parent col by name (or PK if no name given)
3. Verify parent index exists for parent col
4. Check type compatibility
5. Auto-name if needed: `format!("fk_{table_name}_{child_col_name}_{ref_table}")`
6. Check name uniqueness (scan existing FKs for this table)
7. `create_fk_index_if_needed(...)` → get `fk_index_id`
8. `writer.create_foreign_key(FkDef { ... })?`

**Step 2.2** — `alter_add_constraint` in `executor.rs`:

Replace the `TableConstraint::ForeignKey { .. } => Err(NotImplemented)` stub with:
1. Same `persist_fk_constraint` call as CREATE TABLE
2. After persisting, validate existing data:
   ```rust
   let snap = txn.active_snapshot()?;
   let rows = TableEngine::scan_table(storage, &child_def, &child_cols, snap, None)?;
   for (_, row) in &rows {
       check_fk_child_insert(row, &[new_fk_def], storage, txn, bloom)?;
   }
   // If any error: drop newly created FK + its auto-index, return error
   ```

**Step 2.3** — `alter_drop_constraint` in `executor.rs`:

After existing CHECK constraint drop logic, add FK branch:
```rust
// Check if it's a FK constraint
let fk_list = reader.list_fk_constraints(table_def.def.id)?;
if let Some(fk) = fk_list.iter().find(|f| f.name == constraint_name) {
    let fk_index_id = fk.fk_index_id;
    let fk_id = fk.fk_id;
    writer.drop_foreign_key(fk_id)?;
    if fk_index_id != 0 {
        execute_drop_index_by_id(fk_index_id, storage, txn, bloom)?;
    }
    ctx.invalidate_all();
    return Ok(QueryResult::Empty);
}
```

**Verify:** `cargo test -p axiomdb-sql -- integration_executor` passes.

---

### Phase 3 — DML INSERT / UPDATE child validation

**Step 3.1** — Create `crates/axiomdb-sql/src/fk_enforcement.rs`:

```rust
pub fn check_fk_child_insert(
    row: &[Value],
    foreign_keys: &[FkDef],
    storage: &mut dyn StorageEngine,
    txn: &TxnManager,
    bloom: &mut BloomRegistry,
) -> Result<(), DbError>

pub fn check_fk_child_update(
    old_row: &[Value],
    new_row: &[Value],
    foreign_keys: &[FkDef],
    storage: &mut dyn StorageEngine,
    txn: &TxnManager,
    bloom: &mut BloomRegistry,
) -> Result<(), DbError>
// Only calls check_fk_child_insert for FKs where new_row[col] != old_row[col]
```

Both cache the parent index lookup by table_id to avoid re-reading the catalog
for multi-row INSERTs into the same table.

**Step 3.2** — Wire into `execute_insert_ctx` (`executor.rs`):

In the VALUES loop, before `TableEngine::insert_row`:
```rust
if !resolved.foreign_keys.is_empty() {
    fk_enforcement::check_fk_child_insert(
        &full_values, &resolved.foreign_keys, storage, txn, bloom,
    )?;
}
```

Same in the INSERT SELECT row loop, and in `execute_insert` (non-ctx path).

**Step 3.3** — Wire into `execute_update_ctx` (`executor.rs`):

In the per-row section (secondary indexes path), after building `new_values`,
before `TableEngine::update_row`:
```rust
if !resolved.foreign_keys.is_empty() {
    fk_enforcement::check_fk_child_update(
        &old_values, &new_values, &resolved.foreign_keys, storage, txn, bloom,
    )?;
}
```

Same in `execute_update` (non-ctx path). In the batch path (no secondary indexes),
we still need FK check: validate before building `heap_updates`.

**Verify:** `cargo test -p axiomdb-sql -- fk` — INSERT tests pass.

---

### Phase 4 — DML DELETE / UPDATE parent enforcement

**Step 4.1** — Add to `fk_enforcement.rs`:

```rust
/// Enforce FK constraints when rows are deleted from `parent_table_id`.
/// Called BEFORE the parent rows are deleted from the heap.
/// For CASCADE: also deletes child rows (returns nothing — side effects in storage).
/// For SET NULL: updates child rows.
/// For RESTRICT/NO ACTION: returns error if children exist.
pub fn enforce_fk_on_parent_delete(
    deleted_rows: &[(RecordId, Vec<Value>)],
    parent_table_id: u32,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    depth: u32,
) -> Result<(), DbError>

/// Enforce FK constraints when parent key columns are updated.
/// Only RESTRICT/NO ACTION: returns error if children exist and key changed.
pub fn enforce_fk_on_parent_update(
    old_rows: &[(RecordId, Vec<Value>)],
    new_rows: &[Vec<Value>],
    parent_table_id: u32,
    storage: &mut dyn StorageEngine,
    txn: &TxnManager,
) -> Result<(), DbError>
```

**Step 4.2** — Wire into `execute_delete_ctx` and `execute_delete`:

After collecting `to_delete` but BEFORE `TableEngine::delete_rows_batch`:
```rust
fk_enforcement::enforce_fk_on_parent_delete(
    &to_delete, resolved.def.id, storage, txn, bloom, 0,
)?;
```

For the no-WHERE fast path in `execute_delete_ctx`:
- Keep the fast path only when `secondary_indexes.is_empty()` AND no FKs reference this table.
- To check: `CatalogReader::list_fk_constraints_referencing(table_id)?.is_empty()`
- If FKs exist: fall through to full scan path (same logic as secondary indexes).

**Step 4.3** — Wire into `execute_update_ctx` and `execute_update`:

After collecting `to_update` but before applying changes:
```rust
// Check if any FK-referenced column is being updated
// (only matters if this table is a parent — check by loading referencing FKs)
fk_enforcement::enforce_fk_on_parent_update(
    &old_new_pairs, resolved.def.id, storage, txn,
)?;
```

**Verify:** All integration tests pass.

---

## Tests to write

### Unit tests (in `bloom.rs`, `schema.rs`, `writer.rs`, `reader.rs`)

```rust
// schema.rs
fn test_fk_def_roundtrip() — to_bytes() / from_bytes() for every FkAction variant
fn test_fk_action_encoding() — every u8 value round-trips through TryFrom<u8>

// writer + reader (catalog integration tests)
fn test_create_and_list_fk() — write FkDef, read back, assert fields equal
fn test_list_fk_constraints_referencing() — write 3 FKs, list by parent_table_id
fn test_drop_fk() — create + drop, list returns empty
fn test_alloc_fk_id_monotonic() — 3 creates return 1, 2, 3
fn test_legacy_db_fk_root_zero() — reader with root=0 returns empty vec (no panic)
```

### Integration tests (`tests/integration_fk.rs`)

```rust
// DDL
fn test_create_table_persists_fk()
fn test_create_table_fk_table_constraint()
fn test_create_table_fk_auto_creates_index()
fn test_create_table_fk_reuses_existing_index()
fn test_create_table_fk_parent_not_found_error()
fn test_create_table_fk_no_parent_index_error()
fn test_alter_table_add_fk()
fn test_alter_table_add_fk_existing_violation_error()
fn test_alter_table_drop_fk_removes_auto_index()
fn test_alter_table_drop_fk_keeps_user_index()

// INSERT
fn test_insert_valid_fk()
fn test_insert_null_fk_passes()
fn test_insert_invalid_fk_error()
fn test_insert_multiple_fks_both_checked()

// UPDATE child
fn test_update_fk_column_valid()
fn test_update_fk_column_invalid_error()
fn test_update_non_fk_column_no_check()

// DELETE parent — RESTRICT
fn test_delete_parent_restrict_error()
fn test_delete_parent_no_children_ok()
fn test_delete_parent_null_children_ok() — children have NULL FK, parent deletable

// DELETE parent — CASCADE
fn test_delete_parent_cascade_removes_children()
fn test_delete_parent_cascade_multi_level() — A→B→C, delete A removes B and C
fn test_delete_parent_cascade_depth_exceeded()

// DELETE parent — SET NULL
fn test_delete_parent_set_null()
fn test_delete_parent_set_null_not_nullable_error()

// UPDATE parent
fn test_update_parent_key_with_children_restrict_error()
fn test_update_parent_key_no_children_ok()
```

---

## Anti-patterns to avoid

- **DO NOT** call `list_fk_constraints_referencing` inside the per-row loop of
  `enforce_fk_on_parent_delete`. Load the FK list ONCE per DELETE statement (before
  the `to_delete` iteration), not once per deleted row — otherwise a DELETE of N
  rows makes N catalog scans.

- **DO NOT** check FK constraints when a row is deleted as part of a CASCADE.
  The recursive call to `enforce_fk_on_parent_delete` handles deeper levels.
  Checking constraints on CASCADE children would double-validate and potentially
  cause false RESTRICT errors on the child's own children.

- **DO NOT** use `unwrap()` in any production path (src/ outside #[cfg(test)]).
  All catalog lookups return `Result` — use `?`.

- **DO NOT** forget schema cache invalidation: after ALTER TABLE ADD/DROP CONSTRAINT FK,
  call `ctx.invalidate_all()` so the next query reloads the FK list from the catalog.

- **DO NOT** build the bloom shortcut for FK parent lookup using the parent's bloom
  `might_exist()` in a way that can produce false negatives. The bloom returns `true`
  conservatively for unknown indexes — this is the correct safe path.

- **DO NOT** auto-name FKs with a counter suffix (e.g., `fk_orders_user_1`). Use the
  deterministic pattern `fk_{child_table}_{child_col}_{parent_table}`. If that name
  already exists (duplicate FK definition), return an error.

- **DO NOT** allow `fk_index_id = 0` to be interpreted as "index 0 exists". Index IDs
  start from 1 (monotonic from 1). Zero is the sentinel "no auto-created index".
  Verify this invariant in `alloc_fk_id`.

---

## Risks

| Risk | Mitigation |
|------|-----------|
| Circular cascade (A→B→A) causes infinite recursion | Depth limit of 10 terminates the loop |
| `list_fk_constraints_referencing` O(n) scan slow for many FK definitions | Acceptable for Phase 6.5 (FK count is small); add an index on `parent_table_id` in Phase 6.9 if needed |
| Pre-6.5 database opens with `foreign_keys` root = 0 | All reader methods check root == 0 → return empty vec |
| DELETE with CASCADE + secondary indexes on children: bloom + B-Tree state | `delete_from_indexes` already handles this; call it for CASCADE-deleted children just as regular deletes do |
| Auto-index name collision (`_fk_*`) if user created index with same name | Pre-check with `list_indexes` — if name already taken, append `_2`, `_3` etc. |
| UPDATE parent key with ON UPDATE CASCADE deferred → confusing behavior | Return clear `NotImplemented` with message "ON UPDATE CASCADE — Phase 6.9" |
