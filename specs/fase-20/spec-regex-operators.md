# Spec: regex-operators

Phase: 20 — Types + import/export
Task: PostgreSQL regex operators (~, ~*, !~, !~*) + REGEXP_LIKE + REGEXP_REPLACE
Status: approved

## Context

The expression evaluator already supports MySQL-style `REGEXP`/`RLIKE` via
`BinaryOp::Regexp`. PostgreSQL uses four tilde-based operators (`~`, `~*`, `!~`,
`!~*`) that are widely used for data validation, log parsing, and pattern extraction.
Two scalar functions (`REGEXP_LIKE`, `REGEXP_REPLACE`) round out the regex surface
needed by most applications. The `regex` crate is already in `axiomdb-sql/Cargo.toml`.

## Goal

Add four PostgreSQL-compatible regex binary operators and two scalar functions so
that regex patterns can be applied in WHERE, SELECT, and expression contexts with
full NULL semantics.

## Non-goals

- `REGEXP_MATCH(text, pat)` — returns `TEXT[]` (first match groups); deferred because
  it requires an array return value and scalar functions today return `Value`, not `Vec<Value>`.
- `REGEXP_SPLIT_TO_TABLE` / `REGEXP_SPLIT_TO_ARRAY` — set-returning; separate phase.
- Regex indexes (GIN/GiST) — separate phase.
- Changing MySQL `REGEXP`/`RLIKE` behavior — they remain case-sensitive, unchanged.

## Behavior

### Binary operators

| Operator | Meaning | Case |
|----------|---------|------|
| `~`      | text matches regex pattern | sensitive |
| `~*`     | text matches regex pattern | insensitive |
| `!~`     | text does NOT match regex  | sensitive |
| `!~*`    | text does NOT match regex  | insensitive |

Syntax (same precedence as `REGEXP` / comparison level):
```sql
text_expr ~ pattern_expr
text_expr ~* pattern_expr
text_expr !~ pattern_expr
text_expr !~* pattern_expr
```

#### Semantics

- Left operand: `TEXT`. Right operand: `TEXT` (a POSIX extended regular expression).
- Returns `BOOL` (true/false) or `NULL` if either operand is `NULL`.
- Pattern is compiled with the `regex` crate on every evaluation (no caching required
  for V1; correctness over performance).
- `~*` / `!~*`: case-insensitive flag applied via `regex::RegexBuilder::case_insensitive(true)`.
- `!~` / `!~*`: result is the boolean negation of the match (`!re.is_match(&text)`).

#### AST representation

Four new `BinaryOp` variants:
```rust
/// `~`  — regex match, case-sensitive
RegexpTilde,
/// `~*` — regex match, case-insensitive
RegexpITilde,
/// `!~` — regex not-match, case-sensitive
RegexpNotTilde,
/// `!~*` — regex not-match, case-insensitive
RegexpNotITilde,
```

#### Lexer tokens

Three new tokens (logos):
```
TildeAsterisk   — "~*"
BangTilde       — "!~"
BangTildeAsterisk — "!~*"
```

`Token::Tilde` already exists. When it appears in binary operator position
(inside `parse_predicate`, after the left-hand expression), it is treated as
the `~` regex operator rather than the unary bitwise-NOT.

Token priority: logos picks the longest match, so `!~*` > `!~` > `!` and
`~*` > `~`. The existing `NotEq` token (`!=`) is unaffected — logos matches
`!=` before `!~` because both are registered and `!=` is the longer prefix match
for that specific sequence. Actually, `!~` and `!=` share only `!` as a prefix,
so there is no ambiguity.

### Scalar functions

#### REGEXP_LIKE

```sql
REGEXP_LIKE(text, pattern)
REGEXP_LIKE(text, pattern, flags)
```

Returns `BOOL`. `flags` is an optional `TEXT` parameter; supported flag characters:
- `'i'` — case-insensitive match

Example:
```sql
SELECT REGEXP_LIKE('Hello World', 'hello', 'i');  -- TRUE
SELECT REGEXP_LIKE('foo123', '^[a-z]+$');          -- FALSE
```

- Arity: 2 or 3 arguments. Any other arity → `DbError::InvalidValue`.
- If `text` or `pattern` is `NULL` → `NULL`.
- If `flags` is `NULL` → treated as empty string (no flags).
- Unknown flag characters are silently ignored (per MySQL/PG convention).

#### REGEXP_REPLACE

```sql
REGEXP_REPLACE(text, pattern, replacement)
REGEXP_REPLACE(text, pattern, replacement, flags)
```

Returns `TEXT`. Replaces occurrences of `pattern` in `text` with `replacement`.

`flags` is an optional `TEXT` parameter; supported flag characters:
- `'g'` — replace all occurrences (default: replace first only)
- `'i'` — case-insensitive match
- Both can be combined: `'gi'`

Replacement string supports backreferences:
- `$0` or `${0}` — entire match
- `$1` … `$9` or `${1}` … `${9}` — capture group N (Rust `regex` crate syntax)

