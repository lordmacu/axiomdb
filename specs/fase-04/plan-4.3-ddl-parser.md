# Plan: 4.3 + 4.3a–4.3d — DDL Parser

## Files to create / modify

| File | Action | Description |
|---|---|---|
| `crates/axiomdb-sql/src/parser/mod.rs` | CREATE | `Parser` struct, helpers, `parse()` entry point |
| `crates/axiomdb-sql/src/parser/expr.rs` | CREATE | Expression sub-parser |
| `crates/axiomdb-sql/src/parser/ddl.rs` | CREATE | DDL statement parsers |
| `crates/axiomdb-sql/src/parser/dml.rs` | CREATE | Stub (Phase 4.4) |
| `crates/axiomdb-sql/src/lib.rs` | MODIFY | `pub mod parser` + re-export `parse` |
| `crates/axiomdb-sql/tests/integration_ddl_parser.rs` | CREATE | Integration tests |

---

## Algorithm / Data structure

### Parser struct

```rust
pub(crate) struct Parser<'a> {
    tokens: &'a [SpannedToken],
    pos: usize,
}

impl<'a> Parser<'a> {
    pub(crate) fn new(tokens: &'a [SpannedToken]) -> Self {
        Self { tokens, pos: 0 }
    }

    /// Current token without advancing.
    pub(crate) fn peek(&self) -> &Token {
        self.tokens.get(self.pos)
            .map(|st| &st.token)
            .unwrap_or(&Token::Eof)
    }

    /// Look-ahead: token at pos + offset.
    pub(crate) fn peek_at(&self, offset: usize) -> &Token {
        self.tokens.get(self.pos + offset)
            .map(|st| &st.token)
            .unwrap_or(&Token::Eof)
    }

    /// Byte position of the current token (for error messages).
    pub(crate) fn current_pos(&self) -> usize {
        self.tokens.get(self.pos)
            .map(|st| st.span.start)
            .unwrap_or(0)
    }

    /// Consume current token and advance.
    pub(crate) fn advance(&mut self) -> &SpannedToken {
        let st = &self.tokens[self.pos];
        self.pos += 1;
        st
    }

    /// Consume if token matches; else return ParseError.
    pub(crate) fn expect(&mut self, expected: &Token) -> Result<(), DbError> {
        if self.peek() == expected {
            self.pos += 1;
            Ok(())
        } else {
            Err(DbError::ParseError {
                message: format!(
                    "expected {:?} but found {:?} at position {}",
                    expected, self.peek(), self.current_pos()
                ),
            })
        }
    }

    /// Consume if token matches; return false if not.
    pub(crate) fn eat(&mut self, expected: &Token) -> bool {
        if self.peek() == expected {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Consume Ident token and return the name. Validates 4.3d (≤ 64 chars).
    pub(crate) fn parse_identifier(&mut self) -> Result<String, DbError> {
        match self.peek().clone() {
            Token::Ident(name) | Token::QuotedIdent(name) | Token::DqIdent(name) => {
                self.pos += 1;
                validate_identifier_length(&name, self.current_pos())?;
                Ok(name)
            }
            other => Err(DbError::ParseError {
                message: format!(
                    "expected identifier but found {:?} at position {}",
                    other, self.current_pos()
                ),
            }),
        }
    }

    /// Parse `[schema '.'] name` as TableRef.
    pub(crate) fn parse_table_ref(&mut self) -> Result<TableRef, DbError> {
        let name = self.parse_identifier()?;
        if self.eat(&Token::Dot) {
            // name was actually the schema
            let table = self.parse_identifier()?;
            Ok(TableRef { schema: Some(name), name: table, alias: None })
        } else {
            Ok(TableRef { schema: None, name, alias: None })
        }
    }
}
```

### 4.3d — identifier length validation

```rust
const MAX_IDENTIFIER_LEN: usize = 64;

fn validate_identifier_length(name: &str, pos: usize) -> Result<(), DbError> {
    if name.len() > MAX_IDENTIFIER_LEN {
        return Err(DbError::ParseError {
            message: format!(
                "identifier '{}' exceeds maximum length of {} characters ({} chars) at position {}",
                name, MAX_IDENTIFIER_LEN, name.len(), pos
            ),
        });
    }
    Ok(())
}
```

### Token comparison helper

`Token` doesn't implement `PartialEq` in a way that ignores payload (e.g.,
`Token::Ident("x")` vs `Token::Ident("y")` are not equal). For `expect` and
`eat`, we compare only the discriminant:

```rust
// Use matches! macro for variant-only comparison:
fn is_token_type(tok: &Token, expected: &Token) -> bool {
    std::mem::discriminant(tok) == std::mem::discriminant(expected)
}
```

