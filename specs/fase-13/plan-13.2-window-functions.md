# Plan: 13.2 — Window functions

Phase: 13 — Advanced PostgreSQL
Task: 13.2 window functions
Spec: specs/fase-13/spec-13.2-window-functions.md
Status: executed 2026-04-23

## Summary

This plan delivers the first bounded window-function slice without inventing a
full generic window engine. The order is: add parser/AST support for
`OVER (...)`, reject illegal placements during analysis, then evaluate
`ROW_NUMBER`, `RANK`, and `DENSE_RANK` in a post-scan row-decoration phase over
materialized result rows. Metadata and wire smoke close the subphase after the
SQL behavior is stable.

## Dependencies

Must be done first:
- [x] `spec-13.2-window-functions.md` approved

Blocks (until this plan is done):
- [ ] Phase 13 follow-ups for `LAG/LEAD`, aggregate windows, and frame clauses

## Affected files

New files:
- `crates/axiomdb-sql/tests/integration_window_functions.rs` — dedicated SQL acceptance coverage

Modified files:
- `crates/axiomdb-sql/src/lexer.rs` — add `OVER` token if the grammar uses it as reserved keyword
- `crates/axiomdb-sql/src/ast.rs` — add window AST nodes
- `crates/axiomdb-sql/src/parser/expr.rs` — parse `func(...) OVER (...)`
- `crates/axiomdb-sql/src/analyzer_stmt.rs` / related analyzer files — reject illegal placements and unsupported mixes
- `crates/axiomdb-sql/src/executor/select_core.rs` / `select_ctx.rs` / helper modules — evaluate window columns
- `tools/wire-test.py` — bounded `13.2` smoke
- closeout docs/memory files

## Step 1 — Parse and model window expressions

**Goal:** represent supported `OVER (...)` syntax explicitly in the AST.
**Files:** `lexer.rs`, `ast.rs`, `parser/expr.rs`, parser tests if needed.
**Approach:** TDD — add parser tests for supported and malformed syntax before wiring execution.

### Test to add

Add parser coverage for:

- `ROW_NUMBER() OVER (ORDER BY id)`
- `RANK() OVER (PARTITION BY dept ORDER BY salary DESC)`
- malformed cases like missing `ORDER BY`

### Implementation outline

- Introduce a `WindowSpec` AST shape with `partition_by` and `order_by`.
- Represent supported window calls explicitly instead of as plain `Expr::Function`.
- Parse `OVER (...)` immediately after a normal function call.
- Keep unsupported options (`ROWS`, `RANGE`, named windows) rejected early.

### Verification

```bash
cargo test -p axiomdb-sql --lib parser
```

## Step 2 — Semantic validation and scope rules

**Goal:** reject unsupported placements and combinations before execution.
**Files:** analyzer modules, aggregate/window validation helpers.
**Approach:** TDD — add failing tests for illegal placements and grouped-query mixes.

### Test to add

- window function in `WHERE` errors
- `LAG(...) OVER (...)` rejected as unsupported
- grouped aggregate + window in same query rejected

### Implementation outline

- Detect window expressions in `SELECT`.
- Walk `WHERE`, `GROUP BY`, `HAVING`, join predicates, and DDL expression sites
  to reject any window usage there.
- Add one validation pass that forbids grouped aggregate/window coexistence in
  this MVP.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_window_functions
```

## Step 3 — Execute ranking windows over materialized rows

**Goal:** compute `ROW_NUMBER`, `RANK`, and `DENSE_RANK` correctly per partition/order.
**Files:** `select_core.rs`, `select_ctx.rs`, helper module if needed.
**Approach:** TDD — start with unpartitioned `ROW_NUMBER`, then ties/partitions/rank variants.

### Test to add

- basic `ROW_NUMBER() OVER (ORDER BY id)`
- partition reset with `PARTITION BY dept`
- tie behavior for `RANK` and `DENSE_RANK`
- outer `ORDER BY` differing from window `ORDER BY`

### Implementation outline

- Materialize candidate rows after `FROM` + `WHERE`.
- For each distinct window spec, derive partition groups and an internal sort
  order over row indexes.
- Compute output vectors for each supported function:
  - row number: sequential index
  - rank: peer-aware skipped rank
  - dense rank: peer-aware dense rank
- Inject computed values during final projection.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_window_functions
```

## Step 4 — Metadata, wire smoke, and closeout

**Goal:** close the slice with protocol-visible coverage and docs.
**Files:** `tools/wire-test.py`, `docs/progreso.md`, `docs/fase-13.md`, memory files.
**Approach:** add one bounded smoke and update closeout records only after targeted tests are green.

### Test to add

- wire smoke for one ranking query over partitions

### Implementation outline

- Add `[13.2 window functions]` block to `tools/wire-test.py`.
- Update subphase state, architecture notes, and lessons.
- Run full workspace gates.

### Verification against spec

- [x] `OVER (...)` syntax parses into explicit AST nodes
- [x] `ROW_NUMBER`, `RANK`, and `DENSE_RANK` work with `PARTITION BY` + `ORDER BY`
- [x] illegal placements error explicitly
- [x] dedicated SQL + wire coverage exists
- [x] `cargo test --workspace` passes
- [x] `cargo clippy --workspace -- -D warnings` passes

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Window evaluation accidentally fights existing aggregate/group pipeline | medium | keep grouped-query coexistence out of scope in Step 2 |
| Reusing final `ORDER BY` semantics for window peer comparison diverges subtly | medium | pin tie/null-order tests in Step 3 |
| Parser accepts too much future syntax (`ROWS`, named windows) | low | reject unsupported tokens early in Step 1 |

## Estimated effort

Total: 1.5–2.5 days

- Step 1: 2–3 hours
- Step 2: 2–4 hours
- Step 3: 6–10 hours
- Step 4: 2–3 hours
