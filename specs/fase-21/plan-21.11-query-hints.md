# Plan: 21.11 — Query hints

Phase: 21 — Advanced SQL
Task: 21.11 Query hints
Spec: specs/fase-21/spec-21.11-query-hints.md
Status: completed

## Summary

Implement `21.11` as a bounded optimizer-comment MVP for `SELECT` only.
First preserve `/*+ ... */` comments before the lexer discards ordinary block
comments. Then parse those hints into `SelectStmt.hints`. After that wire the
two real semantics:

- `INDEX(table index)` steers planner index choice when compatible
- `HASH_JOIN` overrides the join size-threshold heuristic for eligible equijoins

`PARALLEL(n)` is parsed and propagated as advisory metadata only. Finish with
parser/planner/EXPLAIN/wire tests and subphase closeout.

## Dependencies

Must be done first:
- [ ] `specs/fase-21/spec-21.11-query-hints.md` approved.
- [ ] Existing SELECT modifier parsing (`HIGH_PRIORITY`, `STRAIGHT_JOIN`, etc.)
      remains green.

Blocks:
- [ ] Future richer optimizer-hint work can build on the stored hint model.

## Affected files

New files:
- `crates/axiomdb-sql/tests/integration_query_hints.rs`
- `specs/fase-21/spec-21.11-query-hints.md`
- `specs/fase-21/plan-21.11-query-hints.md`

Modified files:
- `crates/axiomdb-sql/src/lexer.rs` — preserve optimizer comments instead of
  dropping them with ordinary block comments.
- `crates/axiomdb-sql/src/ast.rs` — add `SelectHint` and `SelectStmt.hints`.
- `crates/axiomdb-sql/src/parser/dml.rs` — parse `SELECT /*+ ... */` hints.
- `crates/axiomdb-sql/src/parser/mod.rs` — thread hint-aware preprocessing if needed.
- `crates/axiomdb-sql/src/planner_select.rs` — hinted index selection.
- `crates/axiomdb-sql/src/planner_ctx.rs` — context-aware hint filtering if required.
- `crates/axiomdb-sql/src/executor/joins.rs` — hash-join threshold override.
- `crates/axiomdb-sql/src/executor/exec_explain.rs` — visible explain output.
- `crates/axiomdb-sql/src/table_scan.rs` or select execution plumbing — optional
  advisory `PARALLEL(n)` threading if surfaced.
- `tools/wire-test.py` — 21.11 smoke.
- `docs/progreso.md`, `memory/project_state.md`, `docs/fase-21.md`,
  `memory/architecture.md`, `memory/lessons.md` — closeout.

## Step 1 — Hint capture before comment skipping

**Goal:** preserve optimizer comments so they can reach the parser.
**Files:** `lexer.rs`, possibly parser entry plumbing.
**Approach:** keep ordinary `/* ... */` comments skipped, but add a narrow path
for `/*+ ... */` so only optimizer hints survive.

### Tests to add

```rust
#[test]
fn tokenize_optimizer_hint_after_select() { ... }

#[test]
fn regular_block_comments_are_still_skipped() { ... }

#[test]
fn version_comments_keep_existing_behavior() { ... }
```

### Implementation outline

- Add a preprocessing step or lexer-side extraction for optimizer hints.
- Preserve only the payload needed for `SELECT /*+ ... */`.
- Leave regular block comments and MySQL version comments unchanged.

### Verification

```bash
cargo test -p axiomdb-sql lexer
```

## Step 2 — AST and parser support

**Goal:** parse bounded query hints into a real AST representation.
**Files:** `ast.rs`, `parser/dml.rs`, parser tests.
**Approach:** introduce a small `SelectHint` enum and reject unsupported hint
names early so the MVP stays deterministic.

### Tests to add

```rust
#[test]
fn parse_select_hash_join_hint() { ... }

#[test]
fn parse_select_parallel_hint() { ... }

#[test]
fn parse_select_index_hint() { ... }

#[test]
fn reject_unknown_optimizer_hint() { ... }

#[test]
fn reject_parallel_zero() { ... }
```

### Implementation outline

