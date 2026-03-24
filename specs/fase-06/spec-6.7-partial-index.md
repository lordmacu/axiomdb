# Spec: Partial UNIQUE Index (Phase 6.7)

## What to build (not how)

A partial index is a B-Tree index that covers only the rows matching a WHERE
predicate. The predicate is evaluated at index build time, at INSERT/UPDATE/DELETE
time, and at query planning time. Uniqueness (when `UNIQUE` is specified) is
enforced only among rows satisfying the predicate.

Primary use case: soft-delete uniqueness — `CREATE UNIQUE INDEX uq_email ON
users(email) WHERE deleted_at IS NULL` — enforces unique emails among non-deleted
users without rejecting duplicate emails for deleted records.

Reference: PostgreSQL's `pg_index.indpred` (AST stored as `pg_node_tree`). AxiomDB
stores the predicate as a SQL string (same pattern as `ConstraintDef.check_expr`),
re-parsed once per statement and evaluated row-by-row — equivalent correctness,
simpler implementation.

---

## Inputs / Outputs

### DDL — CREATE INDEX with predicate

**Input:**
```sql
CREATE UNIQUE INDEX uq_active_email ON users(email) WHERE deleted_at IS NULL;
CREATE INDEX idx_pending ON orders(user_id) WHERE status = 'pending';
CREATE UNIQUE INDEX uq_sku ON products(sku) WHERE active = TRUE;
```

**Output:** `QueryResult::Empty` on success.

**Errors:**
- `TableNotFound` — table does not exist
- `IndexAlreadyExists` — index name already exists on this table
- `ColumnNotFound` — indexed column not found in table
- `ParseError` — predicate SQL cannot be parsed
- Any expression evaluation error in predicate during build scan

### DDL — DROP INDEX (unchanged)

Dropping a partial index works identically to dropping a full index. No
special handling needed.

### DML — INSERT

**Input:**
```sql
INSERT INTO users (id, email, deleted_at) VALUES (1, 'a@b.com', NULL);
INSERT INTO users (id, email, deleted_at) VALUES (2, 'a@b.com', '2025-01-01');
```

**Output:**
- Row 1: indexed (predicate `deleted_at IS NULL` satisfied).
- Row 2: **not indexed** (predicate not satisfied). No B-Tree insert. No uniqueness check.

**Uniqueness:**
```sql
INSERT INTO users VALUES (3, 'a@b.com', NULL);  -- ❌ UniqueViolation (rows 1 and 3 both active)
INSERT INTO users VALUES (4, 'a@b.com', '2025-06-01');  -- ✅ not in index predicate
```

### DML — UPDATE

- If the UPDATE changes a row such that the predicate changes from `true` to
  `false`: remove old key from index (treat as delete-side of index maintenance).
- If the predicate changes from `false` to `true`: insert new key into index.
- If the predicate is unchanged: apply normal index key update logic.

### DML — DELETE

- If the deleted row satisfied the predicate: remove its key from the index.
- If the deleted row did not satisfy the predicate: skip index (no key to remove).

### SELECT — planner (conservative)

A partial index is usable for a query only when the planner can verify that
the query's WHERE clause **implies** the index predicate.

**Usable:**
```sql
-- Index: WHERE deleted_at IS NULL
SELECT * FROM users WHERE email = 'a@b.com' AND deleted_at IS NULL;
-- ✅ Query WHERE contains `deleted_at IS NULL` — index usable
```

**Not usable (conservative fallback):**
```sql
SELECT * FROM users WHERE email = 'a@b.com';
-- ❌ Query WHERE does not contain the predicate — index not used (full scan or other index)
```

---

## Catalog: IndexDef on-disk format extension

### Current format (Phase 6.1)
```
[index_id:4][table_id:4][root_page_id:8][flags:1][name_len:1][name bytes]
[ncols:1][col_idx:2 LE, order:1]×ncols
```

