# Spec: Foreign Key Constraints (Phase 6.5 + 6.6)

## What to build (not how)

A complete foreign key constraint system covering the full lifecycle:
catalog storage, DDL (CREATE TABLE + ALTER TABLE), and DML enforcement
(INSERT/UPDATE on child, DELETE/UPDATE on parent with RESTRICT, CASCADE,
and SET NULL actions).

A foreign key constraint on a child table column means: every non-NULL value in
that column must reference an existing row in the parent table's referenced column.
The referenced column must be indexed (PRIMARY KEY or UNIQUE).

When a parent row is deleted or its key updated, the ON DELETE / ON UPDATE action
determines what happens to child rows that reference it.

---

## Scope

### In scope (6.5 + 6.6)
- Single-column FKs defined inline (`col INT REFERENCES parent(id)`) or as table
  constraint (`CONSTRAINT name FOREIGN KEY (col) REFERENCES parent(id)`)
- `CREATE TABLE` — persists FK definitions and auto-creates index on FK column
- `ALTER TABLE ADD CONSTRAINT FOREIGN KEY` — adds FK post-creation with validation
- `ALTER TABLE DROP CONSTRAINT fk_name` — removes FK and its auto-created index
- INSERT into child → validate parent key exists
- UPDATE child FK column → validate new value references existing parent row
- DELETE parent row → RESTRICT, CASCADE, SET NULL
- UPDATE parent key → RESTRICT only (CASCADE/SET NULL on UPDATE deferred → see ⚠️)
- NULL FK column values → constraint passes (SQL standard MATCH SIMPLE)

### ⚠️ DEFERRED
- Composite (multi-column) FKs → Phase 6.9
- ON UPDATE CASCADE / ON UPDATE SET NULL → Phase 6.9
- Deferred constraint checking (DEFERRABLE INITIALLY DEFERRED) → Phase 7 (MVCC)
- Cascade depth > 10 levels → error, not silently truncated
- Self-referential FKs (table references itself) → Phase 6.9

---

## Inputs / Outputs

### DDL — CREATE TABLE with FK

**Input:**
```sql
CREATE TABLE orders (
  id       INT PRIMARY KEY,
  user_id  INT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  CONSTRAINT fk_orders_product
    FOREIGN KEY (product_id) REFERENCES products(id) ON DELETE RESTRICT
);
```

**Output:** `QueryResult::Empty` on success.

**Errors:**
- `TableNotFound { name }` — parent table does not exist
- `ColumnNotFound { name, table }` — referenced column does not exist in parent
- `ForeignKeyError::NoParentIndex` — parent column has no PRIMARY KEY or UNIQUE index
- `ForeignKeyError::TypeMismatch` — child column type incompatible with parent column type
- `ForeignKeyError::DuplicateName { name }` — FK constraint name already exists on this table

### DDL — ALTER TABLE ADD CONSTRAINT FK

**Input:**
```sql
ALTER TABLE orders
  ADD CONSTRAINT fk_orders_user
  FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE RESTRICT;
```

**Output:** `QueryResult::Empty`.

**Extra validation:** Every existing non-NULL value in the child FK column must have
a matching parent row. If any violation exists → `ForeignKeyViolation` error, no
FK is created.

### DDL — ALTER TABLE DROP CONSTRAINT (FK)

**Input:**
```sql
ALTER TABLE orders DROP CONSTRAINT fk_orders_user;
```

**Output:** `QueryResult::Empty`.

**Side effect:** If the FK had an auto-created index (stored in `FkDef.fk_index_id`),
that index is also dropped.

### DML — INSERT child

**Input:**
```sql
INSERT INTO orders (user_id, product_id) VALUES (42, 99);
```

**Output:** `QueryResult::Affected { count: 1, .. }` on success.

**Errors:**
- `ForeignKeyViolation { table: "orders", column: "user_id", value: "42" }` — no row
  in `users` with `id = 42`.

### DML — DELETE parent (RESTRICT)

**Input:**
```sql
DELETE FROM users WHERE id = 42;
-- orders table has rows with user_id = 42
```

**Output:** `DbError::ForeignKeyViolation` — cannot delete parent, children exist.

### DML — DELETE parent (CASCADE)

**Input:**
```sql
DELETE FROM users WHERE id = 42;
-- orders.user_id references users.id with ON DELETE CASCADE
```

