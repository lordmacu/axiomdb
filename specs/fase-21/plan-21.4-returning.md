# Plan: 21.4 — RETURNING

## Files

### Modify

- `crates/axiomdb-sql/src/lexer.rs` — add `Token::Returning`.
- `crates/axiomdb-sql/src/ast.rs` — add `pub returning: Vec<SelectItem>`
  to `InsertStmt`, `UpdateStmt`, `DeleteStmt`.
- `crates/axiomdb-sql/src/parser/dml.rs`:
  - After parsing the tail of each DML (after LIMIT / ON DUPLICATE),
    consume optional `RETURNING select_item_list`.
  - `RETURNING *` → `vec![SelectItem::Wildcard]`.
  - Otherwise reuse the existing select-list parser logic.
- `crates/axiomdb-sql/src/analyzer_ddl.rs::analyze_insert` /
  `analyze_update` / `analyze_delete` — resolve each `SelectItem` in
  `returning` against the target table's `BindContext`. `*` stays as
  `Wildcard`; expanded downstream.
- `crates/axiomdb-sql/src/executor/insert_helpers.rs` — INSERT DML
  executor capture loop: collect each inserted row (post-defaults,
  post-auto-increment), project through `returning` items, emit as
  `QueryResult::Rows`.
- `crates/axiomdb-sql/src/executor/update_entry.rs` — UPDATE executor:
  collect post-update row values (not pre-update) into the projection
  buffer.
- `crates/axiomdb-sql/src/executor/` (delete_entry or similar) —
  DELETE executor: collect pre-delete row values **before** the
  storage delete.
- Every call site in exec_dispatch that returns `Affected` from DML
  must check `returning.is_empty()` first and route to the Rows
  branch when present.

### Create

- `crates/axiomdb-sql/tests/integration_returning.rs` — 12+ tests.

## Algorithm

### Parser

```rust
// After existing DML parse tail, uniformly:
let returning = if p.eat(&Token::Returning) {
    if p.eat(&Token::Star) {
        vec![SelectItem::Wildcard]
    } else {
        parse_select_item_list(p)?  // reuse from parse_select
    }
} else {
    Vec::new()
};
```

### Analyzer

```rust
for item in &mut stmt.returning {
    if matches!(item, SelectItem::Wildcard | SelectItem::QualifiedWildcard(_)) {
        continue;
    }
    if let SelectItem::Expr { expr, .. } = item {
        let taken = std::mem::replace(
            expr,
            Expr::Literal(axiomdb_types::Value::Null),
        );
        *expr = resolve_expr(taken, &ctx)?;
    }
}
```

### Executor (INSERT)

```rust
let mut returning_rows: Vec<Vec<Value>> = Vec::new();
for row in rows_to_insert {
    let rid = heap_chain.insert(row.clone(), ...)?;
    // Fill auto-increment / DEFAULT / GENERATED before projection.
    let materialized = materialize_inserted_row(&row, ...);
    if !returning.is_empty() {
        returning_rows.push(project(&returning, &materialized)?);
    }
}
if returning.is_empty() {
    Ok(QueryResult::Affected { count: n })
} else {
    Ok(QueryResult::Rows {
        columns: build_output_columns(&returning, &target_table),
        rows: returning_rows,
    })
}
```

Same pattern for UPDATE (use post-update row) and DELETE (capture
before `delete_slot`).

## Tests

1. `insert_returning_id_auto_increment`
2. `insert_returning_star_multiple_rows`
3. `insert_returning_expression_alias`
4. `update_returning_post_update_values`
5. `update_returning_star`
6. `update_returning_empty_result_on_no_match`
7. `delete_returning_star_pre_delete_values`
8. `delete_returning_empty_when_where_matches_nothing`
9. `delete_returning_with_order_by_limit`
10. `insert_without_returning_still_returns_affected`
11. `update_without_returning_still_returns_affected`
12. `insert_returning_qualified_wildcard`

Wire smoke: 2 assertions:
- `INSERT ... RETURNING id` — returns auto-increment.
- `DELETE ... RETURNING *` — returns captured rows.

## Phases

1. Lexer: `Token::Returning` (~3 LoC).
2. AST: `returning` fields (~3 LoC, 3 structs).
3. Parser: RETURNING clause in 3 DML parsers (~30 LoC).
4. Analyzer: resolve expressions (~30 LoC, 3 funcs).
5. INSERT executor: row capture + projection (~60 LoC).
6. UPDATE executor: same (~60 LoC).
7. DELETE executor: same (~40 LoC — capture before delete).
8. Integration tests.
9. Close protocol.

## Anti-patterns

- Don't re-read rows from storage after write — use the post-write
  values directly (important because post-fsync re-read adds latency
  and can race with concurrent writers).
- Don't collect rows when `returning.is_empty()` — zero overhead
  for the common non-RETURNING path.
- Don't capture pre-delete rows after issuing the storage delete
  (they'd be unreadable or snapshot-visible inconsistently).

## Risks

- AUTO_INCREMENT value propagation into the projected row: the
  insert executor already fills the PK slot after the storage
  assigns the next sequence; projection must read the updated row
  slice, not the pre-insert expression list. Verify by test
  `insert_returning_id_auto_increment`.
- MVCC snapshot visibility: RETURNING projects the writer's own
  newly-written values — not visible under the caller's snapshot
  yet, but the writer owns the row. Correct semantics.
- GENERATED STORED columns (21.5f, not yet implemented) would need
  to materialize before projection. Tracked as follow-up.
- Large result sets (INSERT ... SELECT returning): memory usage
  grows with row count. Acceptable for this subphase; streaming
  RETURNING deferred.
