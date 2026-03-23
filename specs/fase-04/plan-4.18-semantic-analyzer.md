# Plan: 4.18 — Semantic Analyzer

## Files to create / modify

| File | Action | Description |
|---|---|---|
| `crates/nexusdb-sql/src/analyzer.rs` | CREATE | `analyze()`, `BindContext`, `BoundTable`, resolver functions |
| `crates/nexusdb-sql/src/lib.rs` | MODIFY | `pub mod analyzer` + re-export `analyze` |
| `crates/nexusdb-sql/Cargo.toml` | MODIFY | Add `nexusdb-catalog = { workspace = true }` |
| `crates/nexusdb-core/src/error.rs` | MODIFY | Add `AmbiguousColumn` variant |
| `crates/nexusdb-sql/tests/integration_analyzer.rs` | CREATE | Integration tests |

---

## Algorithm

### Phase 1 — BindContext construction

```rust
fn build_context(
    from: &Option<FromClause>,
    joins: &[JoinClause],
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_schema: &str,
) -> Result<BindContext, DbError>
```

```
context = BindContext { tables: [] }
col_offset = 0

if from.is_some():
    bound = bound_table_from_from_clause(from, storage, snapshot, default_schema)?
    bound.col_offset = col_offset
    col_offset += bound.columns.len()
    context.tables.push(bound)

for join in joins:
    bound = bound_table_from_from_clause(&join.table, storage, snapshot, default_schema)?
    bound.col_offset = col_offset
    col_offset += bound.columns.len()
    context.tables.push(bound)

return Ok(context)
```

For `FromClause::Table(table_ref)`:
- Look up `table_ref.name` in catalog using `CatalogReader`
- Load columns with `CatalogReader::list_columns(table_id)`
- `BoundTable { alias: table_ref.alias, name: table_ref.name, table_id, columns, col_offset: 0 }`

For `FromClause::Subquery { query, alias }`:
- Recursively analyze the inner SELECT (call `analyze_select`)
- Build `BoundTable` where columns = the SELECT list items of the inner query
- Each column in the virtual table gets col_idx 0, 1, 2, ... (compact)

### Phase 2 — Expression resolution

```rust
fn resolve_expr(expr: Expr, ctx: &BindContext) -> Result<Expr, DbError>
```

Recursive walk:

```
match expr:
    Literal(v) → Ok(Literal(v))   // no column refs

    Column { col_idx: _, name } →
        (qualifier, field) = split_name(name)
        if qualifier.is_some():
            table = find_table_by_name_or_alias(ctx, qualifier)?
            (pos, col) = find_column_in_table(table, field)?
            Ok(Column { col_idx: table.col_offset + pos, name })
        else:
            // search all tables
            matches = find_column_all_tables(ctx, field)
            match matches.len():
                0 → Err(ColumnNotFound with hint)
                1 → Ok(Column { col_idx: matches[0].col_offset + matches[0].pos, name })
                _ → Err(AmbiguousColumn { name, tables: list of table names })

    UnaryOp { op, operand } →
        Ok(UnaryOp { op, operand: Box::new(resolve_expr(*operand, ctx)?) })

    BinaryOp { op, left, right } →
        Ok(BinaryOp { op,
            left: Box::new(resolve_expr(*left, ctx)?),
            right: Box::new(resolve_expr(*right, ctx)?) })

    IsNull { expr, negated } →
        Ok(IsNull { expr: Box::new(resolve_expr(*expr, ctx)?), negated })

    Between { expr, low, high, negated } →
        resolve all three sub-expressions

    Like { expr, pattern, negated } →
        resolve both

    In { expr, list, negated } →
        resolve expr and each item in list

    Function { name, args } →
        args = args.into_iter().map(|a| resolve_expr(a, ctx)).collect::<Result<_,_>>()?
        Ok(Function { name, args })
        // function name validation deferred to Phase 4.19 (built-in functions)
```

### Phase 3 — Statement-level analysis

#### analyze_select

```rust
fn analyze_select(
    mut s: SelectStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_schema: &str,
) -> Result<SelectStmt, DbError>
```

```
// 1. Build context from FROM and JOINs
ctx = build_context(&s.from, &s.joins, storage, snapshot, default_schema)?

// 2. Resolve JOIN conditions
for join in &mut s.joins:
    match join.condition:
        On(expr) → join.condition = On(resolve_expr(expr, &ctx)?)
        Using(cols) → validate each col exists in BOTH joined tables

// 3. Resolve WHERE
s.where_clause = s.where_clause.map(|e| resolve_expr(e, &ctx)).transpose()?

// 4. Resolve GROUP BY
s.group_by = s.group_by.into_iter()
    .map(|e| resolve_expr(e, &ctx))
    .collect::<Result<_,_>>()?

// 5. Resolve HAVING
s.having = s.having.map(|e| resolve_expr(e, &ctx)).transpose()?

// 6. Resolve SELECT list
s.columns = s.columns.into_iter()
    .map(|item| resolve_select_item(item, &ctx))
    .collect::<Result<_,_>>()?

// 7. Resolve ORDER BY
s.order_by = s.order_by.into_iter()
    .map(|item| resolve_order_item(item, &ctx))
    .collect::<Result<_,_>>()?

// 8. Resolve LIMIT / OFFSET (usually literals, but could be expressions)
s.limit = s.limit.map(|e| resolve_expr(e, &ctx)).transpose()?
s.offset = s.offset.map(|e| resolve_expr(e, &ctx)).transpose()?

Ok(s)
```

