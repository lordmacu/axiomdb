# Plan: INSERT ... ON DUPLICATE KEY UPDATE

## Files to create/modify

**Modify:**

- `crates/axiomdb-sql/src/ast.rs`
  - Add `on_duplicate_update: Option<Vec<Assignment>>` to `InsertStmt`.
  - Add new expression variant: `Expr::InsertValue { col_name: String }`
    — encoded identically to `Expr::Column` at the AST level, but
    distinct so the evaluator can route it to the proposed row.

- `crates/axiomdb-sql/src/parser/dml.rs`
  - After the existing source-clause parse (VALUES / DEFAULT VALUES /
    SET / SELECT), accept an optional
    `ON DUPLICATE KEY UPDATE <assignment_list>` tail.
  - Inside that assignment list, thread a flag through `parse_expr`
    so a bare `VALUES ( col )` call becomes `Expr::InsertValue
    { col_name }` instead of a function call.

- `crates/axiomdb-sql/src/parser/expr.rs`
  - Extend `parse_atom` / `parse_ident_or_call` to recognize
    `VALUES` as a keyword-triggered pseudo-function **only** when the
    parser's `in_odku_assignment` flag is true. Emit
    `Expr::InsertValue { col_name }`. Outside ODKU, parsing
    `VALUES(col)` as an expression continues to surface as a
    `ColumnNotFound` / parse error exactly as before (it would
    resolve through `parse_ident_or_call` and fail there).

- `crates/axiomdb-sql/src/parser/mod.rs`
  - Add `in_odku_assignment: bool` to the `Parser` struct; flipped on
    while parsing the ODKU assignment list and restored afterward.

