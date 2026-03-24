# Spec: Secondary Indexes — Subfases 6.1, 6.2, 6.2b, 6.3

## What to build (not how)

Full secondary-index support: catalog stores which columns an index covers, `CREATE
INDEX` scans the table to populate the B-Tree, every DML statement keeps the index
in sync, and the query planner replaces full-table scans with B-Tree lookups when
the WHERE clause matches an indexed column.

---

## Subfase 6.1 — Columns in IndexDef

### What to build

Add `columns: Vec<IndexColumnDef>` to `IndexDef` so the catalog records which column
positions (by `col_idx`) each index covers, along with sort direction.  Existing rows
on disk that predate this field must be readable — `from_bytes` must treat a missing
columns section as an empty column list (used only for the bootstrap PK index, which
is handled separately).

### New type

```rust
/// One column entry within an `IndexDef`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexColumnDef {
    /// Position of this column in the table (matches `ColumnDef.col_idx`).
    pub col_idx: u16,
    /// Sort direction for this column in the index key.
    pub order: SortOrder,   // SortOrder already exists in axiomdb-sql AST
}
```

`SortOrder` must be re-exported or duplicated into `axiomdb-catalog` (preferred:
re-export from `axiomdb-core` so it has no circular dependency).

### On-disk format extension

Current `IndexRow` format (18 + name_len bytes):
```
[index_id:4][table_id:4][root_page_id:8][flags:1][name_len:1][name bytes]
```

New format (18 + name_len + 1 + N*3 bytes):
```
[index_id:4][table_id:4][root_page_id:8][flags:1][name_len:1][name bytes][ncols:1][col_idx:2 LE, order:1]×N
```

- `ncols:1` = number of index columns (0–63).
- Each column entry is 3 bytes: `[col_idx:2 LE][order:1]` where `order` 0 = ASC, 1 = DESC.
- If `ncols` byte is absent (old row length equals 18 + name_len), `columns` defaults to `[]`.

Backward compatibility rule: `from_bytes` checks `bytes.len() >= consumed_so_far + 1`
before reading `ncols`.  If not enough bytes, returns `columns: vec![]` silently.

### Inputs / Outputs
- `IndexDef::to_bytes()` → `Vec<u8>` with columns appended
- `IndexDef::from_bytes(&[u8])` → `(IndexDef, usize)` — backward-compatible
- `IndexDef.columns: Vec<IndexColumnDef>`

### Acceptance criteria
- [ ] `IndexColumnDef` defined with `col_idx: u16` and `order: SortOrder`
- [ ] `IndexDef.columns` field added
- [ ] `to_bytes` serializes columns section
- [ ] `from_bytes` reads columns when present, returns `[]` when absent (old row)
- [ ] Unit tests: roundtrip with 0 cols, 1 col ASC, 2 cols mixed, old-format truncated row
- [ ] `CatalogWriter::create_index` signature updated to accept `columns: Vec<IndexColumnDef>`
- [ ] Bootstrap `create_index` for PK passes `columns: vec![IndexColumnDef { col_idx: 0, order: SortOrder::Asc }]`
- [ ] No `unwrap()` in `src/` (only tests)

### Out of scope
- Multi-column composite index key encoding (6.2)
- Index lookup (6.3)

---

## Subfase 6.2 — CREATE INDEX executor + key encoding

### What to build

`execute_create_index` must:
1. Extract column definitions from the table.
2. Allocate a new B-Tree root page (already done).
3. Scan every row in the table heap, extract the indexed column values, encode them
   as an order-preserving byte key, insert `(key, RecordId)` into the B-Tree.
4. Persist the `IndexDef` with the populated `columns` field.

`execute_drop_index` must walk and free the B-Tree pages (currently leaks them).

### Key encoding — `encode_index_key(values: &[Value]) -> Vec<u8>`

Produces a byte sequence that preserves sort order: if `a < b` as SQL values then
`encode(a) < encode(b)` as byte slices under lexicographic comparison.

Encoding per `Value` variant, concatenated for composite keys:

