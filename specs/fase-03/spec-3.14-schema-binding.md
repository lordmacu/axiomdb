# Spec: 3.14 — Schema Binding

## What to build (not how)

A name-resolution layer over `CatalogReader` that maps SQL identifier strings
(schema name, table name, column name) to typed catalog metadata. This is the
bridge the Phase 4 executor will use to convert AST node references into the
concrete `TableDef`, `ColumnDef`, and `IndexDef` values needed for plan
construction and type checking.

---

## ResolvedTable

Carries all metadata for a resolved table in a single struct, avoiding
multiple round-trips to the catalog for the common case of resolving a FROM
clause.

```rust
pub struct ResolvedTable {
    /// Table definition (id, schema_name, table_name).
    pub def: TableDef,
    /// All visible columns for this table, sorted by `col_idx`.
    pub columns: Vec<ColumnDef>,
    /// All visible indexes for this table.
    pub indexes: Vec<IndexDef>,
}
```

---

## SchemaResolver

```rust
/// Name resolution service over the catalog.
///
/// Wraps a [`CatalogReader`] and applies a default schema so callers
/// (the executor) can resolve unqualified names like `"users"` without
/// always supplying `"public"`.
///
/// ## Case sensitivity
///
/// All identifier comparisons are case-sensitive in Phase 3.
/// Case-insensitive resolution is deferred to Phase 5 (session charset).
pub struct SchemaResolver<'a> {
    // private
}

impl<'a> SchemaResolver<'a> {
    /// Creates a resolver backed by `storage` at the given `snapshot`.
    ///
    /// `default_schema` is used when `schema` is `None` in subsequent calls.
    /// Pass `"public"` for MySQL-compatible behavior.
    ///
    /// # Errors
    /// - [`DbError::CatalogNotInitialized`] if the catalog has not been bootstrapped.
    pub fn new(
        storage: &'a dyn StorageEngine,
        snapshot: TransactionSnapshot,
        default_schema: &'a str,
    ) -> Result<Self, DbError>

    /// Resolves a table name to its full metadata.
    ///
    /// `schema`: the schema to search. `None` uses `default_schema`.
    ///
    /// Retrieves `TableDef`, all visible `ColumnDef`s (sorted by `col_idx`),
    /// and all visible `IndexDef`s in a single operation.
    ///
    /// # Errors
    /// - [`DbError::TableNotFound`] if no visible table with that name exists
    ///   in the given (or default) schema.
    pub fn resolve_table(
        &self,
        schema: Option<&str>,
        table_name: &str,
    ) -> Result<ResolvedTable, DbError>

    /// Resolves a column name within an already-resolved table.
    ///
    /// Returns the `ColumnDef` so the executor can extract `col_idx`,
    /// `col_type`, and `nullable` for type-checking and plan construction.
    ///
    /// # Errors
    /// - [`DbError::ColumnNotFound`] if no column with that name exists
    ///   in the table's visible columns.
    pub fn resolve_column(
        &self,
        table_id: TableId,
        col_name: &str,
    ) -> Result<ColumnDef, DbError>

    /// Returns `true` if a visible table with the given name exists.
    ///
    /// `schema`: the schema to check. `None` uses `default_schema`.
    ///
    /// Cheaper than `resolve_table` when only existence matters.
    pub fn table_exists(
        &self,
        schema: Option<&str>,
        table_name: &str,
    ) -> Result<bool, DbError>
}
```

---

## Inputs / Outputs

| Operation | Input | Output | Errors |
|---|---|---|---|
| `resolve_table` | `schema: Option<&str>`, `table_name: &str` | `ResolvedTable` | `CatalogNotInitialized`, `TableNotFound`, I/O, `ParseError` |
| `resolve_column` | `table_id: TableId`, `col_name: &str` | `ColumnDef` | `ColumnNotFound`, I/O, `ParseError` |
| `table_exists` | `schema: Option<&str>`, `table_name: &str` | `bool` | `CatalogNotInitialized`, I/O |