Or define specific `expect_keyword` helpers for each keyword since keywords
have no payload:

```rust
pub(crate) fn expect_kw(&mut self, kw: &Token) -> Result<(), DbError> {
    // keywords have no payload, so PartialEq works directly
    self.expect(kw)
}
```

Since `Token` derives `PartialEq` and keyword variants have no payload, `==`
works correctly: `Token::Select == Token::Select` is `true`.
For payload variants, `eat(&Token::Ident(String::new()))` won't work — use
`parse_identifier()` instead.

---

### Expression sub-parser (expr.rs)

Recursive descent with precedence:

```rust
pub(crate) fn parse_expr(p: &mut Parser) -> Result<Expr, DbError> {
    parse_or(p)
}

fn parse_or(p: &mut Parser) -> Result<Expr, DbError> {
    let mut left = parse_and(p)?;
    while p.eat(&Token::Or) {
        let right = parse_and(p)?;
        left = Expr::BinaryOp { op: BinaryOp::Or, left: Box::new(left), right: Box::new(right) };
    }
    Ok(left)
}

fn parse_and(p: &mut Parser) -> Result<Expr, DbError> {
    let mut left = parse_not(p)?;
    while p.eat(&Token::And) {
        let right = parse_not(p)?;
        left = Expr::BinaryOp { op: BinaryOp::And, left: Box::new(left), right: Box::new(right) };
    }
    Ok(left)
}

fn parse_not(p: &mut Parser) -> Result<Expr, DbError> {
    if p.eat(&Token::Not) {
        let operand = parse_not(p)?;
        return Ok(Expr::UnaryOp { op: UnaryOp::Not, operand: Box::new(operand) });
    }
    parse_comparison(p)
}

fn parse_comparison(p: &mut Parser) -> Result<Expr, DbError> {
    let left = parse_unary(p)?;
    let op = match p.peek() {
        Token::Eq    => { p.advance(); BinaryOp::Eq }
        Token::NotEq => { p.advance(); BinaryOp::NotEq }
        Token::Lt    => { p.advance(); BinaryOp::Lt }
        Token::LtEq  => { p.advance(); BinaryOp::LtEq }
        Token::Gt    => { p.advance(); BinaryOp::Gt }
        Token::GtEq  => { p.advance(); BinaryOp::GtEq }
        _ => return Ok(left),
    };
    let right = parse_unary(p)?;
    Ok(Expr::BinaryOp { op, left: Box::new(left), right: Box::new(right) })
}

fn parse_unary(p: &mut Parser) -> Result<Expr, DbError> {
    if p.eat(&Token::Minus) {
        let operand = parse_unary(p)?;
        return Ok(Expr::UnaryOp { op: UnaryOp::Neg, operand: Box::new(operand) });
    }
    parse_atom(p)
}

fn parse_atom(p: &mut Parser) -> Result<Expr, DbError> {
    match p.peek().clone() {
        Token::Integer(n)    => { p.advance(); Ok(Expr::Literal(Value::Int(n as i32))) }
        // Note: overflow to BigInt if n > i32::MAX
        Token::Float(f)      => { p.advance(); Ok(Expr::Literal(Value::Real(f))) }
        Token::StringLit(s)  => { p.advance(); Ok(Expr::Literal(Value::Text(s))) }
        Token::True          => { p.advance(); Ok(Expr::Literal(Value::Bool(true))) }
        Token::False         => { p.advance(); Ok(Expr::Literal(Value::Bool(false))) }
        Token::Null          => { p.advance(); Ok(Expr::Literal(Value::Null)) }
        Token::Ident(_) | Token::QuotedIdent(_) | Token::DqIdent(_) => {
            let name = p.parse_identifier()?;
            Ok(Expr::Column { col_idx: 0, name }) // col_idx resolved by semantic analyzer
        }
        Token::LParen => {
            p.advance();
            let expr = parse_expr(p)?;
            p.expect(&Token::RParen)?;
            Ok(expr)
        }
        other => Err(DbError::ParseError {
            message: format!("unexpected token {:?} in expression at position {}",
                other, p.current_pos()),
        }),
    }
}
```

Integer atom: if `n > i32::MAX`, use `Value::BigInt`. If `n < i32::MIN` (after
unary neg), also use `BigInt`.

---

### DDL parsers (ddl.rs)

#### parse_create_table

```
fn parse_create_table(p):
    p.expect(TABLE)
    if_not_exists = p.eat(IF) && p.expect(NOT) && p.expect(EXISTS)
    table = p.parse_table_ref()
    p.expect(LParen)
    columns = []
    table_constraints = []

    loop:
        item = parse_col_or_constraint(p)?
        match item:
            ColItem(col) → columns.push(col)
            ConstraintItem(tc) → table_constraints.push(tc)
        if !p.eat(Comma): break

    p.expect(RParen)
    Ok(Stmt::CreateTable(...))
```

