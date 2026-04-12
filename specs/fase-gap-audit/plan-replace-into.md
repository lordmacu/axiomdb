# Plan: REPLACE INTO

## Files to create/modify

**Modify:**

- `crates/axiomdb-sql/src/ast.rs`
  - Add `replace: bool` to `InsertStmt`. Default `false` for INSERT.

- `crates/axiomdb-sql/src/parser/mod.rs`
  - At `parse_stmt`, accept `Token::Ident("replace")` at statement start
    and route to `parse_replace`. Keep `REPLACE(str, from, to)` working
    as a scalar function in expression context (unchanged — the
    expression parser reads `replace(` with `(` immediately following).

- `crates/axiomdb-sql/src/parser/dml.rs`
  - Add `parse_replace(p) -> Result<Stmt, DbError>` that reuses
    `parse_insert` internals via a shared helper `parse_insert_body(p,
    is_replace)` and sets `replace = true`.
  - Reject `REPLACE IGNORE` with an explicit parse error.
  - Re-export `parse_replace` for the top-level dispatcher.

- `crates/axiomdb-sql/src/executor/insert_heap_ctx.rs`
  - Extract per-row conflict displacement into a helper
    `displace_conflicts_for_row_heap(...)` that runs once per input row
    BEFORE the real INSERT:
    1. Snapshot the table's unique indexes (PRIMARY + UNIQUE, excluding
       fk-auto and partial indexes whose predicate evaluates FALSE on
       the incoming row).
    2. For each unique index, encode the key from the new row; skip if
       any key column is NULL (MATCH SIMPLE — matches SQL std).
    3. Probe the index; for every matching RID, call
       `TableEngine::delete_row` (heap path) so FK cascade / secondary
       indexes / statistics are maintained consistently. Accumulate
       `deleted_count`.
    4. Apply the AUTO_INCREMENT rejection rule: if the AI column has a
       user-provided value (not `NULL`, not `0`, not `DEFAULT`) AND a
       unique-index conflict is the AI column itself, surface
       `DbError::InvalidValue` with a MySQL-compatible message before
       deleting anything.
  - Call the helper when `stmt.replace`; otherwise skip it.
  - Bump `count` accumulator by `deleted_count` per row so
    `affected_rows = inserted + deleted` matches MariaDB.

- `crates/axiomdb-sql/src/executor/insert_clustered_ctx.rs`
  - Mirror the helper as `displace_conflicts_for_row_clustered(...)`
    using `axiomdb_storage::clustered_tree::lookup` for PK and the
    clustered secondary index for the other uniques. Respect the same
    AUTO_INCREMENT rejection rule.

- `crates/axiomdb-sql/src/executor/mod.rs`
  - No change (helpers are included via `include!` alongside the insert
    modules they live in).

- `crates/axiomdb-sql/tests/integration_replace_into.rs` (new)
  - Coverage matrix (see **Tests** below).

**No change**:

- `fk_enforcement.rs`, `index_maintenance.rs`, `clustered_tree.rs`,
  `table_write.rs`. The REPLACE path calls their existing entry points.

## Algorithm / Data structure

### Core procedure (per-row, invoked once per input row when `stmt.replace`)

```text
displace_conflicts_for_row(row, table, unique_indexes, conn_txn):
    deleted = 0
    for idx in unique_indexes (PK first, then UNIQUEs):
        if idx is a partial index:
            predicate = compile_index_predicates(idx, columns)
            if predicate(row) is not TRUE: continue
        key_vals = extract_key_values(row, idx.columns)
        if any(v is NULL for v in key_vals): continue   # SQL std
        if idx is auto_increment AND user-supplied value collides:
            return Err(InvalidValue { ai_rejection_message })
        matching_rids = lookup_unique(storage, idx, key_vals)
        for rid in matching_rids:
            TableEngine::delete_row(storage, txn, conn_txn, table, rid)
            deleted += 1
    return Ok(deleted)
```

**Correctness argument:**

1. Unique indexes (PK + UNIQUE) are the only constraints REPLACE must
   resolve by MySQL definition.
