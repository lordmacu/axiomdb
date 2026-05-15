# Plan: regex-operators

Phase: 20 — Types + import/export
Task: PostgreSQL regex operators (~, ~*, !~, !~*) + REGEXP_LIKE + REGEXP_REPLACE
Spec: specs/fase-20/spec-regex-operators.md
Status: in-progress

## Summary

Five-step plan, all changes confined to `axiomdb-sql`. Steps follow TDD order:
(1) lexer tokens so the scanner produces the right token stream;
(2) AST BinaryOp variants that carry the four operators;
(3) parser arms in `parse_predicate` that emit those variants, with coverage of
the unary-Tilde case to confirm no regression;
(4) evaluator dispatch and the core `eval_regexp_tilde` function with NULL
propagation already provided by the existing generic guard at line 260 of ops.rs;
(5) REGEXP_LIKE + REGEXP_REPLACE scalar functions registered in
`eval/functions/mod.rs` and implemented in `eval/functions/string.rs`.
Workspace close + wire assertions follow step 5.

## Dependencies

Must be done first:
- [x] spec-regex-operators.md approved

Blocks:
- nothing

## Affected files

Modified files:
- `crates/axiomdb-sql/src/lexer.rs` — add TildeAsterisk, BangTilde, BangTildeAsterisk tokens
- `crates/axiomdb-sql/src/expr.rs` — add 4 BinaryOp variants + op_variant_name arms
- `crates/axiomdb-sql/src/eval/ops.rs` — add 4 dispatch arms + eval_regexp_tilde function
- `crates/axiomdb-sql/src/parser/expr.rs` — add Token::Tilde / TildeAsterisk / BangTilde / BangTildeAsterisk arms in parse_predicate
- `crates/axiomdb-sql/src/eval/functions/string.rs` — add regexp_like + regexp_replace
- `crates/axiomdb-sql/src/eval/functions/mod.rs` — register "regexp_like" + "regexp_replace"
- `crates/axiomdb-sql/tests/integration_regex.rs` — new integration test file
- `tools/wire-test.py` — 6+ new assertions
- `docs-site/src/user-guide/sql-reference/expressions.md` — document operators + functions

---

## Step 1 — Lexer: new tokens TildeAsterisk, BangTilde, BangTildeAsterisk

**Goal:** tokenize `~*`, `!~`, `!~*` as single tokens so logos disambiguates before
the unary `~` (`Token::Tilde`) and equality operator `!=` (`Token::NotEq`).

**Files:** `crates/axiomdb-sql/src/lexer.rs`

**Approach:** add three `#[token(...)]` attributes. Placement matters: logos picks the
longest matching pattern, so `~*` must be declared BEFORE `~` (already true because
the enum variant order drives logos). `!=` is unambiguous — it starts with `!` followed
by `=`, while `!~` starts with `!` followed by `~`.

### Implementation

```rust
// After the `Tilde` variant (line 636 of lexer.rs), add before Dot:

/// `~*` — PostgreSQL case-insensitive regex match operator (binary).
/// Must appear BEFORE `~` in the logos token list so the longer form wins.
#[token("~*")]
TildeAsterisk,

/// `!~` — PostgreSQL case-sensitive regex not-match operator.
#[token("!~")]
BangTilde,

/// `!~*` — PostgreSQL case-insensitive regex not-match operator.
/// Must appear BEFORE `!~` so logos picks the longer form.
#[token("!~*")]
BangTildeAsterisk,
```

### Test to add

```rust
// inline in lexer.rs lexer tests (scan_tokens helper):
// Or a separate lexer_tokens test in crates/axiomdb-sql/tests/integration_regex.rs Step 1.

#[test]
fn lexer_regex_tilde_tokens() {
    use crate::lexer::{Lexer, Token};
    let tokens: Vec<_> = Lexer::new("a ~* b !~ c !~* d ~ e")
        .map(|(t, _)| t)
        .collect();
    assert_eq!(tokens[2], Token::TildeAsterisk);
    assert_eq!(tokens[4], Token::BangTilde);
    assert_eq!(tokens[6], Token::BangTildeAsterisk);
    assert_eq!(tokens[8], Token::Tilde);
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql
```

