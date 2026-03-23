# Spec: 4.3 + 4.3a–4.3d — DDL Parser

## What to build (not how)

A recursive descent parser over the `SpannedToken` stream that produces
`Stmt::CreateTable`, `Stmt::CreateIndex`, `Stmt::DropTable`, and
`Stmt::DropIndex` AST nodes.

Sub-variants 4.3a–4.3d are implemented within this phase:
- **4.3a**: Column constraints — `NOT NULL`, `DEFAULT expr`, `UNIQUE`,
  `PRIMARY KEY`, `REFERENCES fk`
- **4.3b**: `CHECK (expr)` at column and table level
- **4.3c**: `AUTO_INCREMENT` and `SERIAL` column constraints
- **4.3d**: 64-character maximum identifier length; clear SQL error when exceeded

A minimal expression sub-parser is also included, sufficient for `DEFAULT`
(literal values) and `CHECK` (comparisons, `AND`, `OR`, `NOT`). This
sub-parser is extended in Phase 4.4 for full DML expression parsing.

---

## Public API

```rust
/// Parse a single SQL statement from `input`.
///
/// Tokenizes `input` (forwarding `max_bytes` to `tokenize`) then parses
/// the resulting token stream into a [`Stmt`].
///
/// # Errors
/// - [`DbError::ParseError`] — input too long, unrecognized token,
///   unexpected token, or identifier exceeds 64 characters.
pub fn parse(input: &str, max_bytes: Option<usize>) -> Result<Stmt, DbError>
```

---

## Parser structure

```
nexusdb-sql/src/
  parser/
    mod.rs     ← parse() entry point; Parser struct; helpers
    expr.rs    ← expression sub-parser (grows in Phase 4.4)
    ddl.rs     ← DDL statement parsers
    dml.rs     ← DML parsers (Phase 4.4, stub for now)
```

`Parser<'a>` holds `tokens: &'a [SpannedToken]` and `pos: usize`.

### Core Parser helpers

```rust
impl<'a> Parser<'a> {
    fn peek(&self) -> &Token                          // current token (Eof at end)
    fn peek_at(&self, offset: usize) -> &Token        // look-ahead (Eof past end)
    fn advance(&mut self) -> &SpannedToken            // consume current, advance
    fn expect(&mut self, expected: &Token) -> Result<&SpannedToken, DbError>
    fn eat(&mut self, expected: &Token) -> bool       // consume if matches, else false
    fn current_span(&self) -> Span                    // span of current token
    fn parse_identifier(&mut self) -> Result<String, DbError>  // includes 4.3d check
    fn parse_table_ref(&mut self) -> Result<TableRef, DbError>
}
```

`peek` returns `&Token::Eof` when `pos >= tokens.len()`.

---

## Grammar (4.3d: all identifiers ≤ 64 chars)

### CREATE TABLE

```
create_table ::=
  CREATE TABLE [IF NOT EXISTS] table_ref
  '(' item (',' item)* ')'

item ::=
  [CONSTRAINT identifier] table_constraint   ← starts with PRIMARY|UNIQUE|FOREIGN|CHECK|CONSTRAINT
  | column_def                                ← starts with identifier

column_def ::= identifier data_type column_constraint*

column_constraint ::=
  NOT NULL
  | NULL
  | DEFAULT simple_expr
  | PRIMARY KEY
  | UNIQUE
  | AUTO_INCREMENT
  | SERIAL                    ← synonym for AUTO_INCREMENT
  | REFERENCES table_ref ['(' identifier ')'] [fk_actions]
  | CHECK '(' expr ')'

table_constraint ::=
  PRIMARY KEY '(' ident_list ')'
  | UNIQUE ['INDEX'|'KEY'] ['(' ident_list ')']
  | FOREIGN KEY ['(' ident_list ')'] REFERENCES table_ref '(' ident_list ')' [fk_actions]
  | CHECK '(' expr ')'

fk_actions ::= (ON DELETE fk_action | ON UPDATE fk_action)+

fk_action ::= CASCADE | RESTRICT | SET NULL | SET DEFAULT | NO ACTION

ident_list ::= identifier (',' identifier)*
```

### CREATE INDEX

```
create_index ::=
  CREATE [UNIQUE] INDEX [IF NOT EXISTS] identifier
  ON table_ref '(' index_column (',' index_column)* ')'

index_column ::= identifier [ASC | DESC]
```

### DROP TABLE

```
drop_table ::=
  DROP TABLE [IF EXISTS] table_ref (',' table_ref)* [CASCADE]
```

