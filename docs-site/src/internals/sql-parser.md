# SQL Parser

The SQL parser lives in `axiomdb-sql` and is split into three stages:
**lexer** (string → tokens), **parser** (tokens → AST), and **semantic analyzer**
(AST → validated AST with resolved column indices). This page covers the lexer and
parser. The semantic analyzer is documented in [Semantic Analyzer](semantic-analyzer.md).

---

## Why logos, Not nom

AxiomDB uses the `logos` crate to generate the lexer, rather than `nom` combinators
or hand-written code.

| Criterion            | logos                            | nom                              |
|----------------------|----------------------------------|----------------------------------|
| Compilation model    | Compiles patterns to DFA at build time | Constructs parsers at runtime  |
| Token scan cost      | O(n), 1–3 instructions/byte      | O(n), higher constant factor     |
| Heap allocations     | Zero (identifiers are `&'src str`) | Possible in combinators         |
| Case-insensitive keys| `ignore(ascii_case)` attribute   | Manual lowercasing pass needed   |
| Error messages       | Byte offsets built-in            | Requires manual tracking         |

**Benchmark result:** AxiomDB's lexer achieves **9–17× higher throughput** than
`sqlparser-rs` (which uses nom internally) for the same SQL inputs. The advantage
holds across simple SELECT, complex multi-join SELECT, and DDL statements.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">9–17× Faster Than the Production Standard</span>
<code>sqlparser-rs</code> is the SQL parser used by Apache Arrow DataFusion, Delta Lake, and InfluxDB. The DFA advantage is structural: logos compiles all keyword patterns into a single transition matrix at build time. Processing each character is one table lookup — nom combinators perform dynamic dispatch and build intermediate allocations for each combinator step.
</div>
</div>

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
     | show_tables_stmt | show_columns_stmt
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

truncate_stmt →
  TRUNCATE TABLE ident

show_tables_stmt →
  SHOW TABLES [FROM ident]

show_columns_stmt →
  SHOW COLUMNS FROM ident
  | DESCRIBE ident
  | DESC ident
```

### SHOW / DESCRIBE Parsing

`SHOW` is a dedicated keyword (`Token::Show`). After consuming it, the parser
peeks at the next token to dispatch:

```
parse_show():
  consume Show
  if peek = Ident("TABLES") | Ident("tables"):   // COLUMNS is not a reserved keyword
    advance
    schema = if eat(From): parse_ident() else "public"
    return Stmt::ShowTables(ShowTablesStmt { schema })
  if peek = Ident("COLUMNS") | Ident("columns"):
    advance; expect(From); table = parse_ident()
    return Stmt::ShowColumns(ShowColumnsStmt { table_name: table })
  else:
    return Err(ParseError { "expected TABLES or COLUMNS after SHOW" })
```

`DESCRIBE` and `DESC` are both tokenized as `Token::Describe` (the lexer
aliases both spellings to the same token). The parser dispatches them directly
to the `ShowColumns` AST node:

```
parse_stmt():
  ...
  Token::Describe => {
    advance; table = parse_ident()
    return Stmt::ShowColumns(ShowColumnsStmt { table_name: table })
  }
  ...
```

`COLUMNS` is not a reserved keyword in AxiomDB — a column or table named
`columns` does not need quoting. The parser matches it by comparing the
identifier string after lowercasing, not by token variant.

### TRUNCATE Parsing

`TRUNCATE` is tokenized as `Token::Truncate`. After consuming it, the parser
expects the literal keyword `TABLE` (also a reserved token) and then the table
name:

```
parse_truncate():
  consume Truncate
  expect(Table)
  table_name = parse_ident()
  return Stmt::Truncate(TruncateTableStmt { table_name })
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

## Subquery Parsing

Subqueries are parsed at three different points in the expression grammar, each
corresponding to a different syntactic form.

### Scalar Subqueries — `parse_atom`

`parse_atom` is the lowest-precedence entry point for all atoms: literals, column
references, function calls, and parenthesised expressions. When `parse_atom`
encounters an `LParen`, it peeks at the next token. If it is `Select`, it parses
a full `select_stmt` recursively and wraps it in `Expr::Subquery(Box<SelectStmt>)`.
Otherwise, it parses the contents as a grouped expression `(expr)`.

```
parse_atom():
  if peek = LParen:
    if peek+1 = Select:
      advance; stmt = parse_select_stmt(); expect(RParen)
      return Expr::Subquery(stmt)
    else:
      advance; e = parse_expr(); expect(RParen)
      return e
  ...
```

This means `(SELECT MAX(id) FROM t)` is valid anywhere an expression is valid:
`SELECT` list, `WHERE`, `HAVING`, `ORDER BY`, even nested inside function calls.

