# Plan: 5.2b — Session-level collation and compat mode

## Files to create/modify

- `crates/axiomdb-sql/Cargo.toml`
  - add `unicode-normalization` for NFC folding and accent stripping
- `crates/axiomdb-sql/src/session.rs`
  - add typed `CompatMode` and `SessionCollation` enums
  - add parse/format helpers for `AXIOM_COMPAT` and `collation`
  - extend `SessionContext` with `compat_mode` and `explicit_collation`
  - add `effective_collation()` and `collation_name()` helpers
- `crates/axiomdb-sql/src/text_semantics.rs` (new)
  - implement folded text-key generation for `binary` and `es`
  - expose comparison and LIKE helpers used by evaluator/executor
- `crates/axiomdb-sql/src/lib.rs`
  - re-export the new text-semantics helpers if needed by the network layer
- `crates/axiomdb-sql/src/eval.rs`
  - factor the evaluator through a session-aware core
  - keep legacy `eval()` / `eval_with()` as binary wrappers
  - add ctx-aware variants for executor use
- `crates/axiomdb-sql/src/planner.rs`
  - add a ctx-aware planner entry point that can reject text index paths when
    the effective session collation is non-binary
- `crates/axiomdb-sql/src/executor.rs`
  - route ctx execution through the session-aware evaluator
  - make ORDER BY, GROUP BY, DISTINCT, MIN/MAX(TEXT), and GROUP_CONCAT respect
    the effective session collation
  - disable presorted text GROUP BY when session collation is non-binary
  - extend `execute_set_ctx(...)` for `AXIOM_COMPAT` and `collation`
- `crates/axiomdb-network/src/mysql/session.rs`
  - store typed compat/collation session state in `ConnectionState`
  - parse `SET AXIOM_COMPAT = ...` and `SET collation = ...`
  - expose `@@axiom_compat` and `@@collation`
- `crates/axiomdb-network/src/mysql/handler.rs`
  - sync `ConnectionState` compat/collation into `SessionContext`
  - include the new variables in `SHOW VARIABLES`
  - replace the substring hack in `show_variables_result(...)` with proper
    wildcard matching
  - reset the new session state on `COM_RESET_CONNECTION`
- `crates/axiomdb-sql/tests/integration_executor.rs`
  - add session-aware executor regressions for compare/order/group/distinct
- `crates/axiomdb-network/tests/integration_protocol.rs`
  - add wire tests for `SET AXIOM_COMPAT`, `SET collation`, `SELECT @@...`,
    `SHOW VARIABLES LIKE`, and reset behavior
- `tools/wire-test.py`
  - add live MySQL-wire assertions for the new session behavior

## Reviewed first

These files were reviewed before writing this plan:

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-embedded/src/lib.rs`
- `crates/axiomdb-network/src/mysql/charset.rs`
- `crates/axiomdb-network/src/mysql/handler.rs`
- `crates/axiomdb-network/src/mysql/session.rs`
- `crates/axiomdb-sql/Cargo.toml`
- `crates/axiomdb-sql/src/eval.rs`
- `crates/axiomdb-sql/src/executor.rs`
- `crates/axiomdb-sql/src/key_encoding.rs`
- `crates/axiomdb-sql/src/planner.rs`
- `crates/axiomdb-sql/src/session.rs`
- `specs/fase-05/spec-5.2a-charset-collation-negotiation.md`
- `specs/fase-05/spec-5.2b-session-collation-compat-mode.md`

Research reviewed before writing this plan:

- `research/mariadb-server/mysql-test/main/ctype_collate.test`
- `research/oceanbase/src/sql/session/ob_basic_session_info.h`
- `research/oceanbase/src/sql/session/ob_basic_session_info.cpp`
- `research/sqlite/src/expr.c`
- `research/postgres/src/backend/utils/mb/mbutils.c`
- `research/datafusion/datafusion/sql/src/statement.rs`
- `research/duckdb/src/planner/expression_binder/order_binder.cpp`
- `research/duckdb/src/main/database.cpp`

## Research synthesis

### AxiomDB-first constraints

- `planner.rs` and `key_encoding.rs` still assume binary text ordering in index
  keys, so session collation cannot be allowed to silently change indexed text
  predicate semantics
- `executor.rs` has two execution worlds:
  - ctx path (`execute_with_ctx`) used by MySQL wire and embedded API
  - non-ctx path (`execute`) used as an internal binary fallback
- `handler.rs` already distinguishes transport state (`collation_connection`)
  from executor session state (`SessionContext`)
- `SHOW VARIABLES` already centralizes visible session-variable rows, so the new
  variables should be added there instead of inventing a second metadata path

### What we borrow

- `research/oceanbase/src/sql/session/ob_basic_session_info.h`
  - borrow: typed session state for compatibility mode and collation-like vars
- `research/oceanbase/src/sql/session/ob_basic_session_info.cpp`
  - borrow: parse compat mode once, store enum, derive visible strings later
- `research/sqlite/src/expr.c`
  - borrow: expression/session collation and index-compatibility are related
    decisions, not separate afterthoughts
- `research/mariadb-server/mysql-test/main/ctype_collate.test`
  - borrow: if collation semantics and index order diverge, correctness wins and
    the optimizer must not trust the index

### What we reject

- turning `collation_connection` into the SQL collation source of truth
- adding `SET collation` as a string-only surface with no executor change
- using text indexes under `es` and hoping WHERE re-evaluation will fix missed rows
- introducing full ICU / sort-key storage in Phase 5
- copying DuckDB/DataFusion's broader collation surface before AxiomDB has the
  Phase 13 layered design

### How AxiomDB adapts it

- AxiomDB uses a lightweight session fold (`binary` or `es`) that fits the
  current executor and can be shared by wire and embedded sessions
- the ctx planner becomes aware of "binary-safe vs not binary-safe" instead of
  trying to become a full collation planner
- the legacy non-ctx execution path remains binary to avoid widening the blast
  radius of an internal helper API

## Algorithm / Data structure

### 1. Add typed session state in `axiomdb-sql`

In `crates/axiomdb-sql/src/session.rs` add:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompatMode {
    #[default]
    Standard,
    MySql,
    PostgreSql,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCollation {
    Binary,
    Es,
}

pub fn parse_compat_mode_setting(raw: &str) -> Result<CompatMode, DbError>;
pub fn compat_mode_name(mode: CompatMode) -> &'static str;

pub fn parse_session_collation_setting(raw: &str)
    -> Result<Option<SessionCollation>, DbError>;
pub fn session_collation_name(c: SessionCollation) -> &'static str;
```