**Output:** `QueryResult::Affected { count: 1 }` (parent deleted; child deletions
are silent — total count reflects only the explicitly targeted rows).

**Side effect:** All `orders` rows with `user_id = 42` are also deleted.

### DML — DELETE parent (SET NULL)

**Input:**
```sql
DELETE FROM users WHERE id = 42;
-- orders.user_id references users.id with ON DELETE SET NULL
```

**Output:** `QueryResult::Affected { count: 1 }`.

**Side effect:** All `orders` rows with `user_id = 42` have `user_id` set to NULL.
The column must be nullable; if it is NOT NULL → `ForeignKeyError::SetNullOnNotNullColumn`.

---

## Catalog: FkDef struct and binary format

### Struct

```rust
/// A row in `axiom_foreign_keys` — one entry per FK constraint.
pub struct FkDef {
    pub fk_id:          u32,  // monotonic catalog-allocated ID
    pub child_table_id: u32,  // table with the FK column
    pub child_col_idx:  u16,  // FK column index in child table
    pub parent_table_id: u32, // referenced table
    pub parent_col_idx:  u16, // referenced column index in parent table
    pub on_delete:       FkAction,
    pub on_update:       FkAction,
    /// index_id of the auto-created index on child FK column.
    /// Zero if the user provided an existing index; in that case we do NOT
    /// drop the index when the FK is dropped.
    pub fk_index_id:    u32,
    pub name:           String,
}

#[repr(u8)]
pub enum FkAction {
    NoAction  = 0,
    Restrict  = 1,
    Cascade   = 2,
    SetNull   = 3,
    SetDefault = 4,
}
```

`NoAction` and `Restrict` are identical in AxiomDB (both enforce immediately,
no deferral). `NoAction` is the SQL default.

### Binary format

```
[fk_id:          4 bytes LE u32]
[child_table_id: 4 bytes LE u32]
[child_col_idx:  2 bytes LE u16]
[parent_table_id:4 bytes LE u32]
[parent_col_idx: 2 bytes LE u16]
[on_delete:      1 byte  u8   ]
[on_update:      1 byte  u8   ]
[fk_index_id:    4 bytes LE u32]
[name_len:       4 bytes LE u32]
[name:           name_len bytes UTF-8]
```

Total fixed header: 26 bytes. Variable: name.

---

## Catalog: new system table `axiom_foreign_keys`

### Meta page offsets (added to `meta.rs`)

```rust
pub const CATALOG_FOREIGN_KEYS_ROOT_BODY_OFFSET: usize = 84;
pub const NEXT_FK_ID_BODY_OFFSET: usize = 92; // u32 at offset 92
```

### WAL system table constant (added to `writer.rs`)

```rust
pub const SYSTEM_TABLE_FOREIGN_KEYS: u32 = u32::MAX - 4;
```

### `CatalogPageIds` (updated)

```rust
pub struct CatalogPageIds {
    pub tables:       u64,
    pub columns:      u64,
    pub indexes:      u64,
    pub constraints:  u64,
    pub foreign_keys: u64,  // NEW — zero on pre-6.5 databases; lazily initialized
}
```

### `CatalogBootstrap::ensure_fk_root(storage)` (new method)

Same pattern as `ensure_constraints_root`: reads offset 84, if zero allocates a
new `PageType::Data` page, writes it to storage, persists the page ID at offset 84.
Returns the root page ID.

### `CatalogWriter` new methods

- `create_foreign_key(def: FkDef) → Result<u32, DbError>`
  1. `alloc_fk_id(storage)` — reads/increments NEXT_FK_ID_BODY_OFFSET
  2. `CatalogBootstrap::ensure_fk_root(storage)?`
  3. Serialize `FkDef` to bytes
  4. `HeapChain::insert(storage, fk_root, &data, txn_id)`
  5. `txn.record_insert(SYSTEM_TABLE_FOREIGN_KEYS, &key, &data, page_id, slot_id)`
  6. Returns `fk_id`

- `drop_foreign_key(fk_id: u32) → Result<(), DbError>`
  1. Scan FK root heap for row with matching `fk_id`
  2. `HeapChain::delete(storage, page_id, slot_id, txn_id)`
  3. `txn.record_delete(SYSTEM_TABLE_FOREIGN_KEYS, &key, &old_bytes, page_id, slot_id)`

### `CatalogReader` new methods

