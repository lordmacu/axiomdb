# Plan: 13.6 — Non-blocking ALTER TABLE

Phase: 13 — Advanced PostgreSQL
Task: 13.6 Non-blocking ALTER TABLE
Spec: specs/fase-13/spec-13.6-non-blocking-alter-table.md
Status: completed

## Summary

`13.6` should not start as “full online DDL”. The deliverable cut is a
shadow-table rewrite path for heap column changes where the expensive copy runs
off the live table and the exclusive schema cutover is short and explicit.

## Preconditions

- [x] `13.5` closed
- [x] spec approved

## Likely affected areas

- `crates/axiomdb-sql/src/executor/ddl_alter_column.rs`
- `crates/axiomdb-sql/src/executor/exec_dispatch.rs`
- `crates/axiomdb-network/src/mysql/handler.rs`
- `crates/axiomdb-network/src/mysql/shared_db.rs`
- schema/cache invalidation paths
- new integration tests for concurrent read vs rewrite behavior
- wire smoke if the bounded behavior is externally testable

## Step 1 — Pin the current blocking behavior

Goal: make today’s limitation explicit before changing coordination.

Add/confirm tests that show:

- rewrite-heavy heap `ALTER TABLE` succeeds functionally
- concurrent reads/writes currently serialize behind the DDL

Verification:

```bash
cargo test -p axiomdb-network --test integration_concurrency
cargo test -p axiomdb-sql --test integration_executor_ddl
```

Status: done

## Step 2 — Introduce rewrite-plan and shadow-relation helpers

Goal: separate “build new table contents” from “publish new table metadata”.

Implement helpers to:

- derive the destination schema from the `ALTER TABLE` op
- allocate/build a hidden shadow heap relation
- copy transformed rows into the shadow relation

Verification:

```bash
cargo test -p axiomdb-sql --test integration_executor_ddl
```

Status: done

## Step 3 — Add table-local rewrite coordination

Goal: stop target-table writes from racing the shadow copy without taking the
global DDL write lock for the full copy duration.

Implement bounded coordination such as:

- a server-side “rewrite in progress” registry keyed by table id
- write paths check/reject/block if the target table is under rewrite
- ordinary reads still proceed against the live table

Verification:

```bash
cargo test -p axiomdb-network --test integration_concurrency
```

Status: done

## Step 4 — Atomic cutover path

Goal: make the final schema switch short and failure-safe.

Implement:

- brief exclusive cutover window
- final validation
- catalog/root swap
- cache/schema_version invalidation exactly once
- cleanup of old root / abandoned shadow on failure

Verification:

```bash
cargo test -p axiomdb-sql --test integration_executor_ddl
cargo test -p axiomdb-network --test integration_connection_lifecycle
```

Status: done

## Step 5 — Acceptance coverage and closeout

Goal: prove the bounded non-blocking behavior and document the scope honestly.

Coverage should include:

- reader continues during shadow copy
- writer to altered table is coordinated predictably
- failed rewrite does not publish partial metadata
- successful cutover exposes the new schema atomically

Verification:

```bash
python3 tools/wire-test.py
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

## Risk register

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| Shadow relation leaks on failure | medium | centralize cleanup in one cutover/failure path |
| Writers still slip through old path during copy | medium | add explicit table-id rewrite guard in shared write dispatch |
| Cache invalidation races at cutover | medium | keep schema_version bump and schema-cache invalidation inside the final cutover path only |
| Scope explodes into full WAL-delta replay | high | keep concurrent writes out of scope for this subphase |

## Estimated effort

Total: high

## Outcome

Completed with the bounded cut from the spec:

- `SharedDatabase` now orchestrates a special non-blocking heap ALTER path.
- `prepare_nonblocking_heap_alter(...)` builds a shadow heap root and rebuilt
  indexes before publish.
- `commit_nonblocking_heap_alter(...)` swaps metadata in one short exclusive
  window and defers old-page reclamation.
- `integration_concurrency.rs` pins reader liveness + writer rejection.
- `tools/wire-test.py` adds the 13.6 smoke.
