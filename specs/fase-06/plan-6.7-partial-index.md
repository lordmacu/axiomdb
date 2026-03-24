# Plan: Partial UNIQUE Index (Phase 6.7)

## Files to create / modify

### Create
- `crates/axiomdb-sql/tests/integration_partial_index.rs` — all partial index tests

### Modify
- `crates/axiomdb-catalog/src/schema.rs` — `IndexDef.predicate: Option<String>` + serde
- `crates/axiomdb-sql/src/ast.rs` — `CreateIndexStmt.predicate: Option<Expr>`
- `crates/axiomdb-sql/src/parser/ddl.rs` — parse `WHERE expr` in CREATE INDEX
- `crates/axiomdb-sql/src/index_maintenance.rs` — new `compiled_preds` param
- `crates/axiomdb-sql/src/planner.rs` — predicate implication check in `find_index_on_col`
- `crates/axiomdb-sql/src/executor.rs` — helper + execute_create_index + all DML callers

---

## Algorithm / Data structures

### On-disk format extension (backward-compatible)

Current `IndexDef.to_bytes()` ends with:
```
[ncols:1][col_idx:2 LE, order:1]×ncols
```

Extended (append after columns section):
```
[pred_len:2 LE][pred_sql: pred_len UTF-8 bytes]
```

`from_bytes`: after reading columns, if `bytes.len() > consumed`:
- Read `pred_len: u16` at `consumed`
- If `pred_len == 0` → `predicate = None`; consumed += 2
- Else → read `pred_sql` bytes; `predicate = Some(string)`; consumed += 2 + pred_len

`to_bytes`: if `predicate.is_none()` → write nothing (old format, old readers stop here).
If `predicate.is_some()` → write `[pred_len:2][pred_sql bytes]`.

### Predicate compilation helper (new in executor.rs)

```rust
/// Compiles partial index predicates from SQL strings to evaluated Expr trees.
/// Returns a Vec parallel to `indexes`: None for full indexes, Some(Expr) for partial.
/// Column references in predicates are resolved against `col_defs`.
fn compile_partial_index_predicates(
    indexes: &[IndexDef],
    col_defs: &[axiomdb_catalog::schema::ColumnDef],
) -> Result<Vec<Option<Expr>>, DbError>
```

Algorithm per index:
1. If `idx.predicate.is_none()` → push `None`
2. If `idx.predicate.is_some(sql)`:
   a. `parse(format!("SELECT 1 WHERE {sql}"), None)?` → `Stmt::Select(s)`
   b. Extract `s.where_clause.unwrap()` → `Expr` with col_idx=0 placeholders
   c. `resolve_predicate_columns(expr, col_defs)?` → `Expr` with correct col_idx
   d. Push `Some(resolved_expr)`

### Column resolver (new private fn in executor.rs)

```rust
fn resolve_predicate_columns(
    expr: Expr,
    col_defs: &[axiomdb_catalog::schema::ColumnDef],
) -> Result<Expr, DbError>
```

Walks the Expr tree recursively. For `Expr::Column { name, .. }`:
- Find `col_def` where `col_def.name == name`
- Replace with `Expr::Column { name, col_idx: col_def.col_idx as usize }`
- Return `ColumnNotFound` if not resolved

Handles: `BinaryOp`, `UnaryOp`, `IsNull`, `Literal`, `Column`. Other node types
(subqueries, OuterColumn, etc.) → `Err(NotImplemented)` since they cannot appear
in simple index predicates.

### insert_into_indexes — new signature

```rust
pub fn insert_into_indexes(
    indexes: &[IndexDef],
    row: &[Value],
    rid: RecordId,
    storage: &mut dyn StorageEngine,
    bloom: &mut BloomRegistry,
    compiled_preds: &[Option<Expr>],   // ← NEW; parallel to indexes
) -> Result<Vec<(u32, u64)>, DbError>
```

Inside the loop:
```rust
for (i, idx) in indexes.iter().enumerate()
    .filter(|(_, i)| !i.is_primary && !i.columns.is_empty())
{
    // Partial index predicate check (Phase 6.7).
    if let Some(Some(pred)) = compiled_preds.get(i) {
        let pred_result = crate::eval::eval(pred, row)?;
        if !crate::eval::is_truthy(&pred_result) {
            continue; // row doesn't satisfy predicate → skip this index
        }
    }
    // ... existing uniqueness check and B-Tree insert
}
```