2. For each such index, the incoming row conflicts with *at most one*
   existing row (that's the UNIQUE guarantee).
3. Deleting every conflicting row before INSERT makes the subsequent
   INSERT succeed on the same constraints (no conflicts remain).
4. The new row may still fail for other reasons — FK child-side
   validation, CHECK constraint, NOT NULL violation — which must fail
   the whole REPLACE (per MariaDB: the displaced row's DELETE is
   rolled back by the outer transaction's rollback on statement error).

**Transaction / atomicity**: the entire REPLACE statement runs inside
the caller's transaction (explicit or implicit). A mid-row failure
rolls back every preceding `delete_row` and `insert_row` for this
statement, matching MariaDB statement-atomic behavior.

**Concurrency**: AxiomDB's MVCC snapshot + the existing IX(table)
lock acquired by INSERT/DELETE prevents interleaved REPLACEs on the
same table from observing each other's uncommitted deletes.

### Partial-index predicate handling

Partial indexes are loaded with their stored SQL predicate
(`IndexDef.predicate`). The existing helper
`partial_index::compile_index_predicates(&[idx], columns)` returns a
compiled predicate that can be evaluated against a row. A conflict is
possible only when the predicate is TRUE for the incoming row — if it
is FALSE or UNKNOWN, the incoming row is not in the index domain, so
there can be no match. Skip the index in that case.

### AUTO_INCREMENT rejection rule

MariaDB rejects `REPLACE` when the AI column has an explicit value
provided by the user AND that value collides on the AI index. This
prevents key-space exhaustion in the degenerate case where a
pathological client REPLACEs forever with the same AI value. Match
this rule:

```text
if schema has auto_increment col at idx ai_col
   AND provided_value(row, ai_col) is not NULL / 0 / DEFAULT
   AND a unique index exists on ai_col
   AND lookup(ai_col_index, provided_value) finds an existing row with
       a DIFFERENT ai_col value in the cached AUTO_INC_SEQ range
:
    return Err(InvalidValue { "REPLACE with explicit AUTO_INCREMENT
        value would exhaust the key space" })
```

(Simplified version for MVP: trigger the error only when user passed
an explicit non-zero value and lookup shows the existing row's AI
column equals that value. This is the only subtle case tests cover.)

## Implementation phases

1. **AST + parser (≤50 LOC):** add `replace` flag, statement dispatch,
   parse_replace reusing INSERT body.
2. **Heap executor helper (~120 LOC):** proactive displace loop with
   FK-aware delete. Unit-test the helper in isolation if possible.
3. **Clustered executor helper (~100 LOC):** mirror for clustered.
4. **Integration tests (~250 LOC):** full coverage matrix.
5. **Close the subphase**: progreso.md, doc sweep, clippy/fmt/workspace
   tests, commit.

## Tests to write

In `tests/integration_replace_into.rs`:

1. `replace_no_conflict_behaves_as_insert` — heap + clustered.
2. `replace_on_primary_key_conflict` — affected_rows == 2.
3. `replace_on_non_pk_unique_conflict` — affected_rows == 2.
4. `replace_on_composite_unique_conflict` — affected_rows == 2.
5. `replace_with_null_in_unique_column_is_insert` — NULL doesn't
   conflict; `affected_rows == 1`.
6. `replace_on_multi_index_conflict_deletes_all_matches` —
   `affected_rows == 1 + deleted`.
7. `replace_fk_cascade_on_displaced_parent` — cascade children delete.
8. `replace_fk_restrict_rolls_back` — RESTRICT errors; no state change.
9. `replace_select_self_reference` — `REPLACE INTO t SELECT ... FROM t`.
10. `replace_partial_unique_predicate_respected` —
    partial-index conflict behavior.
11. `replace_auto_increment_rejection` — matches MariaDB's exhaustion
    prevention.
12. `replace_parser_rejects_replace_ignore` — explicit parse error.
13. `replace_low_priority_prefix_accepted` — accepted and discarded.
14. `replace_default_values` — `REPLACE INTO t DEFAULT VALUES`.
15. `replace_set_syntax` — `REPLACE INTO t SET a = 1, b = 'x'`.
16. `replace_batch_values_per_row_conflict_handling` —
    `REPLACE INTO t VALUES (...), (...), (...)` with mixed conflict/
    no-conflict rows.

## Anti-patterns to avoid

- **Do not** attempt to "optimize" the last-unique-index UPDATE-in-place
  path in MVP. It requires preserving RIDs and knowing the index order
  — deferred.
- **Do not** bypass `TableEngine::delete_row` to "fast-delete" the
  displaced row. FK cascade, secondary indexes, and AUTO_INC_SEQ
  invalidation depend on it.
- **Do not** duplicate the parser body between INSERT and REPLACE.
  Extract a shared `parse_insert_body(p, is_replace)`.
- **Do not** silently swallow the `FK RESTRICT` error — the whole
  statement must fail so the user sees why.
- **Do not** forget to update partial-index predicate evaluation —
  without it, a REPLACE on a table with a partial UNIQUE can spuriously
  fail or silently miss a conflict.
- **Do not** special-case clustered-only or heap-only. Both must work
  from MVP (the spec's acceptance criteria demand it).

## Risks

1. **Partial-index predicate evaluator** is reused from existing code;
   if it has gaps (e.g. correlated expressions), they surface here.
   Mitigation: only a conservative check — if predicate-eval errors,
   treat the index as "may conflict" and probe it.

2. **Clustered tables** have a different delete path
   (`update_clustered_delete`) that interacts with clustered secondary
   indexes. Risk: a REPLACE on a clustered table with multiple secondary
   uniques leaves inconsistent state. Mitigation: mirror exactly the
   existing DELETE/UPDATE paths the clustered executor already uses.

3. **Self-referencing SELECT** — `REPLACE INTO t SELECT * FROM t WHERE
   ...` could enter an infinite delete loop if rows are materialized
   lazily. Mitigation: verify that `InsertSource::Select` buffers rows
   into a `Vec` before the executor begins the REPLACE loop (spot
   check during implementation — our `execute_select_ctx` already
   returns a materialized `QueryResult::Rows`).

4. **`affected_rows` regression** — INSERT tests that assert
   `count == 1` must not break. Risk is zero because INSERT path
   (without `stmt.replace`) doesn't invoke the new helper.

5. **Composite-key NULL semantics** — MySQL's MATCH SIMPLE says any
   NULL in the key makes the constraint unenforceable (no conflict).
   Verify with integration test 5 above.

6. **AUTO_INCREMENT rejection** — the rule is subtle and the MariaDB
   test coverage for it is sparse. Mitigation: implement the simple
   form (explicit user value collides on AI index) and document the
   simplification clearly.