| Type | Encoding |
|------|----------|
| `Value::Null` | `[0x00]` — sorts before all non-NULL |
| `Value::Bool(false)` | `[0x01, 0x00]` |
| `Value::Bool(true)` | `[0x01, 0x01]` |
| `Value::Int(n)` | `[0x02, (n as i64 ^ i64::MIN) as u64: 8 BE]` — sign flip to make unsigned order match signed order |
| `Value::BigInt(n)` | `[0x03, (n ^ i64::MIN) as u64: 8 BE]` |
| `Value::Float(f)` | `[0x04, encode_f64(f)]` — 8 bytes, NaN → all zeros, negative flips all bits |
| `Value::Text(s)` | `[0x05, s.as_bytes(), 0x00]` — NUL-terminated; NUL in string is escaped as `[0xFF, 0x00]` |
| `Value::Bytes(b)` | `[0x06, b, 0x00]` — same escape |
| `Value::Timestamp(t)` | `[0x07, (t ^ i64::MIN) as u64: 8 BE]` |
| `Value::Uuid(u)` | `[0x08, u[0..16]]` — already lexicographically ordered |

For `Value::Float`: if `f.is_nan()` → `[0x04, 0x00, ..., 0x00]` (8 bytes); if `f >= 0.0`
→ `[0x04]` + `f.to_bits().to_be_bytes()` with MSB set; if `f < 0.0` → `[0x04]` + all 8 bytes
of `f.to_bits().to_be_bytes()` with all bits flipped.

DESC columns: append `!byte` for every byte in the column's segment (XOR with 0xFF).

Key length limit: MAX_KEY_LEN = 768 bytes (matches existing `check_key` limit in BTree).
If the encoded key exceeds this, return `DbError::IndexKeyTooLong`.

### BTree shared-storage API

`BTree` currently owns `Box<dyn StorageEngine>`.  For Phase 6 the caller already
holds `&mut MmapStorage`.  Add static functions that take an external `&mut dyn StorageEngine`:

```rust
impl BTree {
    /// Lookup without an owned BTree instance.
    pub fn lookup_in(
        storage: &dyn StorageEngine,
        root_pid: u64,
        key: &[u8],
    ) -> Result<Option<RecordId>, DbError>;

    /// Insert without an owned BTree instance.
    pub fn insert_in(
        storage: &mut dyn StorageEngine,
        root_pid: &AtomicU64,
        key: &[u8],
        rid: RecordId,
    ) -> Result<(), DbError>;

    /// Range scan without an owned BTree instance.
    pub fn range_in<'a>(
        storage: &'a dyn StorageEngine,
        root_pid: u64,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
    ) -> Result<RangeIter<'a>, DbError>;
}
```

These are thin wrappers that delegate to the existing private helpers.

### execute_create_index — new behavior

```
1. resolve table → TableDef + ColumnDef list
2. for each IndexColumn in stmt.columns:
     find ColumnDef by name → col_idx
     build IndexColumnDef { col_idx, order }
3. alloc root page; init as empty B-Tree leaf (is_leaf=1, num_keys=0, next=NULL)
4. let root_pid = AtomicU64::new(root_page_id)
5. scan table heap (HeapChain::scan):
     for each (rid, row_bytes):
         decode row → Vec<Value>
         extract indexed values: cols.iter().map(|c| row[c.col_idx])
         encode key = encode_index_key(&values)
         if key.len() > MAX_KEY_LEN: skip row (log warning; Phase 6 does not abort)
         BTree::insert_in(&mut storage, &root_pid, &key, rid)?
6. write updated root_page_id (may have changed after splits)
7. CatalogWriter::create_index(IndexDef { ..., columns, root_page_id: root_pid.load() })
```

### execute_drop_index — page reclamation

After deleting the catalog row, walk the B-Tree and free every page:
```
fn free_btree_pages(storage: &mut dyn StorageEngine, root_pid: u64) → Result<(), DbError>
    // BFS or iterative DFS via a Vec<u64> stack
    // read each page; if internal, push all child pids; free page
```

### Inputs / Outputs
- `CREATE INDEX name ON table (col [ASC|DESC], ...)` → `QueryResult::Empty` + populated index
- `DROP INDEX name ON table` → `QueryResult::Empty` + pages freed

### Errors
- `DbError::TableNotFound` — table does not exist (already handled in analyzer)
- `DbError::ColumnNotFound` — column in index does not exist (already handled in analyzer)
- `DbError::IndexAlreadyExists { name }` — duplicate index name on same table
- `DbError::IndexKeyTooLong { key_len, max }` — row with key > 768 bytes (skip in CREATE INDEX, error in INSERT)

