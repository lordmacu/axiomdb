# Spec: 21.18 — `NATURAL JOIN`

## What to build

Standard SQL `NATURAL` join modifier: implicit equi-join on all
columns present in both tables with the same name. Matches the
existing `USING(col1, col2, ...)` semantics but with the column list
inferred at analysis time.

```sql
SELECT * FROM a NATURAL JOIN b;
-- ≡ SELECT * FROM a JOIN b USING (shared_col_1, shared_col_2, ...)

SELECT * FROM a NATURAL LEFT JOIN b;
-- ≡ LEFT JOIN variant
```

## Inputs / outputs

### Grammar

```
joined_table :=
    table_ref 'NATURAL' [ join_type ] 'JOIN' table_ref

join_type   := 'INNER' | 'LEFT' [ 'OUTER' ] | 'RIGHT' [ 'OUTER' ] |
               'FULL'  [ 'OUTER' ]
```

No `ON` / `USING` after `NATURAL JOIN` — the column list is computed.
PG rejects `NATURAL CROSS JOIN` (CROSS has no matching condition);
AxiomDB matches.

### Semantics

1. At analysis time, the analyzer walks the left-side bind context
   (accumulated tables so far) and the right-side source's columns.
2. Shared column **names** (case-insensitive per SQL standard)
   produce the USING list, in left-side order.
3. If no columns are shared → parse-time error: "NATURAL JOIN: no
   shared columns between the two sides".
4. The resulting join behaves exactly like `USING(...)` for:
   - Equijoin predicate (equality on each shared column).
   - Projection: `SELECT *` produces each shared column **once**,
     coalesced from the left side for INNER/LEFT, right side for
     RIGHT, and `COALESCE(left, right)` for FULL (per SQL standard).

### AST

Desugar at analyzer time. Parser emits the same
`JoinCondition::Using(Vec<String>)` used by explicit USING, but with
a sentinel empty vec `Using(vec![])` plus a parallel `natural: bool`
flag on the `JoinClause` struct. Analyzer replaces the empty
`Using` with the computed shared-column list when `natural == true`.

Alternative: add `JoinCondition::Natural` as a third variant. Goes
against "desugar cheaply" — plan rejects it.

## Use cases

```sql
-- Common key on both sides.
SELECT * FROM orders NATURAL JOIN customers;
-- ≡ FROM orders JOIN customers USING (customer_id) -- if that's the
--   only shared name

-- Left-preserving form.
SELECT o.id, u.name
  FROM orders o
  NATURAL LEFT JOIN users u;
```

## Acceptance criteria

- [ ] `NATURAL JOIN` parses.
- [ ] `NATURAL INNER JOIN` parses (INNER = default, same semantics).
- [ ] `NATURAL LEFT [OUTER] JOIN` parses and behaves as LEFT JOIN
      USING (shared).
- [ ] `NATURAL RIGHT [OUTER] JOIN` parses.
- [ ] `NATURAL FULL [OUTER] JOIN` parses.
- [ ] `NATURAL CROSS JOIN` is rejected with clear error.
- [ ] No shared columns → clear error at analyze time.
- [ ] Projection dedups shared columns (1 copy per shared name).
- [ ] Shared column match is case-insensitive.
- [ ] `NATURAL JOIN` + `ON` / `USING` after → parse error.
- [ ] Integration tests in `tests/integration_natural_join.rs`.
- [ ] 1 wire smoke assertion.

## Out of scope

- Case-sensitive column match mode (niche; SQL standard is case-
  insensitive on unquoted identifiers — AxiomDB follows).
- NATURAL over subqueries / JSON_TABLE / SRF sources. The shared-
  column set for those already comes from the `BoundTable` virtual
  columns, so it should work. Keep as an informal extension; test
  coverage limited to tables + subqueries.

## Cross-engine

- **PostgreSQL** `gram.y:14615, 14628` — two productions:
  `table_ref NATURAL join_type JOIN table_ref` and `table_ref
  NATURAL JOIN table_ref` (INNER default). Shared columns computed
  in `transformFromClauseItem` at parse-analysis.
- **MySQL 8** — NATURAL [LEFT|RIGHT] JOIN supported; NATURAL FULL
  OUTER JOIN not supported (same as MySQL's FULL OUTER limitation;
  AxiomDB has FULL OUTER via 4.8b so supports NATURAL FULL too).
- **SQL Server** — not supported natively.

## Dependencies

- Existing `JoinCondition::Using(Vec<String>)`.
- Existing USING analyzer + executor paths.
- `BoundTable.columns` in analyzer_bind.rs (for shared-column
  discovery).