### IN Subquery — `parse_predicate`

`parse_predicate` handles comparison operators and the `IN` / `NOT IN` forms.
After detecting the `In` or `Not In` tokens, the parser checks whether the next
token is `LParen` followed by `Select`. If so, it parses a subquery and produces
`Expr::InSubquery { expr, subquery, negated }`. If not, it falls through to the
normal `IN (val1, val2, ...)` list form.

```
parse_predicate():
  lhs = parse_addition()
  if peek = Not:
    advance; expect(In); negated = true
  else if peek = In:
    advance; negated = false
  else: return lhs  // comparison ops handled here too

  expect(LParen)
  if peek = Select:
    stmt = parse_select_stmt(); expect(RParen)
    return Expr::InSubquery { expr: lhs, subquery: stmt, negated }
  else:
    values = parse_expr_list(); expect(RParen)
    return Expr::InList { expr: lhs, values, negated }
```

### EXISTS / NOT EXISTS — `parse_not`

`parse_not` handles unary `NOT`. When the parser sees `Exists` (or `Not Exists`),
it consumes the token, expects `LParen`, recursively parses a `select_stmt`, and
returns `Expr::Exists { subquery, negated }`. The result is always boolean — the
SELECT list contents are irrelevant at the execution level.

```
parse_not():
  if peek = Not:
    advance
    if peek = Exists:
      advance; expect(LParen); stmt = parse_select_stmt(); expect(RParen)
      return Expr::Exists { subquery: stmt, negated: true }
    else:
      return Expr::Not(parse_is_null())
  if peek = Exists:
    advance; expect(LParen); stmt = parse_select_stmt(); expect(RParen)
    return Expr::Exists { subquery: stmt, negated: false }
  return parse_is_null()
```

### Derived Tables — `parse_table_ref`

`parse_table_ref` parses the `FROM` clause. When it encounters `LParen` (without
a prior identifier), it recursively parses a `select_stmt`, expects `RParen`, and
then requires an `AS alias` clause (the alias is mandatory for derived tables):

```
parse_table_ref():
  if peek = LParen:
    advance; stmt = parse_select_stmt(); expect(RParen)
    expect(As); alias = parse_ident()
    return TableRef::Derived { subquery: stmt, alias }
  else:
    name = parse_ident(); alias = optional AS ident
    return TableRef::Named { name, alias }
```

### AST Nodes for Subqueries

```rust
pub enum Expr {
    // A scalar subquery — returns one value (or NULL if no rows)
    Subquery(Box<SelectStmt>),

    // IN (SELECT ...) or NOT IN (SELECT ...)
    InSubquery {
        expr:     Box<Expr>,
        subquery: Box<SelectStmt>,
        negated:  bool,
    },

    // EXISTS (SELECT ...) or NOT EXISTS (SELECT ...)
    Exists {
        subquery: Box<SelectStmt>,
        negated:  bool,
    },

    // Outer column reference (used inside correlated subqueries)
    OuterColumn {
        col_idx: usize,
        depth:   u32,    // 1 = immediate outer query
    },

    // ... other variants unchanged
}

pub enum TableRef {
    Named   { name: String, alias: Option<String> },
    Derived { subquery: Box<SelectStmt>, alias: String },
}
```

### Correlated Column Resolution — Semantic Analyzer

Correlated subqueries introduce `Expr::OuterColumn` during semantic analysis
(`analyze()`), not during parsing. The semantic analyzer maintains a stack of
`BindContext` frames, one per query level. When a column reference inside a
subquery cannot be resolved against the inner context, the analyzer walks up the
stack and resolves it against the outer context, replacing the `Expr::Column`
with `Expr::OuterColumn { col_idx, depth: 1 }`.

This means the parser always produces `Expr::Column` for every column reference;
`OuterColumn` only appears in the analyzed AST, never in the raw parse output.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Parse-Time vs Analyze-Time Correlation</span>
Correlation detection is deferred to the semantic analyzer rather than the parser. The parser always emits <code>Expr::Column</code> for every column reference, regardless of nesting depth. This keeps the parser stateless and context-free. The semantic analyzer's <code>BindContext</code> stack then resolves ambiguity with full schema knowledge. This is the same split used by PostgreSQL's parser/analyzer boundary: the parser builds a syntactic tree; the analyzer attaches semantic meaning (column indices, correlated references, type information).
</div>
</div>

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

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Zero-Copy Token Design</span>
Identifiers are <code>&'src str</code> slices into the original SQL string — no heap allocation during lexing. The Rust lifetime <code>'src</code> enforces at compile time that tokens cannot outlive the input. Only <code>StringLit</code> allocates, because escape processing (<code>\'</code>, <code>\\</code>, <code>\n</code>) must transform the content in place.
</div>
</div>