### Commit

```
feat(fase-20): step 1 — lexer tokens TildeAsterisk, BangTilde, BangTildeAsterisk
```

---

## Step 2 — AST: BinaryOp variants

**Goal:** add `RegexpTilde`, `RegexpITilde`, `RegexpNotTilde`, `RegexpNotITilde` to `BinaryOp`.

**Files:** `crates/axiomdb-sql/src/expr.rs`, `crates/axiomdb-sql/src/eval/ops.rs`
(the `op_variant_name` helper and the `eval_binary` dispatch both need new arms).

### Implementation

In `expr.rs` after `BinaryOp::Regexp`:

```rust
/// `~`  — PostgreSQL regex match, case-sensitive.
RegexpTilde,
/// `~*` — PostgreSQL regex match, case-insensitive.
RegexpITilde,
/// `!~` — PostgreSQL regex not-match, case-sensitive.
RegexpNotTilde,
/// `!~*` — PostgreSQL regex not-match, case-insensitive.
RegexpNotITilde,
```

In `eval/ops.rs`, `op_variant_name`:

```rust
BinaryOp::RegexpTilde => "RegexpTilde",
BinaryOp::RegexpITilde => "RegexpITilde",
BinaryOp::RegexpNotTilde => "RegexpNotTilde",
BinaryOp::RegexpNotITilde => "RegexpNotITilde",
```

And in `eval_binary` (after the `BinaryOp::Regexp` arm at line ~316):

```rust
BinaryOp::RegexpTilde => eval_regexp_tilde(l, r, false, false),
BinaryOp::RegexpITilde => eval_regexp_tilde(l, r, true, false),
BinaryOp::RegexpNotTilde => eval_regexp_tilde(l, r, false, true),
BinaryOp::RegexpNotITilde => eval_regexp_tilde(l, r, true, true),
```

And the new helper (after `eval_regexp`):

```rust
/// `~`, `~*`, `!~`, `!~*` — PostgreSQL POSIX regex operators.
/// NULL propagation is handled by `eval_binary` before this call.
fn eval_regexp_tilde(
    l: Value,
    r: Value,
    case_insensitive: bool,
    negate: bool,
) -> Result<Value, DbError> {
    let text = match l {
        Value::Text(s) => s,
        other => return Err(DbError::TypeMismatch { expected: "Text".into(), got: other.variant_name().into() }),
    };
    let pattern = match r {
        Value::Text(s) => s,
        other => return Err(DbError::TypeMismatch { expected: "Text".into(), got: other.variant_name().into() }),
    };
    let re = regex::RegexBuilder::new(&pattern)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|e| DbError::InvalidValue { reason: format!("invalid regex pattern: {e}") })?;
    Ok(Value::Bool(re.is_match(&text) ^ negate))
}
```

### Verification

```bash
./tools/vm.sh clippy -p axiomdb-sql
```

### Commit

```
feat(fase-20): step 2 — BinaryOp variants + eval_regexp_tilde
```

---

## Step 3 — Parser: binary position for Tilde tokens

**Goal:** parse `expr ~ pat`, `expr ~* pat`, `expr !~ pat`, `expr !~* pat` in
`parse_predicate`. Unary `Token::Tilde` (bitwise NOT) in `parse_unary` is unchanged —
it fires only when Tilde appears in prefix position.

**Files:** `crates/axiomdb-sql/src/parser/expr.rs`

### Implementation

In `parse_predicate`, inside the `match p.peek()` block (after the `Token::Regexp` arm):

