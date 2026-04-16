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

Unquoted identifiers and backtick-quoted identifiers are represented as `&'src str`
— slices into the original SQL string. No heap allocation occurs during lexing for
those identifier classes.

`StringLit` allocates a `String`, because escape sequence processing (`\'`, `\\`,
`\n`) transforms the content in place and cannot be zero-copy. `DqIdent` also
allocates because `ANSI_QUOTES` mode uses doubled quotes (`""`) inside the
identifier and the lexer resolves a raw `"..."` fragment into either
`StringLit` or `DqIdent(String)` after scanning.

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
    Create, Database, Databases, Table, Index, Drop, Alter, Add, Column, Constraint,
    // Transaction keywords
    Begin, Commit, Rollback, Savepoint, Release,
    // Session / introspection
    Use,
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
    RawDoubleQuoted(&'src str), // raw "..." fragment, resolved after scanning
    DqIdent(String),            // ANSI_QUOTES-delimited identifier
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

### Session-Aware Double Quotes

The lexer is intentionally split into two steps for `"..."` fragments:

1. logos scans the raw bytes into `Token::RawDoubleQuoted(&str)`
2. `tokenize_with_sql_mode(..., SqlModeFlags)` resolves that fragment into:
   - `Token::StringLit(String)` when `ANSI_QUOTES` is OFF
   - `Token::DqIdent(String)` when `ANSI_QUOTES` is ON

This keeps the DFA stable while still matching MySQL/MariaDB session semantics.
The public parser entry points `parse_with_sql_mode(...)` and
`parse_expr_only_with_sql_mode(...)` thread the flag from `SessionContext` into
the parser.

The same `ansi_quotes` bit is also consumed by the raw-SQL scanner in the wire
layer. Parameter counting, SQL-string substitution, and multi-statement splitting
all use the same quote-mode decision, so `?` and `;` inside `"..."` are ignored
only when the current session treats those bytes as a string literal.

MySQL variable prefixes are tokenized explicitly as `Token::At` / `Token::AtAt`
instead of being folded into `Ident`. That keeps `SET @@session.autocommit = 1`
parsing fast and avoids widening the hot identifier path just to special-case
wire/session variables.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — MariaDB-Style Session Parse Mode</span>
AxiomDB follows MariaDB/MySQL here: <code>ANSI_QUOTES</code> changes expression
parsing for the current session, not just DDL identifier handling. The lexer keeps
one raw token for <code>"..."</code> and resolves it after scanning so the hot path
for normal identifiers stays zero-copy instead of widening <code>Ident</code> or
adding a second full lexer.
</div>
</div>

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
     | create_database_stmt | drop_database_stmt | use_stmt
     | create_table_stmt | create_index_stmt
     | drop_table_stmt | drop_index_stmt
     | alter_table_stmt | truncate_stmt
     | show_tables_stmt | show_databases_stmt | show_columns_stmt
     | begin_stmt | commit_stmt | rollback_stmt | savepoint_stmt

create_database_stmt →
  CREATE DATABASE ident

drop_database_stmt →
  DROP DATABASE [IF EXISTS] ident

use_stmt →
  USE ident

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

show_databases_stmt →
  SHOW DATABASES

show_columns_stmt →
  SHOW COLUMNS FROM ident
  | DESCRIBE ident
  | DESC ident
```

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — No Half Grammar</span>
AxiomDB now parses <code>CREATE/DROP DATABASE</code>, <code>USE</code>, and
<code>SHOW DATABASES</code>, but it still rejects <code>database.schema.table</code>.
MySQL allows a database qualifier directly in table references; AxiomDB intentionally
deferred that grammar until the analyzer and executor can honor it end-to-end instead
of shipping a misleading parser-only approximation.
</div>
</div>

### SHOW / DESCRIBE Parsing

`SHOW` is a dedicated keyword (`Token::Show`). After consuming it, the parser
peeks at the next token to dispatch:

```
parse_show():
  consume Show
  if peek = Databases:
    advance
    return Stmt::ShowDatabases(ShowDatabasesStmt)
  if peek = Ident("TABLES") | Ident("tables"):   // COLUMNS is not a reserved keyword
    advance
    schema = if eat(From): parse_ident() else "public"
    return Stmt::ShowTables(ShowTablesStmt { schema })
  if peek = Ident("COLUMNS") | Ident("columns"):
    advance; expect(From); table = parse_ident()
    return Stmt::ShowColumns(ShowColumnsStmt { table_name: table })
  else:
    return Err(ParseError { "expected TABLES, DATABASES, or COLUMNS after SHOW" })
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

## Scalar Function Evaluator (`eval/`)

The expression evaluator now lives under `crates/axiomdb-sql/src/eval/`, rooted
at `eval/mod.rs`. The facade keeps the same exported surface (`eval`,
`eval_with`, `eval_in_session`, `eval_with_in_session`, `is_truthy`,
`like_match`, `CollationGuard`, `SubqueryRunner`), but the implementation is
split by responsibility:

- `context.rs` — thread-local session collation, `CollationGuard`, and
  `SubqueryRunner`
- `core.rs` — recursive `Expr` evaluation, CASE dispatch, and subquery-aware paths
- `ops.rs` — boolean logic, comparisons, `IN`, `LIKE`, and truthiness helpers
- `functions/` — built-ins grouped by family (`system`, `nulls`, `numeric`,
  `string`, `datetime`, `binary`, `uuid`, `json`)

Built-in function dispatch still happens by lowercased name inside
`functions/mod.rs`. The registry remains a single `match` arm: no hash map and
no dynamic dispatch.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Split Without Semantic Drift</span>
Like PostgreSQL's separation between expression evaluation helpers and executor nodes,
AxiomDB now splits evaluator internals by responsibility while keeping the same public
entrypoints and static built-in dispatch. The payoff is lower maintenance cost without
adding virtual dispatch or a mutable function registry.
</div>
</div>

### JSON Functions and `->>` (11.4)

The lexer recognizes `JSON` as a DDL type token and `->>` as
`Token::JsonExtractText`. The expression parser treats `->>` as a high-precedence
left-associative extraction operator and lowers it immediately to a scalar
function call:

```rust
data->>'name'
// becomes
Expr::Function {
    name: "json_extract".into(),
    args: vec![
        Expr::Column { name: "data".into(), col_idx: 0 },
        Expr::Literal(Value::Text("$.name".into())),
    ],
}
```

Lowering avoids adding a new `BinaryOp` variant and reuses the existing function
dispatch path in `eval/functions/json.rs`. That module implements
`JSON_EXTRACT`, `JSON_SET`, `JSON_REMOVE`, `JSON_KEYS`, `JSON_VALID`, and
`JSON_TYPE` with `serde_json` and simple dot-path traversal.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Operator Lowering</span>
PostgreSQL keeps <code>->></code> as a JSON operator in its expression operator catalog; AxiomDB lowers it to <code>JSON_EXTRACT</code> during parsing so the evaluator, analyzer, and wire path share one implementation for both MySQL-style and PostgreSQL-style syntax.
</div>
</div>

### Date / Time Functions (4.19d)

Four internal helpers drive the MySQL-compatible date functions:

```rust
// Converts Value::Timestamp(micros_since_epoch) to NaiveDateTime.
// Uses Euclidean division for correct sub-second handling of pre-epoch timestamps.
fn micros_to_ndt(micros: i64) -> NaiveDateTime

// Converts Value::Date(days_since_epoch) to NaiveDate.
fn days_to_ndate(days: i32) -> NaiveDate

// Formats NaiveDateTime using MySQL-style format specifiers.
// Maps specifiers manually — NOT via chrono's format strings — to guarantee
// exact MySQL semantics (e.g. chrono's %m has different behavior).
fn date_format_str(ndt: NaiveDateTime, fmt: &str) -> String

// Parses a string into NaiveDateTime + a has_time flag.
// Returns None on any failure (caller maps to Value::Null).
fn str_to_date_inner(s: &str, fmt: &str) -> Option<(NaiveDateTime, bool)>
```

**`DATE_FORMAT` arm** — evaluates both args, dispatches `ts` on type:

```
ts: Timestamp(micros) → micros_to_ndt → NaiveDateTime
ts: Date(days)        → days_to_ndate → NaiveDate.and_time(MIN) → NaiveDateTime
ts: Text(s)           → try "%Y-%m-%d %H:%i:%s" then "%Y-%m-%d" via str_to_date_inner
ts: NULL              → return NULL immediately
```

**`STR_TO_DATE` arm** — calls `str_to_date_inner` and converts back to a Value:

```
has_time = true  → Value::Timestamp((ndt - epoch).num_microseconds())
has_time = false → Value::Date((ndt.date() - epoch).num_days() as i32)
failure          → Value::Null
```

The epoch used for both conversions is always `NaiveDate(1970-01-01) 00:00:00`
constructed with `from_ymd_opt(1970,1,1).unwrap().and_hms_opt(0,0,0).unwrap()`.
This avoids any `DateTime<Utc>` and is stable across all chrono 0.4.x versions.

**`str_to_date_inner`** processes the format string character by character:

- Literal characters: must match verbatim in the input (returns `None` on mismatch).
- `%Y`: consume exactly 4 digits.
- `%y`: consume 1–2 digits; apply MySQL 2-digit rule (`<70 → +2000`, else `+1900`).
- `%m`, `%c`, `%d`, `%e`, `%H`, `%h`, `%i`, `%s`/`%S`: consume 1–2 digits.
- Unknown specifier: skip one character in the input string.
- After parsing: validate with `NaiveDate::from_ymd_opt` + `NaiveTime::from_hms_opt`
  (catches invalid dates such as Feb 30).

**`take_digits(s, max)`** — helper used by the parser:

```rust
fn take_digits(s: &str, max: usize) -> Option<(u32, &str)> {
    let n = s.bytes().take(max).take_while(|b| b.is_ascii_digit()).count();
    if n == 0 { return None; }
    let val: u32 = s[..n].parse().ok()?;
    Some((val, &s[n..]))
}
```

Uses byte positions (safe for all ASCII date strings) and avoids allocations.

---

## GROUP_CONCAT Parsing

`GROUP_CONCAT` cannot be represented as a plain `Expr::Function { args: Vec<Expr> }` because
its interior grammar — `[DISTINCT] expr [ORDER BY ...] [SEPARATOR 'str']` — is not a
standard argument list. It gets its own AST variant and a dedicated parser branch.

### The `Expr::GroupConcat` Variant

```rust
pub enum Expr {
    // ...
    GroupConcat {
        expr: Box<Expr>,
        distinct: bool,
        order_by: Vec<(Expr, SortOrder)>,
        separator: String,          // defaults to ","
    },
}
```

The variant stores the sub-expression to concatenate, the deduplication flag, an ordered
list of `(sort_key_expr, direction)` pairs, and the separator string.

### `Token::Separator` — Disambiguating the Keyword

`SEPARATOR` is not a reserved word in standard SQL, so the lexer could produce either
`Token::Ident("SEPARATOR")` or a dedicated `Token::Separator`. AxiomDB uses the
dedicated token so that the ORDER BY loop inside `parse_group_concat` can stop cleanly:

```rust
// In the ORDER BY loop — stop if we see SEPARATOR or closing paren
if matches!(p.peek(), Token::Separator | Token::RParen) {
    break;
}
```

Without the dedicated token, the parser would need to look ahead through a comma and an
identifier to decide whether the comma ends the ORDER BY clause or separates two sort
keys.

### `parse_group_concat` — The Parser Branch

Invoked when `parse_ident_or_call` encounters `group_concat` (case-insensitive):

```
parse_group_concat:
  consume '('
  if DISTINCT: set distinct=true, advance
  parse_expr() → sub-expression
  if ORDER BY:
    loop:
      parse_expr() → sort key
      optional ASC|DESC → direction
      if peek == SEPARATOR or RParen: break
      else: consume ','
  if SEPARATOR:
    consume SEPARATOR
    consume StringLit(s) → separator string
  consume ')'
  return Expr::GroupConcat { expr, distinct, order_by, separator }
```

### `string_agg` — PostgreSQL Alias

`string_agg(expr, separator_literal)` is parsed in the same branch with simplified
logic: two arguments separated by a comma, the second being a string literal that
becomes the `separator` field. `distinct` is `false` and `order_by` is empty.

```sql
-- These are equivalent:
SELECT GROUP_CONCAT(name SEPARATOR ', ')   FROM t;
SELECT string_agg(name, ', ')              FROM t;
```

### Aggregate Execution in the Executor

At execution time, `Expr::GroupConcat` is handled by an `AggAccumulator::GroupConcat`
variant. Each row accumulates `(value_string, sort_key_values)`. At finalize:

1. Sort by the `order_by` key vector using `compare_values_null_last` — a type-aware
   comparator that sorts integers numerically and text lexicographically.
2. If `DISTINCT`: deduplicate by value string.
3. Join with separator, truncate at 1 MB.
4. Return `Value::Null` if no non-NULL values were accumulated.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Dedicated AST Variant</span>
MySQL's <code>GROUP_CONCAT</code> syntax is structurally different from a regular function call:
it embeds its own <code>ORDER BY</code> and uses a keyword (<code>SEPARATOR</code>) as a
positional argument delimiter. Forcing it into <code>Expr::Function { args }</code> would
require post-parse AST surgery to extract the separator and ORDER BY. A dedicated variant
keeps parsing and execution logic clean and makes semantic analysis and partial-index rejection straightforward.
</div>
</div>

---

## Error Reporting

### ParseError — structured position field

Parse errors carry a dedicated `position` field (0-based byte offset of the unexpected token):

```rust
DbError::ParseError {
    message: "SQL syntax error: unexpected token 'FORM'".to_string(),
    position: Some(9),   // byte 9 in "SELECT * FORM t"
}
```

The position field is populated from `SpannedToken::span.start` at every error site in the parser.
Non-parser code that constructs `ParseError` (e.g. codec validation, catalog checks) sets `position: None`.

### Visual snippet in MySQL ERR packets

When the MySQL handler sends an ERR packet for a parse error, it builds a 2-line visual snippet:

```
You have an error in your SQL syntax: unexpected token 'FORM'
SELECT * FORM t
         ^
```

The snippet is generated by `build_error_snippet(sql, pos)` in `mysql/error.rs`:

1. Find the line containing `pos` (`line_start` = last `\n` before `pos`, `line_end` = next `\n`).
2. Clamp the line to 120 characters to avoid overwhelming terminal output.
3. Compute `col = pos - line_start` and emit `" ".repeat(col) + "^"` on the second line.

The snippet is appended only when `sql` is available (COM_QUERY path). Prepared statement
execution errors (`COM_STMT_EXECUTE`) receive only the plain message.

### JSON error format

When `error_format = 'json'` is active on the connection, the MySQL ERR packet message is
replaced with a JSON string carrying the full `ErrorResponse`:

```json
{"code":1064,"sqlstate":"42601","severity":"ERROR","message":"SQL syntax error: unexpected token 'FORM'","position":9}
```

The JSON is built by `build_json_error(e, sql)` in `mysql/json_error.rs`. It uses the
`ErrorResponse::from_error(e)` struct for clean, snippet-free fields (the visual snippet is
text-protocol-only). The `JsonErrorPayload` struct lives in `axiomdb-network` to avoid
adding `serde` as a dependency to `axiomdb-core`.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — serde Boundary</span>
<code>axiomdb-core</code> defines <code>DbError</code> and <code>ErrorResponse</code> with no
<code>serde</code> dependency. The JSON payload is assembled in <code>axiomdb-network</code> using
a private <code>#[derive(Serialize)] JsonErrorPayload</code> struct. This keeps the core crate
free of serialization complexity and means error types never accidentally get serialized
somewhere they shouldn't.
</div>
</div>

Lexer errors (invalid characters, unterminated string literals) include the byte span
of the problematic token via the same `position` field.

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

---

## Phase 11.20d3 — LATERAL-correlated JSON_TABLE

Lifts the final LATERAL guardrail: JSON_TABLE on the right side of a
JOIN / CROSS APPLY / OUTER APPLY may now reference outer columns in
both its `doc` argument and any `PASSING expr AS var` bindings. The
PG-compatible `LATERAL` keyword is also accepted as an optional no-op
prefix.

### Detection

```rust
pub fn jsontable_is_correlated(jt: &JsonTable) -> bool {
    doc_has_column_refs(&jt.doc)
        || jt.passing.iter().any(|(expr, _)| doc_has_column_refs(expr))
}
```

Used by both `select_joins_ctx.rs` (to route correlated right sources
into a per-outer-row loop) and by `execute_select_json_table_source`
(to reject correlated `doc` in first-FROM position — there is no
outer source to reference).

### Analyzer

Join-side `FromClause::JsonTable` now flows through
`resolve_json_table(&ctx, outer_scopes, …)` during join iteration.
Previously only first-FROM JSON_TABLE was resolved; join-side JT
doc/passing exprs stayed raw, which accidentally worked for 11.20d1
because PASSING values were literals. `resolve_json_table` itself was
extended to resolve each `jt.passing.iter_mut()` expression against
the same scope as `jt.doc`.

### Executor

`execute_select_with_joins_first_materialized` adds a parallel
tracker `correlated_jt: Vec<Option<JsonTableSpec>>`. Non-correlated
right sources push `None` and materialize once into
`scanned[right_idx]` exactly as before. Correlated sources push
`Some(spec)` and a placeholder `Vec::new()`; during the combine loop
the join dispatches to `apply_correlated_jt_join(left_rows, jt_ast,
spec, …)` instead of `apply_join(…, &scanned[right_idx], …)`.

The per-outer-row helper evaluates `doc` against each outer row,
converts via `doc_to_serde`, materializes with
`materialize_json_table(spec, &sj, outer, &mut NoSubquery)` (the env
builder has been per-invocation since 11.20d1), and then tests the
ON condition per right row. LEFT JOIN / OUTER APPLY NULL-pad when no
rows match; RIGHT JOIN / FULL JOIN raise `NotImplemented` because
outer re-scan semantics are ill-defined (PG rejects them too).

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Hash/spill skipped on correlated JT</span>
The adaptive join selection that picks hash-join and spill-to-disk
(Phase 9.8 / 9.9) requires the full right set pre-built. A correlated
JT right side is fundamentally a different shape — one materialization
per outer row — so the correlated branch always runs nested-loop.
Since the outer loop was already O(|outer|), and JT cardinality per
outer row is bounded by the doc structure, this is a deliberate
limitation rather than a regression.
</div>
</div>

### LATERAL keyword

`Token::Lateral` is consumed optionally at the top of `parse_from_item`,
covering all LATERAL forms:

```
FROM LATERAL JSON_TABLE(...)            -- first-FROM sugar (no-op on JT)
FROM t JOIN LATERAL JSON_TABLE(...) ...  -- PG form
FROM LATERAL (SELECT ...) AS q          -- subquery, no outer context
FROM t, LATERAL (SELECT t.id + 1 AS x) sub  -- comma = CROSS JOIN LATERAL
FROM t JOIN LATERAL (...) sub ON true    -- explicit INNER
FROM t LEFT JOIN LATERAL (...) sub ON true  -- LEFT, null-pads when empty
```

When `lateral=true` is set on a `FromClause::Subquery`, the semantic analyzer
(`analyzer_stmt.rs` for SELECT, `analyzer_ddl.rs` for UPDATE/DELETE) passes the
accumulated `BindContext` as an outer scope to `analyze_select_with_outer`. This
causes column references inside the subquery (e.g. `t.id`) to be resolved as
`Expr::OuterColumn { col_idx, .. }` nodes instead of raising "column not found".

At execution time (`select_joins_ctx.rs`), each join is classified as correlated
or non-correlated:

```rust
let is_correlated = lateral
    && subquery_is_correlated(&query, effective_left_cols);
```

where `effective_left_cols` accumulates columns from all materialized sources
including earlier LATERAL subqueries in the same FROM (`lateral_accum_cols`).
This enables chains like `FROM t, LATERAL (...) s1, LATERAL (... s1.a ...) s2`.

**Correlated path**: the AST is stored in `correlated_sub[right_idx]`; schema
is inferred from the SELECT list (aliases → fallback `colN`). In the combine
loop, `apply_correlated_subquery_join` runs `substitute_outer(subquery, outer_row)`
+ `execute_select_ctx` once per outer row. LEFT JOIN null-pads when the subquery
returns no rows. RIGHT/FULL LATERAL → `NotImplemented` (PG-compatible — re-scan
semantics ill-defined).

**Non-correlated LATERAL**: materialized once with empty scope, then used
identically to a regular derived table — zero overhead vs a plain subquery.

The same `apply_correlated_subquery_dml_join` function handles UPDATE/DELETE JOINs.
`analyze_update`/`analyze_delete` were extended with the same outer-scope injection
so that LATERAL subqueries in DML joins work identically to SELECT.

## Phase 11.20d2 — JSON_TABLE first FROM + CROSS/OUTER APPLY

Two small additions layered on the existing join infrastructure:

```
join_item := 'CROSS' 'APPLY' from_item
           | 'OUTER' 'APPLY' from_item
           | (existing INNER | LEFT | RIGHT | FULL | CROSS) 'JOIN' from_item join_condition
```

`parse_join_clauses` disambiguates the `CROSS` keyword by peeking the next
token: if `APPLY`, it desugars to `JoinType::Inner` with `ON TRUE`; if
`JOIN`, it falls through to the existing `CROSS JOIN` arm. `OUTER` at the
top level of the join match only triggers when followed by `APPLY` —
`LEFT / RIGHT / FULL [OUTER] JOIN` consume `OUTER` inside their own arms
first, so there is no grammar collision. `APPLY` accepts any `from_item`
(table, subquery, `JSON_TABLE(...)`); `ON` or `USING` after `APPLY` is an
explicit parse error because the join condition is implicit `TRUE`.

CROSS/OUTER APPLY desugars at parse time — no new `JoinType` variants
are introduced. Surface-form round-trip is not preserved.

Executor side: `execute_select_with_joins_ctx` is split into a thin
wrapper that resolves the base table and a reusable helper
`execute_select_with_joins_first_materialized(stmt, first_source,
first_rows, exec_ctx, conn_txn, ctx)` that owns the nested-loop join
pipeline. `execute_select_json_table_source` materializes JSON_TABLE
as source 0 and delegates to the same helper with a temp
`ExecutionContext` / `SessionContext` (the same pattern
`execute_select_derived` uses for subquery-first FROM).

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">APPLY as pure desugar, not a new join type</span>
SQL Server, Oracle, and Sybase all expose <code>CROSS APPLY</code> /
<code>OUTER APPLY</code>; PostgreSQL spells the same concept as
<code>JOIN LATERAL ... ON TRUE</code>. Because the non-correlated case
is semantically identical to <code>JOIN ... ON TRUE</code>, AxiomDB
parses APPLY directly into that shape. The join loop, projection
binder, and EXPLAIN output all see a standard <code>InnerJoin</code>
or <code>LeftJoin</code> — no new code paths downstream, and the
LATERAL correlation work in 11.20d3 can specialize the same join
node without reworking APPLY grammar.
</div>
</div>

## Phase 11.20a–d1 — `JSON_TABLE` grammar

```
table_factor := ... | json_table_call

json_table_call := JSON_TABLE '(' expr ',' string_literal
                     [ 'PASSING' passing_item (',' passing_item)* ]
                     COLUMNS '(' column_def (',' column_def)* ')'
                   ')' [ [AS] ident ]

passing_item := expr 'AS' ident

column_def := ident type PATH string_literal
                     [ wrapper_clause ]
                     [ quotes_clause ]
                     [ on_behavior 'ON' 'EMPTY' ]
                     [ on_behavior 'ON' 'ERROR' ]
            | ident 'FOR' 'ORDINALITY'
            | ident type 'EXISTS' 'PATH' string_literal
                     [ exists_on_error ]
            | 'NESTED' [ 'PATH' ] string_literal
                     'COLUMNS' '(' column_def (',' column_def)* ')'

wrapper_clause  := 'WITH' ['CONDITIONAL' | 'UNCONDITIONAL'] ['ARRAY'] 'WRAPPER'
                 | 'WITHOUT' ['ARRAY'] 'WRAPPER'
quotes_clause   := ('KEEP' | 'OMIT') 'QUOTES' ['ON' 'SCALAR' 'STRING']
on_behavior     := 'NULL' | 'ERROR' | 'DEFAULT' expr
exists_on_error := ('TRUE' | 'FALSE' | 'UNKNOWN' | 'ERROR') 'ON' 'ERROR'
```

Phase 11.20b/c accept arbitrary `NESTED` depth (bounded defensively to 32).
Phase 11.20d1 adds `PASSING` and the per-column `WRAPPER` / `QUOTES` clauses.
The `WRAPPER`/`QUOTES` grammar is parsed by
`parser::sql_json_common::{parse_optional_wrapper, parse_optional_quotes}`,
the same helpers `JSON_QUERY` uses. `OMIT QUOTES` on a non-TEXT column is
a parse-time error, as is a duplicate `PASSING` variable name.

### Dispatch

`parse_from_item` peeks for the case-insensitive identifier `JSON_TABLE` followed
by `(`. If present it delegates to `parser::json_table::parse_json_table_call`.
If `JSON_TABLE` appears without an opening `(`, the parser falls through to the
standard table-ref path — a user table named `json_table` still resolves normally.

### Compilation

The analyzer resolves column references inside the `doc` expression and any
`DEFAULT` expressions in ON EMPTY / ON ERROR clauses against the outer
`BindContext` (see `analyzer_stmt.rs::resolve_json_table`). The row path and
every column `PATH` string are compiled via the full
`eval::functions::json::parse_jsonpath` engine (11.20d1 migrated away from the
legacy restricted walker) into `Vec<PathStep>` once per statement in
`json_table::compile_json_table`. Row emission walks the pre-compiled step
list via `execute_jsonpath_owned_env` — no per-row parsing, and filter
expressions / `.size()` / `.type()` / `$var` references all work
uniformly across SQL/JSON surfaces.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Unified JSONPath engine + recursive walk</span>
The executor keeps the MariaDB <code>json_table.cc</code> recursive-walk
model for row emission (no SiblingJoin plan node), but 11.20d1 retired the
restricted per-feature walker. Every path site (row path, column path,
NESTED path) now goes through the same <code>parse_jsonpath</code>
engine that powers <code>jsonb_path_*</code>, <code>@?</code>,
<code>@@</code>, and filter accessors. PASSING variables thread through
<code>PassingEnv</code> — a single <code>HashMap&lt;String,
serde_json::Value&gt;</code> built once per JSON_TABLE invocation and
shared by every filter evaluation at every nesting depth.
</div>
</div>
