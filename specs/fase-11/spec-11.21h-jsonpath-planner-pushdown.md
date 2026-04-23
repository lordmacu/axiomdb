# Spec: 11.21h — JSONPath planner pushdown

## Status

Implemented.

`11.21a-g` already shipped the JSONPath execution surface: `jsonb_path_*`,
`@?`, `@@`, filter combinators, path-vs-path comparison, and arithmetic inside
filters. The remaining gap in this track is planner use of the existing GIN
JSONB index.

Today `planner_select::plan_gin_scan` only extracts probe terms for:

- `col @> jsonb_literal`
- `col ? text_literal`

Even simple JSONPath predicates like `doc @? '$.k'` and `doc @@ '$.flag'`
still fall back to a full scan even when a compatible GIN index already exists.

This subphase adds **bounded planner predicate extraction for simple, safely
indexable JSONPath predicates using the current `jsonb_ops`-style GIN index**.

## What to build

Extend single-table access planning so these predicates can choose
`AccessMethod::GinScan` when a compatible GIN index exists on the JSONB column:

| Predicate | Indexable shape in 11.21h | Probe term |
|---|---|---|
| `doc @? '$.k'` | top-level key existence path | key term `"k"` |
| `doc @@ '$.flag'` | top-level truthy-key match path | key term `"flag"` |

Bounded contract:

- Only **string-literal JSONPath RHS** is considered.
- Only **simple top-level paths** are extracted in this subphase.
- The planner must stay conservative: if a path is ambiguous or not provably
  reducible to a GIN key probe, planning falls back to the existing path.
- Executor semantics remain unchanged: GIN is an acceleration + candidate filter,
  and the original predicate is still re-evaluated on fetched rows.

## Expected behavior

### Planning

- `SELECT ... FROM t WHERE doc @? '$.k'` uses `GinScan` when:
  - `doc` is a base JSONB column
  - RHS is a literal text/json path
  - the path is a simple top-level key lookup
  - there is a GIN index whose first column is `doc`
- `SELECT ... FROM t WHERE doc @@ '$.flag'` uses the same rule for a simple
  top-level boolean-key match path.
- Unsupported JSONPath shapes remain on the existing plan path (`Scan` or any
  other independently chosen access path).

### Recheck

- `GinScan` for JSONPath predicates is always **recheck-required**.
- `EXPLAIN` continues to show `Using GIN index; Using where`.
- False positives are acceptable as long as final row results remain correct
  after predicate re-evaluation.

### JSONPath extraction rules

Supported in `11.21h`:

- `$.k`
- `$.flag`

Explicitly not extracted in `11.21h`:

- nested paths such as `$.a.b`
- array subscripts / wildcards
- filter expressions
- accessor calls like `.size()` / `.type()`
- variable-based paths / PASSING bindings
- function-call forms like `jsonb_path_exists(doc, '$.k')`

## Acceptance criteria

- [ ] Dedicated `11.21h` spec and plan exist.
- [ ] `planner_select::plan_gin_scan` recognizes simple `@?` / `@@` literals.
- [ ] `EXPLAIN SELECT ... WHERE doc @? '$.k'` reports `type = gin` and
      `Using GIN index; Using where` when a matching GIN index exists.
- [ ] `EXPLAIN SELECT ... WHERE doc @@ '$.flag'` reports the same.
- [ ] Unsupported JSONPath shapes do **not** incorrectly force GIN usage.
- [ ] New targeted tests cover:
  - `@? '$.k'` with GIN
  - `@@ '$.flag'` with GIN
  - no GIN index → fallback scan
  - unsupported path shape → fallback scan
  - final row semantics still correct through recheck
- [ ] Wire smoke includes a bounded `11.21h` block.
- [ ] `cargo fmt --check` passes.
- [ ] Targeted SQL tests for JSONPath planner pushdown pass.
- [ ] `python3 tools/wire-test.py` passes.
- [ ] `cargo test --workspace` passes.
- [ ] `cargo clippy --workspace -- -D warnings` passes.

## Out of scope

- New `jsonb_path_ops` GIN opclass.
- Changes to on-disk GIN term encoding.
- Pushdown for nested / filtered / arithmetic JSONPath predicates.
- Planner extraction for `jsonb_path_exists(...)` / `jsonb_path_match(...)`
  function-call syntax.
- Executor-side elimination of predicate recheck.
- Cost-model tuning beyond existing `GinScan` heuristics.

## Approach decision

### Approach A — extend the existing `jsonb_ops` planner path

Pros:
- Reuses shipped `GinScan` infrastructure and existing key-term encoding.
- Keeps the cut small and verifies real value quickly.
- Preserves correctness by leaving full predicate recheck in place.

Cons:
- Only a conservative subset of JSONPath becomes indexable.
- Does not deliver the smaller/faster `jsonb_path_ops` opclass yet.

### Approach B — add `jsonb_path_ops` together with broad JSONPath extraction

Pros:
- Closer to PostgreSQL's full long-term story.
- Better future performance/space potential for path-heavy workloads.

Cons:
- Wrong cut for this subphase.
- Touches catalog / index DDL / maintenance and broadens risk sharply.
- Makes it harder to isolate planner correctness from storage changes.

Chosen: **Approach A**.
