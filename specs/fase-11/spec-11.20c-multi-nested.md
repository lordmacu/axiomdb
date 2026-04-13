# Spec: 11.20c — `JSON_TABLE` multi-sibling + multi-level `NESTED PATH`

## What to build

Lift the two 11.20b scope guards:

1. **Multi-sibling NESTED**: two or more `NESTED PATH` entries inside the
   same `COLUMNS(...)` list — UNION semantics across siblings (PG
   `JsonTableSiblingJoin` / MariaDB `m_next_nested`).
2. **Multi-level NESTED**: `NESTED PATH` inside another `NESTED PATH`
   arbitrary depth (bounded only by the AST recursion limit, in practice
   ≤ 16 levels).

No new SQL surface — the grammar already accepts both (11.20b just runtime-
rejected). 11.20c drops the compile-time guards and generalizes the row
emitter to a fully recursive walker.

## Semantics (UNION across siblings)

For parent match `P` with two sibling NESTED `A` and `B`:

```
rows_for(P) = rows_from_A(P) ∪ rows_from_B(P)
```

where each `rows_from_X(P)` emits `max(1, |child_matches_X|)` rows; the
other siblings' slots stay `NULL` (LEFT-OUTER pad) in those rows.

Consequences:

- If both `A` and `B` are empty → parent emits 2 rows, both all-NULL in the
  nested ranges (one LEFT-OUTER pad per sibling). PG / MariaDB parity.
- Multi-sibling is NOT a cartesian product. `|A|×|B|` rows would require
  an explicit cross-join semantics that the spec does not define.

## Semantics (depth ≥ 2)

Each level repeats the rules of 11.20b:
- Parent emits one row per child match.
- Empty child list → one LEFT-OUTER pad row with the entire nested range
  NULL (including any deeper nested descendants).
- `FOR ORDINALITY` per level is independent — resets to 1 at every entry
  into that level's iteration.

## Use cases

```sql
-- Multi-sibling UNION
SELECT inv_id, price, tag FROM JSON_TABLE(
  '[{"id":1,"prices":[10,20],"tags":["a","b","c"]}]',
  '$[*]' COLUMNS (
      inv_id INT PATH '$.id',
      NESTED PATH '$.prices[*]' COLUMNS (price INT  PATH '$'),
      NESTED PATH '$.tags[*]'   COLUMNS (tag   TEXT PATH '$')
  )
) AS t;
-- (1, 10,   NULL)
-- (1, 20,   NULL)
-- (1, NULL, 'a')
-- (1, NULL, 'b')
-- (1, NULL, 'c')
```

```sql
-- Multi-level
SELECT inv_id, line_id, part FROM JSON_TABLE(
  '[{"id":1,"lines":[
       {"lid":"L1","parts":["P1","P2"]},
       {"lid":"L2","parts":[]}
  ]}]',
  '$[*]' COLUMNS (
      inv_id INT PATH '$.id',
      NESTED PATH '$.lines[*]' COLUMNS (
          line_id TEXT PATH '$.lid',
          NESTED PATH '$.parts[*]' COLUMNS (
              part TEXT PATH '$'
          )
      )
  )
) AS t ORDER BY line_id, part;
-- (1, 'L1', 'P1')
-- (1, 'L1', 'P2')
-- (1, 'L2', NULL)   ← inner empty → LEFT-OUTER pad
```

## Acceptance criteria

- [ ] Multiple `NESTED PATH` siblings in the same `COLUMNS(...)` are
  accepted and produce UNION semantics.
- [ ] NESTED depth ≥ 2 is accepted.
- [ ] Both empty siblings → 2 LEFT-OUTER pad rows per parent match.
- [ ] Inner empty at level ≥ 2 → a single LEFT-OUTER pad row at that
  level (parent + outer-level leaves still populated).
- [ ] `FOR ORDINALITY` at any level resets per enclosing iteration.
- [ ] Unique column names across every level enforced (same as 11.20b).
- [ ] 11.20a + 11.20b regression suites still pass unchanged.
- [ ] Recursion guard: depth > 32 → compile-time error (defensive limit
  to avoid pathological AST recursion; no real workload needs deeper).

## Out of scope (→ 11.20d)

- `WRAPPER` / `QUOTES` on JSON_TABLE columns.
- `PASSING name AS var` on the row path.
- LATERAL-correlated `doc` expressions.
- `JSON_TABLE` as UPDATE / DELETE / MERGE source.
- `JSON_TABLE` as first FROM entry combined with JOINs.

## Dependencies

Builds on 11.20a (parser + AST + first-FROM executor + JOIN right-side
arm) and 11.20b (NESTED AST variant + DFS slot layout + recursive
`column_defs_for_ast`).

## Plan highlights

- `compile_columns_recursive`: drop the `depth >= 1` guard, drop the
  `nested_count > 1` guard. Add a defensive `depth > 32 → error`.
- `materialize_json_table`: refactor into a recursive `emit_rows_rec`
  that handles any depth and any number of sibling NESTED per level.
  One function replaces the current split between "fill leaves" and
  "expand nested" — multi-sibling UNION falls out naturally.
- Delete the two `*_not_yet_implemented` tests from
  `integration_json_table_nested.rs`; replace with success tests in a
  new `integration_json_table_multi.rs`.

## ⚠️ DEFERRED

- 11.20d remains: WRAPPER/QUOTES/PASSING + LATERAL + UPDATE/DELETE/MERGE
  sources + first-FROM JSON_TABLE combined with JOINs.
