# Plan: 21.8 — Expression indexes

Phase: 21 — Advanced SQL
Task: 21.8 Expression indexes
Spec: specs/fase-21/spec-21.8-expression-indexes.md
Status: completed

## Files to create/modify

### `axiomdb-catalog/src/schema_database.rs`
- Add `expr: Option<String>` field to `IndexColumnDef`
- Update `to_bytes` to append `[expr_len: 2 LE][expr_sql]` after each column's order byte (when `expr` is `Some`)
- Update `from_bytes` to read the optional expression section per column (backward-compatible with 3-byte entries from old catalogs)

### `axiomdb-catalog/src/schema_index.rs`
- `IndexDef::to_bytes`: no change needed — `IndexColumnDef` handles its own serialization
- `IndexDef::from_bytes`: no structural change — column loop delegates to `IndexColumnDef::from_bytes`

### `axiomdb-sql/src/executor/ddl_create_index.rs`
- In `execute_create_index`, step 3 (build `IndexColumnDef` list): when `ic.expr` is `Some`, serialize the `Expr` to SQL text via `expr_to_sql_string` and store in `IndexColumnDef.expr`
- In step 5 (heap scan + index build): for expression columns, evaluate the compiled expression per row instead of direct column access
- In `build_index_root_from_heap` and `build_index_root_from_clustered`: pass the per-column compiled expressions through to the row loop

### `axiomdb-sql/src/index_maintenance.rs`
- Add `compile_index_exprs` function (mirrors `compile_index_predicates` from `partial_index.rs`): parses each column's SQL expression to a resolved `Expr` using the same `SELECT 1 WHERE <expr>` wrapping pattern
- Add `index_key_values_if_indexed` overload or extend existing to handle expression columns: evaluates the compiled expression to get key values instead of reading `row[col_idx]`
- Update `insert_into_indexes` / `delete_from_indexes` / `batch_insert_into_indexes` to pass compiled expressions alongside compiled predicates
- `update_affects_index`: uses expression evaluation to determine if the logical key changed

### `axiomdb-sql/src/partial_index.rs`
- Add `compile_index_exprs` public function: returns `Vec<Option<Expr>>` parallel to `indexes`, one `Expr` per index column that has an expression (columns without expressions yield `None`)

### `axiomdb-sql/src/planner_select.rs`
- Extend `find_index_on_col` to also match expression indexes: if WHERE clause contains a function call or expression matching the index's stored expression SQL, use that index
- Add expression matching helper: `expr_matches_index_expr(query_expr, index_expr_sql)` — compares query WHERE against stored expression SQL text
- Add Rule 1b: `LOWER(col) = literal` lookup using expression index (mirrors Rule 1 for bare column)
- Add Rule 2b: `LOWER(col) LIKE 'foo%'` range scan using expression index
- In `plan_composite_eq`: skip expression-indexed columns (not yet supported for composite)

### `axiomdb-sql/src/eval.rs`
- Export `eval` for use by index maintenance code (already exported via `pub use eval::eval`)

### `axiomdb-sql/src/ast.rs`
- `IndexColumn`: already has `expr: Option<Box<Expr>>` from Phase 21 parser work — no changes needed

### New test file: `axiomdb-sql/tests/expression_index.rs`
- Integration tests covering all acceptance criteria via MySQL wire protocol

## Algorithm / Data structure

### Storage format (catalog)

Each `IndexColumnDef` in the on-disk catalog grows by 0–2 bytes for the expression:

```
[col_idx: 2 LE][order: 1][expr_len: 2 LE][expr_sql: expr_len UTF-8]
```

- `expr_len = 0` with no bytes following means `expr = None` (no expression — standard column index)
- `expr_len > 0` means `expr = Some(String)` with the SQL expression text
- Old catalogs with 3-byte column entries are fully backward-compatible: `from_bytes` reads 3 bytes, finds no `expr_len`, sets `expr = None`

### Expression compilation (parse-once)

```
compile_index_exprs(indexes: &[IndexDef], col_defs: &[ColumnDef])
  → Vec<Vec<Option<Expr>>>   // [index_idx][column_idx] → compiled Expr or None
```

Mirrors the `compile_index_predicates` pattern from Phase 6.7. The SQL string is wrapped in `SELECT 1 WHERE <sql>` and re-parsed per index column, then column references are resolved via `resolve_predicate_columns`.

### Expression evaluation during index maintenance

`index_key_values_for_expr(idx, row, compiled_exprs)`:
- For each `IndexColumnDef` with `expr = Some(sql)`: evaluate `compiled_expr` against `row` → `Value`
- For each with `expr = None`: read `row[col_idx]` directly
- Return `Vec<Value>` — same interface as the existing key extraction

### Planner matching

The planner matches WHERE expressions against expression index definitions using text-level equality of the SQL strings:

```
query WHERE: LOWER(email) = 'foo'
index expr: LOWER(email)

→ match found → use IndexLookup with encoded literal
```