- Add `SelectHint::{Index, HashJoin, Parallel}`.
- Add `SelectStmt.hints: Vec<SelectHint>`.
- Parse only the supported `SELECT /*+ ... */` placement.
- Support multiple hints in one comment.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_ddl_parser
```

## Step 3 — Planner semantics for `INDEX(table index)`

**Goal:** let a query hint steer planner index choice without forcing invalid plans.
**Files:** `planner_select.rs`, `planner_ctx.rs`, maybe select planning helpers.
**Approach:** resolve the hinted table/index name against the current SELECT,
prefer that index when compatible, and otherwise fall back to the existing plan.

### Tests to add

```rust
#[test]
fn explain_uses_hinted_index_lookup() { ... }

#[test]
fn explain_uses_hinted_index_range() { ... }

#[test]
fn hinted_unknown_index_errors() { ... }

#[test]
fn incompatible_hinted_index_falls_back_cleanly() { ... }
```

### Implementation outline

- Resolve table/alias matching for `INDEX(...)`.
- Verify the named index belongs to that table.
- Reuse existing compatibility checks for partial / expression / ordinary indexes.
- Never synthesize an invalid lookup/range plan just because a hint was present.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_query_hints
```

## Step 4 — Executor semantics for `HASH_JOIN`

**Goal:** let `HASH_JOIN` bypass the current size heuristic when hash join is legal.
**Files:** `executor/joins.rs`, select execution plumbing, explain coverage.
**Approach:** keep all existing join legality checks; only override the
`HASH_JOIN_MIN_ROWS` heuristic.

### Tests to add

```rust
#[test]
fn explain_hash_join_hint_overrides_small_join_threshold() { ... }

#[test]
fn non_equijoin_hash_join_hint_falls_back_to_nested_loop() { ... }
```

### Implementation outline

- Thread a join-hint flag into `apply_join(...)` or its caller.
- When `HASH_JOIN` is present and `detect_equijoin(...)` succeeds, choose the
  existing hash path regardless of row count.
- Keep CROSS/non-equijoin behavior unchanged.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_query_hints
```

## Step 5 — Advisory `PARALLEL(n)` and EXPLAIN visibility

**Goal:** accept `PARALLEL(n)` without overpromising executor guarantees.
**Files:** parser/executor explain plumbing, possibly `table_scan.rs`.
**Approach:** store the hint, propagate it, and expose enough signal in
`EXPLAIN` or execution metadata to prove it was accepted.

### Tests to add

```rust
#[test]
fn select_parallel_hint_executes_successfully() { ... }

#[test]
fn explain_surfaces_parallel_hint_or_plan_note() { ... }
```

### Implementation outline

- Validate `n >= 1`.
- Thread the hint through planning/execution as advisory metadata.
- Do not fail execution if the engine cannot realize the requested worker count.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_query_hints
```

## Step 6 — Wire smoke and closeout

**Goal:** verify the surface through the MySQL wire path and close the subphase.
**Files:** `tools/wire-test.py`, docs/memory closeout files.

### Tests to add

```python
cur.execute("CREATE TABLE hint_users (id INT PRIMARY KEY, email TEXT)")
cur.execute("CREATE INDEX idx_hint_users_email ON hint_users(email)")
cur.execute("EXPLAIN SELECT /*+ INDEX(hint_users idx_hint_users_email) */ * FROM hint_users WHERE email = 'a'")
cur.execute("EXPLAIN SELECT /*+ HASH_JOIN */ * FROM hint_users u JOIN hint_users v ON u.id = v.id")
cur.execute("SELECT /*+ PARALLEL(2) */ 1")
```

### Verification

```bash
cargo fmt --check
cargo test -p axiomdb-sql --test integration_query_hints
python3 tools/wire-test.py
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

## Risk register

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| Optimizer comments are lost before parsing because block comments are skipped too early | high | Add a narrow pre-lex/lexer preservation path only for `/*+ ... */` |
| `INDEX(...)` forces an invalid plan when the requested index is incompatible | medium | Reuse existing compatibility checks and fall back cleanly |
| `HASH_JOIN` changes semantics on unsupported join predicates | low | Override only the size threshold, not join legality |
| `PARALLEL(n)` becomes user-visible contract debt | medium | Keep it explicitly advisory in spec, code, and tests |
| Old docs still imply SELECT modifiers are the missing gap | medium | Keep tests proving existing modifier support green while landing `21.11` |

## Estimated effort

Total: high

- Step 1: 45-75 min
- Step 2: 45-60 min
- Step 3: 60-90 min
- Step 4: 45-75 min
- Step 5: 30-45 min
- Step 6: 30-45 min