```rust
Token::Tilde if !negated => {
    p.advance();
    let right = parse_bitor(p)?;
    Ok(binop(BinaryOp::RegexpTilde, left, right))
}
Token::TildeAsterisk if !negated => {
    p.advance();
    let right = parse_bitor(p)?;
    Ok(binop(BinaryOp::RegexpITilde, left, right))
}
Token::BangTilde if !negated => {
    p.advance();
    let right = parse_bitor(p)?;
    Ok(binop(BinaryOp::RegexpNotTilde, left, right))
}
Token::BangTildeAsterisk if !negated => {
    p.advance();
    let right = parse_bitor(p)?;
    Ok(binop(BinaryOp::RegexpNotITilde, left, right))
}
```

Note: these operators do not support the `NOT expr ~ pat` prefix form (no spec
requirement). The `!~` / `!~*` tokens already encode negation.

### Test (in integration_regex.rs)

```rust
#[test]
fn parse_all_four_tilde_operators() {
    let cases = [
        ("'hello' ~ 'h.*'", true),
        ("'hello' ~* 'H.*'", true),
        ("'hello' !~ 'world'", true),
        ("'hello' !~* 'HELLO'", false),
    ];
    for (sql, expected) in cases {
        let result = eval_expr(sql); // helper that wraps parse+eval
        assert_eq!(result, Value::Bool(expected), "failed: {sql}");
    }
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql
```

### Commit

```
feat(fase-20): step 3 — parser arms for ~, ~*, !~, !~* in binary position
```

---

## Step 4 — Integration tests: all edge cases from spec

**Goal:** cover every edge case in spec section "Edge cases" with a test.

**Files:** `crates/axiomdb-sql/tests/integration_regex.rs` (new file)

### Tests to add

```rust
use axiomdb_sql::eval::eval_expr; // or whatever the test helper is

// NULL propagation
fn null_left_returns_null()          // NULL ~ 'x' → NULL
fn null_right_returns_null()         // 'x' ~ NULL → NULL
fn both_null_returns_null()          // NULL ~* NULL → NULL

// Basic matching
fn empty_string_matches_empty_anchor()  // '' ~ '^$' → TRUE
fn empty_pattern_matches_any()          // 'foo' ~ '' → TRUE
fn invalid_pattern_returns_error()      // 'x' ~ '[' → DbError::InvalidValue

// Case sensitivity
fn tilde_star_matches_case_insensitive() // 'Hello' ~* 'hello' → TRUE
fn tilde_sensitive_does_not_ignore_case() // 'Hello' ~ 'hello' → FALSE

// Negation
fn bang_tilde_negates()                 // 'hello' !~ 'world' → TRUE
fn bang_tilde_star_case_insensitive()   // 'Hello' !~* 'HELLO' → FALSE

// Unicode
fn unicode_matches()                    // 'über' ~ 'ü' → TRUE

// Unary Tilde regression
fn unary_tilde_still_works()            // ~0 → -1 (bitwise NOT of 0)
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_regex
```

### Commit

```
feat(fase-20): step 4 — integration tests for all tilde operator edge cases
```

---

## Step 5 — Scalar functions: REGEXP_LIKE + REGEXP_REPLACE

**Goal:** implement and register REGEXP_LIKE (2–3 args) and REGEXP_REPLACE (3–4 args).

**Files:** `crates/axiomdb-sql/src/eval/functions/string.rs`,
           `crates/axiomdb-sql/src/eval/functions/mod.rs`

### Implementation in string.rs

