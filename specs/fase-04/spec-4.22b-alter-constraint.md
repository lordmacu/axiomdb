# Spec: 4.22b — ALTER TABLE ADD/DROP CONSTRAINT

## What to build (not how)

Two new ALTER TABLE operations:

1. **`ADD CONSTRAINT name UNIQUE (cols)`** / **`ADD UNIQUE (cols)`** — adds a named
   unique constraint, implemented as a unique index. Reuses the existing
   `CREATE UNIQUE INDEX` infrastructure — zero new catalog schema required for UNIQUE.

2. **`DROP CONSTRAINT name`** — drops a named constraint. Searches `axiom_indexes`
   first (for UNIQUE constraints stored as indexes); if not found, searches
   `axiom_constraints` (for CHECK constraints).

3. **`ADD CONSTRAINT name CHECK (expr)`** — validates the expression against all
   existing rows, then persists the constraint in a new `axiom_constraints` catalog
   table. The check is enforced on every INSERT and UPDATE going forward.

4. **`DROP CONSTRAINT name`** for CHECK — removes the entry from `axiom_constraints`.

**FK constraints** (`ADD CONSTRAINT name FOREIGN KEY`) return `NotImplemented` —
deferred to Phase 6.5/6.6. **PK constraints** (`ADD CONSTRAINT name PRIMARY KEY`)
also return `NotImplemented` — changing PK requires a full table rewrite.

---

## New catalog table: `axiom_constraints`

A minimal fourth system table for storing non-index constraints (currently: CHECK only).

```
axiom_constraints:
  constraint_id  BIGINT AUTO_INCREMENT PRIMARY KEY
  table_id       INT    NOT NULL
  name           TEXT   NOT NULL
  check_expr     TEXT   NOT NULL   -- the SQL expression as a string
```

Stored in the heap under a new `data_root_page_id` in `CatalogBootstrap`.
Encoded/decoded with the existing row codec.

---

## Inputs / Outputs

### Parser changes (`parser/ddl.rs`)

`ALTER TABLE t ADD ...` branch now handles:
```
ADD CONSTRAINT name UNIQUE (cols)   → AlterTableOp::AddConstraint(TableConstraint::Unique)
ADD UNIQUE (cols)                   → AlterTableOp::AddConstraint(TableConstraint::Unique { name: None })
ADD CONSTRAINT name CHECK (expr)    → AlterTableOp::AddConstraint(TableConstraint::Check)
ADD CONSTRAINT name FOREIGN KEY ... → AlterTableOp::AddConstraint(TableConstraint::ForeignKey) [parsed but NotImplemented in executor]
```

`ALTER TABLE t DROP ...` branch now handles:
```
DROP CONSTRAINT [IF EXISTS] name    → AlterTableOp::DropConstraint { name, if_exists: bool }
```

### Executor changes (`executor.rs`)

`AlterTableOp::AddConstraint(TableConstraint::Unique { name, columns })`:
- Auto-generate name if `None`: `axiom_uq_{table}_{col1}_{col2}`
- Validate: columns exist in table, combination is not already indexed uniquely
- Call existing `execute_create_index()` logic with `is_unique = true`
- Result: `QueryResult::Empty`
- Error: `IndexAlreadyExists` if a unique index with that name already exists

`AlterTableOp::AddConstraint(TableConstraint::Check { name, expr })`:
- Validate: `name` is Some (anonymous CHECK not supported in ALTER — require explicit name)
- Scan all existing rows; evaluate `expr` against each row
- If any row fails: `DbError::CheckViolation { table, constraint: name }`
- If all rows pass: insert row into `axiom_constraints`
- Result: `QueryResult::Empty`

`AlterTableOp::DropConstraint { name, if_exists }`:
- Search `axiom_indexes` for an index named `name` on this table → drop it (reuse existing drop logic)
- If not found in indexes: search `axiom_constraints` for a constraint named `name` on this table → delete that row
- If found in neither and `if_exists = false`: `DbError::NotFound { message: "constraint 'name' not found on table 't'" }`
- If found in neither and `if_exists = true`: `QueryResult::Empty` (silent)
- Result: `QueryResult::Empty`

`AlterTableOp::AddConstraint(TableConstraint::ForeignKey { .. })`:
- Return `DbError::NotImplemented { feature: "ADD CONSTRAINT FOREIGN KEY — Phase 6.5" }`

`AlterTableOp::AddConstraint(TableConstraint::PrimaryKey { .. })`:
- Return `DbError::NotImplemented { feature: "ADD CONSTRAINT PRIMARY KEY — requires table rewrite" }`

### CHECK enforcement on INSERT/UPDATE

After `ADD CONSTRAINT name CHECK (expr)`, every subsequent INSERT and UPDATE must
evaluate all active CHECK constraints for the affected table.