- `list_fk_constraints(table_id: u32) → Result<Vec<FkDef>, DbError>`
  Scan FK heap, filter by `fk.child_table_id == table_id`. Return all FKs where
  this table is the child.

- `list_fk_constraints_referencing(parent_table_id: u32) → Result<Vec<FkDef>, DbError>`
  Scan FK heap, filter by `fk.parent_table_id == parent_table_id`. Return all FKs
  where this table is the parent. Used by DELETE/UPDATE parent enforcement.

### `ResolvedTable` (updated)

```rust
pub struct ResolvedTable {
    pub def:          TableDef,
    pub columns:      Vec<ColumnDef>,
    pub indexes:      Vec<IndexDef>,
    pub constraints:  Vec<ConstraintDef>,
    pub foreign_keys: Vec<FkDef>,  // NEW — FKs where this table is the child
}
```

`resolve_table` adds `reader.list_fk_constraints(def.id)?` after loading constraints.

---

## DDL enforcement

### CREATE TABLE — FK processing order

After all columns are created in the catalog:

1. Collect all FK definitions from:
   - `ColumnConstraint::References { table, column, on_delete, on_update }` on each column
   - `TableConstraint::ForeignKey { name, columns, ref_table, ref_columns, ... }`

2. For each FK:
   a. Resolve parent table (must exist)
   b. Resolve parent column (default: PRIMARY KEY column if no column name given)
   c. Verify parent column has a PRIMARY KEY or UNIQUE index
   d. Verify type compatibility (child col type ↔ parent col type — both must be
      the same `ColumnType` family: integer types are compatible with each other;
      TEXT compatible with TEXT; no cross-family comparisons)
   e. Auto-name if unnamed: `fk_{child_table}_{child_col}_{parent_table}`
   f. Check for FK name uniqueness on this table
   g. Check if child column already has an index covering it (search `resolved.indexes`)
      - If yes: use existing index (set `fk_index_id = 0` meaning "not auto-created")
      - If no: create index named `_fk_{fk_name}` on the child FK column
        (same flow as `execute_create_index` but called inline; store `fk_index_id`)
   h. Persist `FkDef` via `writer.create_foreign_key(...)`

### ALTER TABLE ADD CONSTRAINT FK

Same steps as CREATE TABLE FK processing, plus:

**Existing data validation:** After persisting the FK def, scan the child table and
verify every non-NULL FK value has a matching parent row. If any violation found →
`drop_foreign_key(new_fk_id)` + return error. This is the same lookup used by INSERT
validation (see below).

### ALTER TABLE DROP CONSTRAINT (FK)

In `alter_drop_constraint`:
1. Find `FkDef` by constraint name (scan `list_fk_constraints(table_id)`)
2. `writer.drop_foreign_key(fk_def.fk_id)?`
3. If `fk_def.fk_index_id != 0` → also execute `execute_drop_index` for that index

---

## DML enforcement

### INSERT child — FK validation

Called in `execute_insert_ctx` and `execute_insert`, immediately before
`TableEngine::insert_row`, for each row being inserted.

```
fn check_fk_insert(
    row: &[Value],
    foreign_keys: &[FkDef],
    storage: &mut dyn StorageEngine,
    txn: &TxnManager,
) -> Result<(), DbError>
```

For each `fk` in `foreign_keys`:
1. Get `fk_val = row[fk.child_col_idx as usize]`
2. If `fk_val == Value::Null` → skip (NULL exemption)
3. Encode `fk_val` as index key: `encode_index_key(&[fk_val])?`
4. Load parent's primary/unique index root: find index in parent's indexes where
   `idx.is_primary || (idx.is_unique && idx.columns[0].col_idx == fk.parent_col_idx)`
5. `BTree::lookup_in(storage, parent_index_root, &key)?`
6. If `None` → `Err(DbError::ForeignKeyViolation { table, column, value })`

**Bloom optimization:** Before step 5, check `bloom.might_exist(parent_index_id, &key)`.
If false → definitively absent → skip BTree → return violation immediately.

### UPDATE child FK column — FK validation

In `execute_update_ctx` and `execute_update`, after computing `new_values`, before
writing the updated row:

For each FK on this table:
- If `new_values[fk.child_col_idx]` differs from `old_values[fk.child_col_idx]`:
  - Apply same check as INSERT validation for the new value

### DELETE parent — FK enforcement

