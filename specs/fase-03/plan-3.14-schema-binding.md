# Plan: 3.14 — Schema Binding

## Files to create / modify

| File | Action | Description |
|---|---|---|
| `crates/axiomdb-catalog/src/resolver.rs` | CREATE | `ResolvedTable`, `SchemaResolver` |
| `crates/axiomdb-catalog/src/lib.rs` | MODIFY | Add `pub mod resolver` + re-exports |
| `crates/axiomdb-catalog/tests/integration_schema_binding.rs` | CREATE | Integration tests |

No new crates, no new Cargo.toml dependencies.

---

## Algorithm / Data structure

### SchemaResolver internal layout

```rust
pub struct SchemaResolver<'a> {
    reader: CatalogReader<'a>,
    default_schema: &'a str,
}
```

`CatalogReader` already handles the `StorageEngine` + `TransactionSnapshot`
binding. `SchemaResolver` is a thin wrapper that adds:
1. Default schema resolution (`None` → `default_schema`)
2. Qualified error messages (`"public.users"` instead of just `"users"`)
3. `ResolvedTable` aggregation (table + columns + indexes in one call)

### resolve_table algorithm

```
fn resolve_table(schema: Option<&str>, table_name: &str) -> Result<ResolvedTable>:
    schema = schema.unwrap_or(self.default_schema)
    def = self.reader.get_table(schema, table_name)?
           .ok_or(TableNotFound { name: format!("{schema}.{table_name}") })?
    columns = self.reader.list_columns(def.id)?
    indexes = self.reader.list_indexes(def.id)?
    // columns already sorted by col_idx from CatalogReader::list_columns
    return Ok(ResolvedTable { def, columns, indexes })
```

### resolve_column algorithm

```
fn resolve_column(table_id: TableId, col_name: &str) -> Result<ColumnDef>:
    columns = self.reader.list_columns(table_id)?
    for col in columns:
        if col.name == col_name:
            return Ok(col)
    // Build a qualified table name for the error message.
    table_label = self.reader.get_table_by_id(table_id)?
                      .map(|t| format!("{}.{}", t.schema_name, t.table_name))
                      .unwrap_or_else(|| format!("table_id={table_id}"))
    return Err(ColumnNotFound { name: col_name.to_string(), table: table_label })
```

### table_exists algorithm

```
fn table_exists(schema: Option<&str>, table_name: &str) -> Result<bool>:
    schema = schema.unwrap_or(self.default_schema)
    return Ok(self.reader.get_table(schema, table_name)?.is_some())
```

---

## Implementation phases

### Phase 1 — resolver.rs

1. Define `ResolvedTable { def, columns, indexes }`.
2. Define `SchemaResolver<'a> { reader: CatalogReader<'a>, default_schema: &'a str }`.
3. Implement `SchemaResolver::new(storage, snapshot, default_schema)`:
   - Calls `CatalogReader::new(storage, snapshot)` → propagates `CatalogNotInitialized`.
4. Implement `resolve_table(schema, table_name)`.
5. Implement `resolve_column(table_id, col_name)`.
6. Implement `table_exists(schema, table_name)`.

### Phase 2 — lib.rs

1. Add `pub mod resolver;`
2. Re-export: `pub use resolver::{ResolvedTable, SchemaResolver};`

### Phase 3 — integration tests

File: `crates/axiomdb-catalog/tests/integration_schema_binding.rs`

```
test_resolve_table_by_default_schema
test_resolve_table_by_explicit_schema
test_resolve_table_not_found
test_resolve_table_columns_sorted
test_resolve_table_includes_indexes
test_resolve_column_found
test_resolve_column_not_found_error_message
test_table_exists_true
test_table_exists_false
test_mvcc_resolver_does_not_see_uncommitted_table
test_catalog_not_initialized_returns_error
```

---

## Tests to write

### Integration (MemoryStorage + real catalog)

```rust
fn setup() -> (MemoryStorage, TxnManager) { ... }  // same as other tests

#[test]
fn test_resolve_table_by_default_schema() {
    // create "public"."users", commit
    // SchemaResolver::new(storage, snap, "public")
    // resolve_table(None, "users") → Ok(ResolvedTable { def.table_name == "users" })
}

#[test]
fn test_resolve_table_by_explicit_schema() {
    // create "analytics"."events", commit
    // resolve_table(Some("analytics"), "events") → Ok(...)
}

#[test]
fn test_resolve_table_not_found() {
    // resolve_table(None, "ghost") → Err(TableNotFound)
    // error message contains "public.ghost"
}

#[test]
fn test_resolve_table_columns_sorted() {
    // create table with columns col_idx 2, 0, 1 (insertion order)
    // resolve_table → columns[0].col_idx == 0, [1].col_idx == 1, [2].col_idx == 2
}

#[test]
fn test_resolve_table_includes_indexes() {
    // create table + 2 indexes
    // resolve_table → indexes.len() == 2
}

#[test]
fn test_resolve_column_found() {
    // create table with col "email: Text nullable"
    // resolve_column(table_id, "email") → ColumnDef { col_type: Text, nullable: true }
}

#[test]
fn test_resolve_column_not_found_error_message() {
    // resolve_column(table_id, "xyz") → Err(ColumnNotFound)
    // error message contains table name
}

#[test]
fn test_table_exists_true() {
    // create + commit → table_exists(None, name) == true
}

#[test]
fn test_table_exists_false() {
    // table_exists(None, "nonexistent") == false
}

#[test]
fn test_mvcc_resolver_does_not_see_uncommitted_table() {
    // snapshot before begin → create table in txn (not committed)
    // SchemaResolver with old snapshot → resolve_table → Err(TableNotFound)
}

#[test]
fn test_catalog_not_initialized_returns_error() {
    // fresh storage (no init) → SchemaResolver::new → Err(CatalogNotInitialized)
}
```

---

## Anti-patterns to avoid

- **DO NOT** call `list_columns` twice inside `resolve_table` and `resolve_column` —
  if the caller already has `ResolvedTable.columns`, they should iterate those
  rather than calling `resolve_column` again.
- **DO NOT** panic on `unwrap()` in `resolve_column`'s table-label fallback —
  use `unwrap_or_else` with a safe default string.
- **DO NOT** expose `CatalogReader` as a public field — `SchemaResolver` owns the
  reader; callers should not bypass the resolver to access the reader directly.

---

## Risks

| Risk | Mitigation |
|---|---|
| `ColumnNotFound` error message missing table context | `resolve_column` calls `get_table_by_id` for label; falls back to `"table_id=N"` if table itself is gone |
| Default schema mismatch with existing tests | Tests explicitly pass `"public"` as default_schema |
| `resolve_table` called with empty string | Returns `TableNotFound` — empty string is a valid (though unusual) schema name |
