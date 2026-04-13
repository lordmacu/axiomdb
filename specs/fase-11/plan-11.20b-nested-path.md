# Plan: 11.20b — `JSON_TABLE` single-level `NESTED PATH`

## Files to create/modify

### Modify

- `crates/axiomdb-sql/src/ast.rs`
  - Add `Nested { path: String, columns: Vec<JsonTableColumn> }` variant to
    `JsonTableColumn`. Boxing isn't needed at the AST level —
    `JsonTableColumn` already goes through `Box<JsonTable>` indirection.

- `crates/axiomdb-sql/src/parser/json_table.rs`
  - In `parse_column_def`, dispatch on `Token::Nested` first (NESTED is a
    reserved token — to verify; if not, use `eat_ident_ci("NESTED")`).
  - Accept optional `PATH` keyword: `eat_ident_ci("PATH")` — allowed to be
    absent (SQL:2016 `NESTED '...'` shortcut, MariaDB parity).
  - Parse the row-path string, then the recursive `COLUMNS(...)` list via
    the same `parse_column_def` entry — grammar is recursive, runtime
    enforces depth = 1.

- `crates/axiomdb-sql/src/json_table.rs`
  - Extend `JsonTableColumnKind` with `Nested { path: Vec<PathStepOwned>,
    columns: Vec<JsonTableColumnSpec> }`. Compile recursively via
    `compile_column`.
  - Detect **depth ≥ 2** at compile time: during recursive `compile_column`,
    if we already are inside a nested compile context and encounter another
    `Nested`, raise `NotImplemented` pointing to 11.20c. Simplest gate:
    pass a `nesting_depth: usize` argument into a private recursive helper,
    error on `depth > 1`.
  - Detect **multi-sibling NESTED** at compile time: count NESTED columns
    inside any single `COLUMNS(...)` list; > 1 → `NotImplemented` →
    11.20c.
  - Build a flat output schema: `column_defs_for_ast` recurses into nested
    children and appends their (nested) `ColumnDef`s after the direct
    parent columns in declaration order. `column_metas_for_spec` same.
  - Rewrite `materialize_json_table` as a **generic recursive walker**
    (future-proofs 11.20c — the multi-level gate is dropped later, code
    path already supports it). Shape:

    ```rust
    fn emit_rows(spec: &[JsonTableColumnSpec],
                 parent_node: &serde_json::Value,
                 parent_ord: i64,
                 outer_row, sq, depth: usize) -> Vec<Row>
    ```
    - Resolve direct leaf cols against `parent_node`, fill "direct row
      slots" in declaration order.
    - When a `Nested` column is hit:
      - walk the child path against `parent_node`
      - if zero matches → emit ONE row with child slots set to NULL
        (recursive call with empty column set is the LEFT OUTER case)
      - else for each child match → recurse with fresh ord_child counter
      - each recursion returns a `Vec<Row>` where those rows carry the
        parent-direct slots AND the child-slot contribution.
    - Row assembly: use a "row template" that is filled as we walk; when
      the recursion emits, we clone the template and replace only the
      child slot range with each child's slots.

### Create

- `crates/axiomdb-sql/tests/integration_json_table_nested.rs` — ≥ 10
  integration tests covering:
  1. Basic nested shred with 2 parents × 2 children each.
  2. LEFT OUTER NULL padding on empty child array.
  3. Per-level ordinality counters reset per parent.
  4. Nested `EXISTS PATH` column.
  5. Nested `DEFAULT ON EMPTY`.
  6. NESTED grammar without the `PATH` keyword (`NESTED '$.x' COLUMNS(...)`).
  7. Multi-sibling NESTED in same COLUMNS → clear `NotImplemented`.
  8. Multi-level NESTED (depth 2) → clear `NotImplemented`.
  9. WHERE filter crossing parent + child columns.
  10. Duplicate column name across levels → parse error.

### Update (close protocol)

- `tools/wire-test.py` — add 2 new assertions for NESTED + LEFT OUTER pad.
- `docs-site/src/user-guide/sql-reference/dml.md` — add NESTED PATH
  section with the invoice/line-items example.
- `docs-site/src/internals/sql-parser.md` — extend the 11.20a grammar
  block with the NESTED production.
- `docs/fase-11.md` — append 11.20b section.
- `docs/progreso.md` — flip 11.20b from ⏳ to ✅.
- `memory/architecture.md` — update the 2026-04-13 entry with the recursive
  walker generalization.
- `memory/project_state.md` — 11.20b → ✅, next = 11.20c.

## Algorithm / Data structure

### Row template approach

The flat schema has `N = parent_direct + child_direct` slots. For each
parent match:

1. Allocate `Vec<Value>` of length N with all `Value::Null`.
2. Fill parent-direct slots (regular / ordinality / exists / — NOT nested)
   at their fixed offsets.
3. Walk the nested path → get list of child matches.
4. If child list empty: push the row template as-is (child slots already
   NULL). Done.
5. Else for each child match (enumerate starting 1 for the inner
   ordinality):
   - Clone the template.
   - Fill child-direct slots in the clone.
   - Push the clone.

The child slot offsets are computed at compile time into
`JsonTableColumnSpec::Nested { child_slot_range: (usize, usize) }` — no
runtime arithmetic.

### Compile-time slot layout

```
compile_json_table(jt):
    flat_columns = []
    assign_slots_recursive(&mut flat_columns, &jt.columns, 0, depth=0)
    // flat_columns is the Vec<JsonTableColumnSpec> with slot-aware Nested

fn assign_slots_recursive(flat, cols, next_slot, depth):
    for col in cols:
        match col:
            Regular { … } | Ordinality | Exists → push with slot=next_slot; next_slot += 1
            Nested { path, inner } →
                if depth >= 1 → error (NotImplemented, 11.20c)
                if any_sibling_nested_already_pushed → error (NotImplemented, 11.20c)
                start = next_slot
                inner_flat = []
                assign_slots_recursive(&mut inner_flat, inner, start, depth+1)
                end = start + inner_flat.len()
                push Nested { path_compiled, children: inner_flat,
                              slot_range: (start, end) }
                next_slot = end
```

### Materialize loop

```
for (i, parent) in walk(row_path) enumerate(starting 1):
    template = vec![Null; total_slots]
    for col in flat_columns:
        match col:
            Regular {slot, path, …}    → template[slot] = eval_regular(parent, …)
            Ordinality {slot}          → template[slot] = BigInt(i)
            Exists {slot, path, …}     → template[slot] = exists(parent, …)
            Nested {slot_range, children, path_compiled} →
                let child_matches = walk(path_compiled, parent)
                if child_matches.empty:
                    // slots already Null from template init — leave as-is
                    pushed_nested_variant = true
                else:
                    for (j, child) in child_matches.enumerate(starting 1):
                        let mut child_tpl = template.clone()
                        for c in children:
                            fill_child_slot(c, child, j, &mut child_tpl)
                        rows.push(child_tpl)
                    skip the default "push template" below
    if no_nested or child_matches.empty:
        rows.push(template)
```

A tiny state flag distinguishes "emitted child variants already" from
"still need to emit the parent-only row".

### Grammar note (NESTED keyword)

The lexer has `Token::Nested` (verified during implementation — if absent,
fall back to `eat_ident_ci("NESTED")`). After NESTED, `PATH` is optional:
`p.eat_ident_ci("PATH")` then the string literal. This matches MariaDB
and SQL:2016 (PG requires PATH, but we accept both for usability).

## Tests to write

`integration_json_table_nested.rs` — 10 cases per spec acceptance criteria.
Also add 2 parser round-trip tests in `src/parser/json_table.rs` covering:
`NESTED PATH '$.x' COLUMNS (...)` and `NESTED '$.x' COLUMNS (...)`.

## Anti-patterns to avoid

- **Reallocating the template per nested pass.** Allocate once per parent,
  clone once per child match.
- **Walking the nested path twice** (once for emptiness check, once for
  iteration). Walk once, inspect `Vec::is_empty`.
- **Interleaving parent and child walks.** Parent walk first, materialize
  all rows from that parent, then move to next parent. Simpler + cache-
  friendly than generator-style interleaving.
- **Schema mismatch between `column_defs_for_ast` and compile-time slot
  layout.** Both must use the same declaration-order recursion — share a
  helper.
- **Silently truncating the depth error.** Compile-time rejection must
  name the column that exceeded depth and point to 11.20c.

## Risks

| Risk | Mitigation |
|---|---|
| LEFT OUTER semantics surprising (PG default: INNER requires `ERROR ON EMPTY`). | Match MariaDB behavior explicitly: nested emits LEFT OUTER by default. Spec documents the choice. |
| Ordinality scope confusion | Per-level counter is assigned at compile time to each `FOR ORDINALITY` column with the loop it belongs to; inner walker only touches its counter. |
| Row template clone cost | Small — ≤ a few dozen columns. If profiling shows hot, swap to a `SmallVec`-backed template later. |
| Token::Nested may not exist | Confirm during implementation; if absent, use `eat_ident_ci("NESTED")` (same pattern as PATH / COLUMNS / ORDINALITY). |

---

⚡ Effort for `/implement-task`: **high**. Pure AST + executor work; no
storage / concurrency / unsafe. Scope is focused and builds entirely on the
11.20a module.
