# Plan: 21.19 — FETCH FIRST / OFFSET n ROWS

## Files

### Modify

- `crates/axiomdb-sql/src/lexer.rs`: add 3 keyword tokens (FIRST
  already exists):
  - `Token::Fetch`
  - `Token::Next`
  - `Token::Row`, `Token::Rows`
  - `Token::Only`  already exists
- `crates/axiomdb-sql/src/parser/dml.rs::parse_limit_offset`:
  extend the grammar to recognize:
  - `OFFSET expr ('ROW' | 'ROWS')` (noise words)
  - `FETCH ('FIRST' | 'NEXT') [expr] ('ROW' | 'ROWS') 'ONLY'`
  Both clauses optional and order-independent. Emit same
  `(Option<Expr>, Option<Expr>)` tuple. Reject when both `LIMIT`
  and `FETCH FIRST` appear on the same query.

### Create

- `crates/axiomdb-sql/tests/integration_fetch_first.rs` — 8 tests.

## Algorithm

```rust
fn parse_limit_offset(p) -> Result<(Option<Expr>, Option<Expr>)> {
    let mut limit: Option<Expr> = None;
    let mut offset: Option<Expr> = None;
    let mut saw_limit_keyword = false;
    let mut saw_fetch_keyword = false;

    loop {
        if p.eat(&Token::Limit) {
            if saw_fetch_keyword { return Err(mix_error()); }
            saw_limit_keyword = true;
            // existing LIMIT / MySQL comma / OFFSET tail
            let first = parse_expr(p)?;
            if p.eat(&Token::Comma) {
                let count = parse_expr(p)?;
                offset = Some(first);
                limit = Some(count);
            } else {
                limit = Some(first);
                if p.eat(&Token::Offset) {
                    offset = Some(parse_expr(p)?);
                    eat_row_or_rows(p); // noise after OFFSET n [ROW|ROWS]
                }
            }
        } else if p.eat(&Token::Offset) {
            if saw_limit_keyword { /* OFFSET handled by LIMIT path */ }
            let e = parse_expr(p)?;
            offset = Some(e);
            eat_row_or_rows(p);
        } else if p.eat(&Token::Fetch) {
            if saw_limit_keyword { return Err(mix_error()); }
            saw_fetch_keyword = true;
            // FIRST | NEXT (interchangeable)
            if !(p.eat(&Token::First) || p.eat(&Token::Next)) {
                return Err(parse_err("expected FIRST or NEXT after FETCH"));
            }
            // optional count — absent means 1
            let count = if p.peek_is(Token::Row) || p.peek_is(Token::Rows) {
                Expr::Literal(Value::Int(1))
            } else {
                parse_expr(p)?
            };
            if !(p.eat(&Token::Row) || p.eat(&Token::Rows)) {
                return Err(parse_err("expected ROW or ROWS after FETCH FIRST [count]"));
            }
            p.expect(&Token::Only)?;
            limit = Some(count);
        } else {
            break;
        }
    }
    Ok((limit, offset))
}

fn eat_row_or_rows(p) {
    p.eat(&Token::Row) || p.eat(&Token::Rows);
}
```

Noise-word consumption does not error if absent — only matters when
present. The loop terminates at the first unrecognized token.

## Tests

1. `fetch_first_n_rows_only_basic` — `FETCH FIRST 3 ROWS ONLY`.
2. `fetch_first_row_only_implies_one` — `FETCH FIRST ROW ONLY`.
3. `fetch_next_interchangeable_with_first` — `FETCH NEXT 5 ROWS ONLY`.
4. `offset_rows_noise_word` — `OFFSET 2 ROWS`.
5. `offset_row_noise_word` — `OFFSET 2 ROW` (singular after `2`).
6. `combined_offset_fetch_first` — paginated window.
7. `limit_and_fetch_mixed_rejected` — parse error.
8. `legacy_limit_offset_still_works` — regression.

Wire smoke: `OFFSET n ROWS FETCH FIRST m ROWS ONLY` pagination.

## Phases

1. Lexer: 3 new tokens (~5 LoC).
2. Parser: rewrite `parse_limit_offset` (~40 LoC).
3. Tests.
4. Close protocol (docs + progreso + memory + commit).

## Anti-patterns

- Don't add new AST variants — both clauses desugar to existing
  `stmt.limit` / `stmt.offset` expressions.
- Don't silently accept `LIMIT` + `FETCH FIRST` mixed. Nonstandard
  and confuses debuggers. Parse-time error.
- Don't treat `ROW` / `ROWS` as reserved identifiers — they are
  only consumed positionally inside OFFSET / FETCH clauses. A
  column named `row` remains parseable elsewhere.

## Risks

- Keyword `ROW` collides with potential future `ROW(...)`
  constructor (spec out-of-scope row types). Mitigation: `ROW` only
  consumed when immediately following a count expression inside
  OFFSET/FETCH — no standalone `ROW` token consumption.
- `NEXT` could collide with `NEXTVAL` function call. Mitigation:
  `NEXT` only consumed right after `FETCH`; function call grammar
  unaffected.
- `FETCH` currently not a keyword in AxiomDB. Adding it does not
  affect `FETCH` cursor statements (not yet implemented — 21.10
  pending). When cursors land, that path will be a statement-level
  parser branch, orthogonal to this expression tail.