### DROP INDEX

```
drop_index ::=
  DROP INDEX [IF EXISTS] identifier [ON table_ref]
```

### Data types

| Token(s) | `DataType` |
|---|---|
| `TyInt`, `TyInteger` | `DataType::Int` |
| `TyBigint` | `DataType::BigInt` |
| `TyReal`, `TyDouble`, `TyFloat` | `DataType::Real` |
| `TyDecimal`, `TyNumeric` | `DataType::Decimal` — optional `(p)` or `(p,s)` parsed and discarded ⚠️ |
| `TyBool`, `TyBoolean` | `DataType::Bool` |
| `TyText` | `DataType::Text` |
| `TyVarchar`, `TyChar` | `DataType::Text` — optional `(n)` parsed and discarded ⚠️ |
| `TyBlob`, `TyBytea` | `DataType::Bytes` |
| `TyDate` | `DataType::Date` |
| `TyTimestamp`, `TyDatetime` | `DataType::Timestamp` |
| `TyUuid` | `DataType::Uuid` |

⚠️ Type parameters (`DECIMAL(10,2)`, `VARCHAR(255)`) are parsed and discarded
in Phase 4.3. `DataType` gains precision/scale in Phase 4.3 spec update —
see DEFERRED.

---

## Expression sub-parser (used by DEFAULT and CHECK)

Operator precedence (highest to lowest):

```
Atom    ::= Integer | Float | StringLit | TRUE | FALSE | NULL | identifier | '(' expr ')'
Unary   ::= '-' Unary | NOT Unary | Atom
Compare ::= Unary (('=' | '<>' | '!=' | '<' | '<=' | '>' | '>=') Unary)?
And     ::= Compare (AND Compare)*
Expr    ::= And (OR And)*
```

`DEFAULT` accepts any `Expr`.
`CHECK` requires `'(' Expr ')'`.

---

## 4.3d — Identifier length validation

Every call to `parse_identifier()` validates:
```rust
if name.len() > 64 {
    return Err(DbError::ParseError {
        message: format!(
            "identifier '{}' exceeds maximum length of 64 characters ({} chars)",
            name, name.len()
        ),
    });
}
```

Applies to: table names, column names, index names, constraint names, alias names.

---

## Use cases

1. **Simple CREATE TABLE**:
   ```sql
   CREATE TABLE users (id BIGINT, name TEXT)
   ```
   → `CreateTableStmt { table: "users", columns: [ColumnDef{id, BigInt}, ColumnDef{name, Text}] }`

2. **CREATE TABLE IF NOT EXISTS with constraints**:
   ```sql
   CREATE TABLE IF NOT EXISTS orders (
     id BIGINT PRIMARY KEY AUTO_INCREMENT,
     user_id BIGINT NOT NULL,
     total REAL DEFAULT 0.0,
     note TEXT NULL,
     FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
   )
   ```

3. **Table-level PRIMARY KEY**:
   ```sql
   CREATE TABLE t (a INT, b INT, PRIMARY KEY (a, b))
   ```

4. **CREATE UNIQUE INDEX**:
   ```sql
   CREATE UNIQUE INDEX IF NOT EXISTS idx_email ON users (email ASC)
   ```

5. **DROP TABLE multiple**:
   ```sql
   DROP TABLE IF EXISTS a, b, c CASCADE
   ```

6. **DROP INDEX MySQL style**:
   ```sql
   DROP INDEX idx_email ON users
   ```

7. **CHECK constraint**:
   ```sql
   CREATE TABLE products (price REAL CHECK (price > 0 AND price < 10000))
   ```

8. **REFERENCES with FK actions**:
   ```sql
   CREATE TABLE orders (
     id BIGINT,
     user_id BIGINT REFERENCES users(id) ON DELETE CASCADE ON UPDATE RESTRICT
   )
   ```

9. **Named CONSTRAINT**:
   ```sql
   CREATE TABLE t (id INT, CONSTRAINT pk PRIMARY KEY (id))
   ```

10. **4.3d — identifier too long**:
    ```sql
    CREATE TABLE very_long_name_exceeding_sixty_four_characters_total_xxxxxxxxxxx (id INT)
    ```
    → `Err(ParseError { "identifier exceeds maximum length of 64 characters" })`

11. **SERIAL column**:
    ```sql
    CREATE TABLE t (id INT SERIAL, name TEXT)
    ```
    → `ColumnConstraint::AutoIncrement` on `id`