Called in `execute_delete_ctx` and `execute_delete` after collecting `to_delete`
rows but before the batch heap delete.

```
fn check_fk_on_parent_delete(
    to_delete: &[(RecordId, Vec<Value>)],
    parent_table_id: u32,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<(), DbError>
```

1. Load `fk_list = reader.list_fk_constraints_referencing(parent_table_id)?`
2. If `fk_list` is empty → return Ok (no FKs reference this table, fast path)
3. For each `fk` in `fk_list`:
   a. Load child table def + columns + FK index
   b. For each `(_, parent_row)` in `to_delete`:
      - Encode `parent_row[fk.parent_col_idx]` as key
      - Look up child FK index: `BTree::lookup_in(child_fk_index, &key)?`
      - If children found:
        - `FkAction::Restrict | FkAction::NoAction` → return `ForeignKeyViolation`
        - `FkAction::Cascade` → collect child rids for deletion (see cascade below)
        - `FkAction::SetNull` → collect child rids for SET NULL update (see below)

**CASCADE delete** (called before the parent heap delete):
```
1. Find all child RecordIds via BTree range on FK index
2. Call TableEngine::delete_rows_batch(child_rids)
3. Call delete_from_indexes for child secondary indexes
4. Recursively call check_fk_on_parent_delete for the child table (depth tracking)
   - If depth > 10 → Err(ForeignKeyError::CascadeDepthExceeded)
5. WAL-log child deletions
```

**SET NULL update** (called before the parent heap delete):
```
1. Find all child RecordIds with FK value = deleted_parent_key
2. For each child row:
   - Read current child row values
   - Set child_row[fk.child_col_idx] = Value::Null
   - Check column is nullable (if NOT NULL → ForeignKeyError::SetNullOnNotNullColumn)
   - TableEngine::update_row(child_rid, new_child_values)
   - Update child secondary indexes (delete old key, insert new key)
   - Mark FK bloom dirty (child FK value changed to NULL)
```

### UPDATE parent key — FK enforcement

When `UPDATE parent SET pk_col = new_val WHERE ...`:
- For each updated parent row where the PK/referenced column changed:
  - Load FKs referencing this table
  - If any children reference the old key:
    - `FkAction::Restrict | FkAction::NoAction` → error
    - CASCADE / SET NULL on UPDATE → `NotImplemented` (deferred to Phase 6.9)

---

## New error variants

Add to `DbError` (already has `ForeignKeyViolation` for child insert violation):

```rust
/// Parent row cannot be deleted/updated because child rows reference it.
#[error("foreign key constraint {constraint}: {child_table}.{child_column} references this row")]
ForeignKeyParentViolation {
    constraint: String,
    child_table: String,
    child_column: String,
},

/// FK cascade exceeded maximum depth (prevents infinite loops).
#[error("foreign key cascade depth exceeded limit of {limit}")]
ForeignKeyCascadeDepth { limit: u32 },

/// ON DELETE SET NULL attempted on a NOT NULL column.
#[error("cannot set FK column {table}.{column} to NULL: column is NOT NULL")]
ForeignKeySetNullNotNullable { table: String, column: String },

/// No PRIMARY KEY or UNIQUE index on parent column.
#[error("no unique index on {table}.{column} to satisfy foreign key")]
ForeignKeyNoParentIndex { table: String, column: String },
```

SQLSTATE codes:
- `ForeignKeyViolation` → `"23503"` (already mapped)
- `ForeignKeyParentViolation` → `"23503"` (same class, parent-side)
- `ForeignKeyCascadeDepth` → `"23503"`
- `ForeignKeySetNullNotNullable` → `"23000"` (integrity constraint violation)
- `ForeignKeyNoParentIndex` → `"42830"` (invalid foreign key)

---

## Use cases

### 1. Happy path — INSERT valid FK
```sql
INSERT INTO users (id, name) VALUES (1, 'Alice');
INSERT INTO orders (id, user_id) VALUES (10, 1);
-- ✅ orders.user_id = 1 exists in users.id
```

### 2. FK violation on INSERT
```sql
INSERT INTO orders (id, user_id) VALUES (10, 999);
-- ❌ ForeignKeyViolation: no row in users with id = 999
```

### 3. NULL FK column — passes constraint
```sql
INSERT INTO orders (id, user_id) VALUES (10, NULL);
-- ✅ NULL exemption — passes even if no user exists
```