For `SelectItem::Wildcard` and `SelectItem::QualifiedWildcard(table)`: no
column resolution needed — the executor expands these at runtime. For
`QualifiedWildcard`, validate the table name/alias is in scope.

#### analyze_insert

```
validate table exists → get columns → validate column list (if provided)
if source is Select: analyze recursively
if source is Values: no column refs expected, just literal expressions (validate they parse cleanly)
```

#### analyze_update

```
validate table → build single-table context
for each assignment: validate column exists; set assignment col_idx
resolve where_clause
```

#### analyze_delete

```
validate table → build single-table context → resolve where_clause
```

#### analyze_ddl (CREATE/DROP TABLE, CREATE/DROP INDEX)

```
CREATE TABLE: for each FK References{table}: validate table exists; if column: validate column
DROP TABLE: if !if_exists: validate each table exists
CREATE INDEX: validate table exists; validate each index column exists
DROP INDEX: no validation (index name lookup at execute time)
```

### Phase 4 — Levenshtein hint for typos

```rust
fn closest_match<'a>(name: &str, candidates: impl Iterator<Item=&'a str>) -> Option<&'a str> {
    candidates
        .filter(|c| levenshtein(name, c) <= 2)
        .min_by_key(|c| levenshtein(name, c))
}

fn levenshtein(a: &str, b: &str) -> usize {
    // Standard DP implementation, O(|a|*|b|)
    // Both strings are short (≤ 64 chars, 4.3d limit), so this is fast
}
```

---

## Implementation phases

### Phase 1 — nexusdb-core: add AmbiguousColumn
Add `AmbiguousColumn { name: String, tables: String }` to `error.rs`.

### Phase 2 — Cargo.toml
Add `nexusdb-catalog = { workspace = true }` to `nexusdb-sql/Cargo.toml`.

### Phase 3 — analyzer.rs: data structures
1. `BoundTable` struct.
2. `BindContext` struct with helper methods:
   - `find_by_qualifier(name: &str) -> Option<&BoundTable>`
   - `find_column(col_name: &str) -> Vec<(usize, &BoundTable, &ColumnDef)>`
3. `split_name(name: &str) -> (Option<&str>, &str)` — splits "u.email" into ("u", "email").

### Phase 4 — analyzer.rs: BindContext construction
`build_context` function.

### Phase 5 — analyzer.rs: expression resolution
`resolve_expr` function — recursive walk.

### Phase 6 — analyzer.rs: statement analysis
`analyze_select`, `analyze_insert`, `analyze_update`, `analyze_delete`,
`analyze_create_table`, `analyze_drop_table`, `analyze_create_index`.

### Phase 7 — analyzer.rs: levenshtein + public entry point
`levenshtein` helper, `closest_match`, public `analyze` function.

### Phase 8 — lib.rs
Add `pub mod analyzer` and `pub use analyzer::analyze`.

### Phase 9 — Integration tests
File: `crates/nexusdb-sql/tests/integration_analyzer.rs`

Tests:
```
valid_single_table_select_resolves_col_idx
valid_join_select_resolves_combined_col_idx
alias_from_resolves_correctly
implicit_alias_resolves_correctly
wildcard_select_no_error
unknown_table_returns_error
unknown_column_returns_error_with_hint
ambiguous_column_returns_error_with_detail
qualified_column_resolves_ambiguity
subquery_from_builds_virtual_table
insert_validates_column_list
update_validates_set_columns
delete_resolves_where
create_table_validates_fk_reference
drop_table_validates_existence
levenshtein_hint_for_close_typo
no_hint_for_very_different_name
```

---

## Anti-patterns to avoid

- **DO NOT** panic on empty `BindContext` — SELECT without FROM (SELECT 1) has
  no tables; `resolve_expr` on literals works fine with an empty context.
- **DO NOT** assume a single table when multiple tables share a column name —
  always check all tables and report ambiguity.
- **DO NOT** mutate `col_idx` in-place on a shared reference — clone the Expr
  node when resolving (Expr derives Clone).
- **DO NOT** add `nexusdb-catalog` to `nexusdb-types` — only to `nexusdb-sql`.
- **DO NOT** call `CatalogReader` more than once per table — cache the column
  list in `BoundTable` at context-build time.

---

## Risks

| Risk | Mitigation |
|---|---|
| SELECT without FROM (SELECT 1) needs empty context | `build_context` with `from=None` returns empty `BindContext`; resolve_expr on literals ignores context |
| Subquery columns don't have real table_id | Use `table_id = 0` as virtual; columns built from SELECT list items |
| JOIN USING(col) needs col in BOTH tables | Check both tables' column lists; error if missing from either |
| Levenshtein on very long strings is slow | Max identifier length is 64 chars (4.3d); O(64²) = O(4096) is fine |
| circular subquery reference | Not possible with SQL syntax — parsers don't allow forward references |
