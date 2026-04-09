# Spec: 4.2f — ANSI_QUOTES OFF double-quoted strings

## What to build (not how)

Make AxiomDB follow MySQL session semantics for double quotes:

- When `sql_mode` does **not** include `ANSI_QUOTES` (MySQL default), `"..."` must be treated as a string literal.
- When `sql_mode` **does** include `ANSI_QUOTES`, `"..."` must be treated as a quoted identifier.

This behavior must apply consistently across:

- normal SQL parsing/execution
- prepared statements
- plan-cache normalization / reuse
- multi-statement `COM_QUERY` splitting
- embedded mode (`axiomdb-embedded`)
- wire mode (`axiomdb-network`)

Session changes via `SET sql_mode = ...` must take effect on the **next** statement in the same session, including prepared-statement prepare/re-prepare paths. New sessions and reset sessions must default to `ANSI_QUOTES` OFF.

For this subphase, double-quoted string literals in `ANSI_QUOTES` OFF follow the same escape/quote semantics that AxiomDB already applies to single-quoted string literals today.

## Inputs / Outputs

- Input:
  - raw SQL text
  - session parse mode derived from normalized `sql_mode`
  - specifically: `ansi_quotes: bool`
- Output:
  - correct AST / execution semantics for double-quoted fragments
  - mode-correct prepared-statement placeholder counting/substitution
  - mode-correct plan-cache normalization and cache lookup
  - mode-correct multi-statement splitting for `;`
- Errors:
  - `DbError::ParseError` for malformed double-quoted tokens
  - normal identifier-resolution errors (`ColumnNotFound`, `TableNotFound`, `AmbiguousColumn`) when `ANSI_QUOTES` is ON and the quoted identifier is invalid
  - no cross-session or cross-mode plan reuse that applies the wrong interpretation of `"`

## Use cases

1. MySQL-default behavior: double-quoted string literal
```sql
SELECT "hello";
```

2. Session toggle to identifier mode
```sql
SET sql_mode = 'ANSI_QUOTES';
SELECT "users"."name" FROM "users";
```

3. Toggle back to MySQL default in the same session
```sql
SET sql_mode = '';
SELECT "hello";
```

4. Prepared statement with `?` inside a double-quoted string
```sql
SELECT "a ? b", ?;
```

5. Multi-statement query with `;` inside a double-quoted token
```sql
SELECT "a;b"; SELECT 1;
```

6. Same SQL text compiled under different `sql_mode` values in one connection
```sql
SET sql_mode = '';
SELECT "name";
SET sql_mode = 'ANSI_QUOTES';
SELECT "name" FROM t;
```

## Acceptance criteria

- [ ] New `SessionContext` and new wire connections default to `ANSI_QUOTES` OFF
- [ ] `SET sql_mode = 'ANSI_QUOTES'` enables double-quoted identifiers for subsequent statements in the same session
- [ ] `SET sql_mode = ''` and `SET sql_mode = DEFAULT` restore `ANSI_QUOTES` OFF
- [ ] In default mode, `SELECT "hello"` parses/executed as a text literal, not an identifier
- [ ] In `ANSI_QUOTES` mode, double-quoted names parse as identifiers anywhere identifiers are currently allowed
- [ ] The embedded API and MySQL wire path observe the same `ANSI_QUOTES` behavior
- [ ] Prepared-statement parameter counting does not count `?` inside double-quoted strings when `ANSI_QUOTES` is OFF
- [ ] Prepared-statement literal substitution does not replace `?` inside double-quoted strings when `ANSI_QUOTES` is OFF
- [ ] Multi-statement splitting does not split on `;` inside double-quoted tokens
- [ ] Plan cache entries compiled with `ANSI_QUOTES` OFF are never reused as if they were compiled with `ANSI_QUOTES` ON, and vice versa
- [ ] Plan-cache normalization treats double-quoted literals as literals only when `ANSI_QUOTES` is OFF
- [ ] `cargo test -p axiomdb-sql` and the directly affected wire tests pass with new coverage

## Out of scope

- `NO_BACKSLASH_ESCAPES`
- `PIPES_AS_CONCAT`, `IGNORE_SPACE`, or any other parser-affecting `sql_mode` bit besides `ANSI_QUOTES`
- A general SQL-dialect framework beyond the `ANSI_QUOTES` toggle
- MySQL protocol status-flag parity for `SERVER_STATUS_ANSI_QUOTES`

## Dependencies

- Existing `sql_mode` normalization helpers in `axiomdb-sql::session`
- Existing wire `SET` interception in `axiomdb-network`
- Existing parser / lexer / prepared-statement infrastructure
- Existing per-connection plan cache and prepared-statement caches

## ⚠️ DEFERRED

- `NO_BACKSLASH_ESCAPES`-specific string semantics for both single- and double-quoted literals
  → pending in a future SQL-mode semantics subphase
- Other parser-affecting SQL modes (`PIPES_AS_CONCAT`, `IGNORE_SPACE`, `ANSI`, etc.)
  → pending in future MySQL-compat subphases
