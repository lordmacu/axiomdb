# Spec: 5.2b — Session-level collation and compat mode

## Reviewed first

These AxiomDB files were reviewed before writing this spec:

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
- `docs-site/src/internals/architecture.md`
- `docs-site/src/user-guide/getting-started.md`
- `docs-site/src/user-guide/sql-reference/dml.md`
- `specs/fase-05/spec-5.2a-charset-collation-negotiation.md`

These research files were reviewed before writing this spec:

- `research/mariadb-server/mysql-test/main/ctype_collate.test`
- `research/oceanbase/src/sql/session/ob_basic_session_info.h`
- `research/oceanbase/src/sql/session/ob_basic_session_info.cpp`
- `research/sqlite/src/expr.c`
- `research/postgres/src/backend/utils/mb/mbutils.c`
- `research/datafusion/datafusion/sql/src/statement.rs`
- `research/duckdb/src/planner/expression_binder/order_binder.cpp`
- `research/duckdb/src/main/database.cpp`

## What to build (not how)

Phase `5.2a` made MySQL charset/collation negotiation real at the transport
boundary, but it explicitly did **not** change SQL comparison semantics inside
the engine. AxiomDB still compares `Value::Text` with exact Rust string order,
hashes `GROUP BY` and `DISTINCT` on raw UTF-8 bytes, and encodes text index
keys with binary ordering.

`5.2b` adds **session-visible text semantics** on top of the current executor
without pretending that AxiomDB already has the full layered collation system
from Phase `13.13`.

This subphase adds two session variables:

```sql
SET AXIOM_COMPAT = 'standard' | 'mysql' | 'postgresql' | DEFAULT;
SET collation    = 'binary' | 'es' | DEFAULT;
```

They are per-connection / per-embedded-session, affect all subsequent queries
in that session, and reset on reconnect / `COM_RESET_CONNECTION`.

In `5.2b`, `AXIOM_COMPAT` is a **text-semantics** switch only. It does not
change:

- `strict_mode`
- `sql_mode`
- `on_error`
- date coercion rules
- numeric coercion rules
- transport charset / collation variables from `5.2a`

### Session variables and visibility

The new variables are visible through:

- `SELECT @@axiom_compat`
- `SELECT @@session.axiom_compat`
- `SELECT @@collation`
- `SELECT @@session.collation`
- `SHOW VARIABLES LIKE 'axiom_compat'`
- `SHOW VARIABLES LIKE 'collation'`

Canonical visible values are:

- `@@axiom_compat` → `standard` | `mysql` | `postgresql`
- `@@collation` → `binary` | `es`

### Default values

The default session state is:

```sql
axiom_compat = 'standard'
collation    = 'binary'
```

This preserves AxiomDB's current exact text semantics unless the session opts
into a different behavior.

### Effective behavior model

`5.2b` distinguishes:

1. **compat mode** — a high-level policy (`standard` / `mysql` / `postgresql`)
2. **session collation** — the effective text-compare behavior (`binary` / `es`)

Rules:

- `AXIOM_COMPAT = 'standard'` implies default session collation `binary`
- `AXIOM_COMPAT = 'postgresql'` implies default session collation `binary`
- `AXIOM_COMPAT = 'mysql'` implies default session collation `es`
- `SET collation = ...` overrides the compat-derived default
- `SET collation = DEFAULT` clears the explicit override and returns to the
  compat-derived default
- `SET AXIOM_COMPAT = ...` does **not** erase an explicit `SET collation = ...`
  override already chosen by the session
- `SET AXIOM_COMPAT = DEFAULT` resets compat mode to `standard`

### Meaning of the supported session collations in 5.2b

`5.2b` supports exactly two executor-visible text behaviors:

#### 1. `binary`

Exact, current AxiomDB behavior:

- `a != A`
- `a != á`
- `LIKE` is case-sensitive and accent-sensitive
- ordering uses exact Rust string order
- `GROUP BY`, `DISTINCT`, and `MIN/MAX(TEXT)` treat different UTF-8 strings as
  different values