Accepted `collation` forms:

- `DEFAULT` -> `Ok(None)`
- `binary` -> `Some(Binary)`
- `es` -> `Some(Es)`
- everything else -> `InvalidValue`

Extend `SessionContext` with:

```rust
pub compat_mode: CompatMode,
pub explicit_collation: Option<SessionCollation>,
```

And add exact helpers:

```rust
impl SessionContext {
    pub fn effective_collation(&self) -> SessionCollation;
    pub fn effective_collation_name(&self) -> &'static str;
}
```

Rules are closed here, not later:

- `explicit_collation = Some(x)` wins
- otherwise `CompatMode::MySql => Es`
- otherwise `Binary`

### 2. Introduce a dedicated text-semantics module

Create `crates/axiomdb-sql/src/text_semantics.rs` with a focused API:

```rust
pub fn canonical_text(c: SessionCollation, s: &str) -> std::borrow::Cow<'_, str>;
pub fn compare_text(c: SessionCollation, a: &str, b: &str) -> std::cmp::Ordering;
pub fn text_eq(c: SessionCollation, a: &str, b: &str) -> bool;
pub fn like_match_collated(c: SessionCollation, text: &str, pattern: &str) -> bool;
pub fn value_to_session_key_bytes(c: SessionCollation, v: &Value) -> Vec<u8>;
```

Exact behavior:

- `Binary`:
  - `canonical_text` returns borrowed input
  - compare = exact `a.cmp(b)`
  - LIKE = current `like_match(text, pattern)`
- `Es`:
  - canonicalize with `unicode-normalization`:
    - `s.nfc()`
    - lowercase
    - `nfd()` + remove combining marks
  - `text_eq` compares canonicalized strings
  - `like_match_collated` canonicalizes both `text` and `pattern`, then calls
    the existing wildcard matcher
  - `compare_text` compares canonicalized strings first, then raw strings as a
    deterministic tie-break

`value_to_session_key_bytes(...)` must reuse the existing binary serialization
for non-text values and only switch `Value::Text` onto the session canonical
text key.

This closes the DISTINCT / GROUP BY / COUNT(DISTINCT) ambiguity up front.

### 3. Factor `eval.rs` through a session-aware core

Do **not** change the semantics of the existing public:

- `eval(...)`
- `eval_with(...)`

Instead:

1. move the current implementation into one internal core that accepts a
   `SessionCollation`
2. keep `eval(...)` / `eval_with(...)` as binary wrappers
3. add new ctx-aware entry points used by the executor:

```rust
pub fn eval_in_session(
    expr: &Expr,
    row: &[Value],
    collation: SessionCollation,
) -> Result<Value, DbError>;

pub fn eval_with_in_session<R: SubqueryRunner>(
    expr: &Expr,
    row: &[Value],
    sq: &mut R,
    collation: SessionCollation,
) -> Result<Value, DbError>;
```