The executor already evaluates inline column CHECK constraints (from CREATE TABLE).
The new mechanism: before inserting/updating a row, read all rows from
`axiom_constraints` where `table_id = current_table_id` and evaluate each
`check_expr` against the row values.

---

## Use cases

### 1. ADD UNIQUE — Django UniqueConstraint
```sql
ALTER TABLE users ADD CONSTRAINT uq_users_email UNIQUE (email);
-- Creates unique index named 'uq_users_email' on users(email)
-- Fails if email column has duplicate values: UniqueViolation
-- Fails if index 'uq_users_email' already exists: IndexAlreadyExists
```

### 2. ADD UNIQUE anonymous
```sql
ALTER TABLE users ADD UNIQUE (username);
-- Auto-name: 'axiom_uq_users_username'
```

### 3. DROP CONSTRAINT — removes unique index
```sql
ALTER TABLE users DROP CONSTRAINT uq_users_email;
-- Searches axiom_indexes → finds 'uq_users_email' → drops it
```

### 4. DROP CONSTRAINT IF EXISTS — silent if not found
```sql
ALTER TABLE users DROP CONSTRAINT IF EXISTS uq_users_old;
-- Not found → returns Empty without error
```

### 5. ADD CHECK
```sql
ALTER TABLE orders ADD CONSTRAINT chk_positive_amount CHECK (amount > 0);
-- Scans all existing rows → if any has amount ≤ 0: CheckViolation
-- If all pass → inserts into axiom_constraints
-- Future INSERTs/UPDATEs on orders evaluate 'amount > 0'
```

### 6. DROP CONSTRAINT — removes CHECK
```sql
ALTER TABLE orders DROP CONSTRAINT chk_positive_amount;
-- Not in axiom_indexes → searches axiom_constraints → found → deletes row
-- Future INSERTs no longer check 'amount > 0'
```

### 7. Constraint already exists
```sql
ALTER TABLE users ADD CONSTRAINT uq_users_email UNIQUE (email);
ALTER TABLE users ADD CONSTRAINT uq_users_email UNIQUE (email);  -- second time
-- Error: IndexAlreadyExists { name: "uq_users_email", table: "users" }
```

---

## Acceptance criteria

- [ ] `ALTER TABLE t ADD CONSTRAINT name UNIQUE (col)` creates a unique index and
  enforces uniqueness on existing data (fails with `UniqueViolation` if duplicates exist)
- [ ] `ALTER TABLE t ADD UNIQUE (col)` creates a unique index with auto-generated name
- [ ] `ALTER TABLE t DROP CONSTRAINT name` drops the unique index named `name`
- [ ] `ALTER TABLE t DROP CONSTRAINT IF EXISTS name` is a no-op if constraint not found
- [ ] `ALTER TABLE t ADD CONSTRAINT name CHECK (expr)` validates existing rows and
  persists in `axiom_constraints`; future INSERTs fail with `CheckViolation` if expr is false
- [ ] `ALTER TABLE t DROP CONSTRAINT name` removes the CHECK from `axiom_constraints`;
  future INSERTs no longer check it
- [ ] `ADD CONSTRAINT ... FOREIGN KEY` returns `NotImplemented`
- [ ] `ADD CONSTRAINT ... PRIMARY KEY` returns `NotImplemented`
- [ ] `cargo test --workspace` passes

---

## Out of scope

- `ADD FOREIGN KEY` — Phase 6.5/6.6
- `ADD PRIMARY KEY` — requires full table rewrite (Phase 7+)
- Anonymous CHECK in ALTER TABLE (require explicit name)
- `SHOW CONSTRAINTS` introspection command — Phase 4.20+
- `NOT ENFORCED` deferred constraints
- Per-column constraint metadata in `SHOW COLUMNS` / `DESCRIBE`

---

## Dependencies

- `AlterTableOp::AddConstraint` + `DropConstraint` — in AST ✅
- `parse_table_constraint()` — in parser/ddl.rs ✅
- `execute_create_index()` logic — in executor.rs ✅ (reuse for UNIQUE)
- `execute_drop_index()` logic — in executor.rs ✅ (reuse for DROP)
- `CatalogBootstrap` — needs 4th system table for `axiom_constraints`
- `CatalogWriter` — needs `create_constraint()` + `drop_constraint()`
- `CatalogReader` — needs `list_constraints(table_id)` for CHECK enforcement
- Row codec — already handles TEXT columns ✅

---

## ⚠️ DEFERRED

- `ADD CONSTRAINT FOREIGN KEY` → Phase 6.5
- `ADD CONSTRAINT PRIMARY KEY` → Phase 7+
- Anonymous CHECK in ALTER TABLE → future
- `information_schema.TABLE_CONSTRAINTS` view → Phase 8+

✅ Spec written. You can now switch to `/effort high` for the Plan phase.
