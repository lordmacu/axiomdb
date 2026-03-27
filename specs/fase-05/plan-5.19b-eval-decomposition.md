# Plan: 5.19b — Eval decomposition

## Files to create/modify
- `crates/axiomdb-sql/src/eval.rs`
  - delete/replace with directory module
- `crates/axiomdb-sql/src/eval/mod.rs`
  - public facade; stable re-exports for `eval`, `eval_with`, `eval_in_session`,
    `eval_with_in_session`, `is_truthy`, `like_match`, `ClosureRunner`,
    `CollationGuard`, `NoSubquery`, `SubqueryRunner`, and `current_eval_collation`
- `crates/axiomdb-sql/src/eval/context.rs`
  - thread-local collation state, `current_eval_collation`, `CollationGuard`,
    `SubqueryRunner`, `NoSubquery`, `ClosureRunner`
- `crates/axiomdb-sql/src/eval/core.rs`
  - recursive `Expr` traversal: `eval`, `eval_with`, session wrappers,
    short-circuit helpers for subquery-aware paths, and the central expression match
- `crates/axiomdb-sql/src/eval/ops.rs`
  - evaluator-local value operations: `is_truthy`, NULL helpers, short-circuit non-subquery
    helpers, unary/binary evaluation, arithmetic, comparison, `IN`, concat, and `like_match`
- `crates/axiomdb-sql/src/eval/functions/mod.rs`
  - static function dispatcher plus narrow shared utilities used by function families
- `crates/axiomdb-sql/src/eval/functions/system.rs`
  - system/session built-ins such as `version`, `current_user`, `current_database`,
    `last_insert_id`, and `row_count`
- `crates/axiomdb-sql/src/eval/functions/nulls.rs`
  - `coalesce`, `ifnull`, `nvl`, `nullif`, `isnull`, `if`/`iff`, and small conversion/type
    helpers that belong with null-handling semantics
- `crates/axiomdb-sql/src/eval/functions/numeric.rs`
  - numeric built-ins such as `abs`, `ceil`, `floor`, `round`, `pow`, `sqrt`, `mod`, `sign`
- `crates/axiomdb-sql/src/eval/functions/string.rs`
  - text built-ins such as `length`, `upper`, `lower`, `trim`, `substr`, `concat`,
    `concat_ws`, `repeat`, `replace`, `reverse`, `left`, `right`, `lpad`, `rpad`,
    `locate`, `instr`, `ascii`, `char`, `space`, `strcmp`
- `crates/axiomdb-sql/src/eval/functions/datetime.rs`
  - datetime built-ins and helpers: `now`, `current_date`, `unix_timestamp`, extractors,
    `datediff`, `date_format`, `str_to_date`, `find_in_set`, and helper functions
    such as `micros_to_ndt`, `days_to_ndate`, `date_format_str`, `str_to_date_inner`,
    `take_digits`, `find_in_set_inner`
- `crates/axiomdb-sql/src/eval/functions/binary.rs`
  - BLOB/base64/hex encode-decode built-ins and helper functions
- `crates/axiomdb-sql/src/eval/functions/uuid.rs`
  - UUID built-ins and UUID string parsing helper
- `crates/axiomdb-sql/src/eval/tests.rs`
  - moved evaluator-local unit tests currently at the end of `eval.rs`
- `crates/axiomdb-sql/src/lib.rs`
  - keep `pub mod eval;` and the existing `pub use eval::{...};` surface unchanged;
    adjust only if module-path comments need refreshing
- `crates/axiomdb-sql/src/executor/select.rs`
  - no semantic change; only touch if imports need refreshing after the module split
- `crates/axiomdb-sql/src/executor/aggregate.rs`
  - no semantic change; only touch if `current_eval_collation` import path requires refresh
- `docs-site/src/internals/sql-parser.md`
  - update evaluator source-layout description if implementation moves from a file to a directory
- `docs-site/src/internals/architecture.md`
  - update `axiomdb-sql` architecture text to describe the new `eval/` module layout

## Algorithm / Data structure
### 1. Replace the file module with a facade module
Rust module layout after the refactor:

```text
eval/
  mod.rs
  context.rs
  core.rs
  ops.rs
  tests.rs
  functions/
    mod.rs
    system.rs
    nulls.rs
    numeric.rs
    string.rs
    datetime.rs
    binary.rs
    uuid.rs
```

Rules:
- `mod.rs` stays small and owns only:
  - `mod` declarations
  - narrow re-exports that preserve the current `crate::eval::*` surface
  - no large evaluator logic
- all recursive expression traversal stays out of the facade

### 2. Split by semantic responsibility, not by line count
Move code according to ownership:

```text
context.rs
  EVAL_COLLATION thread-local
  current_eval_collation
  CollationGuard
  SubqueryRunner / NoSubquery / ClosureRunner

core.rs
  eval
  eval_with
  eval_in_session
  eval_with_in_session
  eval_and_with / eval_or_with / eval_in_with
  central Expr match for pure + subquery-aware traversal

ops.rs
  is_truthy
  apply_and_values / apply_not
  eval_and / eval_or
  eval_unary / eval_binary
  eval_arithmetic / int_arith / bigint_arith / real_arith / decimal_arith
  eval_comparison / compare_values
  eval_concat / eval_in / like_match

functions/mod.rs
  eval_function dispatcher
  family-module wiring

functions/system.rs
  version/current_user/current_database/current_schema/connection_id/row_count
  last_insert_id / lastval

functions/nulls.rs
  coalesce/ifnull/nvl/nullif/isnull/if/iff/typeof/to_char/tostring

functions/numeric.rs
  abs/ceil/floor/round/pow/power/sqrt/mod/sign

functions/string.rs
  length/char_length/character_length/len
  octet_length/byte_length
  upper/lower/trim/ltrim/rtrim
  substr/substring/mid
  concat/concat_ws/repeat/replace/reverse/left/right/lpad/rpad
  locate/position/instr/ascii/char/chr/space/strcmp

functions/datetime.rs
  now/current_timestamp/getdate/sysdate
  current_date/curdate/today
  unix_timestamp
  year/month/day/hour/minute/second
  datediff
  date_format / str_to_date / find_in_set
  micros_to_ndt / days_to_ndate / date_format_str / str_to_date_inner / take_digits / find_in_set_inner

functions/binary.rs
  from_base64 / to_base64 / encode / decode
  b64_encode / b64_decode / hex_encode / hex_decode

functions/uuid.rs
  gen_random_uuid / uuid_generate_v4 / random_uuid / newid
  uuid_generate_v7 / uuid7
  is_valid_uuid / is_uuid
  parse_uuid_str
```

### 3. Dependency direction inside `eval/`
Keep dependency edges one-way:

```text
mod.rs -> {context, core, ops, functions, tests}
core.rs -> {context, ops, functions}
ops.rs -> {context}
functions/mod.rs -> {system, nulls, numeric, string, datetime, binary, uuid}
family modules -> {core facade calls only, ops/context when already required}
tests.rs -> {facade exports only}
```

Rules:
- `ops.rs` must not depend on function-family modules.
- function-family modules must not reimplement arithmetic, comparison, or NULL truth tables.
- built-in families may call the stable evaluator entrypoint (`crate::eval::eval`) when needed
  to preserve current argument-evaluation semantics exactly.
- `current_eval_collation` must live in `context.rs` and be re-exported from `mod.rs`
  so executor modules keep their current import path.

### 4. Preserve current semantics before any cleanup
Implementation order matters because several behaviors are subtle and must not drift:
- three-valued NULL logic for `AND`, `OR`, `BETWEEN`, `IN`, and comparisons
- collation-sensitive text comparison and `LIKE`
- `eval_with` subquery dispatch and cardinality errors
- current static built-in dispatch and current argument-evaluation behavior
- `last_insert_id_value()` coupling to the executor facade

Therefore:
1. create the new module files;
2. move code mechanically with minimal edits until it compiles;
3. only then tighten imports and visibility;
4. do not mix in semantic fixes or “small cleanups”.

### 5. Keep the built-in dispatcher static
`docs-site/src/internals/sql-parser.md` documents the built-in evaluator as a
single lowered-name match, not a runtime registry. Preserve that model:

```text
eval_function(name, args, row):
  lower = name.to_ascii_lowercase()
  match lower.as_str():
    system family   -> functions::system::eval(...)
    null family     -> functions::nulls::eval(...)
    numeric family  -> functions::numeric::eval(...)
    string family   -> functions::string::eval(...)
    datetime family -> functions::datetime::eval(...)
    binary family   -> functions::binary::eval(...)
    uuid family     -> functions::uuid::eval(...)
    _               -> NotImplemented("function 'name' ...")
```

Do not replace this with:
- `HashMap<&'static str, fn(...) -> ...>`
- trait objects
- registration macros that widen the runtime surface

## Implementation phases
1. Create `crates/axiomdb-sql/src/eval/` and `mod.rs`, keeping the current public facade intact.
2. Extract `context.rs` and re-export its stable items from `mod.rs`.
3. Extract `ops.rs` with evaluator-local value operations and non-subquery helpers.
4. Extract `core.rs` and keep the recursive expression traversal centralized there.
5. Extract `functions/mod.rs` plus function-family modules, moving code mechanically and
   preserving the current `eval_function` dispatch shape.
6. Move evaluator-local unit tests into `eval/tests.rs`.
7. Delete the old monolithic `eval.rs` file and keep only the new directory module.
8. Tighten imports and visibility so no accidental new public API leaks out.
9. Refresh `docs-site` pages that describe the evaluator source layout.
10. Run regression validation focused on evaluator, subquery, date-function, and executor paths.

## Tests to write
- unit:
  - move the current evaluator-local unit tests into `eval/tests.rs`
  - add at least one module-level smoke test if the split introduces a new internal helper
    contract that was not previously exercised directly
- integration:
  - run `crates/axiomdb-sql/tests/integration_eval.rs`
  - run `crates/axiomdb-sql/tests/integration_date_functions.rs`
  - run `crates/axiomdb-sql/tests/integration_subqueries.rs`
  - run `crates/axiomdb-sql/tests/integration_executor.rs`
    for SQL-visible built-in regressions such as `LAST_INSERT_ID`, `ABS`, `SUBSTR`,
    `COALESCE`, `NOW`, and collation-sensitive behavior
- bench:
  - no new benchmark is required for the refactor itself
  - re-run existing SQL/evaluator-sensitive benches only to confirm there is no >5% regression
    from structure-only changes

## Anti-patterns to avoid
- Do not mix this refactor with new scalar functions or semantics fixes.
- Do not “fix” the current function-argument subquery behavior inside this subphase.
- Do not split files by arbitrary 500/1000-line chunks.
- Do not move helper ownership into executor modules just because they consume evaluator exports.
- Do not replace the static function `match` with a registry, macro system, or dynamic dispatch.
- Do not widen helper visibility just to make compilation easier; prefer narrow re-exports in
  `eval/mod.rs`.
- Do not duplicate expression traversal logic across function families or helper modules.

## Risks
- Risk: moving `eval_with` and `eval_function` together accidentally changes subquery behavior,
  especially for nested expressions or function arguments.
  Mitigation: preserve call patterns mechanically first; treat any semantic change as a blocker.

- Risk: splitting collation state from value operations breaks `current_eval_collation()` users in
  `executor/aggregate.rs` and `executor/select.rs`.
  Mitigation: keep `current_eval_collation` in `context.rs` and re-export it from `crate::eval`.

- Risk: `last_insert_id_value()` path breaks once function families move out of the monolith.
  Mitigation: preserve the call through `crate::executor::last_insert_id_value()` unchanged.

- Risk: the refactor appears “green” but only because evaluator-local unit coverage is shallow for
  built-in families.
  Mitigation: use integration suites (`integration_eval`, `integration_date_functions`,
  `integration_subqueries`, `integration_executor`) as the regression oracle.

- Risk: doc pages become stale because they still describe `eval.rs` as a single file.
  Mitigation: update `docs-site/src/internals/sql-parser.md` and
  `docs-site/src/internals/architecture.md` in the same implementation subphase.

## Assumptions
- This is a pure structural refactor done before any future evaluator-semantics work.
- The existing integration suites are strong enough to act as the primary regression oracle for
  behavioral stability.
- Future evaluator work will benefit from a responsibility-based module tree in the same way the
  executor already benefited from `5.19a`.
