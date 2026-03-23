# SQL Parser

The SQL parser lives in `nexusdb-sql` and is split into three stages:
**lexer** (string → tokens), **parser** (tokens → AST), and **semantic analyzer**
(AST → validated AST with resolved column indices). This page covers the lexer and
parser. The semantic analyzer is documented in [Semantic Analyzer](semantic-analyzer.md).

---

## Why logos, Not nom

NexusDB uses the `logos` crate to generate the lexer, rather than `nom` combinators
or hand-written code.

| Criterion            | logos                            | nom                              |
|----------------------|----------------------------------|----------------------------------|
| Compilation model    | Compiles patterns to DFA at build time | Constructs parsers at runtime  |
| Token scan cost      | O(n), 1–3 instructions/byte      | O(n), higher constant factor     |
| Heap allocations     | Zero (identifiers are `&'src str`) | Possible in combinators         |
| Case-insensitive keys| `ignore(ascii_case)` attribute   | Manual lowercasing pass needed   |
| Error messages       | Byte offsets built-in            | Requires manual tracking         |

**Benchmark result:** NexusDB's lexer achieves **9–17× higher throughput** than
`sqlparser-rs` (which uses nom internally) for the same SQL inputs. The advantage
holds across simple SELECT, complex multi-join SELECT, and DDL statements.

The primary reason is the DFA: logos compiles all keyword patterns into a single
Deterministic Finite Automaton at compile time. Processing each character is a table
lookup in a pre-computed transition matrix — constant time per character with a very
small constant. nom combinators perform dynamic dispatch and allocate intermediate
results.

---

## Lexer Design

### Zero-Copy Tokens

Identifiers and quoted identifiers are represented as `&'src str` — slices into the
original SQL string. No heap allocation occurs during lexing for identifiers.

Only `StringLit` allocates a `String`, because escape sequence processing (`\'`, `\\`,
`\n`) transforms the content in place and cannot be zero-copy.

```rust
pub struct SpannedToken<'src> {
    pub token: Token<'src>,
    pub span: Span,          // byte offsets (start, end) in the original string
}
```

The lifetime `'src` ensures that token slices cannot outlive the input string.

### Token Enum

The `Token<'src>` enum has approximately 85 variants:

```rust
pub enum Token<'src> {
    // DML keywords (case-insensitive)
    Select, From, Where, Insert, Into, Values, Update, Set, Delete,
    // DDL keywords
    Create, Table, Index, Drop, Alter, Add, Column, Constraint,
    // Transaction keywords
    Begin, Commit, Rollback, Savepoint, Release,
    // Data types
    Bool, Boolean, TinyInt, SmallInt, Int, Integer, BigInt, HugeInt,
    Real, Float, Double, Decimal, Numeric, Char, VarChar, Text, Bytea, Blob,
    Date, Time, Timestamp, Uuid, Json, Jsonb, Vector,
    // Clause keywords
    Join, Inner, Left, Right, Cross, On, Using,
    Group, By, Having, Order, Asc, Desc, Nulls, First, Last,
    Limit, Offset, Distinct, All,
    // Constraint keywords
    Primary, Key, Unique, Not, Null, Default, References, Check,
    Auto, Increment, Serial, Bigserial, Foreign, Cascade, Restrict, NoAction,
    // Logical operators
    And, Or,
    // Functions
    Is, In, Between, Like, Ilike, Exists, Case, When, Then, Else, End,
    Coalesce, NullIf,
    // Identifier variants
    Ident(&'src str),           // unquoted identifier
    QuotedIdent(&'src str),     // backtick-quoted `identifier`
    DqIdent(&'src str),         // double-quote "identifier"
    // Literals
    IntLit(i64), FloatLit(f64), StringLit(String), HexLit(Vec<u8>),
    TrueLit, FalseLit, NullLit,
    // Punctuation
    LParen, RParen, Comma, Semicolon, Dot, Star, Eq, Ne, Lt, Le, Gt, Ge,
    Plus, Minus, Slash, Percent, Bang, BangEq, Arrow, FatArrow,
    // Sentinel
    Eof,
}
```

### Keyword Priority Over Identifiers

logos resolves ambiguities by matching keywords before identifiers. The rule is:
longer matches take priority; if lengths are equal, keywords take priority over
`Ident`. This is expressed in logos as:

```rust
#[token("SELECT", ignore(ascii_case))]
Select,

#[regex(r"[A-Za-z_][A-Za-z0-9_]*")]
Ident(&'src str),
```

`SELECT`, `select`, and `Select` all produce `Token::Select`, not `Token::Ident`.
A hypothetical column named `select` must be escaped: `` `select` `` or `"select"`.

### Comment Stripping

All three MySQL-compatible comment styles are skipped automatically:

```
-- single-line comment (SQL standard)
# single-line comment  (MySQL extension)
/* block comment */
```

### fail-fast Limits

`tokenize(sql, max_bytes)` checks the SQL length before scanning. If `sql.len() > max_bytes`,
it returns `DbError::ParseError` immediately without touching the DFA. This protects
against memory exhaustion from maliciously large queries.

---

## Parser Design

The parser is a hand-written recursive descent parser. It does not use any parser
combinator library — the grammar is simple enough that combinators would add overhead
without benefit.

### Parser State

```rust
struct Parser<'src> {
    tokens: Vec<SpannedToken<'src>>,
    pos: usize,
}

impl<'src> Parser<'src> {
    fn peek(&self) -> &Token<'src>;         // current token, no advance
    fn advance(&mut self) -> &Token<'src>;  // consume and return current token
    fn expect(&mut self, t: &Token) -> Result<(), DbError>;  // consume or error
    fn eat(&mut self, t: &Token) -> bool;   // consume if matching, else false
}
```

