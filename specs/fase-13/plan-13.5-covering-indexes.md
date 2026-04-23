# Plan: 13.5 — Covering indexes

Phase: 13 — Advanced PostgreSQL
Task: 13.5 Covering indexes
Spec: specs/fase-13/spec-13.5-covering-indexes.md
Status: draft

## Summary

`13.5` is not a new SQL surface sprint. The parser/catalog pieces already
exist; the missing work is physical storage plus runtime use of
`include_columns`. The plan is to extend heap secondary-index leaf encoding
with INCLUDE payloads, thread that through index maintenance, then upgrade
planner/executor coverage logic from “key columns only” to “key + include”.

## Preconditions

- [x] `13.4` closed
- [ ] `spec-13.5-covering-indexes.md` approved

## Affected files

Likely touched:

- `crates/axiomdb-sql/src/planner_select.rs`
- `crates/axiomdb-sql/src/planner_types.rs`
- `crates/axiomdb-sql/src/executor/select_ctx.rs`
- `crates/axiomdb-sql/src/executor/select_helpers.rs`
- index maintenance / key encoding paths in `crates/axiomdb-sql/src/`
- heap/index leaf decode helpers in storage/index crates if needed
- `crates/axiomdb-sql/tests/integration_index_only.rs`
- `tools/wire-test.py`
- closeout docs/memory files

## Step 1 — Audit and pin the current gap

Goal: make the missing behavior explicit in tests before changing storage.

Add or tighten tests that show:

- `INCLUDE (...)` is accepted and persisted
- planner does **not** currently cover non-key projected columns
- fallback to heap access still returns correct results

Verification:

```bash
cargo test -p axiomdb-sql --test integration_index_only
```

## Step 2 — Add INCLUDE payload storage for heap secondary leaves

Goal: make a secondary-index leaf entry able to carry non-key projected values.

Approach:

- extend the leaf-entry encoding in a backward-compatible way
- keep key bytes and RID semantics intact
- decode included payload separately from key columns

Verification:

```bash
cargo test -p axiomdb-sql --test integration_index_only
```

## Step 3 — Maintain included payloads on writes

Goal: keep covering payload bytes correct across INSERT / UPDATE / DELETE and
existing rebuild paths.

Approach:

- update shared secondary-index maintenance helpers
- ensure rewrites of key columns and included columns both regenerate entries
- validate rebuild paths (`CREATE INDEX`, `ALTER TABLE` rebuilds) keep payloads

Verification:

```bash
cargo test -p axiomdb-sql
```

## Step 4 — Upgrade planner and executor coverage

Goal: allow real `IndexOnlyScan` on projections covered by key + include.

Approach:

- extend planner coverage detection to include `index_def.include_columns`
- extend `AccessMethod::IndexOnlyScan` metadata if needed so executor knows
  which projected columns come from key bytes vs included payload
- keep `SELECT *` and complex projections conservative

Verification:

```bash
cargo test -p axiomdb-sql --test integration_index_only
```

## Step 5 — Wire smoke and closeout

Goal: pin the user-visible outcome over MySQL wire and document the bounded
heap-only contract honestly.

Verification:

```bash
python3 tools/wire-test.py
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

## Risk register

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| leaf-format change breaks legacy indexes | medium | add versioned/backward-compatible decode and legacy regression coverage |
| planner claims covering scan but executor cannot reconstruct row shape | medium | carry explicit projection metadata in `IndexOnlyScan` instead of inferring ad hoc |
| write paths miss one index-rebuild path | medium | lean on shared maintenance helpers and rebuild-focused tests |
| clustered tables accidentally take the new heap-only path | low | keep clustered normalization/fallback explicit and tested |

## Estimated effort

Total: high