#### 2. `es`

A lightweight, session-level CI+AI behavior meant to support the roadmap's
`SET collation = 'es'` use case and the MySQL-style compat mode.

For `es`, text comparison uses this fold:

1. normalize to NFC
2. lowercase with Unicode-aware lowercase conversion
3. strip combining accent marks

Consequences:

- `Jose`, `JOSE`, and `José` compare equal
- `LIKE 'jos%'` matches `José`
- `GROUP BY`, `DISTINCT`, and `COUNT(DISTINCT ...)` collapse those spellings
  into the same group

For **ordering** surfaces (`ORDER BY`, `MIN`, `MAX`, `GROUP_CONCAT ... ORDER BY`):

- compare the folded text first
- if the folded text is equal, break ties with the original text so the result
  order is deterministic

This is intentionally **not** a full Spanish CLDR / ICU collation. It is a
session-level CI+AI fold that fits the current codebase and matches the
compatibility goal of Phase `5.2b`.

### Accepted SET values

#### `SET AXIOM_COMPAT = ...`

Accepted values:

- `standard`
- `mysql`
- `postgresql`
- `DEFAULT`

Any other value returns `DbError::InvalidValue`.

#### `SET collation = ...`

Accepted values:

- `binary`
- `es`
- `DEFAULT`

Everything else returns `DbError::InvalidValue`.

Important boundary:

- `SET collation_connection = ...` remains the **transport / MySQL wire**
  setting from `5.2a`
- `SET collation = ...` is the **executor-visible session behavior** added by
  `5.2b`

These two variables are related conceptually but are **not** the same source of
truth in AxiomDB today.

### Surfaces affected by the session collation

`5.2b` applies the effective session collation to the `execute_with_ctx(...)`
path used by:

- the MySQL wire session
- the embedded `Db` API

It affects these SQL surfaces for `TEXT` values:

- `=`, `!=`, `<`, `<=`, `>`, `>=`
- `BETWEEN`
- `IN`
- `LIKE`
- `WHERE`
- `JOIN ... ON`
- `HAVING`
- `ORDER BY`
- `GROUP BY`
- `DISTINCT`
- `COUNT(DISTINCT ...)`
- `MIN(TEXT)` / `MAX(TEXT)`
- `GROUP_CONCAT(... DISTINCT ...)`
- `GROUP_CONCAT(... ORDER BY ...)`

It does **not** retroactively change the legacy non-session `execute(...)` /
`eval(...)` helper path. That internal path stays binary; the public sessionful
surfaces are the ones upgraded in `5.2b`.

### Planner / index correctness rule

The current planner and storage layer still assume binary text ordering:

- `plan_select(...)` encodes literal text predicates with `encode_index_key(...)`
- text secondary indexes store binary-ordered UTF-8 keys
- the presorted `GROUP BY` strategy trusts index order

Therefore `5.2b` must keep correctness by disabling incompatible optimizations:

- when effective session collation is `es`, the planner must **not** use
  text index lookups / text range scans / composite text-prefix index scans
  whose correctness depends on binary text ordering
- when effective session collation is `es`, the executor must **not** use the
  presorted index-based `GROUP BY` strategy for text group keys

Falling back to a scan / hash path is required. Silent wrong answers are not
acceptable.

Numeric, boolean, temporal, UUID, and byte-column index usage stays unchanged.

### SHOW VARIABLES LIKE semantics

`SHOW VARIABLES LIKE ...` must use real SQL wildcard behavior (`%` and `_`)
instead of the current substring hack in `handler.rs`.

This matters in `5.2b` because the new variables must be testable through the
same MySQL surface as the existing charset/session variables.

### Session reset semantics

`COM_RESET_CONNECTION` must restore:

- `axiom_compat = 'standard'`
- `collation = 'binary'`
- existing defaults from previous subphases (`autocommit`, charset vars,
  `strict_mode`, `sql_mode`, `on_error`, `max_allowed_packet`, warnings, etc.)