### Extended format (Phase 6.7) — backward-compatible
```
[index_id:4][table_id:4][root_page_id:8][flags:1][name_len:1][name bytes]
[ncols:1][col_idx:2 LE, order:1]×ncols
[pred_len:2 LE][pred_sql: utf-8 bytes]   ← NEW; only present if pred_len > 0
```

**Backward-compatibility rule:**
- `from_bytes`: after reading columns, if `bytes.len() > consumed`, read 2-byte
  `pred_len`, then `pred_sql`. If consumed == bytes.len() (old row), `predicate = None`.
- `to_bytes`: if `predicate.is_none()`, write nothing (pred_len bytes omitted).
  Old readers that don't know about the predicate field just stop reading at
  `consumed` after columns — they never see the trailing bytes.

### IndexDef struct addition

```rust
pub struct IndexDef {
    pub index_id: u32,
    pub table_id: TableId,
    pub name: String,
    pub root_page_id: u64,
    pub is_unique: bool,
    pub is_primary: bool,
    pub columns: Vec<IndexColumnDef>,
    /// WHERE predicate as SQL string. `None` = full index (no filter).
    /// Stored after the columns section; absent in pre-6.7 rows (backward-compat).
    pub predicate: Option<String>,
}
```

---

## Parse-once-per-statement pattern

Inspired by PostgreSQL's lazy `ExprState` compilation: the predicate SQL is parsed
**once per statement** before iterating over rows, never per-row.

In `execute_create_index`, `execute_insert_ctx`, `execute_update_ctx`,
`execute_delete_ctx`: before the row loop, compile all partial index predicates
into `Vec<Option<Expr>>` (one entry per index, `None` for full indexes).

```
Before row loop:
  for idx in secondary_indexes:
    compiled_preds.push(
      match &idx.predicate:
        None → None
        Some(sql) → Some(parse(sql)? → analyze()? → Expr)
    )

For each row:
  for (idx, pred) in zip(secondary_indexes, compiled_preds):
    if let Some(pred_expr) = pred:
      if !eval(pred_expr, &row) → skip this index
    // otherwise: proceed with normal insert/delete/uniqueness
```

`insert_into_indexes` and `delete_from_indexes` receive
`compiled_preds: &[Option<Expr>]` (aligned with the `indexes` slice).

---

## Uniqueness semantics

For a partial UNIQUE index `CREATE UNIQUE INDEX idx ON t(col) WHERE predicate`:

1. Row is being inserted with `col = X`.
2. Check predicate against the new row.
3. If predicate is `false`: **skip uniqueness check** and **skip B-Tree insert**. No error.
4. If predicate is `true`: check B-Tree for existing key `X`. If found → `UniqueViolation`.
   If not found → insert into B-Tree.

This guarantees that only rows satisfying the predicate are indexed and checked.

---

## Planner: conservative predicate implication

The planner currently selects indexes for `WHERE col = literal` and
`WHERE col BETWEEN lo AND hi` queries (Phase 6.3). For Phase 6.7, add a
predicate compatibility check before selecting a partial index.

### Algorithm

For each candidate partial index (where `predicate.is_some()`):

1. Parse the index predicate SQL → `pred_expr: Expr`
2. Collect equality and IS NULL conditions from the query's WHERE clause
3. Check: does the query's WHERE **contain** the index predicate?

**Simple implication rules (Phase 6.7 scope):**

| Index predicate | Query WHERE must contain | Match? |
|---|---|---|
| `col IS NULL` | `col IS NULL` | ✅ |
| `col = literal` | `col = same_literal` | ✅ |
| `col = TRUE` | `col = TRUE` or `col` (implicit bool) | ✅ |
| Anything else | Can't verify | ❌ skip index |

If the implication cannot be confirmed → conservative fallback: skip this
partial index. Use a full index or full scan instead. **Always correct.**

---

## Use cases