- `crates/axiomdb-sql/src/analyzer_expr.rs`
  - Resolve `Expr::InsertValue { col_name }` in the current scope —
    if the column name matches a column of the target table, rewrite
    to `Expr::InsertValue { col_name: resolved }` carrying the table's
    `col_idx` (piggyback on `Expr::Column`'s field). Mismatch →
    `ColumnNotFound`.

- `crates/axiomdb-sql/src/eval/core.rs`
  - Teach `eval`/`eval_with` to recognize `Expr::InsertValue`: either
    it unwraps to `Value::Null` when evaluated against a non-ODKU row
    (safe fallback), OR — when evaluated with the ODKU helper's
    `eval_with_odku_context` shim — reads from a second `proposed_row`
    slice supplied via a new trait or by passing a paired slice.

  Simpler alternative (chosen): expose a small new function
  `eval_with_proposed(expr, existing_row, proposed_row)` used only by
  the ODKU helper. `eval` stays unchanged for non-ODKU paths.

- `crates/axiomdb-sql/src/executor/insert_heap_ctx.rs`
  - In every row-processing path (Values, Select, DefaultValues),
    before the INSERT write, if `stmt.on_duplicate_update.is_some()`:
    run `apply_odku_heap(...)`. The helper returns one of:
      `OdkuOutcome::Inserted`            → fall through to normal INSERT
      `OdkuOutcome::UpdatedChanged`      → skip the INSERT; count += 2
      `OdkuOutcome::UpdatedNoChange`     → skip the INSERT; count += 0
      `OdkuOutcome::Error(DbError)`      → propagate

- `crates/axiomdb-sql/src/executor/insert_clustered_ctx.rs`
  - Return `DbError::NotImplemented` when `stmt.on_duplicate_update.
    is_some()` — same shape as REPLACE.

- `crates/axiomdb-sql/src/executor/replace_helpers.rs`
  - Extract the PK/UNIQUE probe loop into a shared function:
      `find_first_conflicting_rid(storage, bloom, snap, resolved,
          schema_cols, row_values) -> Result<Option<(RecordId,
          Vec<Value>, &IndexDef)>, DbError>`
  - REPLACE reuses it (delete-every-conflict path); ODKU reuses it
    (update-first-conflict-only path).

- `crates/axiomdb-sql/src/executor/odku_helpers.rs` (**new file**)
  - `apply_odku_heap(...)` — per-row executor helper.
  - Resolves `Vec<Assignment>` into `Vec<(col_idx, Expr)>` once per
    statement, then per row:
    1. probe for conflict (`find_first_conflicting_rid`),
    2. if none → return `Inserted` (caller performs the plain INSERT),
    3. if found:
       - read the existing row,
       - build `new_values` = existing_row cloned,
       - for each `(col_idx, rhs_expr)` evaluate `eval_with_proposed
         (rhs_expr, existing_row, proposed_row)` and overwrite
         `new_values[col_idx]`,
       - enforce text constraints, row constraints,
         `check_fk_child_update`, `enforce_fk_on_parent_update`,
       - if `new_values == existing_row` → return `UpdatedNoChange`,
       - otherwise call `TableEngine::update_row` (heap path — delete
         + insert at new RID) and update every affected secondary
         index via `index_maintenance::update_affects_index` +
         `delete_many_from_single_index` +
         `insert_many_into_single_index`,
       - invalidate the AUTO_INCREMENT reclamation by resetting
         `ctx.stats` row-changes counter like the plain UPDATE path
         does — and for the AI column specifically: if the caller
         generated a fresh AI value that is now being discarded,
         call `rewind_last_auto_inc_if_applicable` (new tiny helper,
         see Risks).
  - Returns `OdkuOutcome`.

- `crates/axiomdb-sql/src/executor/mod.rs`
  - `include!("odku_helpers.rs")` next to `replace_helpers.rs`.

- `crates/axiomdb-sql/tests/integration_insert_on_dup.rs` (**new**)
  - Coverage matrix (see **Tests** below).

**No change:** `fk_enforcement.rs`, `index_maintenance.rs`,
`table_write.rs` — existing entry points are sufficient.

## Algorithm / Data structure

### Parser

```text
parse_insert_body(p, is_replace):
    ... (existing logic — builds table / columns / source / ignore)

    let mut on_dup = None;
    // ODKU can't coexist with REPLACE — semantic guard:
    if !is_replace and p.eat(&Token::On):
        p.expect(&Token::Ident("DUPLICATE"))
        p.expect(&Token::Ident("KEY"))
        p.expect(&Token::Update)
        on_dup = Some(parse_odku_assignment_list(p))

    InsertStmt { table, columns, source, ignore, replace, on_duplicate_update: on_dup }

parse_odku_assignment_list(p):
    p.in_odku_assignment = true
    loop:
        col = parse_identifier()
        p.expect(Eq)
        rhs = parse_expr(p)   // VALUES(col) detected via flag
        push (col, rhs)
        break if !p.eat(Comma)
    p.in_odku_assignment = false
    return list

parse_ident_or_call — extra branch at the start:
    if self.in_odku_assignment && name.eq_ignore_ascii_case("values")
       && next == LParen:
        consume LParen
        col = parse_identifier()   // disallow arbitrary expressions
        consume RParen
        return Expr::InsertValue { col_name: col }
    ... existing body
```

### Executor helper — per row

```text
apply_odku_heap(resolved, schema_cols, assignments, proposed_row,
                storage, txn, conn_txn, bloom, ctx):
    conflict = find_first_conflicting_rid(storage, bloom, snap,
                    resolved, schema_cols, proposed_row)?
    if conflict is None:
        return OdkuOutcome::Inserted   // caller runs plain INSERT

    let (rid, existing_row, _idx) = conflict
    let mut new_row = existing_row.clone()
    for (col_idx, rhs) in assignments:
        let v = eval_with_proposed(rhs, &existing_row, &proposed_row)?
        new_row[col_idx] = v

    enforce_text_constraints(schema_cols, &mut new_row)?
    check_row_constraints(&resolved.constraints, &new_row, &table_name)?

    if resolved.foreign_keys is non-empty:
        check_fk_child_update(&existing_row, &new_row,
            &resolved.foreign_keys, storage, txn, conn_txn, bloom)?

    // parent-side: only if any FK-referenced column moved
    if any_parent_key_changed(existing_row, new_row, resolved):
        enforce_fk_on_parent_update([(rid, existing_row.clone())],
            [&new_row], resolved.def.id,
            storage, txn, conn_txn, bloom)?

    if new_row == existing_row:
        return OdkuOutcome::UpdatedNoChange

    let new_rid = TableEngine::update_row(storage, txn, conn_txn,
        &resolved.def, schema_cols, rid, new_row.clone())?
    maintain_secondary_indexes_for_update(
        &secondary_indexes, &existing_row, rid, &new_row, new_rid, ...)?

    ctx.stats.on_rows_changed(resolved.def.id, 1)
    return OdkuOutcome::UpdatedChanged
```

### AUTO_INCREMENT reclamation on UPDATE branch

When the caller has already reserved an AI value for the proposed row
before ODKU runs, and ODKU takes the UPDATE branch, that AI value is
wasted. Mitigation for MVP: rely on the natural gap — the next insert
takes the next AI value and the reserved one is lost. MariaDB behaves
the same way under `insert_id_for_cur_row = 0` semantics (the user-
visible LAST_INSERT_ID() is unaffected, but the counter moves).
Document this in acceptance criteria and accept the slot loss.

### Evaluator shim

```rust
pub fn eval_with_proposed(
    expr: &Expr,
    existing_row: &[Value],
    proposed_row: &[Value],
) -> Result<Value, DbError>
```

Walks the expression tree; `Expr::Column { col_idx, .. }` reads from
`existing_row`, `Expr::InsertValue { col_idx, .. }` reads from
`proposed_row`, everything else recurses. Implemented as a thin
wrapper over `eval::core::eval_with_runner` — we pass a small
`OdkuRowContext` that selects the source row on each node.

## Implementation phases

1. **AST + parser skeleton (~80 LOC)**: `on_duplicate_update` field,
   `Expr::InsertValue`, parser ON DUPLICATE KEY tail, `VALUES(col)`
   inside the ODKU assignment list only.
2. **Evaluator support (~40 LOC)**: `eval_with_proposed` + one new
   `Expr::InsertValue` arm in `eval::core`.
3. **Shared probe helper (~80 LOC)**: extract `find_first_conflicting
   _rid` from the REPLACE helper so both features share one definition.
4. **ODKU executor helper (~220 LOC)**: the `apply_odku_heap`
   function above.
5. **Wire into `insert_heap_ctx.rs` (~60 LOC)**: insert the ODKU call
   before every write site; route the outcome enum into `count`.
6. **Integration tests (~350 LOC)**: coverage matrix.
7. **Close**: progreso.md, commit.

## Tests to write

In `tests/integration_insert_on_dup.rs`:

1. `odku_parses_after_all_source_forms` — VALUES, SET, DEFAULT VALUES,
   SELECT.
2. `odku_values_pseudo_fn_resolves_to_proposed_row`.
3. `odku_values_outside_odku_is_rejected` — `SELECT VALUES(col)` must
   fail to parse (it would resolve to a column reference + function
   call and fail — assert the error shape).
4. `odku_no_conflict_behaves_as_insert` — `count = 1`.
5. `odku_on_pk_conflict_updates_in_place` — `count = 2`, RID preserved
   (verified via a secondary-index lookup that still returns the row).
6. `odku_update_unchanged_is_zero_affected` — the assignment leaves the
   row at its current values → `count = 0`.
7. `odku_on_non_pk_unique_conflict`.
8. `odku_on_composite_unique_conflict`.
9. `odku_null_in_unique_is_insert`.
10. `odku_multi_index_conflict_picks_first` — two different rows
    conflict on two different unique indexes, ODKU updates the one on
    the first-in-catalog-order unique index; the other stays untouched.
11. `odku_update_that_creates_new_unique_conflict_errors` — the
    `UPDATE` clause moves a unique column onto a third row → the
    statement fails.
12. `odku_fk_child_validation_on_updated_row` — FK column on updated
    row still must reference a valid parent.
13. `odku_fk_parent_update_cascade` — updating a parent key column via
    ODKU cascades to children.
14. `odku_last_insert_id_only_for_insert_branch` — verify
    LAST_INSERT_ID() after an update-branch ODKU is NOT the AI value
    of the inserted proposed row.
15. `odku_batch_values_mixed_insert_and_update` — `count` sums correctly.
16. `odku_with_ignore_prefix_parses` — `INSERT IGNORE ... ON DUPLICATE
    KEY UPDATE` parses and runs.
17. `odku_on_clustered_table_returns_not_implemented` — parity with
    REPLACE MVP.
18. `odku_from_select_source` — `INSERT INTO t SELECT ... ON DUPLICATE
    KEY UPDATE v = VALUES(v)`.

## Anti-patterns to avoid

- **Do not** duplicate the unique-index probe between REPLACE and
  ODKU. Extract the shared helper first (Phase 3 above), then have
  both call it.
- **Do not** bypass `TableEngine::update_row` + the existing
  `index_maintenance` entry points. The update path already handles:
  RID change (heap delete + insert), secondary-index delete/insert,
  FK fast-path. Rebuilding any of this in the ODKU helper creates
  a second source of truth.
- **Do not** eagerly evaluate the assignment RHS before the conflict
  check. The `col` column references must bind to the *existing*
  row, which only exists after the probe.
- **Do not** forget the no-op detection (`new_row == existing_row`
  → `count += 0`). The MySQL spec is explicit here, ORMs depend on it.
- **Do not** special-case AUTO_INCREMENT inside the helper — the
  caller generates the AI value before the helper runs, and the
  helper doesn't need to reclaim it for MVP (see slot-loss note).
- **Do not** allow `VALUES(expr)` with a non-bare-identifier argument
  — MariaDB rejects that, and our grammar should too. Only
  `VALUES(col_ident)` is valid.

## Risks

1. **`Expr::InsertValue` evaluator plumbing** — adding a new Expr
   variant touches every walk function in the codebase (pretty-print,
   `collect_column_indices`, `expr_has_outer_ref`, subquery
   rewrites). Mitigation: pattern-match on `Expr::Column` and
   `Expr::InsertValue` together with the same handler wherever the
   walker just needs "a leaf column reference", using a small
   `is_column_ref` helper. Verify every file in the repo still
   compiles.

2. **Post-UPDATE unique re-check** — if the SET clause moves a
   unique-indexed column to a value that already exists in a THIRD
   row, the insert-into-secondary-index call will fail. This is
   correct behavior (error surfaces cleanly) but needs a test so we
   notice regressions. Covered by test #11.

3. **Heap update RID change** — `TableEngine::update_row` may return
   a new RID when the new row doesn't fit on the old page. Secondary
   indexes must be rewritten to point at the new RID; this is what
   `index_maintenance::update_affects_index` handles. Risk: the ODKU
   helper forgets to pass the pair `(old_rid, new_rid)` to the index
   maintenance path. Mitigation: cross-check against the existing
   UPDATE executor's call sequence.

4. **REPLACE's extracted probe helper** — extracting
   `find_first_conflicting_rid` from `replace_helpers.rs` is a real
   refactor; risk of silently breaking REPLACE. Mitigation: run the
   full REPLACE integration test suite after the extraction, before
   wiring ODKU in.

5. **Batch path disabled** — like REPLACE, ODKU forces the per-row
   insert path. For bulk INSERT ... SELECT this is slower than the
   batch path. Acceptable for MVP — ODKU semantics require per-row
   conflict bookkeeping anyway.

6. **`INSERT IGNORE + ODKU` interaction** — IGNORE silences `NotNull`
   / `CheckFailed` / `FkChildInsertViolation`; ODKU silences unique.
   If both fire on the same row (e.g. ODKU updates a row and the
   update hits a CHECK violation), IGNORE must convert the error to
   a warning. Mitigation: the update-branch already returns errors
   that the caller can route through the existing `ignore && is
   _ignorable_insert_error(&e)` gate. Add a test for it (future).

7. **AUTO_INCREMENT slot loss on UPDATE branch** — accepted as-is for
   MVP. Document in progreso.md and leave a TODO for reclamation via
   `AUTO_INC_SEQ` rollback (would require propagating the generated
   ID down to the ODKU helper and freeing it when the update branch
   wins).