## Research synthesis

### AxiomDB-first constraints

- `crates/axiomdb-sql/src/eval.rs` compares `Value::Text` with exact `String`
  ordering and runs `LIKE` without session state
- `crates/axiomdb-sql/src/executor.rs` hashes `GROUP BY` and `DISTINCT` keys
  from raw text bytes and sorts text with raw lexicographic order
- `crates/axiomdb-sql/src/planner.rs` and `crates/axiomdb-sql/src/key_encoding.rs`
  assume binary text ordering for indexed predicates
- `crates/axiomdb-network/src/mysql/session.rs` already distinguishes
  `collation_connection` at the wire layer, so `5.2b` must not overload that
  field as if transport collation and SQL collation were already the same
- `crates/axiomdb-embedded/src/lib.rs` keeps a persistent `SessionContext`,
  so this subphase can and should work for embedded sessions too

### What we borrow

- `research/oceanbase/src/sql/session/ob_basic_session_info.h`
  - borrow: keep compat mode and collation as typed session state, not as
    ad-hoc strings spread across the executor
- `research/oceanbase/src/sql/session/ob_basic_session_info.cpp`
  - borrow: parse compatibility mode once into a typed enum and make the
    session state the source of truth
- `research/sqlite/src/expr.c`
  - borrow: text comparison behavior is an expression/session concern and
    affects whether an index is semantically usable
- `research/mariadb-server/mysql-test/main/ctype_collate.test`
  - borrow: user-visible surface for collation behavior and the rule that the
    optimizer must not trust an index when the effective collation no longer
    matches the comparison semantics

### What we reject

- copying MariaDB's full collation catalog or per-column coercibility system
  into Phase 5
- pretending `collation_connection` from `5.2a` already gives full engine-side
  SQL collation semantics
- implementing DDL-level / per-column / per-expression `COLLATE` before the
  layered Phase `13.13` design exists
- pushing ICU / CLDR / sort-key storage into `5.2b`
- syntax-only support with no executor semantics, like the explicitly rejected
  `default_ddl_collation` path in
  `research/datafusion/datafusion/sql/src/statement.rs`

### How AxiomDB adapts it

- session state is typed and shared between wire and embedded paths
- the executor gets a lightweight text-folding mode (`binary` or `es`) rather
  than a full ICU engine
- correctness wins over premature optimization: text indexes fall back to scan
  when the session collation is incompatible with binary key order

## Inputs / Outputs

- Input:
  - `SET AXIOM_COMPAT = ...`
  - `SET collation = ...`
  - `COM_RESET_CONNECTION`
  - sessionful SQL statements executed through `execute_with_ctx(...)`
  - `SHOW VARIABLES LIKE ...` and `SELECT @@...`
- Output:
  - per-session typed compat/collation state
  - session-visible `@@axiom_compat` and `@@collation`
  - session-aware text comparison, sorting, grouping, and `LIKE` behavior on
    the ctx execution path
  - planner fallback away from incompatible text-index paths when needed
- Errors:
  - unsupported `AXIOM_COMPAT` value → `DbError::InvalidValue`
  - unsupported `collation` value → `DbError::InvalidValue`
  - no silent wrong result due to binary text index use under `es`

## Use cases

1. Default session:
   - `@@axiom_compat = 'standard'`
   - `@@collation = 'binary'`
   - `'José' = 'jose'` is `FALSE`
2. `SET AXIOM_COMPAT = 'mysql'`:
   - `@@axiom_compat = 'mysql'`
   - `@@collation = 'es'` unless the user previously chose an explicit
     `SET collation = ...`
   - `'José' = 'jose'` becomes `TRUE`
3. `SET collation = 'es'`:
   - changes session text behavior even if `@@collation_connection` stays
     `utf8mb4_bin`
   - `LIKE 'jos%'` matches `José`
4. `SET AXIOM_COMPAT = 'postgresql'` after MySQL compat:
   - returns to exact binary-style behavior unless an explicit
     `SET collation = ...` override is still active