#### parse_col_or_constraint

```
fn parse_col_or_constraint(p):
    // CONSTRAINT name ... or PRIMARY/UNIQUE/FOREIGN/CHECK → table constraint
    match p.peek():
        CONSTRAINT → parse named table constraint
        PRIMARY | UNIQUE | FOREIGN | CHECK → parse table constraint
        Ident → parse column def
        _ → Err
```

#### parse_column_def

```
fn parse_column_def(p):
    name = p.parse_identifier()
    data_type = parse_data_type(p)
    constraints = []
    loop:
        match p.peek():
            NOT    → p.advance(); p.expect(NULL); constraints.push(NotNull)
            NULL   → p.advance(); constraints.push(Null)
            DEFAULT → p.advance(); constraints.push(Default(parse_expr(p)?))
            PRIMARY → p.advance(); p.expect(KEY); constraints.push(PrimaryKey)
            UNIQUE  → p.advance(); constraints.push(Unique)
            AUTO_INCREMENT → p.advance(); constraints.push(AutoIncrement)
            SERIAL  → p.advance(); constraints.push(AutoIncrement)  // 4.3c
            REFERENCES → parse_references(p) → constraints.push(References{...})
            CHECK   → parse_check(p) → constraints.push(Check(expr))
            _ → break
    Ok(ColumnDef { name, data_type, constraints })
```

#### parse_data_type

```
fn parse_data_type(p):
    match p.peek():
        TyInt | TyInteger → p.advance(); Ok(DataType::Int)
        TyBigint → p.advance(); Ok(DataType::BigInt)
        TyReal | TyDouble | TyFloat → p.advance(); Ok(DataType::Real)
        TyDecimal | TyNumeric →
            p.advance()
            eat_optional_precision_scale(p)  // discard, DEFERRED
            Ok(DataType::Decimal)
        TyBool | TyBoolean → p.advance(); Ok(DataType::Bool)
        TyText → p.advance(); Ok(DataType::Text)
        TyVarchar | TyChar →
            p.advance()
            eat_optional_length(p)  // discard, DEFERRED
            Ok(DataType::Text)
        TyBlob | TyBytea → p.advance(); Ok(DataType::Bytes)
        TyDate → p.advance(); Ok(DataType::Date)
        TyTimestamp | TyDatetime → p.advance(); Ok(DataType::Timestamp)
        TyUuid → p.advance(); Ok(DataType::Uuid)
        _ → Err(ParseError "expected data type")

fn eat_optional_precision_scale(p):
    if p.eat(LParen):
        expect Integer (precision, discard)
        if p.eat(Comma): expect Integer (scale, discard)
        p.expect(RParen)

fn eat_optional_length(p):
    if p.eat(LParen):
        expect Integer (discard)
        p.expect(RParen)
```

#### parse_fk_action

```
fn parse_fk_action(p):
    match p.peek():
        CASCADE → p.advance(); Ok(ForeignKeyAction::Cascade)
        RESTRICT → p.advance(); Ok(ForeignKeyAction::Restrict)
        SET →
            p.advance()
            match p.peek():
                NULL → p.advance(); Ok(SetNull)
                DEFAULT → p.advance(); Ok(SetDefault)
                _ → Err
        NO →
            p.advance(); p.expect(ACTION); Ok(NoAction)
        _ → Ok(NoAction)  // default if no action specified
```

#### parse_references

```
fn parse_references(p):
    p.expect(REFERENCES)
    table = p.parse_identifier()
    column = if p.eat(LParen): Some(p.parse_identifier()?) else None
    if p.eat(RParen) if column was Some...
    on_delete = if next is ON and peek_at(1)==DELETE: parse fk_action
    on_update = if next is ON and peek_at(1)==UPDATE: parse fk_action
    Ok(ColumnConstraint::References { table, column, on_delete, on_update })
```

---

## Implementation phases

### Phase 1 — Directory structure
1. Create `crates/axiomdb-sql/src/parser/` directory.
2. Create stub files: `mod.rs`, `expr.rs`, `ddl.rs`, `dml.rs`.

### Phase 2 — Parser struct (mod.rs)
1. `Parser<'a>` struct with `tokens` and `pos`.
2. All helpers: `peek`, `peek_at`, `advance`, `expect`, `eat`, `current_pos`.
3. `parse_identifier` with 4.3d length check.
4. `parse_table_ref`.
5. `validate_identifier_length` helper.
6. `parse()` public entry: tokenize + Parser::new + parse_stmt.
7. `parse_stmt`: dispatches on first token.