### Grammar — LL(1) for DDL, LL(2) for DML

Most DDL productions are LL(1): the first token uniquely determines the production.
Some DML productions require one lookahead token:

- `SELECT * FROM t` vs `SELECT a, b FROM t` — the parser sees `SELECT` then peeks at
  the next token to decide whether to parse `*` or a projection list.
- `INSERT INTO t VALUES (...)` vs `INSERT INTO t SELECT ...` — after consuming `INTO t`,
  peek determines whether to parse a VALUES list or a sub-SELECT.

### Expression Precedence

The expression sub-parser implements the standard precedence chain using separate
functions for each precedence level. This is equivalent to a Pratt parser without the
extra machinery:

```
parse_expr()           (entry point — calls parse_or)
  parse_or()           OR
    parse_and()        AND
      parse_not()      unary NOT
        parse_is_null()    IS NULL / IS NOT NULL
          parse_predicate()  =, <>, !=, <, <=, >, >=, BETWEEN, LIKE, IN
            parse_addition()  + and -
              parse_multiplication()  *, /, %
                parse_unary()  unary minus -x
                  parse_atom()  literal, column ref, function call, subexpr
```

Each level calls the next level to parse its right-hand side, naturally implementing
left-to-right associativity and the correct precedence hierarchy.

### DDL Grammar Sketch

```
stmt → select_stmt | insert_stmt | update_stmt | delete_stmt
     | create_table_stmt | create_index_stmt
     | drop_table_stmt | drop_index_stmt
     | alter_table_stmt | truncate_stmt
     | begin_stmt | commit_stmt | rollback_stmt | savepoint_stmt

create_table_stmt →
  CREATE TABLE [IF NOT EXISTS] ident
  LPAREN column_def_list [COMMA table_constraint_list] RPAREN

column_def →
  ident type_name [column_constraint...]

column_constraint →
    NOT NULL
  | DEFAULT expr
  | PRIMARY KEY
  | UNIQUE
  | AUTO_INCREMENT | SERIAL | BIGSERIAL
  | REFERENCES ident LPAREN ident RPAREN [on_action] [on_action]
  | CHECK LPAREN expr RPAREN

table_constraint →
    PRIMARY KEY LPAREN ident_list RPAREN
  | UNIQUE LPAREN ident_list RPAREN
  | FOREIGN KEY LPAREN ident_list RPAREN REFERENCES ident LPAREN ident_list RPAREN
  | CHECK LPAREN expr RPAREN
  | CONSTRAINT ident (primary_key | unique | foreign_key | check)
```

### SELECT Grammar Sketch

```
select_stmt →
  SELECT [DISTINCT] select_list
  FROM table_ref [join_clause...]
  [WHERE expr]
  [GROUP BY expr_list]
  [HAVING expr]
  [ORDER BY order_item_list]
  [LIMIT int_lit [OFFSET int_lit]]

select_list → STAR | select_item (COMMA select_item)*
select_item → expr [AS ident]

table_ref → ident [AS ident]

join_clause →
  [INNER | LEFT [OUTER] | RIGHT [OUTER] | CROSS]
  JOIN table_ref join_condition

join_condition → ON expr | USING LPAREN ident_list RPAREN

order_item → expr [ASC | DESC] [NULLS (FIRST | LAST)]
```

---

## Output — The AST

The parser returns a `Stmt` enum. After parsing, all `Expr::Column` nodes have
`col_idx = 0` as a placeholder. The semantic analyzer fills in the correct indices.

```rust
pub enum Stmt {
    Select(SelectStmt),
    Insert(InsertStmt),
    Update(UpdateStmt),
    Delete(DeleteStmt),
    CreateTable(CreateTableStmt),
    CreateIndex(CreateIndexStmt),
    DropTable(DropTableStmt),
    DropIndex(DropIndexStmt),
    AlterTable(AlterTableStmt),
    Truncate(TruncateTableStmt),
    Begin, Commit, Rollback,
    Savepoint(String),
    ReleaseSavepoint(String),
    RollbackToSavepoint(String),
    ShowTables(ShowTablesStmt),
    ShowColumns(ShowColumnsStmt),
}
```

---

## Error Reporting

Parse errors include the position (byte offset) where the unexpected token was found:

```rust
DbError::ParseError {
    message: "expected column name after 'SELECT', found 'FROM' at byte 7".to_string(),
}
```

Lexer errors (invalid characters, unterminated string literals) include the byte span
of the problematic token.

---

## Performance Numbers

Measured on Apple M2 Pro, single-threaded, 1 million iterations each:

| Query                                   | Throughput (logos lexer + parser) |
|-----------------------------------------|-----------------------------------|
| `SELECT * FROM t`                       | 492 ns / query → 2.0M queries/s   |
| `SELECT a, b, c FROM t WHERE id = 1`   | 890 ns / query → 1.1M queries/s   |
| Complex SELECT (3 JOINs, subquery)      | 2.7 µs / query → 370K queries/s   |
| `CREATE TABLE` (10 columns)            | 1.1 µs / query → 910K queries/s   |
| `INSERT ... VALUES (...)` (5 values)   | 680 ns / query → 1.5M queries/s   |

These numbers represent parse throughput only — before semantic analysis or execution.
At 2 million simple queries per second, parsing is never the bottleneck for OLTP
workloads at realistic connection concurrency.