5. A query on an indexed text column under `collation = 'es'` still returns
   correct rows even though the text index is binary-encoded, because the
   planner must fall back to a scan instead of silently missing matches.

## Acceptance criteria

- [ ] `SET AXIOM_COMPAT = 'mysql'` changes the effective session text behavior
      for subsequent ctx-based queries; `SET AXIOM_COMPAT = 'postgresql'`
      restores exact binary behavior.
- [ ] `SET collation = 'es'`; then `SET AXIOM_COMPAT = 'postgresql'` leaves
      `@@collation = 'es'` until `SET collation = DEFAULT` is executed.
- [ ] `SET collation = 'es'` and `SET collation = 'binary'` override the
      compat-derived default for the current session; `SET collation = DEFAULT`
      restores the compat-derived default.
- [ ] `SET collation_connection = ...` does **not** change `@@collation`, and
      `SET collation = ...` does **not** mutate `@@collation_connection`.
- [ ] `SET AXIOM_COMPAT = ...` does **not** change `strict_mode`, `sql_mode`,
      `on_error`, or the transport charset / collation variables.
- [ ] Under `collation = 'binary'`, `Jose`, `JOSE`, and `José` remain distinct
      for `=`, `LIKE`, `GROUP BY`, and `DISTINCT`.
- [ ] Under `collation = 'es'`, `Jose`, `JOSE`, and `José` compare equal for
      `=`, `LIKE`, `GROUP BY`, and `DISTINCT`.
- [ ] Under `collation = 'es'`, `ORDER BY`, `MIN(TEXT)`, `MAX(TEXT)`, and
      `GROUP_CONCAT(... ORDER BY ...)` use folded text comparison, with a
      deterministic raw-text tie-break for equal folded keys.
- [ ] Under `collation = 'es'`, `GROUP_CONCAT(DISTINCT ...)` deduplicates text
      using the same folded semantics as `DISTINCT`.
- [ ] The ctx planner never uses a text index lookup/range/composite lookup
      whose correctness depends on binary text ordering when
      `@@collation = 'es'`.
- [ ] The ctx executor never uses the presorted index-driven `GROUP BY`
      strategy for text group keys when `@@collation = 'es'`.
- [ ] `SHOW VARIABLES LIKE 'axiom_%'` and `SHOW VARIABLES LIKE 'collation'`
      use real `%` / `_` wildcard behavior instead of substring matching.
- [ ] `COM_RESET_CONNECTION` resets `axiom_compat` and `collation` to their
      defaults.

## Out of scope

- full ICU / CLDR locale-aware sort order
- per-database compat mode
- per-table / per-column / per-expression `COLLATE`
- text index sort keys or storage-layer collation-aware encoding
- transport charset conversion (already handled by `5.2a`)
- locale-specific `UPPER` / `LOWER` / `LENGTH` behavior
- mixed-collation conflict rules between two different explicit collations

## ⚠️ DEFERRED

- full layered collation system → pending in `13.13`
- per-database compat mode → pending in `13.13b`
- collation registry / aliases beyond `binary` and `es` → pending in `13.13c`
- CLDR / ICU locale tailoring → pending in `13.13e`
- collation-aware B+Tree sort keys → pending in `26.5`
- full "LIKE respects collation" beyond the `binary` / `es` session modes from
  this subphase → pending in `26.8`

## Dependencies

- `crates/axiomdb-sql/src/session.rs`
- `crates/axiomdb-sql/src/eval.rs`
- `crates/axiomdb-sql/src/executor.rs`
- `crates/axiomdb-sql/src/planner.rs`
- `crates/axiomdb-sql/src/key_encoding.rs`
- `crates/axiomdb-network/src/mysql/session.rs`
- `crates/axiomdb-network/src/mysql/handler.rs`
- `crates/axiomdb-embedded/src/lib.rs`
- `specs/fase-05/spec-5.2a-charset-collation-negotiation.md`
