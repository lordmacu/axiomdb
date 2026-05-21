# Plan: faster INSERT parser (VALUES literal fast-path)

Phase: perf-sqlite-gap — write parity with SQLite (inserts)
Task: implement the VALUES literal fast-path
Spec: specs/fase-perf-sqlite-gap/spec-faster-insert-parser.md
Status: in-progress

## Summary

Four steps. (1) Extract the canonical literal-token→`Expr` converter and make
`parse_atom` delegate to it, so there is one source of truth — behavior-preserving,
measured for no SELECT regression. (2) Add `parse_value_expr` (the fast-path) and
wire it into the `INSERT … VALUES` element loop, plus pre-size the per-row `Vec`.
(3) Exhaustive edge-case + fast-vs-slow equivalence tests. (4) Workspace validation
on Lima + before/after `--diagnose-parse` and `--compare`. TDD throughout: the
equivalence test (`VALUES` element == `parse_expr_only` of that element) is the
correctness oracle and is written before the fast-path code.

## Dependencies

Must be done first:
- [x] spec-faster-insert-parser approved

Blocks (until done):
- [ ] clean before/after baseline for the executor-trim subphase (task #6)

## Affected files

New files:
- none (tests go in the existing `#[cfg(test)]` module of `parser/dml.rs`, or
  `crates/axiomdb-sql/tests/parser_insert_fastpath.rs` if cleaner)

Modified files:
- `crates/axiomdb-sql/src/parser/expr.rs` — add `literal_token_to_expr`; `parse_atom`
  delegates its 7 literal arms to it
- `crates/axiomdb-sql/src/parser/dml.rs` — add `parse_value_expr`; use at the two
  VALUES element sites in `parse_insert_body`; pre-size row `Vec`
- (tests) parser test module

## Step 1 — Canonical literal converter; `parse_atom` delegates

**Goal:** one source of truth for literal-token→`Expr`, no behavior change.
**Files:** `parser/expr.rs`.
**Approach:** TDD — add a unit test pinning each literal token's mapping (incl. the
`i32`/`BigInt` boundary), then extract the helper and route `parse_atom` through it.

### Test to add

```rust
// parser/expr.rs #[cfg(test)]
#[test]
fn literal_token_to_expr_mappings() {
    use axiomdb_types::Value;
    assert_eq!(literal_token_to_expr(Token::Integer(7)), Some(Expr::Literal(Value::Int(7))));
    assert_eq!(literal_token_to_expr(Token::Integer(2147483648)),
               Some(Expr::Literal(Value::BigInt(2147483648))));
    assert_eq!(literal_token_to_expr(Token::Integer(2147483647)),
               Some(Expr::Literal(Value::Int(2147483647))));
    assert_eq!(literal_token_to_expr(Token::Float(1.5)), Some(Expr::Literal(Value::Real(1.5))));
    assert_eq!(literal_token_to_expr(Token::HexLit(255)), Some(Expr::Literal(Value::BigInt(255))));
    assert_eq!(literal_token_to_expr(Token::True), Some(Expr::Literal(Value::Bool(true))));
    assert_eq!(literal_token_to_expr(Token::Null), Some(Expr::Literal(Value::Null)));
    assert_eq!(literal_token_to_expr(Token::Ident("x")), None);
}
```

### Implementation outline