Inside the evaluator:

- `Value::Text` comparison in `compare_values(...)` must use `compare_text(...)`
- `LIKE` must use `like_match_collated(...)`
- all non-text behavior remains unchanged

This avoids stringly conditionals scattered through the executor and keeps one
authoritative comparison implementation.

### 4. Extend executor-side SET handling

In `execute_set_ctx(...)` add exact new branches:

- `axiom_compat`
- `collation`

Rules:

- `SET AXIOM_COMPAT = DEFAULT` -> `CompatMode::Standard`
- `SET collation = DEFAULT` -> `explicit_collation = None`
- invalid values return `DbError::InvalidValue`
- `SET AXIOM_COMPAT` must not erase an explicit collation override
- `SET AXIOM_COMPAT` must not mutate `strict_mode`, `sql_mode`, `on_error`,
  transport charset variables, or `collation_connection`
- existing `autocommit` / `strict_mode` / `sql_mode` / `on_error` logic stays unchanged

### 5. Make ctx execution paths use the session-aware evaluator

Every ctx path in `executor.rs` that currently calls binary `eval(...)` /
`eval_with(...)` for user-visible query semantics must switch to the session-aware
variant with `ctx.effective_collation()`.

This includes:

- single-table WHERE filtering
- join ON filtering
- projections in ctx path
- HAVING via `eval_with_aggs(...)`
- aggregate argument evaluation for text `MIN/MAX`
- GROUP BY key evaluation
- ORDER BY key evaluation
- `GROUP_CONCAT` ORDER BY key evaluation

Do **not** widen the non-ctx path in this subphase.

### 6. Make DISTINCT / GROUP BY / GROUP_CONCAT session-aware

Replace binary-only helpers in ctx execution with session-aware variants:

- `value_to_key_bytes` -> keep for legacy / binary paths
- add `group_key_bytes_with_session(...)`
- add `apply_distinct_with_session(...)`
- add `compare_values_null_last_with_session(...)`
- add `agg_compare_with_session(...)`

Concrete call-site decisions:

- hash GROUP BY in `execute_select_grouped_hash(...)` uses
  `group_key_bytes_with_session(...)`
- sorted GROUP BY equality uses `compare_group_key_lists_with_session(...)`
- `apply_distinct(...)` stays for legacy paths; ctx path calls
  `apply_distinct_with_session(...)`
- `GROUP_CONCAT DISTINCT` deduplicates text values on canonical folded bytes,
  not raw strings
- `MIN/MAX(TEXT)` uses the session-aware comparison helper

### 7. Add planner safety gate for text indexes

Do **not** overload the existing `plan_select(...)` signature used by non-ctx
helpers. Add a new ctx-aware entry point:

```rust
pub fn plan_select_ctx(
    where_clause: Option<&Expr>,
    indexes: &[IndexDef],
    columns: &[ColumnDef],
    table_id: u32,
    table_stats: &[StatsDef],
    stale_tracker: &mut StaleStatsTracker,
    select_col_idxs: &[u16],
    collation: SessionCollation,
) -> AccessMethod
```

Implementation rule:

- if `collation == Binary`, behavior stays identical to current `plan_select(...)`
- if `collation != Binary`, reject any candidate access method whose search or
  range semantics depend on a `TEXT` indexed column

Closed cases:

- single-column `col = literal` where indexed column type is `TEXT` -> `Scan`
- single-column text range predicate -> `Scan`
- composite equality when any matched leading indexed column is `TEXT` -> `Scan`
- non-text indexes remain eligible
- index-only scan is only rejected when the chosen candidate itself was rejected
  for text-semantic reasons; no second hidden rule is needed

This closes the "planner might silently miss rows" gap explicitly.

### 8. Disable presorted text GROUP BY under non-binary collation

Change `choose_group_by_strategy_ctx(...)` to accept the effective session
collation and the resolved table schema columns.

Exact rule:

- if `effective_collation == Binary`, keep current behavior
- if `effective_collation != Binary` and any `GROUP BY` key column is `TEXT`,
  return `GroupByStrategy::Hash`
- non-text group keys can still use the current presorted optimization

Do **not** introduce `Sorted { presorted: false }` in this subphase; keep the
change minimal and correctness-first.

### 9. Mirror the state in `ConnectionState`

Extend `crates/axiomdb-network/src/mysql/session.rs` with the same typed fields:

```rust
compat_mode: CompatMode,
explicit_collation: Option<SessionCollation>,
```

And exact getters:

```rust
pub fn compat_mode(&self) -> CompatMode;
pub fn explicit_collation(&self) -> Option<SessionCollation>;
pub fn effective_collation(&self) -> SessionCollation;
pub fn effective_collation_name(&self) -> &'static str;
```

