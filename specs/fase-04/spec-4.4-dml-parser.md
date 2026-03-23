# Spec: 4.4 — DML Parser

## What to build (not how)

Extension of the expression parser and implementation of the DML statement
parsers: SELECT, INSERT, UPDATE, DELETE. This completes the SQL parser such
that the executor (Phase 4.5) has fully-typed AST nodes to work with.

---

## Expression parser extensions

The expression parser in `parser/expr.rs` is extended with:

### New levels in the precedence hierarchy (from lowest to highest)

```
expr           ::= or_expr
or_expr        ::= and_expr (OR and_expr)*
and_expr       ::= not_expr (AND not_expr)*
not_expr       ::= NOT not_expr | is_null_expr
is_null_expr   ::= predicate (IS [NOT] NULL)?       ← NEW
predicate      ::= addition               ← NEW: BETWEEN, LIKE, IN dispatched here
  | addition [NOT] BETWEEN addition AND addition
  | addition [NOT] LIKE atom [ESCAPE atom]
  | addition [NOT] IN '(' expr (',' expr)* ')'
  | addition (cmp_op addition)?           ← existing comparisons promoted here
addition       ::= multiplication (('+' | '-' | '||') multiplication)*  ← NEW
multiplication ::= unary (('*' | '/' | '%') unary)*  ← NEW
unary          ::= '-' unary | NOT unary | atom
atom           ::= literal | col_ref | fn_call | '(' expr ')'
col_ref        ::= identifier ['.' identifier]       ← NEW: table.column
fn_call        ::= identifier '(' ([*] | [expr (',' expr)*]) ')'  ← NEW
```

### `Expr::IsNull` and `Expr::Between` and `Expr::Like` and `Expr::In`

These already exist in the `Expr` enum (Phase 4.17). The parser now produces them:

- `age IS NULL` → `Expr::IsNull { expr: col("age"), negated: false }`
- `age IS NOT NULL` → `Expr::IsNull { expr: col("age"), negated: true }`
- `age BETWEEN 18 AND 65` → `Expr::Between { expr, low, high, negated: false }`
- `age NOT BETWEEN 18 AND 65` → `Expr::Between { ..., negated: true }`
- `name LIKE 'A%'` → `Expr::Like { expr, pattern, negated: false }`
- `name NOT LIKE 'A%'` → `Expr::Like { ..., negated: true }`
- `id IN (1, 2, 3)` → `Expr::In { expr, list: [...], negated: false }`
- `id NOT IN (1, 2, 3)` → `Expr::In { ..., negated: true }`

### Table-qualified column references

`table.column` → `Expr::Column { col_idx: 0, name: "table.column" }`

The full name `"table.column"` is stored; the semantic analyzer (Phase 4.18)
resolves it to the correct column index. A single identifier `column` →
`Expr::Column { col_idx: 0, name: "column" }`.

### Function calls

`fn_name(arg1, arg2)` → `Expr::Function { name: "fn_name", args: [arg1, arg2] }`

`COUNT(*)` → `Expr::Function { name: "count", args: vec![] }` (empty args
represents `f(*)`; `f()` is also empty args; executor distinguishes in 4.9c).

---

## SELECT statement

```
SELECT [DISTINCT]
  select_list
  [FROM from_item [join_clause*]]
  [WHERE expr]
  [GROUP BY expr (',' expr)*]
  [HAVING expr]
  [ORDER BY order_item (',' order_item)*]
  [LIMIT expr [OFFSET expr]]
```

### SELECT list items

```
select_list  ::= select_item (',' select_item)*
select_item  ::=
    '*'                           → SelectItem::Wildcard
  | identifier '.' '*'           → SelectItem::QualifiedWildcard(table_name)
  | expr [AS identifier]         → SelectItem::Expr { expr, alias }
```

The `AS` keyword is required for column aliases in SELECT list.
Implicit alias (`SELECT id myalias`) is NOT supported in Phase 4.4.

### FROM clause

```
from_item ::=
    table_ref [AS? identifier]   → FromClause::Table(TableRef { alias })
  | '(' select_stmt ')' AS identifier  → FromClause::Subquery { query, alias }
```

Both `FROM t AS alias` and `FROM t alias` (implicit alias) are accepted.

### JOIN clauses

```
join_clause ::=
  join_type JOIN from_item join_condition

join_type ::= INNER | LEFT [OUTER] | RIGHT [OUTER] | FULL [OUTER] | CROSS | ε (= INNER)

join_condition ::= ON expr | USING '(' ident_list ')'
```

### ORDER BY

```
order_item ::= expr [ASC | DESC] [NULLS (FIRST | LAST)]
```