For LIKE range scans, the planner extracts bounds from the pattern literal.

## Implementation phases

### Phase A — Catalog changes (low risk)
1. Add `expr: Option<String>` to `IndexColumnDef` in `schema_database.rs`
2. Update `to_bytes` / `from_bytes` serialization
3. Add unit tests for old-format backward compatibility
4. Verify `cargo test -p axiomdb-catalog` passes

### Phase B — Expression compilation (new code, no existing behavior change)
1. Add `compile_index_exprs` to `partial_index.rs`
2. Add `index_key_values_for_expr` helper
3. Unit test with parsed expressions against sample rows

### Phase C — Index build (CREATE INDEX)
1. Modify `execute_create_index` to store expression SQL in `IndexColumnDef`
2. Modify `build_index_root_from_heap` to evaluate expressions per row
3. Modify `build_index_root_from_clustered` similarly
4. Test `CREATE INDEX ON t(LOWER(col))` builds correctly

### Phase D — Index maintenance (INSERT/UPDATE/DELETE)
1. Update `insert_into_indexes` to accept and use compiled expressions
2. Update `delete_from_indexes` similarly
3. Update `batch_insert_into_indexes` similarly
4. Update `update_affects_index` to evaluate expressions for both old and new rows
5. Verify `cargo test -p axiomomdb-sql` passes

### Phase E — Planner matching
1. Add expression SQL comparison helper in `planner_select.rs`
2. Add Rule 1b for expression-based equality lookups
3. Add Rule 2b for expression-based LIKE range scans
4. Add `IndexRange` support for expression indexes (with `lo`/`hi` bounds from literal)
5. Verify `cargo test -p axiomdb-sql` passes

### Phase F — Integration tests
1. Write wire-test assertions in `tools/wire-test.py`
2. Run full `cargo test --workspace`
3. Run `cargo clippy --workspace -- -D warnings`
4. Run `cargo fmt --check`

## Tests to write

### Unit tests (in `axiomdb-sql/src/partial_index.rs`)
- `compile_index_exprs` with a simple `LOWER(col)` expression → resolved `Expr::Function`
- `compile_index_exprs` with mixed expression + non-expression columns
- Expression with multiple column references: `col1 + col2`

### Unit tests (in `axiomdb-sql/src/index_maintenance.rs`)
- `index_key_values_for_expr` evaluates `LOWER(col)` correctly per row
- `index_key_values_for_expr` with arithmetic: `price * qty`
- Expression evaluating to NULL → row not indexed (consistent with existing NULL skip behavior)

### Integration tests (in `axiomdb-sql/tests/` or `tests/`)
- `CREATE INDEX ON t(LOWER(email))` + `INSERT INTO t VALUES (...)` + verify index entry exists
- `SELECT * FROM t WHERE LOWER(email) = 'foo'` uses expression index (EXPLAIN check)
- Expression index + partial index combined
- `UPDATE t SET email = 'BAR' WHERE id = 1` updates expression index entry
- `DELETE FROM t WHERE id = 1` removes expression index entry
- Multi-column expression: `CREATE INDEX ON t(col1 + col2)`
- Rejection: `CREATE INDEX ON t((SELECT ...))` — verify error at CREATE time

## Anti-patterns to avoid

1. **Don't re-parse the expression SQL on every row** — compile-once and cache the `Expr` tree; `index_maintenance.rs` already has this pattern via `compiled_preds`

2. **Don't change the on-disk format incompatibly** — the `expr_len = 0` sentinel with absent bytes maintains backward compatibility with existing catalogs

3. **Don't allow subqueries/aggregates/window functions in expression indexes** — reject at compile time in `partial_index.rs::resolve_predicate_columns` (same blocklist already exists for partial index predicates)

4. **Don't use `unwrap()` in production `src/`** — all expression compilation and evaluation returns `Result`, use `?` or `map_err`

5. **Don't skip NULL handling for expression results** — if `LOWER(col)` evaluates to NULL, the row is not indexed (same as existing behavior for NULL column values)

## Risks

1. **Expression evaluation during DML hot path** — evaluating a SQL expression per row in `insert_into_indexes` adds CPU overhead. This is unavoidable but acceptable: expression indexes are opt-in by user declaration, and the alternative (storing raw per-row expression values) is not feasible.

2. **Planner false positives** — if the expression matcher is too loose, the planner might use an expression index when the query semantics don't match. Mitigation: use strict SQL text equality (normalized) for matching, not structural `Expr` comparison.

3. **Backward compatibility with old catalogs** — old `IndexColumnDef` entries have exactly 3 bytes. The new format adds optional bytes after the order byte. Reader must check available bytes before reading `expr_len` — already handled in the plan's `from_bytes` design.

4. **Expression compilation failures** — if the stored SQL expression text is malformed or references a dropped column, `compile_index_exprs` will return an error at DML time. This surfaces as a user-facing error on the first INSERT/UPDATE after the index is created. Mitigation: validate expression compilation during CREATE INDEX itself.