### Acceptance criteria
- [ ] `encode_index_key` implemented and order-preserving for all Value variants
- [ ] `encode_index_key` unit tests: NULL < Bool < Int; negative ints sort before positive; floats; text lexicographic; DESC bit-flip
- [ ] `BTree::lookup_in`, `insert_in`, `range_in` static functions added
- [ ] `execute_create_index` scans table and populates B-Tree
- [ ] `execute_create_index` stores `columns` in IndexDef
- [ ] `execute_drop_index` frees all B-Tree pages
- [ ] `DbError::IndexAlreadyExists` variant added
- [ ] `DbError::IndexKeyTooLong` variant added
- [ ] Integration test: CREATE INDEX → lookup by key → result matches full scan
- [ ] No `unwrap()` in `src/`

### Out of scope
- DML maintenance (6.2b)
- Query planner (6.3)
- Composite index with > 1 column is supported by the encoding but only 1-column indexes are exercised in 6.2 tests

---

## Subfase 6.2b — Index maintenance on INSERT / UPDATE / DELETE

### What to build

Every DML that changes a row must keep all secondary indexes on that table consistent.

The executor calls a helper `index_maintenance::insert_row_into_indexes` /
`delete_row_from_indexes` after each heap mutation.

### Helper API (new module `axiomdb-sql/src/index_maintenance.rs`)

```rust
/// Returns all IndexDefs for a table (from catalog + SchemaCache).
pub fn indexes_for_table(
    table_id: TableId,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
) -> Result<Vec<IndexDef>, DbError>;

/// Inserts (key → rid) into every non-primary secondary index for the table.
/// For unique indexes: checks for duplicate key and returns DbError::UniqueViolation if found.
pub fn insert_into_indexes(
    indexes: &[IndexDef],
    row: &[Value],
    rid: RecordId,
    storage: &mut dyn StorageEngine,
) -> Result<(), DbError>;

/// Removes (key, rid) from every non-primary secondary index.
/// Uses range scan + linear probe to find the exact rid; not an error if not found.
pub fn delete_from_indexes(
    indexes: &[IndexDef],
    row: &[Value],
    rid: RecordId,
    storage: &mut dyn StorageEngine,
) -> Result<(), DbError>;
```

### Integration into DML executors

**INSERT** (in `execute_insert`):
```
1. decode inserted values → Vec<Value>
2. insert row into heap → rid
3. indexes = indexes_for_table(table_id, storage, snapshot)?
4. filter out primary index (is_primary = true)
5. insert_into_indexes(&secondary_indexes, &values, rid, storage)?
```

**DELETE** (in `execute_delete`):
```
For each deleted row:
1. capture rid + values before deletion
2. delete from heap
3. delete_from_indexes(&indexes, &values, rid, storage)?
```

**UPDATE** (in `execute_update`):
```
For each updated row:
1. capture old_rid + old_values
2. write new row to heap → new_rid (UPDATE is delete old + insert new in current impl)
3. delete_from_indexes(&indexes, &old_values, old_rid, storage)?
4. insert_into_indexes(&indexes, &new_values, new_rid, storage)?
```

### Errors
- `DbError::UniqueViolation { index_name, key_repr }` — attempted duplicate key in UNIQUE index

### Acceptance criteria
- [ ] `index_maintenance` module created
- [ ] `indexes_for_table` returns correct list
- [ ] `insert_into_indexes` called from INSERT executor
- [ ] `delete_from_indexes` called from DELETE executor
- [ ] UPDATE executor calls delete + insert (existing UPDATE already does delete+reinsert)
- [ ] Integration test: CREATE INDEX → INSERT duplicates → UniqueViolation; DELETE → index entry removed; UPDATE → index updated
- [ ] `DbError::UniqueViolation` variant added
- [ ] No `unwrap()` in `src/`

### Out of scope
- Primary-key maintenance (already handled by heap engine in Phase 4)
- Deferred constraint checking (Phase 9)

---

## Subfase 6.3 — Query planner: WHERE col = ? and WHERE col BETWEEN

### What to build

A minimal query planner that, before executing a `SELECT`, detects whether the
WHERE clause matches a simple equality or range predicate on an indexed column,
and rewrites the execution plan to use a B-Tree lookup instead of a full table scan.

