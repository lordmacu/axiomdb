# Spec: 4.18 — Semantic Analyzer

## What to build (not how)

A semantic analysis pass that transforms a parsed `Stmt` (where all
`Expr::Column.col_idx` are `0` placeholders) into a fully-resolved `Stmt`
where every column reference has the correct index into the combined row
produced by the FROM clause.

Also validates that every table and column reference exists in the catalog,
producing clear, structured errors for any violation.

---

## The core problem: col_idx in the combined row

The parser sets `col_idx = 0` for all column references. The semantic
analyzer replaces those zeros with the actual position of the column in the
row that the executor will process.

**Single-table SELECT** — the row is the table's columns in declaration order:
```
users: [id=0, name=1, age=2, email=3]
SELECT name FROM users WHERE age > 18
  → name: col_idx=1, age: col_idx=2
```

**JOIN** — the row is the concatenation of all joined tables' columns:
```
users:  [id=0, name=1, age=2, email=3]          (4 cols, offset=0)
orders: [id=0, user_id=1, total=2, status=3]    (4 cols, offset=4)
Combined row: [u.id=0, u.name=1, u.age=2, u.email=3,
               o.id=4, o.user_id=5, o.total=6, o.status=7]

SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id
  → u.name:     col_idx = 0 + 1 = 1   (users offset + name position)
  → o.total:    col_idx = 4 + 2 = 6   (orders offset + total position)
  → u.id:       col_idx = 0 + 0 = 0
  → o.user_id:  col_idx = 4 + 1 = 5
```

Rule: `col_idx = table.col_offset + column_position_within_table`

---

## BindContext — the resolution environment

Built from the FROM clause and JOIN clauses before resolving any expressions.

```rust
pub(crate) struct BindContext {
    /// Tables available in this query scope, in order.
    /// col_offset of each table = sum of all preceding tables' column counts.
    tables: Vec<BoundTable>,
}

pub(crate) struct BoundTable {
    pub alias: Option<String>,   // "u" from FROM users AS u
    pub name: String,            // "users" — real catalog name
    pub table_id: TableId,
    pub columns: Vec<ColumnDef>, // from CatalogReader::list_columns()
    pub col_offset: usize,       // start position in the combined row
}
```

The first table has `col_offset = 0`. Each subsequent table's offset is the
sum of all previous tables' column counts.

For subqueries in FROM: the subquery is analyzed recursively, and its result
is treated as a virtual table whose columns are the SELECT list items.

---

## Column resolution algorithm

Given `Expr::Column { col_idx: 0, name: "u.email" }`:

1. Split on `.`: qualifier = `"u"`, field = `"email"`.
2. Find `BoundTable` where `alias == "u"` or `name == "u"`.
3. In that table, find column where `col.name == "email"`, position `p`.
4. `col_idx = table.col_offset + p`.

Given `Expr::Column { col_idx: 0, name: "email" }` (no qualifier):

1. Search all `BoundTable`s for a column named `"email"`.
2. Found in exactly one table → resolve as above.
3. Found in **multiple** tables → `DbError::AmbiguousColumn`.
4. Found in **none** → `DbError::ColumnNotFound` with suggestion.

**Typo suggestion:** if a name is not found, search for columns with
Levenshtein distance ≤ 2 and include the closest match in the hint.

---

## Public API

```rust
/// Semantic analysis pass: validate references and resolve col_idx.
///
/// `stmt` comes from the parser with all `col_idx = 0`. This function
/// returns the same `Stmt` with correct `col_idx` for every column
/// reference, or an error describing the first validation failure.
///
/// The `snapshot` is used to read the catalog at a consistent point in time.
///
/// # Errors
/// - `DbError::TableNotFound`      — unknown table or alias in FROM/JOIN
/// - `DbError::ColumnNotFound`     — column not in any in-scope table
/// - `DbError::AmbiguousColumn`    — unqualified name exists in multiple tables
pub fn analyze(
    stmt: Stmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
) -> Result<Stmt, DbError>
```

---

## Validation scope per statement type

### SELECT
1. Build `BindContext` from `from` and `joins`.
2. Resolve `where_clause` expressions.
3. Resolve `select_list` expressions.
4. Resolve `group_by` expressions.
5. Resolve `having` expression.
6. Resolve `order_by` expressions.
7. Resolve `limit` and `offset` (these are usually literals, but can be col refs).
8. **Subqueries in FROM**: analyze recursively; result columns = subquery SELECT list.

### INSERT
1. Validate target table exists.
2. If `columns` list is provided: validate each column name exists in the table.
3. If `source` is `Select`: analyze the subquery.
4. `Values`: analyze each expression in each row (validate literals — no col refs allowed in VALUES rows without a context).

### UPDATE
1. Validate target table exists.
2. Build single-table `BindContext` from the target table.
3. Validate each `assignment.column` exists in the table; resolve its col_idx.
4. Resolve `where_clause`.

### DELETE
1. Validate target table exists.
2. Build single-table `BindContext`.
3. Resolve `where_clause`.