---

## Error mapping

| Situation | Error |
|---|---|
| Table not found in schema | `DbError::TableNotFound { name: "<schema>.<table>" }` |
| Column not found in table | `DbError::ColumnNotFound { name: "<col>", table: "<schema>.<table>" }` |
| Catalog not bootstrapped | `DbError::CatalogNotInitialized` |

The `TableNotFound` message uses the qualified form `"schema.table"` so SQL
error messages are unambiguous.

---

## Use cases

1. **Unqualified name with default schema**: `resolve_table(None, "users")` with
   `default_schema = "public"` → same as `resolve_table(Some("public"), "users")`.

2. **Qualified name**: `resolve_table(Some("analytics"), "events")` → searches
   `schema_name = "analytics"`.

3. **Table not found**: `resolve_table(None, "nonexistent")` → `Err(TableNotFound)`.

4. **Column resolution**: after `resolve_table` returns `ResolvedTable`, call
   `resolve_column(table_id, "email")` → `ColumnDef { col_idx: 2, col_type: Text, nullable: true }`.

5. **Column not found**: `resolve_column(table_id, "xyz")` →
   `Err(ColumnNotFound { name: "xyz", table: "public.users" })`.

6. **table_exists — positive**: table was created and committed → `true`.

7. **table_exists — negative**: table was dropped or never created → `false`.

8. **MVCC isolation**: resolver is constructed with a snapshot. A table created
   by a concurrent uncommitted transaction is NOT visible.

9. **Columns sorted**: `resolve_table` always returns `columns` sorted by
   `col_idx`, regardless of insertion order.

---

## Acceptance criteria

- [ ] `ResolvedTable` has `def: TableDef`, `columns: Vec<ColumnDef>` (sorted by `col_idx`), `indexes: Vec<IndexDef>`
- [ ] `SchemaResolver::new` returns `Err(CatalogNotInitialized)` if catalog is not bootstrapped
- [ ] `resolve_table(None, name)` uses `default_schema`
- [ ] `resolve_table(Some(schema), name)` uses the given schema
- [ ] `resolve_table` returns `Err(TableNotFound)` for unknown tables
- [ ] `resolve_table` returns `ResolvedTable` with correctly populated `columns` and `indexes`
- [ ] `columns` in `ResolvedTable` are sorted by `col_idx`
- [ ] `resolve_column` returns `ColumnDef` for known columns
- [ ] `resolve_column` returns `Err(ColumnNotFound)` for unknown columns
- [ ] `table_exists` returns `true` for a committed table
- [ ] `table_exists` returns `false` for a non-existent table
- [ ] MVCC: resolver with snapshot taken before a commit does NOT see new tables
- [ ] No `unwrap()` in `src/`

---

## ⚠️ DEFERRED

- **Case-insensitive resolution** — deferred to Phase 5 (session charset/collation settings)
- **Wildcard expansion** (`SELECT *`) — the resolver provides `ResolvedTable.columns`; the executor handles `*` expansion using that list (Phase 4.5)
- **Schema introspection** (`SHOW SCHEMAS`) — Phase 4.20
- **Ambiguous column resolution** (same column name in multiple JOIN sources) — Phase 4.8 (JOIN)
- **`information_schema` virtual tables** — Phase 4.20

---

## Out of scope

- Parsing SQL — the resolver receives already-extracted identifier strings
- Plan construction — the resolver returns metadata; the planner uses it
- Type coercion — the resolved `ColumnType` is returned as-is; coercion is Phase 4.18b

---

## Dependencies

- `axiomdb-catalog`: `CatalogReader`, `TableDef`, `ColumnDef`, `IndexDef`, `TableId`
- `axiomdb-core`: `DbError`, `TransactionSnapshot`
- `axiomdb-storage`: `StorageEngine` (passed through to `CatalogReader::new`)