This is a **rewrite pass** — it transforms a `SelectStmt` with `AccessMethod::Scan`
into `AccessMethod::IndexLookup { index_def, key_values }` or
`AccessMethod::IndexRange { index_def, lo, hi }`.

### New enum

```rust
/// How the executor should access rows for a table reference.
#[derive(Debug, Clone, PartialEq)]
pub enum AccessMethod {
    /// Full sequential scan (default).
    Scan,
    /// Point lookup: read exactly the rows matching key_values.
    IndexLookup {
        index_def: IndexDef,
        key_values: Vec<Value>,  // already-evaluated constants
    },
    /// Range scan between encoded lo and hi keys (None = unbounded).
    IndexRange {
        index_def: IndexDef,
        lo: Option<Vec<Value>>,
        hi: Option<Vec<Value>>,
        inclusive_lo: bool,
        inclusive_hi: bool,
    },
}
```

`AccessMethod` is stored in `SelectStmt.from` metadata or in a wrapper struct (see
plan note below).

### Planner function

```rust
/// Rewrites `select` to use an index when possible.
/// Called from `execute_with_ctx` after analyze, before execution.
pub fn plan_select(
    select: &mut SelectStmt,
    table_id: TableId,
    indexes: &[IndexDef],
    columns: &[ColumnDef],
) -> AccessMethod;
```

Pattern matching rules (applied in order — first match wins):

1. **Equality on single indexed column**:
   - WHERE predicate is `Expr::BinOp { op: BinOp::Eq, left: col_ref, right: literal }` or vice-versa
   - `col_ref` is a bare column name (no table qualifier) matching exactly one `ColumnDef`
   - `literal` is `Expr::Literal(Value::*)` (constant, already evaluated at analyze time)
   - The column is the **first column** of exactly one non-primary `IndexDef`
   → `AccessMethod::IndexLookup { index_def, key_values: vec![value] }`

2. **Range on single indexed column**:
   - WHERE predicate is `col > lo AND col < hi` (or `>=`, `<=`)
   - Both sides reference the same column, which is the first column of a non-primary index
   → `AccessMethod::IndexRange { ... }`

3. **No match** → `AccessMethod::Scan` (unchanged behavior)

Planner does **not** handle:
- OR predicates
- Multi-column composite indexes (deferred to 6.8)
- Subqueries
- Joins

### Executor changes

`execute_select` dispatched on `AccessMethod`:

- `Scan`: existing full-scan path (unchanged)
- `IndexLookup`:
  ```
  key = encode_index_key(&key_values)
  rid = BTree::lookup_in(storage, index_def.root_page_id, &key)?
  if let Some(rid) = rid:
      row = heap.read(rid)?
      apply remaining WHERE conditions (any not consumed by the index)
      emit row
  ```
- `IndexRange`:
  ```
  lo_key = lo.map(|v| encode_index_key(&v))
  hi_key = hi.map(|v| encode_index_key(&v))
  for (rid, _) in BTree::range_in(storage, root_pid, lo, hi)?:
      row = heap.read(rid)?
      apply remaining conditions
      emit row
  ```

### Inputs / Outputs
- `SELECT * FROM t WHERE indexed_col = 42` → result via B-Tree lookup (O(log n))
- `SELECT * FROM t WHERE name = 'alice'` → result via text index
- `SELECT * FROM t WHERE age > 20 AND age < 30` → range scan via index
- Fall-through: any query that doesn't match pattern 1 or 2 → unchanged full scan

### Acceptance criteria
- [ ] `AccessMethod` enum defined
- [ ] `plan_select` function implemented
- [ ] Equality pattern (rule 1) recognized and produces `IndexLookup`
- [ ] Range pattern (rule 2) recognized and produces `IndexRange`
- [ ] `execute_select` dispatches on `AccessMethod`
- [ ] Integration test: table with 1M rows; query with `WHERE id = X` uses index (verify no full scan via row-read count metric or timing)
- [ ] Integration test: `WHERE age > 20 AND age < 30` range via index
- [ ] No regression on queries without a matching index (fall through to Scan)
- [ ] No `unwrap()` in `src/`
- [ ] Benchmark: `SELECT * FROM users WHERE id = ?` with 1M rows; target < 50 µs (vs ~500 ms full scan)