```rust
// parser/expr.rs — canonical converter (keep in sync with the fast-path;
// the equivalence test in Step 2 enforces agreement with parse_expr).
pub(super) fn literal_token_to_expr(tok: Token<'_>) -> Option<Expr> {
    use axiomdb_types::Value;
    Some(match tok {
        Token::Integer(n) => Expr::Literal(
            if (i32::MIN as i64..=i32::MAX as i64).contains(&n) {
                Value::Int(n as i32)
            } else {
                Value::BigInt(n)
            }),
        Token::Float(f)     => Expr::Literal(Value::Real(f)),
        Token::HexLit(n)    => Expr::Literal(Value::BigInt(n)),
        Token::StringLit(s) => Expr::Literal(Value::Text(s)),
        Token::True         => Expr::Literal(Value::Bool(true)),
        Token::False        => Expr::Literal(Value::Bool(false)),
        Token::Null         => Expr::Literal(Value::Null),
        _ => return None,
    })
}

// parse_atom: bind the clone once, try the literal converter, else the big match.
fn parse_atom(p: &mut Parser) -> Result<Expr, DbError> {
    let pos = p.current_pos();
    let tok = p.peek().clone();
    if matches!(tok,
        Token::Integer(_) | Token::Float(_) | Token::HexLit(_)
        | Token::StringLit(_) | Token::True | Token::False | Token::Null)
    {
        p.advance();
        return Ok(literal_token_to_expr(tok).expect("matched literal arm"));
    }
    match tok { /* remaining non-literal arms unchanged */ }
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql            # full parser suite green (Lima)
./tools/vm.sh clippy -p axiomdb-sql -- -D warnings
# SELECT-parse no-regression check (macOS): build + diagnose a SELECT-heavy
# parse path is unchanged; parse_atom delegation must not slow the slow path.
```

### Commit

```
refactor(perf-sqlite-gap): extract literal_token_to_expr; parse_atom delegates

Step 1 of specs/fase-perf-sqlite-gap/plan-faster-insert-parser.md
```

**Contingency:** if the `parse_atom` delegation shows any measurable SELECT-parse
regression, revert `parse_atom` to its inline arms and keep `literal_token_to_expr`
used only by the fast-path (the Step 2 equivalence test still guards consistency).

---

## Step 2 — `parse_value_expr` fast-path + VALUES wiring + row Vec pre-size

**Goal:** skip the precedence ladder for bare-literal VALUES elements.
**Files:** `parser/dml.rs`.
**Approach:** TDD — write the fast-vs-slow equivalence test (oracle) first, then the
fast-path.

### Test to add (the correctness oracle)

```rust
// parse a VALUES row, compare each element to parse_expr_only of that element.
fn first_values_row(sql: &str) -> Vec<Expr> {
    match parse(sql, None).unwrap() {
        Stmt::Insert(s) => match s.source {
            InsertSource::Values(mut rows) => rows.remove(0),
            _ => panic!("not VALUES"),
        },
        _ => panic!("not INSERT"),
    }
}

#[test]
fn values_fastpath_equals_parse_expr() {
    let row = first_values_row(
        "INSERT INTO t VALUES (1, 'a', 18, TRUE, 1.5, NULL, 0xFF, 2147483648)");
    let want = ["1","'a'","18","TRUE","1.5","NULL","0xFF","2147483648"]
        .iter().map(|s| parse_expr_only(s).unwrap()).collect::<Vec<_>>();
    assert_eq!(row, want);   // Expr derives PartialEq
}
```

### Implementation outline

```rust
// parser/dml.rs
fn parse_value_expr(p: &mut Parser) -> Result<Expr, DbError> {
    // Fast-path only when a bare literal is immediately followed by ',' or ')':
    // then no operator/postfix/cast can follow, so this == parse_expr exactly.
    if matches!(p.peek_at(1), Token::Comma | Token::RParen)
        && matches!(p.peek(),
            Token::Integer(_) | Token::Float(_) | Token::HexLit(_)
            | Token::StringLit(_) | Token::True | Token::False | Token::Null)
    {
        let tok = p.peek().clone();
        p.advance();
        return Ok(super::expr::literal_token_to_expr(tok).expect("literal"));
    }
    parse_expr(p)
}
```

Wire into `parse_insert_body` (the `Token::Values` arm, ~dml.rs:1893):

```rust
let mut row = Vec::with_capacity(arity_hint);     // arity_hint: 8 for first row,
let mut first = true;                              // first_row_len for later rows
row.push(parse_value_expr(p)?);
while p.eat(&Token::Comma) { row.push(parse_value_expr(p)?); }
// after first row: arity_hint = row.len() for subsequent rows
```

(The `SET col=val` path keeps `parse_expr` — out of scope.)

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql
./tools/vm.sh clippy -p axiomdb-sql -- -D warnings
```

### Commit

```
perf(perf-sqlite-gap): VALUES literal fast-path in INSERT parser