### LIMIT / OFFSET

```
LIMIT expr [OFFSET expr]
```

MySQL `LIMIT n, m` (offset, count) is DEFERRED.

### SELECT without FROM (4.5a prerequisite)

`SELECT 1`, `SELECT NOW()`, `SELECT VERSION()` — `from` is `None`.
The parser sets `from: None` when no `FROM` keyword follows the select list.

---

## INSERT statement

```
INSERT INTO table_ref ['(' ident_list ')']
(
    VALUES '(' expr_list ')' (',' '(' expr_list ')')*
  | select_stmt
  | DEFAULT VALUES
)
```

---

## UPDATE statement

```
UPDATE table_ref
  SET identifier '=' expr (',' identifier '=' expr)*
  [WHERE expr]
```

---

## DELETE statement

```
DELETE FROM table_ref [WHERE expr]
```

---

## Inputs / Outputs

| Input | Output |
|---|---|
| `SELECT 1` | `Stmt::Select(SelectStmt { from: None, columns: [Expr::Literal(Int(1))], ... })` |
| `SELECT * FROM users WHERE id = 1` | `Stmt::Select(...)` |
| `SELECT a, b FROM t ORDER BY a DESC LIMIT 10` | `Stmt::Select(...)` |
| `INSERT INTO t (a, b) VALUES (1, 'x')` | `Stmt::Insert(...)` |
| `UPDATE t SET a = 1 WHERE id = 2` | `Stmt::Update(...)` |
| `DELETE FROM t WHERE id = 3` | `Stmt::Delete(...)` |

---

## Use cases

1. **SELECT without FROM**: `SELECT 1 + 1` → arithmetic in select list.

2. **SELECT with WHERE and comparison**: `SELECT * FROM users WHERE age > 18`.

3. **SELECT with IS NULL**: `SELECT * FROM t WHERE email IS NULL`.

4. **SELECT with BETWEEN**: `SELECT * FROM t WHERE age BETWEEN 18 AND 65`.

5. **SELECT with LIKE**: `SELECT * FROM t WHERE name LIKE 'Alice%'`.

6. **SELECT with IN**: `SELECT * FROM t WHERE id IN (1, 2, 3)`.

7. **SELECT with NOT IN**: `SELECT * FROM t WHERE id NOT IN (1, 2)`.

8. **SELECT DISTINCT**: `SELECT DISTINCT country FROM users`.

9. **SELECT with alias**: `SELECT id AS user_id, name AS user_name FROM users`.

10. **SELECT with qualified wildcard**: `SELECT users.* FROM users`.

11. **SELECT with table.column**: `SELECT u.id, o.total FROM users u JOIN orders o ON u.id = o.user_id`.

12. **SELECT with INNER JOIN**: full join including ON condition.

13. **SELECT with LEFT JOIN**.

14. **SELECT with GROUP BY + HAVING**: `SELECT country, COUNT(*) FROM users GROUP BY country HAVING COUNT(*) > 10`.

15. **SELECT with ORDER BY + NULLS LAST**: `SELECT * FROM t ORDER BY price DESC NULLS LAST`.

16. **SELECT with LIMIT + OFFSET**: `SELECT * FROM t LIMIT 20 OFFSET 40`.

17. **SELECT with subquery in FROM**: `SELECT * FROM (SELECT id FROM users WHERE active) AS sub`.

18. **INSERT VALUES single row**: `INSERT INTO t (a, b) VALUES (1, 'x')`.

19. **INSERT VALUES multiple rows**: `INSERT INTO t VALUES (1), (2), (3)`.

20. **INSERT DEFAULT VALUES**: `INSERT INTO t DEFAULT VALUES`.

21. **INSERT SELECT**: `INSERT INTO t2 SELECT * FROM t1 WHERE active = TRUE`.

22. **UPDATE single column**: `UPDATE users SET name = 'Alice' WHERE id = 1`.

23. **UPDATE multiple columns**: `UPDATE t SET a = 1, b = 2 WHERE id = 3`.

24. **DELETE with WHERE**: `DELETE FROM t WHERE id = 42`.

25. **DELETE without WHERE**: `DELETE FROM t` (deletes all rows).

26. **Arithmetic in expression**: `SELECT price * 1.1 AS with_tax FROM products`.

27. **Function call**: `SELECT COUNT(*), MAX(price) FROM products`.

28. **Nested expression**: `WHERE (a > 1 AND b < 10) OR c IS NULL`.

---

## Acceptance criteria