Callers that don't have partial indexes pass `&[]` (empty slice). Since
`compiled_preds.get(i)` returns `None` for out-of-bounds, this is equivalent
to "no predicate" for every index.

### delete_from_indexes — new signature

```rust
pub fn delete_from_indexes(
    indexes: &[IndexDef],
    row: &[Value],
    storage: &mut dyn StorageEngine,
    bloom: &mut BloomRegistry,
    compiled_preds: &[Option<Expr>],   // ← NEW; parallel to indexes
) -> Result<Vec<(u32, u64)>, DbError>
```

Same predicate check before each B-Tree delete — if predicate not satisfied,
skip deletion (the row was never in the index, no entry to remove).

### Planner — predicate implication check

In `find_index_on_col`, add a guard for partial indexes:

```rust
fn find_index_on_col<'a>(
    col_name: &str,
    indexes: &'a [IndexDef],
    columns: &[ColumnDef],
    query_where: Option<&Expr>,   // ← NEW parameter (the query's WHERE clause)
) -> Option<&'a IndexDef> {
    let col_idx = columns.iter().find(|c| c.name == col_name)?.col_idx;
    indexes.iter().find(|idx| {
        if idx.is_primary { return false; }
        if idx.columns.first().map(|c| c.col_idx) != Some(col_idx) { return false; }
        // Partial index: only usable if query WHERE implies predicate.
        if let Some(pred_sql) = &idx.predicate {
            predicate_implied_by_query(pred_sql, query_where, columns)
        } else {
            true  // full index — always usable
        }
    })
}

/// Conservative implication check: returns true if we can verify that
/// the query WHERE clause implies the index predicate.
fn predicate_implied_by_query(
    pred_sql: &str,
    query_where: Option<&Expr>,
    columns: &[ColumnDef],
) -> bool
```

`predicate_implied_by_query` algorithm (Phase 6.7 scope):

