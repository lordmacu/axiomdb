# Spec: 21.3 — `WITH RECURSIVE` CTE

## What to build

Self-referencing CTEs — body is `base UNION [ALL] step` where `step`
references the CTE name. Iterates until step yields empty.

```sql
-- Counter 1..10
WITH RECURSIVE counter(n) AS (
  SELECT 1
  UNION ALL
  SELECT n + 1 FROM counter WHERE n < 10
)
SELECT n FROM counter;

-- Tree expansion
WITH RECURSIVE sub AS (
  SELECT id, name, manager_id, 0 AS depth FROM emp WHERE id = 1
  UNION ALL
  SELECT e.id, e.name, e.manager_id, s.depth + 1
    FROM emp e JOIN sub s ON e.manager_id = s.id
)
SELECT * FROM sub;
```

## Grammar

Same WITH list as 21.2 but with optional `RECURSIVE` keyword after
`WITH`:

```
WITH RECURSIVE cte_list
```

`RECURSIVE` applies to the whole list; any CTE in the list may be
recursive. Each recursive CTE body must be a SetOp shape:

```
SELECT <base>
UNION [ALL]
SELECT <step>   -- may reference cte_name
```

Multi-step chains (base UNION SELECT UNION SELECT) not supported in
this subphase.

## AST

New `FromClause::RecursiveCte`:

```rust
pub enum FromClause {
    ...
    RecursiveCte(Box<RecursiveCteClause>),
}

pub struct RecursiveCteClause {
    pub alias: String,
    pub column_names: Option<Vec<String>>,
    pub base: Box<SelectStmt>,
    pub step: Box<SelectStmt>,
    pub union_all: bool,   // true = UNION ALL, false = UNION (dedup)
}
```

Add flag to parser context so `RECURSIVE` keyword is consumed per
WITH list.

## Analyzer

Expansion:
1. Analyze base (no CTE self-reference allowed in base).
2. Column types + names inferred from base select list (overridable
   by `column_names`).
3. Analyze step with a stub binding — the CTE name resolves to the
   base's schema (types/col count). At execute time the stub is
   replaced by the working set.
4. Substitute `FromClause::Table(cte_name)` refs in the outer query
   into `FromClause::RecursiveCte { base, step, ... }`.

## Executor

Runtime algorithm (PG-parity):

```
wt = exec(base)                    # working set
rt = wt.clone()                    # result set
iteration_depth = 0
while !wt.is_empty() and iteration_depth < MAX_RECURSION {
    # bind cte_name → wt, run step
    new_rows = exec(step with cte_name bound to wt as VALUES)
    if !union_all {
        new_rows = new_rows - rt     # dedup
    }
    if new_rows.is_empty() { break }
    rt.extend(new_rows.clone())
    wt = new_rows
    iteration_depth += 1
}
return rt
```

MAX_RECURSION = 1000 (PG default). Configurable later via
`max_recursion_depth` session var.

## Acceptance criteria

- [ ] Counter `1..10` returns 10 rows.
- [ ] Self-ref in step resolved via working set.
- [ ] Tree expansion (2-level hierarchy) returns all descendants.
- [ ] UNION dedup: `UNION` (no ALL) eliminates duplicates.
- [ ] UNION ALL keeps duplicates.
- [ ] `WITH` without RECURSIVE keyword + self-ref → parse error
      (points to 21.3 usage).
- [ ] Max recursion depth enforced (blocks infinite loops with
      clear error).
- [ ] Non-recursive CTE still works (regression).
- [ ] Integration tests 8+.

## Out of scope

- Multi-step SetOp chains (`base UNION s1 UNION s2`).
- CTEs referencing earlier recursive CTEs.
- SEARCH / CYCLE clauses (PG 14+).
- Recursive CTE in DML sources.

## Dependencies

- Parser already has SetOp for UNION in 21.2.
- Need `FromClause::RecursiveCte` variant (~10 match sites to update).
