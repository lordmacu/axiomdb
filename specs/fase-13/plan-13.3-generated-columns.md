# Plan: 13.3 — Generated columns

Phase: 13 — Advanced PostgreSQL
Task: 13.3 generated columns
Spec: specs/fase-13/spec-13.3-generated-columns.md
Status: executed 2026-04-23

## Summary

This plan closes `13.3` as a bounded acceptance-and-alignment subphase over the
generated-columns machinery already implemented in `21.5f`. The order is:
first confirm the current behavior and any remaining mismatch with the Phase 13
roadmap, then add only the minimum extra coverage or smoke needed for a clean
Phase-13 closeout, and finally update progress/memory docs to describe the real
contract (`STORED` supported, `VIRTUAL` and `ALTER TABLE` deferred).

## Dependencies

Must be done first:
- [x] `spec-13.3-generated-columns.md` approved
- [x] `13.2` closeout committed

Blocks (until this plan is done):
- [x] Accurate Phase 13 progress for generated columns

## Affected files

New files:
- `specs/fase-13/spec-13.3-generated-columns.md` — task contract
- `specs/fase-13/plan-13.3-generated-columns.md` — execution plan

Modified files:
- `docs/progreso.md` — close `13.3` with explicit bounded scope
- `docs/fase-13.md` — add `13.3` closeout section
- `memory/project_state.md` — move active subphase forward
- `memory/architecture.md` — note the Phase-13 generated-columns contract
- `memory/lessons.md` — capture closeout lesson if new
- `tools/wire-test.py` — optional bounded `13.3` alias/smoke if needed
- `crates/axiomdb-sql/tests/integration_generated_columns.rs` — only if a
  closeout gap appears during audit

## Step 1 — Audit the real contract

**Goal:** confirm exactly what is already implemented and what remains deferred.
**Files:** `integration_generated_columns.rs`, generated-column executor paths,
`docs/progreso.md`, `memory/architecture.md`
**Approach:** read-first audit, then add a failing test only if the current
closeout claim is missing coverage.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_generated_columns
```

## Step 2 — Add bounded closeout coverage if needed

**Goal:** ensure the Phase-13 subphase has a visible acceptance hook, without
re-implementing `VIRTUAL`.
**Files:** `tools/wire-test.py`, optionally
`crates/axiomdb-sql/tests/integration_generated_columns.rs`
**Approach:** prefer a narrow smoke showing the user-facing happy path and one
explicit deferred-path rejection.

### Verification

```bash
python3 tools/wire-test.py
```

## Step 3 — Close docs and memory

**Goal:** mark `13.3` closed with the true scope.
**Files:** `docs/progreso.md`, `docs/fase-13.md`, `memory/project_state.md`,
`memory/architecture.md`, `memory/lessons.md`
**Approach:** describe `STORED` as supported, document `VIRTUAL` and
`ALTER TABLE` as deferred, and reference the existing `21.5f` implementation as
the underlying delivery.

### Verification

```bash
cargo fmt --check
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

## Step 4 — Final closeout

**Goal:** verify the subphase gates and prepare the phase-style commit.

### Verification against spec

- [x] `13.3` scope matches the actual repo state
- [x] dedicated spec/plan exist
- [x] acceptance coverage is green
- [x] wire smoke is green
- [x] docs/memory reflect the true scope
- [x] `cargo test -p axiomdb-sql --test integration_generated_columns` passes
- [x] `python3 tools/wire-test.py` passes
- [x] `cargo test --workspace` passes
- [x] `cargo clippy --workspace -- -D warnings` passes

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `13.3` wording in roadmap implies full `VIRTUAL` parity | high | close explicitly as bounded `STORED` slice with deferred items called out |
| hidden coverage gap in one DML path | medium | reuse `integration_generated_columns.rs` and add only one focused regression if audit finds it |
| duplicated documentation between `13.3` and `21.5f` becomes confusing | medium | phrase `13.3` as a phase closeout over the existing implementation, not a second feature launch |

## Rollback plan

If the audit finds that `13.3` actually requires new product behavior rather
than a closeout:

1. keep the spec in `draft`
2. revise the plan around the missing behavior
3. do not mark `13.3` closed in docs until that implementation lands

## Estimated effort

Total: medium
Per step:
- Step 1: 30-45 min
- Step 2: 30-60 min
- Step 3: 20-30 min
- Step 4: validation time