1. Parse `pred_sql` to `Expr` (same parse + resolve as compile helper)
2. If `query_where.is_none()` → return `false` (no WHERE → can't imply anything)
3. Extract AND-clauses from `query_where`
4. Check if any AND-clause matches the predicate:
   - `col IS NULL` matches if query has `col IS NULL`
   - `col = literal` matches if query has `col = same_literal`
   - `col = TRUE` matches if query has `col = TRUE`
5. If any AND-clause matches → return `true`
6. Otherwise → return `false` (conservative)

AND-clause extraction: walk `BinaryOp(And, left, right)` recursively,
collecting leaf expressions.

### Planner wiring

`plan_select` passes `query_where` to `find_index_on_col` in Rule 1 and Rule 2.
`extract_range` also receives `query_where`.

---

## Implementation phases

### Phase 1 — Catalog + AST + Parser

**Step 1.1** — `schema.rs`: Add `predicate: Option<String>` to `IndexDef`.
Update `to_bytes()` / `from_bytes()` with backward-compatible predicate section.
Update all struct literal constructions of `IndexDef` in tests and executor
to include `predicate: None`.

**Step 1.2** — `ast.rs`: Add `predicate: Option<Expr>` to `CreateIndexStmt`.

**Step 1.3** — `parser/ddl.rs` in `parse_create_index`: after `p.expect(&Token::RParen)?`
for the columns list, add:
```rust
let predicate = if p.eat(&Token::Where) {
    Some(parse_expr(p)?)
} else {
    None
};
Ok(Stmt::CreateIndex(CreateIndexStmt { ..., predicate }))
```

**Step 1.4** — `executor.rs` in `execute_create_index`: serialize predicate.
After the existing bloom population, before returning:
```rust
// Store predicate as SQL string for persistence.
let predicate_str = stmt.predicate.as_ref().map(|e| expr_to_sql_string(e));
```
Pass `predicate: predicate_str` to the `IndexDef` in `writer.create_index()`.

**Verify:** `cargo build --workspace` clean. `cargo test -p axiomdb-sql` passes.

---

### Phase 2 — Build + Maintenance

**Step 2.1** — `executor.rs`: Add private helpers:
- `compile_partial_index_predicates(indexes, col_defs) -> Result<Vec<Option<Expr>>>`
- `resolve_predicate_columns(expr, col_defs) -> Result<Expr>`

**Step 2.2** — `executor.rs` in `execute_create_index`: filter rows during build.
Before the existing row loop, compile the predicate once:
```rust
let pred_expr: Option<Expr> = if let Some(sql) = &predicate_str {
    // Use a dummy col_defs with just the table's columns.
    let compiled = compile_partial_index_predicates(
        &[IndexDef { predicate: Some(sql.clone()), ..dummy }],
        &col_defs,
    )?;
    compiled.into_iter().next().flatten()
} else {
    None
};
```
In the row loop, add:
```rust
// Partial index: skip rows that don't satisfy the predicate.
if let Some(pred) = &pred_expr {
    if !is_truthy(&eval(pred, &row_vals)?) {
        continue;
    }
}
```

**Step 2.3** — `index_maintenance.rs`: update `insert_into_indexes` and
`delete_from_indexes` signatures and inner loop (see algorithm above).

**Step 2.4** — Update all callers of `insert_into_indexes` and `delete_from_indexes`:

| Caller | Change |
|--------|--------|
| `execute_insert_ctx` (VALUES path) | Pre-compile with `compile_partial_index_predicates`; pass result |
| `execute_insert_ctx` (SELECT path) | Same |
| `execute_insert` (single row) | Pass `&[]` (no ctx, no col_defs, no partial index optimization — deferred) |
| `execute_insert` (multi-row batch) | Pass `&[]` |
| `execute_insert` (SELECT path) | Pass `&[]` |
| `execute_update_ctx` | Pre-compile; pass to both delete_from and insert_into |
| `execute_update` | Pass `&[]` |
| `execute_delete_ctx` | Pre-compile; pass to delete_from |
| `execute_delete` | Pass `&[]` |
| `fk_enforcement.rs` (CASCADE) | Pass `&[]` |
| `execute_create_index` (during build) | Handled separately in step 2.2 |

Note: non-ctx paths (`execute_insert`, `execute_update`, `execute_delete`) pass
`&[]` (empty slice = treat all indexes as full). Partial index predicate evaluation
for these paths is deferred to Phase 6.9. Correctness is maintained because:
- Non-ctx paths are not used by the embedded or network layers (which use
  `execute_with_ctx`)
- `&[]` means "no predicate filtering" → rows are inserted into partial index even
  when predicate is false → over-indexing, which is never incorrect (uniqueness
  checks fire for rows that shouldn't be there, but those would be found by
  `BTree::lookup_in` and returned as a violation — which is conservative)

Actually — this IS a correctness issue: if non-ctx INSERT inserts a row that
doesn't satisfy the predicate, it gets added to the partial index. Then a
subsequent ctx INSERT of a row that DOES satisfy the predicate would see a
false UniqueViolation. To avoid this: the non-ctx paths must also compile
and pass predicates.

**Revised:** All callers must compile predicates. For non-ctx paths, we need
the column definitions. These are already loaded in non-ctx paths via the
`resolved` struct or `CatalogReader`. Add the compile step there too.

The pattern for non-ctx paths (they already have `schema_cols` loaded):
```rust
let compiled_preds = compile_partial_index_predicates(&secondary_indexes, &schema_cols)?;
// ... pass compiled_preds to insert_into_indexes / delete_from_indexes
```

**Verify:** `cargo test --workspace` clean with 0 failures.

---

### Phase 3 — Planner

**Step 3.1** — `planner.rs`: Update `find_index_on_col` signature to accept
`query_where: Option<&Expr>`.

**Step 3.2** — `planner.rs`: Add `predicate_implied_by_query` function (see
algorithm above). Uses `parse()` + `resolve_predicate_columns()` from executor.

Wait — `planner.rs` is in the same crate as executor.rs (`axiomdb-sql`). The
`compile_partial_index_predicates` and `resolve_predicate_columns` helpers in
`executor.rs` should be moved to a shared module or made `pub(crate)` so that
`planner.rs` can use them without circular dependency.

**Resolution:** Move `resolve_predicate_columns` to a new module
`crates/axiomdb-sql/src/partial_index.rs` (or `index_pred.rs`), make it
`pub(crate)`. Both `executor.rs` and `planner.rs` import from there.

**Step 3.3** — `planner.rs` in `plan_select`: pass `where_clause` to
`find_index_on_col` in Rule 1 and Rule 2 path.

**Step 3.4** — `executor.rs` in `execute_select_ctx` (IndexLookup path):
the bloom shortcut for absent keys is unchanged (partial indexes that have
an entry guarantee it was predicate-satisfying when inserted).

**Verify:** Planner tests, partial index planner test pass.

---

## Tests to write

### Unit tests (in `schema.rs`)
```rust
fn test_index_def_roundtrip_no_predicate()   // predicate = None, backward compat
fn test_index_def_roundtrip_with_predicate() // predicate = Some("deleted_at IS NULL")
fn test_index_def_old_format_no_predicate()  // bytes without pred_len → predicate = None
```

### Integration tests (`tests/integration_partial_index.rs`)
```rust
// DDL
fn test_create_partial_index_persists_predicate()
fn test_create_partial_unique_index()
fn test_create_partial_index_invalid_predicate_error()

// INSERT — predicate satisfied
fn test_insert_satisfies_predicate_indexed()
// INSERT — predicate not satisfied
fn test_insert_not_satisfies_predicate_not_indexed()
// Uniqueness: both active → violation
fn test_partial_unique_violation_both_active()
// Uniqueness: one active, one deleted → ok
fn test_partial_unique_no_violation_one_deleted()
// Uniqueness: both deleted → ok
fn test_partial_unique_no_violation_both_deleted()
// Uniqueness: NULL FK value → ok (NULL not indexed)
fn test_partial_unique_null_passes()

// UPDATE
fn test_update_moves_row_into_predicate()
fn test_update_moves_row_out_of_predicate()
fn test_update_row_stays_in_predicate()

// DELETE
fn test_delete_row_satisfying_predicate_removes_from_index()
fn test_delete_row_not_satisfying_predicate_no_op()

// DROP INDEX
fn test_drop_partial_index()

// Planner
fn test_planner_uses_partial_index_when_where_implies_predicate()
fn test_planner_skips_partial_index_when_where_does_not_imply_predicate()
fn test_planner_uses_full_index_even_with_partial_available()

// Backward compatibility
fn test_pre_67_index_opens_without_predicate()
```

---

## Anti-patterns to avoid

- **DO NOT** leave partial indexes with partial inserts: if predicate check is
  skipped in non-ctx path, partial indexes become over-indexed (stale entries).
  Every INSERT path must evaluate the predicate.

- **DO NOT** use `expr_to_sql_string` for complex predicates not yet supported
  (subqueries, aggregate functions). The parser ensures they can't be written;
  the `resolve_predicate_columns` returns `Err(NotImplemented)` for unhandled
  Expr variants as a safety net.

- **DO NOT** share `compiled_preds` across statements. Compile fresh per
  statement invocation. `Expr` is not `Send`/`Sync` safe across concurrent use.

- **DO NOT** add predicate to primary key indexes (created in `execute_create_table`
  for `PRIMARY KEY` and `UNIQUE` constraints). These are always full indexes
  (`predicate: None`). The `create_empty_index` helper already sets `predicate: None`.

- **DO NOT** change the uniqueness check logic in `insert_into_indexes` to skip
  for partial indexes when the predicate is satisfied — the existing check
  (`idx.is_unique && BTree::lookup_in(...)`) is already correct after the
  predicate gate added in Step 2.3.

---

## Risks

| Risk | Mitigation |
|------|-----------|
| Non-ctx paths over-index partial indexes (insert rows outside predicate) | Compile predicates in all paths including non-ctx |
| `resolve_predicate_columns` fails on valid SQL that uses unsupported Expr variants | Return `Err(NotImplemented)`; this surfaces as a clear error at CREATE INDEX time, not silently at query time |
| Planner parse overhead for partial index predicate on every query | Cache parsed predicate in `IndexDef` within `SessionContext` (Phase 6.9 optimization); for Phase 6.7 re-parse is O(microseconds) per query |
| `expr_to_sql_string` doesn't round-trip all expressions correctly | Only simple predicates (IS NULL, = literal) are needed in Phase 6.7; tested with roundtrip unit tests |
| Backward-compat: existing databases have IndexDef rows without pred_len | `from_bytes` checks `bytes.len() > consumed` before reading pred_len — old rows naturally have `predicate = None` |
