# Plan: 11.18c — JSONB path operators follow-up

## Status

Done.

## Goal

Close `11.18c` around the implementation that already exists in the repo:
`#>`, `#>>`, and `#-` with JSONB-array RHS, dedicated tests, wire smoke, and
clean docs.

## Sprint

Estimate: 1 focused session

├── Task 1: Audit the existing operator surface
│   Description: confirm lexer, parser, evaluator, and tests already cover the
│   intended `11.18c` contract.
│   Dependencies: none
│   Done criterion: the exact supported behavior and the repo/documentation
│   drift are enumerated.
│
├── Task 2: Normalize the subphase contract
│   Description: add dedicated `11.18c` spec/plan files and rewrite stale
│   progress wording so the accepted divergence is explicit.
│   Dependencies: Task 1
│   Done criterion: `specs/fase-11/` has `spec-11.18c-*` and `plan-11.18c-*`,
│   and `docs/progreso.md` reflects JSONB-array RHS instead of a `TEXT[]`
│   blocker.
│
├── Task 3: Validate the existing implementation
│   Description: run targeted SQL + wire gates for `integration_jsonb_path_ops`
│   and confirm the current implementation still passes in the full workspace.
│   Dependencies: Task 1
│   Done criterion: targeted tests, wire smoke, workspace tests, and clippy are
│   green.
│
└── Task 4: Close the subphase
    Description: update `docs/fase-11.md`, `memory/project_state.md`,
    `memory/architecture.md`, and `memory/lessons.md`, then commit the closeout.
    Dependencies: Task 2, Task 3
    Done criterion: `11.18c` appears closed across progress/docs/memory and the
    repo is ready for the next Phase 11 follow-up.

## Risks

- `docs/progreso.md` currently mixes “pending” and “implemented” language for
  `11.18c`; closeout must leave a single canonical statement.
- Wire smoke counts in old notes are stale and must be refreshed to the current
  suite size.
- If the current implementation deviates from the intended JSONB-array contract,
  this subphase can expand from closeout into real bug-fix work.