12. **DEFAULT with expression**:
    ```sql
    CREATE TABLE t (score INT DEFAULT -1, active BOOL DEFAULT TRUE)
    ```

---

## Acceptance criteria

- [ ] `parse("CREATE TABLE t (id BIGINT)", None)` returns `Stmt::CreateTable`
- [ ] `IF NOT EXISTS` is parsed for CREATE TABLE
- [ ] Column definitions: `name data_type constraint*` parsed correctly
- [ ] `NOT NULL` → `ColumnConstraint::NotNull`
- [ ] `NULL` → `ColumnConstraint::Null`
- [ ] `DEFAULT literal` → `ColumnConstraint::Default(Expr::Literal(...))`
- [ ] `DEFAULT -1` → `ColumnConstraint::Default(Expr::UnaryOp(Neg, Literal(Int(1))))`
- [ ] `DEFAULT TRUE` → `ColumnConstraint::Default(Expr::Literal(Bool(true)))`
- [ ] `PRIMARY KEY` on column → `ColumnConstraint::PrimaryKey`
- [ ] `UNIQUE` on column → `ColumnConstraint::Unique`
- [ ] `AUTO_INCREMENT` → `ColumnConstraint::AutoIncrement`
- [ ] `SERIAL` → `ColumnConstraint::AutoIncrement` (same variant)
- [ ] `REFERENCES t` → `ColumnConstraint::References { table, column: None, ... }`
- [ ] `REFERENCES t(col)` → `References { column: Some("col"), ... }`
- [ ] `ON DELETE CASCADE` → `ForeignKeyAction::Cascade`
- [ ] `ON DELETE RESTRICT` → `ForeignKeyAction::Restrict`
- [ ] `ON DELETE SET NULL` → `ForeignKeyAction::SetNull`
- [ ] `ON DELETE NO ACTION` → `ForeignKeyAction::NoAction`
- [ ] `CHECK (expr)` on column → `ColumnConstraint::Check(expr)`
- [ ] Table-level `PRIMARY KEY (cols)` → `TableConstraint::PrimaryKey`
- [ ] Table-level `UNIQUE (cols)` → `TableConstraint::Unique`
- [ ] Table-level `FOREIGN KEY ... REFERENCES ...` → `TableConstraint::ForeignKey`
- [ ] Table-level `CHECK (expr)` → `TableConstraint::Check`
- [ ] `CONSTRAINT name PRIMARY KEY ...` → constraint with `name: Some("name")`
- [ ] `CREATE [UNIQUE] INDEX [IF NOT EXISTS] name ON table (cols)` parsed
- [ ] Index columns have optional `ASC`/`DESC` direction
- [ ] `DROP TABLE [IF EXISTS] t1, t2 [CASCADE]` parsed
- [ ] `DROP INDEX name [ON table]` parsed
- [ ] All 12 data type tokens mapped to `DataType`
- [ ] `DECIMAL(10,2)` and `VARCHAR(255)`: params parsed and discarded (no error)
- [ ] 4.3d: identifier > 64 chars → `Err(ParseError)`
- [ ] 4.3d: identifier = 64 chars → `Ok`
- [ ] Unexpected token → `Err(ParseError)` with position and token info
- [ ] `parse("", None)` → `Err(ParseError)` (empty input)
- [ ] No `unwrap()` in `src/`

---

## ⚠️ DEFERRED

- `DataType::Decimal(precision, scale)` parameters — `DataType` gains fields in
  Phase 4.18b (type coercion matrix)
- `VARCHAR(n)` max-length enforcement — Phase 4.5 (executor)
- `ALTER TABLE` parsing — Phase 4.22
- `TRUNCATE TABLE` parsing — Phase 4.21 (trivial, added then)
- Multi-statement parsing (`stmt; stmt;`) — Phase 4.5 executor wraps this
- `CREATE TABLE AS SELECT ...` — Phase 4.6

---

## Out of scope

- Semantic validation (table already exists, column types compatible) — Phase 4.18
- Execution (creating actual tables) — Phase 4.5
- DML parsing (SELECT, INSERT, UPDATE, DELETE) — Phase 4.4

---

## Dependencies

- `nexusdb-sql`: `lexer.rs` (Token, tokenize), `ast.rs` (Stmt, CreateTableStmt, etc.),
  `expr.rs` (Expr, BinaryOp, UnaryOp)
- `nexusdb-types`: `Value`, `DataType`
- `nexusdb-core`: `DbError`
- No new Cargo.toml dependencies