### Out of scope
- JOIN pushdown
- Composite multi-column index lookups
- Cost-based optimization (Phase 8)
- `EXPLAIN` statement (Phase 8)

---

## Use cases

1. **Happy path — equality lookup**:
   ```sql
   CREATE INDEX users_email_idx ON users (email);
   SELECT * FROM users WHERE email = 'alice@example.com';
   -- Uses B-Tree lookup; returns in O(log n) instead of O(n)
   ```

2. **Happy path — range scan**:
   ```sql
   CREATE INDEX orders_created_idx ON orders (created_at);
   SELECT * FROM orders WHERE created_at > '2024-01-01' AND created_at < '2024-12-31';
   -- Uses B-Tree range scan
   ```

3. **Happy path — unique violation**:
   ```sql
   CREATE UNIQUE INDEX users_email_uniq ON users (email);
   INSERT INTO users (email) VALUES ('alice@example.com');
   INSERT INTO users (email) VALUES ('alice@example.com');  -- UniqueViolation
   ```

4. **Happy path — drop index**:
   ```sql
   DROP INDEX users_email_idx ON users;
   SELECT * FROM users WHERE email = 'alice@example.com';  -- falls back to full scan
   ```

5. **Edge case — NULL in indexed column**:
   `NULL` encodes as `[0x00]`, which sorts before all other values.
   Two `NULL`s in a UNIQUE index: `NULL != NULL` in SQL — must NOT trigger UniqueViolation.
   For UNIQUE indexes, skip the uniqueness check if the key value is NULL.

6. **Edge case — old catalog row without columns**:
   A database created before 6.1 has IndexDef rows without the columns section.
   `from_bytes` returns `columns: []`. These indexes are treated as "not usable by
   planner" — planner falls back to Scan.

7. **Edge case — index on column updated in UPDATE**:
   UPDATE changes `email` of a row → old index entry removed, new entry inserted.

8. **Edge case — key too long**:
   A TEXT column with a 2000-character value → key encoding exceeds 768 bytes →
   `DbError::IndexKeyTooLong`.  CREATE INDEX skips such rows with a warning.
   INSERT/UPDATE on tables with such an index returns `IndexKeyTooLong`.

---

## Dependencies

- 6.1 must complete before 6.2 (IndexDef needs columns field)
- 6.2 must complete before 6.2b (B-Tree static API needed)
- 6.2b must complete before 6.3 (indexes must be maintained to be useful)
- SchemaCache (Phase 4.12b) already exists — use it in `indexes_for_table`

## Files to create / modify

| File | Change |
|------|--------|
| `crates/axiomdb-catalog/src/schema.rs` | Add `IndexColumnDef`, extend `IndexDef` |
| `crates/axiomdb-catalog/src/writer.rs` | Update `create_index` signature |
| `crates/axiomdb-catalog/src/bootstrap.rs` | Pass columns to `create_index` |
| `crates/axiomdb-index/src/tree.rs` | Add `lookup_in`, `insert_in`, `range_in` |
| `crates/axiomdb-index/src/lib.rs` | Re-export new static functions |
| `crates/axiomdb-sql/src/executor.rs` | Update `execute_create_index`, `execute_drop_index` |
| `crates/axiomdb-sql/src/index_maintenance.rs` | New module |
| `crates/axiomdb-sql/src/planner.rs` | New module — `plan_select`, `AccessMethod` |
| `crates/axiomdb-sql/src/executor.rs` | Integrate planner into `execute_select` |
| `crates/axiomdb-core/src/error.rs` | Add `IndexAlreadyExists`, `IndexKeyTooLong`, `UniqueViolation` |
| `docs-site/src/user-guide/features/indexes.md` | User guide — CREATE INDEX, DROP INDEX, uniqueness |
| `docs-site/src/internals/btree.md` | Add static-API section, key encoding |
| `docs-site/src/internals/planner.md` | New page — access method selection |

## ⚠️ DEFERRED

- Composite multi-column indexes (> 1 column) — encoding supported, planner deferred to 6.8
- `EXPLAIN` to show access method chosen — Phase 8
- Cost-based optimizer — Phase 8
- Covering indexes (return data from index without heap read) — Phase 9
- Index-only scans — Phase 9
- Partial indexes (`WHERE` predicate in index) — Phase 10
