# Plan: 13.12 — Statement-level triggers

Phase: 13 — Advanced PostgreSQL
Task: 13.12 Statement-level triggers
Spec: specs/fase-13/spec-13.12-statement-level-triggers.md
Status: completed

## Summary

This plan delivers `13.12` as a bounded validation-trigger MVP, not as the full
trigger system deferred to Phase 16. The order is: add parser/AST support for a
statement-level `AFTER` trigger with a single `SELECT` body, persist trigger
metadata in the table catalog, execute matching triggers once after
`INSERT/UPDATE/DELETE`, expose minimal trigger metadata through session
variables, and lock the behavior down with SQL and wire acceptance tests.

## Dependencies

Must be done first:
- [x] `spec-13.12-statement-level-triggers.md` approved
- [x] `13.6` closeout committed

Blocks (until this plan is done):
- [x] later Phase 16 trigger work can assume a base catalog model exists

## Affected files

New files:
- `specs/fase-13/spec-13.12-statement-level-triggers.md`
- `specs/fase-13/plan-13.12-statement-level-triggers.md`
- likely `crates/axiomdb-sql/tests/integration_statement_triggers.rs`

Modified files:
- parser / AST files for `CREATE TRIGGER`, `DROP TRIGGER`, and trigger body
- catalog table metadata files to persist trigger definitions
- DDL executor files for trigger create/drop
- DML executor files for post-statement trigger dispatch
- `crates/axiomdb-sql/src/session.rs` for trigger execution context variables
- `tools/wire-test.py`
- closeout docs / memory files

## Step 1 — Parse bounded trigger DDL

**Goal:** represent the MVP trigger surface explicitly and reject out-of-scope
forms early.

**Files:** lexer/parser/AST files.

**Approach:** TDD around:
- accepted: `AFTER INSERT|UPDATE|DELETE ... FOR EACH STATEMENT AS SELECT ...`
- rejected: `BEFORE`, `FOR EACH ROW`, non-`SELECT` bodies, unsupported clauses

### Verification

```bash
cargo test -p axiomdb-sql --test integration_ddl_parser
```

## Step 2 — Persist trigger metadata in catalog

**Goal:** store trigger definitions as table-owned metadata with stable firing
order.

**Files:** catalog row format and `TableDef` serialization / deserialization.

**Approach:** keep the on-disk model narrow: event, timing, granularity, name,
body SQL, creation ordinal.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_executor_ddl
```

## Step 3 — Hook post-statement trigger execution

**Goal:** execute matching triggers exactly once after successful
`INSERT/UPDATE/DELETE` statement work and before the outer statement is
finalized.

**Files:** DML executor entry points and shared statement-finalization helpers.

**Approach:** centralize dispatch at the statement boundary instead of inside
row loops so multi-row statements still fire once.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_statement_triggers
```

## Step 4 — Validation query semantics and trigger context

**Goal:** let the trigger body validate the just-applied statement and abort it
cleanly when needed.

**Files:** trigger execution helper, session variable plumbing.

**Approach:** execute the stored `SELECT` in a read-only trigger context with
`@@trigger_name`, `@@trigger_table`, `@@trigger_event`, and
`@@trigger_row_count`; treat any returned row as validation failure.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_statement_triggers
```

## Step 5 — Wire smoke and closeout

**Goal:** freeze the externally visible MVP and document it honestly.

**Files:** `tools/wire-test.py`, `docs/progreso.md`, `docs/fase-13.md`,
`memory/project_state.md`, `memory/architecture.md`, `memory/lessons.md`

**Approach:** wire smoke should prove the intended accounting-style case:
balanced batch insert passes, unbalanced batch insert fails, and the failed
statement leaves no rows behind.

### Verification against spec

- [x] bounded trigger DDL parses
- [x] metadata persists in catalog
- [x] triggers fire once per statement
- [x] validation query can reject the outer statement
- [x] `@@trigger_row_count` is visible inside the trigger body
- [x] balanced batch passes, unbalanced batch rolls back
- [x] wire smoke is green
- [x] docs/memory reflect the delivered scope
- [x] `cargo test -p axiomdb-sql` for touched tests passes
- [x] `python3 tools/wire-test.py` passes
- [x] `cargo test --workspace` passes
- [x] `cargo clippy --workspace -- -D warnings` passes

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| trigger dispatch gets duplicated across many DML paths | high | centralize post-statement firing in shared executor finalization helpers |
| trigger failure breaks savepoint / statement rollback semantics | medium | reuse existing statement rollback machinery and add savepoint regression tests |
| allowing generic trigger bodies explodes scope toward stored procedures | high | restrict the MVP body to one read-only `SELECT` |
| catalog format change leaks into unrelated metadata paths | medium | keep trigger metadata table-owned and versioned narrowly |

## Rollback plan

If catalog or executor impact grows too much:

1. keep the spec/plan in draft
2. do not mark `13.12` closed
3. narrow the public syntax further before implementation

## Estimated effort

Total: high

Per step:
- Step 1: 45-75 min
- Step 2: 1-2 h
- Step 3: 1-2 h
- Step 4: 1 h
- Step 5: 30-45 min