### Phase 3 — Expression sub-parser (expr.rs)
1. `parse_expr` (calls `parse_or`)
2. `parse_or`, `parse_and`, `parse_not`, `parse_comparison`, `parse_unary`, `parse_atom`
3. Integer overflow to BigInt in `parse_atom`.

### Phase 4 — DDL parsers (ddl.rs)
1. `parse_create_table`
2. `parse_col_or_constraint` (LL(2) dispatch)
3. `parse_column_def`
4. `parse_data_type` + `eat_optional_precision_scale` + `eat_optional_length`
5. `parse_column_constraint`
6. `parse_table_constraint`
7. `parse_references` (4.3a)
8. `parse_fk_action`
9. `parse_create_index`
10. `parse_drop_table`
11. `parse_drop_index`

### Phase 5 — DML stub (dml.rs)
1. `parse_dml(p) -> Result<Stmt, DbError>` returning `Err(NotImplemented)`.

### Phase 6 — lib.rs
1. Add `pub mod parser;` with `pub use parser::parse;`.

### Phase 7 — Integration tests
File: `crates/axiomdb-sql/tests/integration_ddl_parser.rs`

Tests:
```
CREATE TABLE basic
CREATE TABLE IF NOT EXISTS
CREATE TABLE with NOT NULL, NULL
CREATE TABLE with DEFAULT literals (int, float, string, bool, null, negative)
CREATE TABLE with DEFAULT expression (DEFAULT TRUE, DEFAULT -1)
CREATE TABLE with PRIMARY KEY (column-level)
CREATE TABLE with UNIQUE (column-level)
CREATE TABLE with AUTO_INCREMENT (4.3c)
CREATE TABLE with SERIAL (4.3c synonym)
CREATE TABLE with REFERENCES basic
CREATE TABLE with REFERENCES + ON DELETE CASCADE
CREATE TABLE with REFERENCES + ON DELETE SET NULL
CREATE TABLE with REFERENCES + ON UPDATE RESTRICT
CREATE TABLE with CHECK (basic comparison) (4.3b)
CREATE TABLE with CHECK (AND expression) (4.3b)
CREATE TABLE with table-level PRIMARY KEY
CREATE TABLE with table-level UNIQUE
CREATE TABLE with table-level FOREIGN KEY
CREATE TABLE with CONSTRAINT name PRIMARY KEY
CREATE TABLE multiple columns mixed types
All 12 data types
DECIMAL(10,2) accepted (params discarded)
VARCHAR(255) accepted (params discarded)
CREATE UNIQUE INDEX
CREATE INDEX IF NOT EXISTS
CREATE INDEX with ASC/DESC
DROP TABLE basic
DROP TABLE IF EXISTS
DROP TABLE multiple tables
DROP TABLE CASCADE
DROP INDEX basic
DROP INDEX IF EXISTS
DROP INDEX ON table (MySQL style)
4.3d: identifier exactly 64 chars → Ok
4.3d: identifier 65 chars → ParseError
4.3d: table name too long → ParseError
Error: empty input
Error: unexpected token
Error: missing closing paren
```

---

## Anti-patterns to avoid

- **DO NOT** use `unwrap()` anywhere in `src/` — all token accesses must use
  safe `peek()`/`peek_at()` which return `&Token::Eof` at end of input.
- **DO NOT** advance past the end of the token stream — `advance()` must check
  bounds or be guarded by `peek() != &Token::Eof`.
- **DO NOT** implement `parse_atom` with `advance()` before matching — clone
  the token first, then advance after confirming type.
- **DO NOT** forget the `Eof` at the end — after parsing the statement,
  check that the next token is `Eof` (for single-statement input) or
  `Semicolon` (for multi-statement input). In Phase 4.3, require `Eof`.

---

## Risks

| Risk | Mitigation |
|---|---|
| `UNIQUE` ambiguity: column constraint vs table constraint start | LL(2): `UNIQUE '('` = table constraint; `UNIQUE` at column level is column constraint |
| `INTEGER` integer literal overflows i32 | In `parse_atom`: if `n > i32::MAX as i64`, produce `Value::BigInt(n)` |
| `peek()` at end panics | `peek()` returns `&Token::Eof` when `pos >= tokens.len()` |
| FK ON DELETE / ON UPDATE order | Both are optional, both are checked in a loop; order doesn't matter |
| `SERIAL` token — is it lexed? | Yes, `Token::Serial` is defined in the lexer |
| LL(2) for col_def vs table_constraint | Use `peek()` for discriminant, `peek_at(1)` for disambiguation when needed |
