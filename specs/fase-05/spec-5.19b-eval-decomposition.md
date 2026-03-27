# Spec: 5.19b — Eval decomposition

## What to build (not how)
Split the monolithic expression evaluator implementation in `crates/axiomdb-sql/src/eval.rs`
into a directory module under `crates/axiomdb-sql/src/eval/`, organized by responsibility,
while preserving the current public API and all SQL-visible behavior.

This subphase is a structural refactor only. It must not change expression semantics, NULL
propagation, short-circuit behavior, text-collation behavior, function error semantics,
subquery execution behavior, aggregate interaction, or benchmark targets by itself.

The new evaluator layout must:
- keep `axiomdb_sql::{eval, eval_with, eval_in_session, eval_with_in_session}` as the stable
  public evaluator entrypoints;
- keep `axiomdb_sql::{is_truthy, like_match, ClosureRunner, CollationGuard, NoSubquery, SubqueryRunner}`
  available under the same import paths;
- keep `crate::eval::current_eval_collation()` available to sibling modules such as
  `executor/aggregate.rs`;
- replace the single 2.7K-line implementation file with smaller modules grouped by evaluator
  responsibility;
- isolate collation/subquery context, recursive expression traversal, value operations,
  scalar built-in function families, and evaluator-local tests so future subphases can work on
  one area without re-reading the whole evaluator;
- preserve the current function-dispatch strategy: static lowered-name dispatch, not a runtime
  registry or hash map;
- preserve the current built-in argument-evaluation semantics exactly, including the fact that
  `eval_function` continues to evaluate arguments through the pure `eval(...)` path unless the
  current implementation already does otherwise.

## Inputs / Outputs
- Input:
  - current evaluator feature set in `crates/axiomdb-sql/src/eval.rs`
  - stable public API exported from `crates/axiomdb-sql/src/lib.rs`
  - internal evaluator dependencies from:
    - `crates/axiomdb-sql/src/executor/mod.rs`
    - `crates/axiomdb-sql/src/executor/select.rs`
    - `crates/axiomdb-sql/src/executor/aggregate.rs`
    - `crates/axiomdb-sql/tests/integration_eval.rs`
    - `crates/axiomdb-sql/tests/integration_date_functions.rs`
    - `crates/axiomdb-sql/tests/integration_subqueries.rs`
    - `docs-site/src/internals/sql-parser.md`
    - `docs-site/src/internals/architecture.md`
- Output:
  - `crates/axiomdb-sql/src/eval/` directory module with the same externally visible behavior
  - a small facade module that preserves existing import paths
- Errors:
  - no new SQL-visible errors
  - no changed error messages or SQLSTATE mappings
  - build/test failures are blockers because this subphase is behavioral no-op refactoring

## Use cases
1. A contributor implementing a new string or date scalar built-in can work in a dedicated
   evaluator submodule instead of editing a 2.7K-line file that also owns NULL logic,
   subquery dispatch, and arithmetic.

2. A contributor debugging `LIKE`, `IN`, or comparison semantics can read the value-operation
   module without navigating built-in functions, date parsing helpers, and thread-local
   collation setup in the same file.

3. `executor/select.rs` continues to use `CollationGuard`, and `executor/aggregate.rs`
   continues to use `current_eval_collation()`, without any import-path breakage.

4. Existing evaluator, date-function, subquery, and executor integration tests continue to
   pass unchanged, proving the split is structural only.

## Acceptance criteria
- [ ] `crates/axiomdb-sql/src/eval.rs` is replaced by a directory module rooted at
      `crates/axiomdb-sql/src/eval/mod.rs`.
- [ ] The public API remains stable: all items currently re-exported from `crate::eval`
      continue to compile for callers without import changes.
- [ ] `crate::eval::current_eval_collation()` remains available to sibling modules after the split.
- [ ] Recursive expression traversal remains centralized in evaluator-core code rather than being
      duplicated across function-family modules.
- [ ] Scalar built-ins are split by responsibility, not arbitrary line-count chunks.
- [ ] The static function-dispatch strategy is preserved; this refactor does not introduce a
      runtime function registry, hash map lookup, or trait-object dispatch for built-ins.
- [ ] `Expr::Function` behavior remains unchanged: built-in functions still evaluate their
      arguments exactly as they do today, including current subquery/non-subquery semantics.
- [ ] No SQL-visible behavior changes: expression results, NULL semantics, collation-sensitive
      comparisons, cardinality errors, and error text remain unchanged.
- [ ] Existing evaluator-related integration tests continue to pass without being rewritten for
      different behavior.
- [ ] Visibility is kept as narrow as possible (`pub(crate)` or private inside `eval/`);
      the refactor does not expose new public crate API unless already required.
- [ ] No new scalar functions, semantics fixes, or planner/executor changes are folded into this
      refactor.

## Out of scope
- Adding new built-in functions
- Changing NULL, collation, type-coercion, or subquery semantics
- Making built-in function arguments subquery-aware if they are not already
- Rewriting `eval_function` into a registry/table-driven dispatch system
- Planner, executor, catalog, or wire-protocol changes
- Benchmark tuning beyond what falls out mechanically from code movement

## Dependencies
- Current evaluator feature set through:
  - `4.17`
  - `4.17b`
  - `4.19`
  - `4.19b`
  - `4.19c`
  - `4.19d`
  - `4.24`
  - `5.2b`
- Existing stable exports in `crates/axiomdb-sql/src/lib.rs`
- Existing internal evaluator consumers in:
  - `crates/axiomdb-sql/src/executor/select.rs`
  - `crates/axiomdb-sql/src/executor/aggregate.rs`
  - `crates/axiomdb-sql/src/executor/mod.rs`

## ⚠️ DEFERRED
- Any semantic fix to current built-in argument evaluation or subquery behavior inside scalar
  functions → pending in a future evaluator-semantics subphase
- Any built-in function registry/table abstraction → pending in a future evaluator maintainability
  subphase if still needed after this split
- Any test-coverage expansion beyond what is necessary to prove structural parity → pending in a
  future evaluator hardening subphase