### 1. Soft-delete unique email
```sql
CREATE TABLE users (id INT PRIMARY KEY, email TEXT, deleted_at TIMESTAMP);
CREATE UNIQUE INDEX uq_email ON users(email) WHERE deleted_at IS NULL;

INSERT INTO users VALUES (1, 'alice@x.com', NULL);           -- ✅ indexed
INSERT INTO users VALUES (2, 'alice@x.com', '2025-01-01');   -- ✅ not indexed
INSERT INTO users VALUES (3, 'alice@x.com', NULL);           -- ❌ UniqueViolation
INSERT INTO users VALUES (4, 'alice@x.com', '2025-06-01');   -- ✅ not indexed
```

### 2. Partial index on status column
```sql
CREATE INDEX idx_pending ON orders(user_id) WHERE status = 'pending';
-- Planner uses index for: WHERE user_id = 42 AND status = 'pending'
-- Planner skips index for: WHERE user_id = 42 (status not constrained)
```

### 3. Numeric predicate
```sql
CREATE UNIQUE INDEX uq_active_sku ON products(sku) WHERE active = TRUE;
-- Only active products need unique SKU
```

### 4. Update changes predicate membership
```sql
-- User 1 (alice@x.com) is active (deleted_at IS NULL)
UPDATE users SET deleted_at = NOW() WHERE id = 1;
-- alice@x.com key removed from partial index (predicate no longer satisfied)

INSERT INTO users VALUES (5, 'alice@x.com', NULL);  -- ✅ now allowed (index was vacated)
```

### 5. DROP INDEX (unchanged behavior)
```sql
DROP INDEX uq_email ON users;  -- ✅ works same as full index
```

### 6. Backward compatibility
```
-- Pre-6.7 database opens without error; partial-index features silently unavailable
-- (indexes created before 6.7 have predicate = None → treated as full indexes)
```

---

## Acceptance criteria

- [ ] `CREATE [UNIQUE] INDEX name ON table(col) WHERE expr` persists predicate in catalog
- [ ] `IndexDef.predicate` is `None` for pre-6.7 rows (backward-compatible deserialization)
- [ ] INSERT into a table with a partial UNIQUE index:
  - Row satisfies predicate → indexed, uniqueness enforced
  - Row does not satisfy predicate → NOT indexed, NO uniqueness check
- [ ] DELETE of a row satisfying the predicate → key removed from index
- [ ] DELETE of a row NOT satisfying the predicate → index unchanged
- [ ] UPDATE that moves a row from predicate=false to predicate=true → key inserted
- [ ] UPDATE that moves a row from predicate=true to predicate=false → key removed
- [ ] Planner uses partial index when query WHERE implies predicate
- [ ] Planner does NOT use partial index when implication cannot be verified
- [ ] `DROP INDEX` on partial index works identically to full index
- [ ] `expr_to_sql_string()` serializes predicate correctly for round-trip
- [ ] Pre-6.7 databases open without error (no migration needed)
- [ ] Integration tests for all acceptance criteria above

---

## Out of scope

- Complex predicate implication (e.g., `col > 5` implies `col > 0`) → Phase 6.9
- Composite predicates with AND/OR → Phase 6.9 (Phase 6.7 handles simple single-clause preds)
- `REINDEX` command → Phase 6.15
- Partial index statistics (planner cost model based on predicate selectivity) → Phase 6.10
- `INCLUDE (col1, col2)` covering indexes → Phase 6.13

## ⚠️ DEFERRED
- Complex predicate implication (AND/OR combinations) → Phase 6.9
- Planner cost estimation for partial index size reduction → Phase 6.10

---

## Dependencies

- Phase 6.1–6.3: Secondary indexes (IndexDef, insert_into_indexes, delete_from_indexes)
- Phase 6.4: Bloom registry (not affected by predicates — bloom ignores predicate)
- `expr_to_sql_string()` — already exists in executor.rs
- `parse()` + `analyze()` — already exist; used to compile predicate once per statement
- `eval()` — already exists; used to evaluate predicate per row