`apply_set(...)` gains two validated branches:

- `axiom_compat`
- `collation`

`variables` map must always expose:

- `"axiom_compat"` -> canonical lowercase compat name
- `"collation"` -> current effective collation name (`binary` or `es`)

The source of truth stays typed fields, not the string map.

### 10. Sync wire session state into executor session state

In `handler.rs`, after intercepted `SET` statements and in
`COM_RESET_CONNECTION`, sync:

```rust
session.compat_mode = conn_state.compat_mode();
session.explicit_collation = conn_state.explicit_collation();
```

Do not infer these through `get_variable(...)`; sync the typed fields directly.

### 11. Fix `SHOW VARIABLES LIKE` in the same pass

`show_variables_result(...)` must stop stripping `%` and doing `contains(...)`.

Use the already-tested wildcard helper:

```rust
axiomdb_sql::like_match(...)
```

with lowercase normalization on both variable name and pattern, matching the
existing `status.rs` approach.

Add rows for:

- `axiom_compat`
- `collation`

and keep the existing charset / strict / on_error rows intact.

## Implementation phases

1. Add typed compat/collation state and parse helpers in `axiomdb-sql/src/session.rs`.
2. Add `text_semantics.rs` and session-aware compare / key / LIKE helpers.
3. Refactor `eval.rs` to expose binary wrappers plus session-aware entry points.
4. Extend `execute_set_ctx(...)` for `AXIOM_COMPAT` and `collation`.
5. Switch ctx executor paths to session-aware evaluation and key generation.
6. Add `plan_select_ctx(...)` and route ctx single-table execution through it.
7. Gate presorted GROUP BY on binary-safe text semantics only.
8. Mirror typed state in `ConnectionState`, parse wire `SET`, and expose `@@` vars.
9. Update `handler.rs` sync/reset logic and fix `SHOW VARIABLES LIKE`.
10. Add unit, integration, and wire tests before closing docs/review.

## Tests to write

- unit:
  - `session.rs`: parse/format/default/override precedence for `AXIOM_COMPAT`
    and `collation`
  - `text_semantics.rs`: `binary` vs `es` compare, equality, LIKE, and key-byte behavior
  - `planner.rs`: text index candidate rejected under `es`, numeric index still allowed
- integration:
  - `integration_executor.rs`:
    - `SET AXIOM_COMPAT = 'mysql'` changes `WHERE name = 'jose'`
    - `SET collation = 'es'` changes `LIKE`, `ORDER BY`, `GROUP BY`, `DISTINCT`
    - explicit `SET collation = 'es'` survives `SET AXIOM_COMPAT = 'postgresql'`
      until `SET collation = DEFAULT`
    - `SET AXIOM_COMPAT = 'postgresql'` restores binary behavior
    - indexed text query under `es` still returns the correct rows
    - `MIN/MAX(TEXT)` and `GROUP_CONCAT DISTINCT/ORDER BY` follow session semantics
  - `integration_protocol.rs`:
    - `SELECT @@axiom_compat`
    - `SELECT @@collation`
    - `SHOW VARIABLES LIKE 'axiom_%'`
    - `SHOW VARIABLES LIKE 'collation'`
    - `COM_RESET_CONNECTION` resets both vars
- bench:
  - extend `executor_e2e` or `sql_components` with a text-comparison microbench
    that measures `binary` vs `es`
  - verify binary default path does not regress noticeably for exact-compare workloads

## Anti-patterns to avoid

- Do not treat `collation_connection` as if it were the SQL executor collation.
- Do not let `plan_select_ctx(...)` keep using text indexes under `es`.
- Do not widen the legacy non-ctx `eval()` API semantics in this phase.
- Do not hash DISTINCT / GROUP BY on raw text bytes once session collation is `es`.
- Do not implement syntax-only `SET collation` with no change to compare/order/group behavior.
- Do not pull in ICU or storage-layer sort keys in Phase 5.

## Risks

- Hidden binary-eval call sites remain in ctx execution.
  - Mitigation: grep-based audit plus executor integration tests covering WHERE,
    JOIN, HAVING, GROUP BY, DISTINCT, ORDER BY, MIN/MAX, and GROUP_CONCAT.
- Planner still uses an incompatible text index under `es`.
  - Mitigation: dedicated `planner.rs` unit tests and an executor integration
    test on an indexed `TEXT` column.
- Folded text allocations make `binary` default slower.
  - Mitigation: fast-path `SessionCollation::Binary` to borrowed input and keep
    legacy binary wrappers unchanged.
- User confusion between `collation` and `collation_connection`.
  - Mitigation: different variables, explicit tests, and docs that state the
    separation clearly.