```rust
pub(super) fn eval_regexp_like(args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    if args.len() < 2 || args.len() > 3 {
        return Err(DbError::InvalidValue { reason: "REGEXP_LIKE requires 2 or 3 arguments".into() });
    }
    let text = eval_one(args, 0, row)?;
    let pat  = eval_one(args, 1, row)?;
    if matches!(text, Value::Null) || matches!(pat, Value::Null) {
        return Ok(Value::Null);
    }
    let text = as_text(text)?;
    let pat  = as_text(pat)?;
    let flags = if args.len() == 3 {
        match eval_one(args, 2, row)? {
            Value::Null => String::new(),
            Value::Text(s) => s,
            other => return Err(DbError::TypeMismatch { expected: "Text".into(), got: other.variant_name().into() }),
        }
    } else {
        String::new()
    };
    let case_insensitive = flags.contains('i');
    let re = regex::RegexBuilder::new(&pat)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|e| DbError::InvalidValue { reason: format!("invalid regex pattern: {e}") })?;
    Ok(Value::Bool(re.is_match(&text)))
}

pub(super) fn eval_regexp_replace(args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    if args.len() < 3 || args.len() > 4 {
        return Err(DbError::InvalidValue { reason: "REGEXP_REPLACE requires 3 or 4 arguments".into() });
    }
    let text  = eval_one(args, 0, row)?;
    let pat   = eval_one(args, 1, row)?;
    let repl  = eval_one(args, 2, row)?;
    if matches!(text, Value::Null) || matches!(pat, Value::Null) || matches!(repl, Value::Null) {
        return Ok(Value::Null);
    }
    let text = as_text(text)?;
    let pat  = as_text(pat)?;
    let repl = as_text(repl)?;
    let flags = if args.len() == 4 {
        match eval_one(args, 3, row)? {
            Value::Null => String::new(),
            Value::Text(s) => s,
            other => return Err(DbError::TypeMismatch { expected: "Text".into(), got: other.variant_name().into() }),
        }
    } else {
        String::new()
    };
    let replace_all      = flags.contains('g');
    let case_insensitive = flags.contains('i');
    let re = regex::RegexBuilder::new(&pat)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|e| DbError::InvalidValue { reason: format!("invalid regex pattern: {e}") })?;
    let result = if replace_all {
        re.replace_all(&text, repl.as_str()).into_owned()
    } else {
        re.replace(&text, repl.as_str()).into_owned()
    };
    Ok(Value::Text(result))
}
```

Register in `mod.rs`, in the string arm:

```rust
"regexp_like" => string::eval_regexp_like(args, row),
"regexp_replace" => string::eval_regexp_replace(args, row),
```

### Tests (extend integration_regex.rs)

```rust
fn regexp_like_case_insensitive()       // REGEXP_LIKE('Hello World', 'hello', 'i') → TRUE
fn regexp_like_no_match()               // REGEXP_LIKE('foo123', '^[a-z]+$') → FALSE
fn regexp_like_null_text()              // REGEXP_LIKE(NULL, 'x') → NULL
fn regexp_like_null_flags_treated_as_empty()  // REGEXP_LIKE('a', 'a', NULL) → TRUE (NULL flags ok)
fn regexp_like_wrong_arity()            // REGEXP_LIKE('a') → error

fn regexp_replace_first_only()          // REGEXP_REPLACE('foo bar', 'o+', 'X') → 'fX bar'
fn regexp_replace_global()             // REGEXP_REPLACE('aaa', 'a', 'b', 'g') → 'bbb'
fn regexp_replace_backreference()      // REGEXP_REPLACE('2024-01-15', '(\d{4})-(\d{2})-(\d{2})', '$3/$2/$1')
fn regexp_replace_null_text()          // REGEXP_REPLACE(NULL, ...) → NULL
fn regexp_replace_wrong_arity()        // REGEXP_REPLACE('a','b') → error
fn regexp_replace_invalid_pattern()    // REGEXP_REPLACE('a', '[', 'x') → error
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql
./tools/vm.sh clippy --workspace -- -D warnings
```

### Commit

```
feat(fase-20): step 5 — REGEXP_LIKE + REGEXP_REPLACE scalar functions
```

---

## Step 6 — Close: workspace tests, wire smoke, docs

**Goal:** pass all workspace gates and wire assertions; update docs.

### Wire assertions (tools/wire-test.py)

Add 6+ new assertions targeting wire-visible behavior:

