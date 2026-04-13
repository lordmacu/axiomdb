# Spec: 11.20d4 — `JSON_TABLE` as UPDATE/DELETE source

## What to build (not how)

Lift the `NotImplemented 11.20d` error at
`crates/axiomdb-sql/src/executor/dml_join.rs:334` so that
`JSON_TABLE(...)` can appear on the right side of a JOIN / CROSS APPLY /
OUTER APPLY inside an `UPDATE` or `DELETE` statement. The target of the
UPDATE / DELETE is still a real table — JSON_TABLE acts purely as a
row source to drive the modification via JOIN conditions.

Non-correlated and LATERAL-correlated `doc` / PASSING are both
supported, mirroring the SELECT path (11.20a / 11.20d3). `MERGE`
remains out of scope until the `MERGE` statement itself lands.

## Inputs / outputs

### Grammar

No new grammar. `JSON_TABLE(...)` already parses in any `FromClause`
position via `parse_from_item` (11.20a), and `CROSS APPLY` /
`OUTER APPLY` / `LATERAL` already work for any right-side source
(11.20d2 / 11.20d3). This subphase is purely executor-level.

### AST deltas

None. `FromClause::JsonTable(JsonTable)` already flows through
the UPDATE/DELETE AST.

### Executor path

At `dml_join.rs:334` (inside `collect_dml_join_candidates_ctx`) the
`FromClause::JsonTable` branch is currently a `NotImplemented` wall.
Replace with the same two-branch logic used in
`select_joins_ctx.rs::execute_select_with_joins_first_materialized`:

- **Non-correlated** (neither `doc` nor any PASSING expr references
  outer columns, per `jsontable_is_correlated(jt)`): evaluate `doc`
  once against an empty row, call `materialize_json_table`, push rows
  into `scanned[right_idx]` as `DmlJoinRow { values, target: None }`
  — same shape the subquery branch already uses.

- **Correlated**: push a `Vec::new()` placeholder into
  `scanned[right_idx]`, tag the join index in a new
  `correlated_jt: Vec<Option<JsonTableSpec>>` tracker, and during the
  combine loop dispatch to a new `apply_correlated_jt_dml_join(...)`
  helper instead of `apply_dml_join(...)`. The helper re-materializes
  JSON_TABLE once per outer `DmlJoinRow.values`.

The existing `DmlJoinRow` representation (value vector + optional
target RID) cleanly supports JSON_TABLE rows: they have no RID, so
`target = None` — exactly like subquery join rows.

### Algorithm / Semantics

Identical to 11.20a (non-correlated) and 11.20d3 (correlated), except
the row type is `DmlJoinRow` instead of `Row`. The UPDATE / DELETE
engine already handles the target-selection downstream:
`apply_dml_join` propagates `target` from the left side (the table
being modified) through the combined row, so the DML executor knows
which physical row to modify. JSON_TABLE contributes pure data rows
with `target = None`, so a join with a JSON_TABLE right side cannot
promote those JSON rows to UPDATE/DELETE targets — only the left
table's rows get modified. This is the correct SQL semantics (PG,
MySQL, Oracle all agree).

Join-type matrix:

| Join type | Correlated JT | Non-correlated JT |
|---|---|---|
| INNER / CROSS APPLY / CROSS JOIN | emit matches, drop outer | materialize once, INNER join |
| LEFT JOIN / OUTER APPLY | NULL-pad outer if no JT match | materialize once, LEFT join |
| RIGHT / FULL | `NotImplemented` (PG-compat) | `NotImplemented` (no target on JT rows) |

**Rationale for RIGHT/FULL rejection on non-correlated:** JSON_TABLE
rows have no RID; a RIGHT JOIN would try to emit JT-only rows with
NULL target, and the UPDATE / DELETE engine cannot modify anything
from such a row. Clearer to reject than to silently no-op.

## Use cases

```sql
-- Bulk priority update from a JSON payload.
UPDATE orders o
   JOIN JSON_TABLE('[{"id":1,"pri":5},{"id":2,"pri":9}]', '$[*]'
        COLUMNS (id INT PATH '$.id', pri INT PATH '$.pri')) AS j
     ON o.id = j.id
 SET o.priority = j.pri;

-- Correlated: per-order JSON payload drives the UPDATE.
UPDATE orders o
   CROSS APPLY JSON_TABLE(o.meta, '$.flags[*]'
        COLUMNS (flag TEXT PATH '$')) AS j
 SET o.tag = j.flag
 WHERE j.flag LIKE 'urgent%';

-- DELETE driven by JSON_TABLE.
DELETE o FROM orders o
   JOIN JSON_TABLE('[1,2,3]', '$[*]' COLUMNS (id INT PATH '$')) AS j
     ON o.id = j.id;

-- OUTER APPLY in DELETE (still INNER-like semantics for DELETE:
-- unmatched outer rows are NOT deleted, so OUTER APPLY is pointless
-- in a DELETE target — accepted but a no-op vs INNER).
```

## Acceptance criteria

- [ ] `UPDATE t JOIN JSON_TABLE(...) AS j ON ...` modifies `t` rows
      matched against the materialized JSON rows.
- [ ] `UPDATE t CROSS APPLY JSON_TABLE(t.doc, ...) AS j` re-
      materializes per outer row, as in 11.20d3.
- [ ] `DELETE t FROM t JOIN JSON_TABLE(...) AS j ON ...` deletes
      matched rows of `t`.
- [ ] `DELETE t FROM t CROSS APPLY JSON_TABLE(t.doc, ...) AS j`
      correlated form deletes matched rows of `t`.
- [ ] `LEFT JOIN JSON_TABLE(...)` in UPDATE preserves outer row in
      the candidate list but NULL-pads JT columns.
- [ ] NESTED PATH / multi-sibling / WRAPPER / QUOTES / PASSING
      (11.20b/c/d1) all work when JT is a DML source.
- [ ] `UPDATE JSON_TABLE(...)` as first-FROM is rejected — JSON
      rows have no RID, can't be modified.
- [ ] `RIGHT JOIN JSON_TABLE(...)` in UPDATE/DELETE is rejected
      with clear error.
- [ ] Non-correlated regression: existing subquery / table DML
      joins unchanged.
- [ ] 8–12 integration tests in
      `tests/integration_json_table_dml.rs`.
- [ ] 2 new wire smoke assertions under `[11.20d4]`.
- [ ] `cargo test --workspace`, `cargo clippy -- -D warnings`,
      `cargo fmt --check` all clean.

## Out of scope

- `MERGE` with JSON_TABLE source — MERGE itself is not implemented
  yet; this subphase stays narrow to UPDATE/DELETE.
- `INSERT INTO t SELECT ... FROM JSON_TABLE(...)` — already works
  via the SELECT path (11.20a).
- Updating through a JSON_TABLE (as if JT were the target) — SQL
  semantics don't allow it; not a TODO.
- Indexing / predicate pushdown of JT filters — planner work
  (→ 11.21h).

## Dependencies

- Phase 11.20a — `FromClause::JsonTable`, `compile_json_table`,
  `materialize_json_table`, `column_metas_for_spec`,
  `doc_to_serde`.
- Phase 11.20b/c — NESTED PATH infrastructure.
- Phase 11.20d1 — WRAPPER / QUOTES / PASSING.
- Phase 11.20d2 — CROSS APPLY / OUTER APPLY parser sugar.
- Phase 11.20d3 — `jsontable_is_correlated`, per-outer-row
  `apply_correlated_jt_join` pattern.
- Existing `apply_dml_join` and `collect_dml_join_candidates_ctx`
  in `dml_join.rs`.
