# Plan: 4.2f — ANSI_QUOTES OFF double-quoted strings

## Files to create/modify

- `crates/axiomdb-sql/src/lexer.rs` — add a raw double-quoted lexeme path and expose mode-aware tokenization that resolves `"` to `StringLit` vs `DqIdent`
- `crates/axiomdb-sql/src/parser/mod.rs` — add mode-aware parse entry points and keep `parse()` as the MySQL-default wrapper
- `crates/axiomdb-sql/src/lib.rs` — export the new mode-aware lexer/parser API
- `crates/axiomdb-sql/src/session.rs` — add `ANSI_QUOTES` helpers and session state needed by embedded execution
- `crates/axiomdb-sql/src/executor/exec_dispatch.rs` — make `SET sql_mode = ...` update both `strict_mode` and `ansi_quotes` in `SessionContext`
- `crates/axiomdb-network/src/mysql/session.rs` — derive `ansi_quotes` from connection `sql_mode` and make placeholder counting mode-aware
- `crates/axiomdb-network/src/mysql/database.rs` — use session-aware parse entry points in direct execution paths
- `crates/axiomdb-network/src/mysql/handler.rs` — sync `session.ansi_quotes`, use mode-aware parse/prepare/re-prepare, and key plan-cache activity on parse mode
- `crates/axiomdb-network/src/mysql/plan_cache.rs` — include parse mode in cache keys and make normalization treat double-quoted literals as literals only when `ANSI_QUOTES` is OFF
- `crates/axiomdb-network/src/mysql/prepared.rs` — make `?` substitution/counting respect double-quoted strings in `ANSI_QUOTES` OFF
- `crates/axiomdb-network/src/mysql/handler_sql_intercept.rs` — make multi-statement splitting quote-mode-aware for `;`
- `crates/axiomdb-embedded/src/lib.rs` — use session-aware parse entry points so `SET sql_mode` affects later embedded statements
- `crates/axiomdb-sql/tests/integration_lexer.rs` — add lexer coverage for `ANSI_QUOTES` ON/OFF
- `crates/axiomdb-sql/tests/integration_mysql_compat.rs` — add parser/executor compatibility tests for double-quoted strings and identifiers
- `crates/axiomdb-network/src/mysql/session.rs` tests — add wire-session tests for `ANSI_QUOTES` derivation/defaults
- `tools/wire-test.py` — later, during review/closure, add end-to-end wire assertions for toggling `sql_mode`

## Algorithm / Data structure

Introduce one shared parse-mode bit:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SqlModeFlags {
    pub ansi_quotes: bool,
}
```

Session derivation:

```text
normalized = normalize_sql_mode(raw_sql_mode)
flags.ansi_quotes = normalized contains "ANSI_QUOTES"
flags.strict_mode = normalized contains STRICT_*   // existing behavior
```

Lexer strategy:

```text
logos scans a raw double-quoted fragment using a regex that preserves
the full lexeme and supports doubled quotes / backslash escapes
tokenize_with_mode():
  if raw fragment and ansi_quotes = false -> decode to Token::StringLit
  if raw fragment and ansi_quotes = true  -> decode to Token::DqIdent
```

This keeps the DFA simple while avoiding a session-global lexer mode.

Raw SQL helper strategy for wire paths:

```text
walk sql bytes with a tiny shared state machine:
  states = outside | single_quote | double_quote | backtick | line_comment | block_comment

double_quote means:
  string literal when ansi_quotes = false
  quoted identifier when ansi_quotes = true

rules:
  '?' counts/replaces only in outside state
  ';' splits only in outside state
  normalize quoted text to '?' only when it is a literal
```

Plan-cache key:

```text
key = hash(ansi_quotes_bit || normalized_sql)
```

That prevents the same SQL text from reusing an AST compiled under the wrong quote mode.

## Implementation phases

1. Add `sql_mode_has_ansi_quotes()`-style helpers and `SessionContext.ansi_quotes`, defaulting to `false`.
2. Extend `SET sql_mode` execution paths so embedded/session execution updates `ansi_quotes` immediately, and wire-session sync propagates the derived flag into `SessionContext`.
3. Add mode-aware lexer/parser entry points (`tokenize_with_mode` / `parse_with_mode`) and keep the legacy `tokenize()` / `parse()` wrappers as MySQL-default (`ANSI_QUOTES` OFF).
4. Rework double-quoted lexing so `"` can become `StringLit` in default mode while preserving quoted-identifier behavior in `ANSI_QUOTES` mode.
5. Thread parse mode through all network and embedded parse call sites, including prepare/re-prepare and DDL pre-parse paths.
6. Make plan-cache normalization mode-aware and include `ansi_quotes` in the cache key.
7. Replace ad hoc raw-SQL scans in prepared statements and multi-statement splitting with a shared mode-aware scanner so `?` and `;` inside double-quoted tokens are ignored correctly.
8. Add targeted tests for lexer, parser, embedded/session execution, prepared statements, plan cache, and wire-visible behavior.

## Tests to write

- unit: `sql_mode_has_ansi_quotes()` helper and `SessionContext` default flag
- unit: lexer/tokenizer returns `StringLit` for `"hello"` in default mode
- unit: lexer/tokenizer returns `DqIdent` for `"hello"` in `ANSI_QUOTES` mode
- unit: double-quoted escaping matches current string-literal behavior in default mode
- integration: `SELECT "hello"` returns a text literal in default mode
- integration: `SET sql_mode = 'ANSI_QUOTES'` makes `"table"."col"` parse as identifiers
- integration: `SET sql_mode = DEFAULT` or `''` turns identifier mode back off
- integration: prepared-statement parameter counting ignores `?` inside `"..."` when `ANSI_QUOTES` is OFF
- integration: multi-statement splitter ignores `;` inside `"..."` for both modes
- integration: same connection can toggle `sql_mode` and get different semantics on the next statement without stale plan reuse
- wire: `tools/wire-test.py` should validate `SET sql_mode`, `SELECT "hello"`, and prepared execution under both modes
- bench: no new standalone benchmark required, but validate no material regression in parser/normalizer hot paths during review

## Anti-patterns to avoid

- Do not key the plan cache only on normalized SQL text; that will reuse the wrong AST across `ANSI_QUOTES` mode flips
- Do not patch only `parse()` and ignore prepared-statement / multi-statement raw SQL scanners
- Do not store `ANSI_QUOTES` only in `ConnectionState`; embedded mode must observe the same behavior through `SessionContext`
- Do not implement double-quoted strings by blindly reinterpreting the old `DqIdent` slice; that loses proper string-decoding semantics
- Do not regress backtick-quoted identifiers or existing `STRICT_TRANS_TABLES` synchronization

## Risks

- Cache-key collision across modes → mitigate by including the `ansi_quotes` bit in key derivation and adding a same-connection toggle regression test
- Placeholder counting/substitution drift vs parser semantics → mitigate by using one shared raw-SQL scanner for `count_params`, substitution, and statement splitting
- Embedded/wire divergence after `SET sql_mode` → mitigate with dedicated tests in both paths and by deriving flags from the same helper
- Lexer regression on existing quoted identifiers → mitigate with explicit `ANSI_QUOTES`-ON lexer/parser tests and by keeping default wrappers MySQL-compatible
- Hot-path performance regression in plan-cache lookup → mitigate by using a lightweight scanner for normalization/splitting rather than full parse on every lookup