- [ ] `parse("SELECT 1", None)` → `Stmt::Select` with `from: None`
- [ ] `SELECT *` → `SelectItem::Wildcard`
- [ ] `SELECT t.*` → `SelectItem::QualifiedWildcard("t")`
- [ ] `SELECT expr AS alias` → `SelectItem::Expr { alias: Some("alias") }`
- [ ] `SELECT DISTINCT` sets `distinct: true`
- [ ] `FROM table` → `FromClause::Table`
- [ ] `FROM table AS alias` → `TableRef { alias: Some("alias") }`
- [ ] `FROM table alias` (implicit alias) → `TableRef { alias: Some("alias") }`
- [ ] `FROM (SELECT ...) AS sub` → `FromClause::Subquery`
- [ ] `INNER JOIN ... ON expr` → `JoinClause { join_type: Inner, condition: On(expr) }`
- [ ] `LEFT JOIN` → `JoinType::Left`
- [ ] `LEFT OUTER JOIN` → same as LEFT JOIN
- [ ] `CROSS JOIN` → `JoinType::Cross`
- [ ] `JOIN ... USING (col1, col2)` → `JoinCondition::Using`
- [ ] `WHERE expr` sets `where_clause: Some(expr)`
- [ ] `GROUP BY expr, expr` populates `group_by`
- [ ] `HAVING expr` sets `having`
- [ ] `ORDER BY expr ASC` → `SortOrder::Asc`
- [ ] `ORDER BY expr DESC NULLS LAST` → `SortOrder::Desc, nulls: Some(NullsOrder::Last)`
- [ ] `LIMIT n` sets `limit: Some(n)`
- [ ] `LIMIT n OFFSET m` sets both `limit` and `offset`
- [ ] `IS NULL` → `Expr::IsNull { negated: false }`
- [ ] `IS NOT NULL` → `Expr::IsNull { negated: true }`
- [ ] `BETWEEN a AND b` → `Expr::Between { negated: false }`
- [ ] `NOT BETWEEN a AND b` → `Expr::Between { negated: true }`
- [ ] `LIKE 'pattern'` → `Expr::Like { negated: false }`
- [ ] `NOT LIKE 'pattern'` → `Expr::Like { negated: true }`
- [ ] `IN (v1, v2)` → `Expr::In { negated: false, list: [...] }`
- [ ] `NOT IN (v1, v2)` → `Expr::In { negated: true }`
- [ ] `a + b`, `a - b`, `a * b`, `a / b`, `a % b` → `BinaryOp::Add` etc.
- [ ] `a || b` → `BinaryOp::Concat`
- [ ] `table.column` → `Expr::Column { name: "table.column" }`
- [ ] `fn(a, b)` → `Expr::Function { name: "fn", args: [a, b] }`
- [ ] `COUNT(*)` → `Expr::Function { name: "count", args: [] }`
- [ ] `INSERT INTO t (cols) VALUES (vals)` → `Stmt::Insert`
- [ ] `INSERT INTO t VALUES (v1), (v2)` → multi-row insert
- [ ] `INSERT INTO t DEFAULT VALUES` → `InsertSource::DefaultValues`
- [ ] `INSERT INTO t SELECT ...` → `InsertSource::Select`
- [ ] `UPDATE t SET col = expr WHERE ...` → `Stmt::Update`
- [ ] `DELETE FROM t WHERE expr` → `Stmt::Delete`
- [ ] `DELETE FROM t` (no WHERE) → `Stmt::Delete { where_clause: None }`
- [ ] No `unwrap()` in `src/`

---

## ⚠️ DEFERRED

- `LIMIT n, m` MySQL reversed syntax — Phase 4.x
- Implicit column alias without AS (`SELECT id myalias`) — Phase 4.x
- Scalar subqueries in expressions (`(SELECT MAX(id) FROM t)`) — Phase 4.11
- `CASE WHEN ... THEN ... END` — Phase 4.24
- `CAST(expr AS type)` — Phase 4.12b
- `EXISTS (subquery)` — Phase 4.11
- `SOME/ALL/ANY (subquery)` — Phase 4.x
- `UNION / INTERSECT / EXCEPT` — Phase 4.x
- `WITH (CTE)` — Phase 4.x

---

## Out of scope

- Semantic validation — Phase 4.18
- Execution — Phase 4.5
- DDL parsing — Phase 4.3 (done)

---

## Dependencies

- `axiomdb-sql/src/parser/expr.rs` — extended in-place
- `axiomdb-sql/src/parser/dml.rs` — fully implemented (was stub)
- `axiomdb-sql/src/ast.rs` — all types already defined in Phase 4.1
- `axiomdb-types`: `Value`, `DataType`
- No new Cargo.toml dependencies