Example:
```sql
SELECT REGEXP_REPLACE('foo bar', 'o+', 'X');         -- 'fX bar'
SELECT REGEXP_REPLACE('foo bar', 'o+', 'X', 'g');    -- 'fX bar' (only 1 match anyway)
SELECT REGEXP_REPLACE('Foo Bar', '[a-z]', '_', 'gi'); -- '_oo _ar'
SELECT REGEXP_REPLACE('2024-01-15', '(\d{4})-(\d{2})-(\d{2})', '$3/$2/$1');
  -- '15/01/2024'
```

- Arity: 3 or 4 arguments. Any other arity → `DbError::InvalidValue`.
- If `text`, `pattern`, or `replacement` is `NULL` → `NULL`.
- If `flags` is `NULL` → treated as empty string (no flags).
- Invalid regex → `DbError::InvalidValue { reason: "invalid regex pattern: ..." }`.

### Error cases

| Condition | Error | Message |
|-----------|-------|---------|
| Invalid regex pattern (operators) | `DbError::InvalidValue` | `"invalid regex pattern: {e}"` |
| Invalid regex pattern (functions) | `DbError::InvalidValue` | `"invalid regex pattern: {e}"` |
| REGEXP_LIKE wrong arity | `DbError::InvalidValue` | `"REGEXP_LIKE requires 2 or 3 arguments"` |
| REGEXP_REPLACE wrong arity | `DbError::InvalidValue` | `"REGEXP_REPLACE requires 3 or 4 arguments"` |
| Non-text left operand (operators) | `DbError::TypeMismatch` | `"expected Text, got {type}"` |
| Non-text right operand (operators) | `DbError::TypeMismatch` | `"expected Text, got {type}"` |

## Edge cases

- [ ] `NULL ~ pattern` → `NULL`
- [ ] `text ~ NULL` → `NULL`
- [ ] `NULL ~* NULL` → `NULL`
- [ ] Empty string `'' ~ '^$'` → `TRUE`
- [ ] Empty pattern `text ~ ''` → `TRUE` (every string matches the empty regex)
- [ ] Invalid pattern `text ~ '['` → `DbError::InvalidValue`
- [ ] `~*` matches case-insensitively (`'Hello' ~* 'hello'` → `TRUE`)
- [ ] `!~` negates correctly (`'hello' !~ 'world'` → `TRUE`)
- [ ] `!~*` negates case-insensitively (`'Hello' !~* 'HELLO'` → `FALSE`)
- [ ] `REGEXP_REPLACE` without `'g'` replaces only first occurrence
- [ ] `REGEXP_REPLACE` with `'g'` replaces all occurrences
- [ ] `REGEXP_REPLACE` backreference `$1` works
- [ ] `REGEXP_LIKE` with `'i'` flag ignores case
- [ ] Unicode: `'über' ~ 'ü'` → `TRUE` (regex crate is Unicode-aware)
- [ ] `Token::Tilde` in unary position (bitwise NOT) still works after change

## Performance budget

| Operation | Target |
|-----------|--------|
| Single regex match (short text, simple pattern) | < 1 µs |

No caching required for V1. Regex compilation on every evaluation is acceptable
for typical query workloads (OLTP). A `regex::Regex` compile for simple patterns
takes ~5–50 µs — acceptable since regex operators appear in WHERE clauses, not
hot inner loops of scans without an index.

## Dependencies

- Depends on: existing `BinaryOp::Regexp` pattern, `regex` crate already present.
- Blocks: nothing.

## Open questions

None — all resolved during brainstorm.

## Done criteria

- [ ] Lexer: `TildeAsterisk`, `BangTilde`, `BangTildeAsterisk` tokens defined and tested.
- [ ] AST: `BinaryOp::RegexpTilde`, `RegexpITilde`, `RegexpNotTilde`, `RegexpNotITilde` added.
- [ ] Parser: `Token::Tilde` in binary position → `RegexpTilde`; `TildeAsterisk` → `RegexpITilde`; etc.
- [ ] Parser: unary `Token::Tilde` (bitwise NOT) still parsed correctly.
- [ ] Evaluator: all 4 operators return correct `Bool` or `NULL`.
- [ ] Functions: `REGEXP_LIKE` (2–3 args) and `REGEXP_REPLACE` (3–4 args) registered and working.
- [ ] All edge cases above have tests.
- [ ] `cargo nextest run -p axiomdb-sql` passes.
- [ ] `cargo nextest run --workspace` passes.
- [ ] `cargo clippy --workspace -- -D warnings` clean.
- [ ] `cargo fmt --check` clean.
- [ ] Wire smoke: 6+ new assertions (564 → 570+).
- [ ] `docs/progreso.md` updated: `20.15 ✅`.
- [ ] `docs-site/src/user-guide/sql-reference/` updated with operators + functions.

## References

- PostgreSQL regex operators: https://www.postgresql.org/docs/current/functions-matching.html#FUNCTIONS-POSIX-REGEXP
- Rust `regex` crate: https://docs.rs/regex/latest/regex/
- Existing MySQL REGEXP: `crates/axiomdb-sql/src/eval/ops.rs:1129`
- Lexer: `crates/axiomdb-sql/src/lexer.rs:636` (Token::Tilde)
- Parser: `crates/axiomdb-sql/src/parser/expr.rs:282` (Token::Regexp handling)
