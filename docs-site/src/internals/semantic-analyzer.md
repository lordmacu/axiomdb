# Semantic Analyzer

The semantic analyzer is the stage between parsing and execution. The parser produces
an AST where every column reference has `col_idx = 0` as a placeholder. The analyzer:

1. Validates all table and column names against the catalog.
2. Resolves each `col_idx` to the correct position in the **combined row** produced
   by the FROM and JOIN clauses.
3. Reports structured errors for unknown tables, unknown columns, and ambiguous
   unqualified column names.

The entry point is `analyze(stmt, storage, snapshot) -> Result<Stmt, DbError>`.

---

## BindContext — Resolution State

`BindContext` is built from the FROM and JOIN clauses of a SELECT before any column
reference is resolved.

```rust
struct BindContext {
    tables: Vec<BoundTable>,
}

struct BoundTable {
    alias:      Option<String>,  // FROM users AS u → alias = Some("u")
    name:       String,          // real table name in the catalog
    columns:    Vec<ColumnDef>,  // columns in declaration order (from CatalogReader)
    col_offset: usize,           // start position in the combined row
}
```

### Building the BindContext

Each table in the FROM clause is added in left-to-right order. The `col_offset`
of each table is the sum of the column counts of all tables added before it.

```
FROM users u JOIN orders o ON u.id = o.user_id

Table 1: users (4 columns: id, name, age, email) → col_offset = 0
Table 2: orders (4 columns: id, user_id, total, status) → col_offset = 4

Combined row layout:
  col 0  u.id
  col 1  u.name
  col 2  u.age
  col 3  u.email
  col 4  o.id
  col 5  o.user_id
  col 6  o.total
  col 7  o.status
```

---

## Column Resolution Algorithm

Given a column reference `(qualifier, name)` from the AST:

### Qualified Reference (`u.email`)

1. Find the `BoundTable` whose alias or name matches `qualifier`.
   - If no table matches: `DbError::TableNotFound { name: qualifier }`.
2. Within that table's `columns`, find the column whose name matches `name`.
   - If not found: `DbError::ColumnNotFound { table: qualifier, column: name }`.
3. Return `col_offset + column_position_within_table`.

```
u.email  →  users.col_offset (0) + position of "email" in users (3) = 3
o.total  →  orders.col_offset (4) + position of "total" in orders (2) = 6
```

### Unqualified Reference (`name` only)

1. Search all tables in `BindContext` for a column named `name`.
2. Collect all matches across all tables.
3. If 0 matches: `DbError::ColumnNotFound`.
4. If 1 match: return the resolved `col_idx`.
5. If 2+ matches: `DbError::AmbiguousColumn { column: name, candidates: [...] }`.

```sql
-- Unambiguous: only users has 'name'
SELECT name FROM users JOIN orders ON ...

-- Ambiguous: both users and orders have 'id'
SELECT id FROM users JOIN orders ON ...
-- ERROR 42702: column reference "id" is ambiguous
-- (appears in: users.id, orders.id)

-- Fix: qualify the reference
SELECT users.id FROM users JOIN orders ON ...
```

---

## Subqueries in FROM

Subqueries in the FROM clause (derived tables) are analyzed recursively:

```sql
SELECT outer.total
FROM (
    SELECT user_id, SUM(total) AS total
    FROM orders
    WHERE status = 'paid'
    GROUP BY user_id
) AS outer
WHERE outer.total > 1000
```

The inner SELECT is analyzed first, producing a virtual `BoundTable` whose columns
are the output columns of the subquery (`user_id`, `total`). The outer `BindContext`
then treats this virtual table exactly like a real catalog table.

---

## What the Analyzer Validates per Statement Type

### SELECT

- FROM clause: every table reference exists in the catalog (or is a valid subquery).
- JOIN conditions: every column in `ON expr` resolves correctly against the BindContext.
- SELECT list: every column reference resolves; computed expressions type-check.
- WHERE clause: every column reference resolves.
- GROUP BY: every expression resolves.
- HAVING: every column reference resolves (must be either in GROUP BY or aggregate).
- ORDER BY: every expression resolves.

### INSERT

- Target table exists in the catalog.
- Each named column in the column list exists in the table.
- If `INSERT ... SELECT`, the inner SELECT is analyzed.
- Column count in VALUES must match the column list (or all non-DEFAULT columns if
  no column list is given).

### UPDATE

- Target table exists in the catalog.
- Every column in SET assignments exists in the table.
- WHERE clause column references resolve against the target table.

### DELETE

- Target table exists in the catalog.
- WHERE clause column references resolve against the target table.

### CREATE TABLE

- No table with the same name exists (unless `IF NOT EXISTS`).
- Each `REFERENCES table(col)` in a foreign key references a table that exists and
  a column that exists in that table and is a primary key or unique column.
- CHECK expressions are parsed and type-checked (must evaluate to boolean).

### DROP TABLE

- Target table exists (unless `IF EXISTS`).
- No other table has a foreign key pointing to the target (unless `CASCADE`).

### CREATE INDEX

- Target table exists in the catalog.
- Every indexed column exists in the table.
- No index with the same name already exists (unless `IF NOT EXISTS`).

---

## Error Types

| Error                   | SQLSTATE | When it occurs                                   |
|-------------------------|----------|--------------------------------------------------|
| `TableNotFound`         | 42P01    | FROM, JOIN, or REFERENCES points to unknown table |
| `ColumnNotFound`        | 42703    | Column name not in any in-scope table             |
| `AmbiguousColumn`       | 42702    | Unqualified column matches in multiple tables     |
| `DuplicateTable`        | 42P07    | CREATE TABLE for an existing table                |
| `TypeMismatch`          | 42804    | Expression type incompatible with column type     |

---

## Snapshot Isolation in the Analyzer

The analyzer calls `CatalogReader::list_tables` and `CatalogReader::list_columns`
with the caller's `TransactionSnapshot`. This means the analyzer sees the schema as
it appeared at the start of the current transaction, not the latest committed schema.

This ensures that:
- A concurrent DDL (`CREATE TABLE`) that commits after the current transaction began
  is invisible to the current transaction's analyzer.
- Schema changes within the same transaction are visible to subsequent statements in
  that same transaction.

---

## Post-Analysis AST

After analysis, every `Expr::Column` in the AST has its `col_idx` set to the correct
position in the combined row. The executor uses `col_idx` to index directly into the
row array — no name lookup occurs at execution time.

```rust
// Before analysis (from parser):
Expr::Column { name: "total".to_string(), table: Some("o".to_string()), col_idx: 0 }

// After analysis (from analyzer):
Expr::Column { name: "total".to_string(), table: Some("o".to_string()), col_idx: 6 }
// col_idx = orders.col_offset (4) + position of "total" in orders (2)
```

This separation of concerns means the executor is a pure interpreter over the analyzed
AST — it never touches the catalog and never performs name resolution. All validation
errors are caught before any I/O begins.