### CREATE TABLE
1. If any `ColumnConstraint::References { table, column }`: validate that the referenced table exists in the catalog.
2. If column is `Some(col)`: validate that column exists in the referenced table.
3. Validate no duplicate column names within the CREATE TABLE statement.

### DROP TABLE
- If `if_exists = true`: no validation (always succeeds).
- If `if_exists = false`: validate each table in the list exists.

### CREATE INDEX
1. Validate target table exists.
2. Validate each index column exists in the table.

### Other DDL (DROP INDEX, TRUNCATE, ALTER TABLE)
- Validate referenced tables and columns exist.

---

## Error messages (Phase 4.25b structured format)

```
[42P01] TableNotFound:
  message: "table \"usres\" does not exist"
  hint:    "did you mean \"users\"?"

[42703] ColumnNotFound:
  message: "column \"eamil\" does not exist"
  detail:  "table \"users\" has columns: id, name, email, age, created_at"
  hint:    "did you mean \"email\"?"

[42702] AmbiguousColumn:
  message: "column \"id\" is ambiguous"
  detail:  "found in \"users\" (position 0) and \"orders\" (position 0)"
  hint:    "qualify as users.id or orders.id"
```

---

## New DbError variant

```rust
#[error("column reference '{name}' is ambiguous — exists in: {tables}")]
AmbiguousColumn { name: String, tables: String },
```

---

## Use cases

1. **Valid single-table SELECT**: columns resolve to correct indices, no error.

2. **Unknown table**: `SELECT * FROM usres` → `TableNotFound` with typo hint.

3. **Unknown column**: `SELECT eamil FROM users` → `ColumnNotFound` with available columns and hint.

4. **Ambiguous column**: `SELECT id FROM users JOIN orders ON ...` → `AmbiguousColumn`.

5. **Qualified reference resolves ambiguity**: `SELECT users.id FROM users JOIN orders ...` → resolved to `col_idx=0`, no error.

6. **JOIN col_idx correct**: `o.total` in a users+orders JOIN resolves to `col_idx=6` (4 users cols + 2 orders cols).

7. **Alias resolution**: `FROM users AS u WHERE u.age > 18` → `u.age` resolves correctly using alias.

8. **Subquery in FROM**: `FROM (SELECT id, name FROM users) AS sub WHERE sub.name LIKE 'A%'` → subquery analyzed first; outer query sees `sub.id` at col_idx=0, `sub.name` at col_idx=1.

9. **INSERT column validation**: `INSERT INTO users (id, eamil) VALUES (1, 'x')` → `ColumnNotFound { "eamil" }`.

10. **FK reference validation**: `CREATE TABLE orders (user_id BIGINT REFERENCES usres(id))` → `TableNotFound { "usres" }`.

---

## Acceptance criteria

- [ ] `analyze` returns `Ok(stmt)` with all `col_idx` correctly set for single-table SELECT
- [ ] `analyze` returns `Ok(stmt)` with combined-row `col_idx` for multi-table JOIN
- [ ] Alias `FROM users AS u` makes `u.col` resolve correctly
- [ ] `FROM users u` (implicit alias) also works
- [ ] Unqualified column in single table resolves without error
- [ ] Unqualified column present in two JOIN tables → `AmbiguousColumn`
- [ ] Qualified column `t.col` where `t` is unknown → `TableNotFound`
- [ ] Unknown column → `ColumnNotFound` with list of available columns
- [ ] `ColumnNotFound` hint includes closest match (Levenshtein ≤ 2) when found
- [ ] Subquery in FROM is analyzed recursively; outer query sees its projected columns
- [ ] INSERT: unknown column in column list → `ColumnNotFound`
- [ ] UPDATE: unknown SET column → `ColumnNotFound`
- [ ] CREATE TABLE REFERENCES: unknown table → `TableNotFound`
- [ ] DROP TABLE without IF EXISTS: unknown table → `TableNotFound`
- [ ] `DbError::AmbiguousColumn` added to axiomdb-core
- [ ] No `unwrap()` in `src/`

---

## ⚠️ DEFERRED

- Type inference of expressions (what type does `price * 1.1` return?) → Phase 4.18b
- GROUP BY completeness check (all SELECT non-aggregates must be in GROUP BY) → Phase 4.18b
- Subquery expressions (`WHERE id IN (SELECT id FROM t2)`) → Phase 4.11
- Window function scope → Phase 13.2
- INSERT ... VALUES column count vs values count mismatch → Phase 4.5 (executor)

---

## Out of scope

- Executing any statement — Phase 4.5
- Type coercion between incompatible types — Phase 4.18b
- `CASE WHEN` resolution — Phase 4.24

---

## Dependencies

- `axiomdb-sql`: `analyzer.rs` (new), uses `Stmt`, `Expr`, `BinaryOp`
- `axiomdb-catalog`: `CatalogReader`, `ColumnDef`, `SchemaResolver` (Phase 3.14)
- `axiomdb-storage`: `StorageEngine`
- `axiomdb-core`: `DbError` (adds `AmbiguousColumn`)
- `axiomdb-sql/Cargo.toml`: add `axiomdb-catalog` dependency