Step 2 of specs/fase-perf-sqlite-gap/plan-faster-insert-parser.md
```

---

## Step 3 — Exhaustive edge-case tests

**Goal:** lock every spec edge case (fast-path AND fall-back) against `parse_expr`.
**Files:** parser test module.
**Approach:** one test per edge-case bullet; each asserts the VALUES element(s)
equal the `parse_expr_only` reference, so fast and slow paths are proven identical.

### Tests to add

```rust
// fall-back cases (must NOT fast-path; must equal parse_expr)
#[test] fn values_compound_falls_back()  { eq("1 + 2"); }       // BinaryOp
#[test] fn values_negative_falls_back()  { eq("-5"); }          // UnaryOp(Neg)
#[test] fn values_function_falls_back()  { eq("CONCAT('a','b')"); }
#[test] fn values_default_falls_back()   { /* DEFAULT -> Expr::Default */ }
#[test] fn values_param_counts()         { /* (?) increments param_count */ }
#[test] fn values_cast_falls_back()      { eq("1::bigint"); }
#[test] fn values_subscript_falls_back() { eq("arr[1]"); }
#[test] fn values_subquery_falls_back()  { eq("(SELECT 1)"); }
// fast-path cases
#[test] fn values_string_escapes()       { eq("'a''b'"); }
#[test] fn values_multi_row_presized()   { /* (1,'a'),(2,'b') identical */ }
#[test] fn values_single_element()       { /* (1) peek_at(1)=RParen */ }
```

where `eq(v)` parses `INSERT INTO t VALUES ({v})` and asserts element ==
`parse_expr_only(v)`.

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql
```

### Commit

```
test(perf-sqlite-gap): INSERT VALUES fast-path edge cases

Step 3 of specs/fase-perf-sqlite-gap/plan-faster-insert-parser.md
```

---

## Step 4 — Validate, benchmark, close

**Goal:** verify the spec's done criteria and budget.

### Verification against spec

- [ ] `cargo nextest run --workspace` green — Lima (`./tools/vm.sh test --workspace`)
- [ ] `cargo clippy --workspace -- -D warnings` clean — Lima
- [ ] `cargo fmt --check` clean
- [ ] Rebuild bench (macOS): `cargo build --release -p axiomdb-bench-comparison`
- [ ] `--diagnose-parse` (medians ≥3): AST-build ≤ ~1.2µs (was ~2.6); full parse
  ≤ ~2.0µs (was ~3.4)
- [ ] `--compare --rows 10000`: insert_batch ratio ≤ ~4.2× (was ~5.3×); crud_flow/
  insert improved; **crud_flow/select unchanged** (SELECT parse no-regression gate)
- [ ] rustdoc on `parse_value_expr` + `literal_token_to_expr`

### Final commit

```
perf(perf-sqlite-gap): faster INSERT parser via VALUES literal fast-path

Implements specs/fase-perf-sqlite-gap/spec-faster-insert-parser.md
Plan: specs/fase-perf-sqlite-gap/plan-faster-insert-parser.md
```

Then `/subfase-completa` (docs + progreso + memory + push) per CLAUDE.md.

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `parse_atom` delegation slows SELECT parse | low | Step 1 measures SELECT parse; contingency reverts to inline arms |
| Fast-path AST diverges from `parse_expr` | low | equivalence test (Step 2) is the oracle; exhaustive edge cases (Step 3) |
| `Expr` not `PartialEq` for assert_eq | low | already derived (statement_cache compares Exprs); else compare `{:?}` |
| `peek_at(1)` was `#[allow(dead_code)]` | none | using it removes the allow; it is a real method |
| gain smaller than budget | medium | honest: parser is half the row; executor subphase (task #6) does the rest |

## Rollback plan

1. `git reset --hard <commit before Step 1>`, or
2. Leave partial work on `abandoned/plan-faster-insert-parser-<date>`
3. Revert spec status to `draft` with a failure note.

## Estimated effort

Total: ~3–4 hours. Step 1 ~45min · Step 2 ~1h · Step 3 ~45min · Step 4 ~1h (mostly
Lima test/bench wall-time). Implementation effort level: `medium`.
