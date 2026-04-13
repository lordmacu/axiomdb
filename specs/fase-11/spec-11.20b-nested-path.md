# Spec: 11.20b — `JSON_TABLE` single-level `NESTED PATH`

## What to build (not how)

Extend 11.20a `JSON_TABLE` with single-level `NESTED PATH` shredding. One
parent row path, one nested child path per parent. Cartesian combination of
parent match × child matches, with **LEFT-OUTER NULL padding** when a parent
match produces zero child matches. Independent ordinality counters at each
level.

Supported grammar addition inside `COLUMNS(...)`:

```
column_def := ...
            | NESTED [ PATH ] 'jsonpath' COLUMNS ( column_def (',' column_def)* )
```

Note: `PATH` keyword is optional after `NESTED` in SQL:2016 and MariaDB, both
accepted.

## Inputs / Outputs

**Input**: same shape as 11.20a (`Value::Jsonb` / `Value::Json` / `Value::Text`).

**Output**: a row source whose column schema is the **flattened** list — all
parent direct columns, followed by all nested child columns, in declaration
order. Row count = Σ over parent matches of `max(1, child_matches)`.

## Use cases

```sql
-- Shred invoices with their line items
SELECT inv_id, item_name, qty
FROM JSON_TABLE(
    '[
       {"id":1, "items":[{"name":"A","qty":2},{"name":"B","qty":3}]},
       {"id":2, "items":[]},
       {"id":3, "items":[{"name":"C","qty":1}]}
     ]',
    '$[*]' COLUMNS (
        inv_id INT PATH '$.id',
        NESTED PATH '$.items[*]' COLUMNS (
            item_name TEXT PATH '$.name',
            qty       INT  PATH '$.qty'
        )
    )
) AS t;
-- (1, 'A', 2)
-- (1, 'B', 3)
-- (2, NULL, NULL)   ← LEFT OUTER pad: id=2 has empty items
-- (3, 'C', 1)

-- Ordinality at both levels
SELECT ord_inv, inv_id, ord_item, item_name
FROM JSON_TABLE(
    '[{"id":10, "items":[{"name":"A"},{"name":"B"}]}]',
    '$[*]' COLUMNS (
        ord_inv FOR ORDINALITY,
        inv_id  INT PATH '$.id',
        NESTED PATH '$.items[*]' COLUMNS (
            ord_item FOR ORDINALITY,
            item_name TEXT PATH '$.name'
        )
    )
) AS t;
-- (1, 10, 1, 'A')
-- (1, 10, 2, 'B')   ← inner ordinal resets per parent match
```

## Acceptance criteria

- [ ] Parser accepts `NESTED [PATH] '<jsonpath>' COLUMNS (...)` inside a
  parent `COLUMNS(...)` list.
- [ ] `NESTED` column can contain `Regular`, `Ordinality`, `Exists` — same
  three forms as top-level (recursive allowed grammatically; multi-level
  enforcement is checked but runtime rejects depth ≥ 2 with explicit
  `NotImplemented` → 11.20c).
- [ ] **Single sibling only at 11.20b**: two `NESTED PATH`s inside the same
  `COLUMNS(...)` → `NotImplemented`; deferred to 11.20c.
- [ ] Output schema is the flattened concatenation of declaration order;
  column aliases unique across the full flattened list (duplicate names →
  parse error).
- [ ] **LEFT OUTER NULL padding**: parent match with zero child matches
  emits one row with all nested-column slots set to `NULL`; parent-column
  slots and any parent `FOR ORDINALITY` still populated normally.
- [ ] **Per-level ordinality**: the parent's `FOR ORDINALITY` increments
  once per parent match (total rows it appears with can be > 1 thanks to
  child fan-out, but the value itself reflects the parent-match index).
  The nested's `FOR ORDINALITY` resets to 1 at the start of each parent
  match and increments once per child match.
- [ ] Parent with N matches, each with K_i children → Σ max(1, K_i) rows;
  verified by integration tests.
- [ ] All 11.20a acceptance criteria still hold — same semantics for
  `ON EMPTY` / `ON ERROR` / `EXISTS PATH` on leaf columns of any level.
- [ ] Column references across levels remain addressable by alias in the
  surrounding SELECT / WHERE (e.g. `WHERE item_name = 'A'` works).
- [ ] Wire-smoke: at least two new assertions covering LEFT-OUTER pad and
  dual-level ordinality.

## Out of scope (→ later subphases)

- **Multi-sibling NESTED PATH** inside the same `COLUMNS(...)` (UNION
  semantics across siblings) → 11.20c.
- **Multi-level nesting** (NESTED inside NESTED, depth ≥ 2) → 11.20c.
- `WRAPPER` / `QUOTES` / `PASSING` on the row path → 11.20d.
- LATERAL-correlated `doc` expression → 11.20d.
- Optimizer cost model for NESTED PATH → future phase.

## Dependencies

- 11.20a — parser, AST, executor skeleton, compile/materialize helpers.
- Same JSONPath walker used at 11.20a (`walk_path_owned` against
  `serde_json::Value`).
- `axiomdb_types::coerce::coerce` for column type coercion on leaves.

## ⚠️ DEFERRED

- Multi-sibling and multi-level NESTED → 11.20c (same recursive-walk
  generalization; grammar already supports it, runtime gate is a single
  depth counter + sibling iteration).