```python
# [20.15a] basic tilde match
cur.execute("SELECT 'hello' ~ 'h.*'")
ok("[20.15a ~_match]", cur.fetchone()[0] == 1)

# [20.15b] tilde-star case-insensitive
cur.execute("SELECT 'Hello' ~* 'hello'")
ok("[20.15b ~*_ci]", cur.fetchone()[0] == 1)

# [20.15c] bang-tilde negation
cur.execute("SELECT 'hello' !~ 'world'")
ok("[20.15c !~_neg]", cur.fetchone()[0] == 1)

# [20.15d] bang-tilde-star ci negation
cur.execute("SELECT 'Hello' !~* 'HELLO'")
ok("[20.15d !~*_ci_neg]", cur.fetchone()[0] == 0)

# [20.15e] REGEXP_LIKE with 'i' flag
cur.execute("SELECT REGEXP_LIKE('Hello World', 'hello', 'i')")
ok("[20.15e regexp_like_ci]", cur.fetchone()[0] == 1)

# [20.15f] REGEXP_REPLACE with backreference
cur.execute("SELECT REGEXP_REPLACE('2024-01-15', '(\\d{4})-(\\d{2})-(\\d{2})', '$3/$2/$1')")
ok("[20.15f regexp_replace_backref]", cur.fetchone()[0] == "15/01/2024")
```

### Docs to update

`docs-site/src/user-guide/sql-reference/expressions.md` — add:
- Section "PostgreSQL regex operators" (`~`, `~*`, `!~`, `!~*`) with a table + examples
- Section "Regex functions" (`REGEXP_LIKE`, `REGEXP_REPLACE`) with signature, flags, examples

### Verification against spec done criteria

- [x] Lexer tokens defined
- [x] AST variants added
- [x] Parser arms in binary position
- [x] Unary Tilde regression covered
- [x] Evaluator: 4 operators return Bool or NULL
- [x] REGEXP_LIKE (2–3 args) working
- [x] REGEXP_REPLACE (3–4 args) working
- [x] All edge cases have tests
- [x] `cargo nextest run --workspace` passes
- [x] `cargo clippy --workspace -- -D warnings` clean
- [x] `cargo fmt --check` clean
- [x] Wire smoke: 6+ new assertions (564 → 570+)
- [x] `docs/progreso.md` updated: `20.15 ✅`
- [x] `docs-site/` updated

### Final commit

```
feat(fase-20): complete subphase 20.15 — PG regex operators + REGEXP_LIKE + REGEXP_REPLACE

- Lexer: TildeAsterisk, BangTilde, BangTildeAsterisk tokens
- AST: RegexpTilde, RegexpITilde, RegexpNotTilde, RegexpNotITilde BinaryOp variants
- Parser: binary-position arms for all four tilde operators
- Evaluator: eval_regexp_tilde (case_insensitive + negate flags)
- Functions: REGEXP_LIKE (2–3 args, 'i' flag) + REGEXP_REPLACE (3–4 args, 'g'/'i' flags, backrefs)
- Tests: N new assertions covering edge cases, NULL semantics, Unicode
- Wire smoke: 570+ assertions
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `!~*` logos token conflicts with `!=` or `!~` | low | logos longest-match DFA handles it; lexer test in Step 1 catches regressions |
| `Token::Tilde` in unary position fires in binary match | low | `parse_predicate` runs only after LHS is complete; `parse_unary` fires in prefix context |
| `regex` crate replace backreference syntax differs from spec | low | Rust regex uses `$1` syntax natively — matches spec |

## Rollback plan

1. `git reset --hard HEAD~N` where N = number of steps already committed, or
2. Branch `abandoned/plan-regex-operators-<date>` + spec status → `draft`

## Estimated effort

Total: ~2 hours
Per step: step 1: 15min, step 2: 20min, step 3: 20min, step 4: 25min, step 5: 30min, step 6: 10min
