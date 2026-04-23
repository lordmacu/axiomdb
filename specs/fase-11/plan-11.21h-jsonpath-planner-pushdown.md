# Plan: 11.21h — JSONPath planner pushdown

## Status

Done.

## Goal

Teach the existing GIN planner path to recognize a small, safe subset of
JSONPath predicates (`@? '$.k'`, `@@ '$.flag'`) and convert them into
`AccessMethod::GinScan`, while preserving executor recheck semantics and
leaving `jsonb_path_ops` for a later subphase.

## Sprint

Estimate: 1 focused session

├── Task 1: Audit the current GIN planner boundary
│   Description: confirm where `plan_gin_scan` extracts probe terms today and
│   what `EXPLAIN` / executor semantics already guarantee for GIN recheck.
│   Dependencies: none
│   Done criterion: the exact supported and unsupported predicate shapes are
│   enumerated before editing code.
│
├── Task 2: Add conservative JSONPath term extraction
│   Description: extend `planner_select.rs` so simple literal `@?` / `@@`
│   predicates on a base column can emit `GinScan` using the existing key-term
│   encoding.
│   Dependencies: Task 1
│   Done criterion: supported simple paths choose GIN; unsupported ones fall
│   back cleanly.
│
├── Task 3: Add planner regression coverage
│   Description: add targeted SQL tests and `EXPLAIN` assertions for both
│   supported and unsupported JSONPath shapes.
│   Dependencies: Task 2
│   Done criterion: planner behavior is pinned by deterministic tests.
│
├── Task 4: Extend wire smoke
│   Description: add a bounded `11.21h` smoke that exercises a GIN-backed
│   JSONPath predicate over the MySQL wire.
│   Dependencies: Task 2
│   Done criterion: wire smoke covers the user-visible happy path.
│
└── Task 5: Close the subphase
    Description: update progress/docs/memory, run final gates, and record the
    exact bounded contract delivered by `11.21h`.
    Dependencies: Task 3, Task 4
    Done criterion: `11.21h` is closed in docs and validation is green.

## Affected areas

New files:

- `specs/fase-11/spec-11.21h-jsonpath-planner-pushdown.md` — behavioral
  contract for the bounded planner slice.
- `specs/fase-11/plan-11.21h-jsonpath-planner-pushdown.md` — execution plan.

Modified files:

- `crates/axiomdb-sql/src/planner_select.rs` — JSONPath-to-GIN extraction.
- `crates/axiomdb-sql/tests/` — targeted planner / `EXPLAIN` coverage.
- `tools/wire-test.py` — bounded `11.21h` smoke.
- `docs/progreso.md` — closeout and clarified scope.
- `memory/project_state.md` — move active follow-up after closure.
- `docs/fase-11.md` — subphase closeout note.
- `memory/architecture.md` / `memory/lessons.md` — planner invariant + lesson.

## Risks

- JSONPath parsing here must remain more conservative than evaluation; a false
  negative is acceptable, a false positive that skips recheck is not.
- `@@` has broader semantics than plain key existence, so the extracted subset
  must stay intentionally narrow.
- Old roadmap wording mentions `jsonb_path_ops`; closeout docs must separate
  “delivered now” from “still deferred”.