### 4. DELETE parent — RESTRICT (default)
```sql
-- users: id=1 'Alice', orders: user_id=1
DELETE FROM users WHERE id = 1;
-- ❌ ForeignKeyParentViolation: orders.user_id references this row
```

### 5. DELETE parent — CASCADE
```sql
-- ON DELETE CASCADE defined
DELETE FROM users WHERE id = 1;
-- ✅ orders rows with user_id=1 also deleted
-- Affected count = 1 (only parent count returned)
```

### 6. DELETE parent — SET NULL
```sql
-- ON DELETE SET NULL defined, orders.user_id is nullable
DELETE FROM users WHERE id = 1;
-- ✅ orders rows with user_id=1 now have user_id=NULL
```

### 7. ALTER TABLE ADD CONSTRAINT with existing violations
```sql
INSERT INTO orders (user_id) VALUES (999); -- user 999 doesn't exist
ALTER TABLE orders ADD CONSTRAINT fk_user FOREIGN KEY (user_id) REFERENCES users(id);
-- ❌ ForeignKeyViolation: existing data violates constraint
```

### 8. DROP CONSTRAINT removes auto-index
```sql
-- FK fk_user was created with auto-index _fk_fk_user on orders.user_id
ALTER TABLE orders DROP CONSTRAINT fk_user;
-- ✅ FK dropped + _fk_fk_user index dropped
```

### 9. Auto-create index skipped when index already exists
```sql
CREATE INDEX idx_user ON orders (user_id);
-- Later:
ALTER TABLE orders ADD CONSTRAINT fk_user FOREIGN KEY (user_id) REFERENCES users(id);
-- ✅ FK created, no new index (fk_index_id = 0), existing idx_user used
```

### 10. Cascade depth exceeded
```sql
-- A→B→C→...→K (11 levels deep)
DELETE FROM A WHERE id = 1;
-- ❌ ForeignKeyCascadeDepth { limit: 10 }
```

---

## Acceptance criteria

- [ ] `CREATE TABLE` with inline `REFERENCES` persists FK in catalog
- [ ] `CREATE TABLE` with `FOREIGN KEY` table constraint persists FK in catalog
- [ ] FK auto-creates index on child FK column (named `_fk_{constraint_name}`)
- [ ] FK does NOT create duplicate index if one already covers the column
- [ ] `ALTER TABLE ADD CONSTRAINT FOREIGN KEY` validates existing data before persisting
- [ ] `ALTER TABLE DROP CONSTRAINT` on FK drops FK + auto-created index
- [ ] `INSERT` into child validates parent key exists (BTree lookup on parent index)
- [ ] `INSERT` with NULL FK value passes constraint without parent lookup
- [ ] `UPDATE` child FK column validates new value
- [ ] `DELETE` parent with RESTRICT/NO ACTION errors if children exist
- [ ] `DELETE` parent with CASCADE deletes all referencing children
- [ ] `DELETE` parent with SET NULL sets child FK columns to NULL
- [ ] Cascade depth > 10 returns `ForeignKeyCascadeDepth` error
- [ ] SET NULL on NOT NULL column returns `ForeignKeySetNullNotNullable`
- [ ] `ForeignKeyViolation` carries SQLSTATE `"23503"`
- [ ] `axiom_foreign_keys` heap root lazily initialized (pre-6.5 DBs open without migration)
- [ ] Integration tests: all use cases above covered

---

## Out of scope

- Composite (multi-column) FK constraints → Phase 6.9
- ON UPDATE CASCADE / ON UPDATE SET NULL → Phase 6.9
- Self-referential FKs → Phase 6.9
- DEFERRABLE / INITIALLY DEFERRED → Phase 7
- FK metadata exposed via `axiom_foreign_keys` system view → Phase 6.9
- `SHOW CREATE TABLE` including FK syntax → Phase 6.9

---

## Dependencies

- Phase 6.1–6.3: Secondary indexes (B-Tree lookup, `IndexDef`, `encode_index_key`)
- Phase 6.4: BloomRegistry (bloom check on parent index before BTree lookup)
- `ForeignKeyViolation` error already exists in `axiomdb-core`
- `ForeignKeyAction` enum already exists in `axiomdb-sql/src/ast.rs`
- `ColumnConstraint::References` and `TableConstraint::ForeignKey` already parsed
