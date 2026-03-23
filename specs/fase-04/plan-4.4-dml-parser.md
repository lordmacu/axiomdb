# Plan: 4.4 — DML Parser

## Files to create / modify

| File | Action | Description |
|---|---|---|
| `crates/axiomdb-sql/src/parser/expr.rs` | MODIFY | Add arithmetic, IS NULL, BETWEEN, LIKE, IN, fn calls, table.col |
| `crates/axiomdb-sql/src/parser/dml.rs` | REWRITE | Full SELECT, INSERT, UPDATE, DELETE parsers |
| `crates/axiomdb-sql/tests/integration_dml_parser.rs` | CREATE | Integration tests |

No Cargo.toml changes needed.

---

## Algorithm — Expression parser extension

### New grammar levels inserted between existing ones

```
EXISTING:  not_expr → comparison → unary → atom
NEW:       not_expr → is_null_expr → predicate → addition → multiplication → unary → atom
```

Full updated chain:

```rust
pub(crate) fn parse_expr(p) → parse_or(p)

parse_or(p):
    left = parse_and(p)
    while eat(OR): right = parse_and; left = BinaryOp(Or, left, right)

parse_and(p):
    left = parse_not(p)
    while eat(AND): right = parse_not; left = BinaryOp(And, left, right)

parse_not(p):
    if eat(NOT): return UnaryOp(Not, parse_not(p))
    return parse_is_null(p)

parse_is_null(p):      ← NEW
    expr = parse_predicate(p)
    if eat(IS):
        negated = eat(NOT)
        expect(NULL)
        return IsNull { expr, negated }
    return expr

parse_predicate(p):    ← NEW (replaces old parse_comparison)
    left = parse_addition(p)
    negated = false
    if eat(NOT): negated = true

    match peek():
        BETWEEN → advance; low = parse_addition; expect(AND); high = parse_addition
                  → Between { expr: left, low, high, negated }
        LIKE    → advance; pattern = parse_atom
                  [if eat(ESCAPE): discard escape_char]
                  → Like { expr: left, pattern, negated }
        IN      → advance; expect(LParen)
                  items = [parse_expr, ...]
                  expect(RParen)
                  → In { expr: left, list: items, negated }
        cmp_op  → if negated: backtrack negated flag → error (NOT before comparison is handled by parse_not)
                  op = consume cmp_op; right = parse_addition
                  → BinaryOp(op, left, right)
        _       → if negated: error (NOT without BETWEEN/LIKE/IN)
                  return left

parse_addition(p):     ← NEW
    left = parse_multiplication(p)
    while peek() in {Plus, Minus, Concat}:
        op = consume; right = parse_multiplication
        left = BinaryOp(op, left, right)

parse_multiplication(p): ← NEW
    left = parse_unary(p)
    while peek() in {Star, Slash, Percent}:
        op = consume; right = parse_unary
        left = BinaryOp(op, left, right)

parse_unary(p):        ← existing, unchanged
    if eat(Minus): return UnaryOp(Neg, parse_unary)
    return parse_atom(p)

parse_atom(p):         ← extended
    match peek():
        Integer(n) → ...existing...
        Float(f)   → ...existing...
        StringLit  → ...existing...
        True/False/Null → ...existing...
        LParen → advance; expr = parse_expr; expect(RParen)
        Ident/QuotedIdent/DqIdent →
            name = parse_identifier
            if peek() == Dot:
                advance; field = parse_identifier
                name = format!("{name}.{field}")
                if peek() == LParen:
                    ERROR: three-part qualified fn call not supported
                return Column { col_idx: 0, name }
            if peek() == LParen:
                // function call
                advance  // consume LParen
                if eat(Star):
                    expect(RParen)
                    return Function { name: name.to_lowercase(), args: vec![] }
                args = []
                if peek() != RParen:
                    args.push(parse_expr)
                    while eat(Comma): args.push(parse_expr)
                expect(RParen)
                return Function { name: name.to_lowercase(), args }
            return Column { col_idx: 0, name }
```

### Handling `NOT` before `BETWEEN/LIKE/IN`

The grammar `a NOT BETWEEN b AND c` has `NOT` after the left operand. This is
tricky because `NOT` normally parses at a higher level (parse_not). The solution:
in `parse_predicate`, after parsing `left`, check if the next token is `NOT`.
If it is, consume it and set `negated = true`, then require the next token to be
BETWEEN, LIKE, or IN. If the next token after NOT is something else, this is a
parse error (NOT before a comparison operator is not valid syntax here — that
would be `NOT (a > b)` which is handled at the `parse_not` level).

---

## Algorithm — DML parsers

### parse_select

```rust
fn parse_select(p) → SelectStmt:
    distinct = eat(DISTINCT)
    columns = parse_select_list(p)
    from = if eat(FROM): Some(parse_from_item(p)) else None
    joins = if from.is_some(): parse_join_clauses(p) else vec![]
    where_clause = if eat(WHERE): Some(parse_expr(p)) else None
    group_by = if eat(GROUP) && expect(BY): parse_expr_list(p) else vec![]
    having = if eat(HAVING): Some(parse_expr(p)) else None
    order_by = if eat(ORDER) && expect(BY): parse_order_items(p) else vec![]
    (limit, offset) = parse_limit_offset(p)
    SelectStmt { distinct, columns, from, joins, where_clause, group_by, having, order_by, limit, offset }
```

### parse_select_list

```
item loop:
  if peek() == Star: advance → SelectItem::Wildcard
  else if peek() == Ident && peek_at(1) == Dot && peek_at(2) == Star:
    name = parse_identifier; advance (Dot); advance (Star) → QualifiedWildcard(name)
  else:
    expr = parse_expr
    alias = if eat(AS): Some(parse_identifier) else None
    → SelectItem::Expr { expr, alias }
  if !eat(Comma): break
```

### parse_from_item

```
if peek() == LParen:
    advance
    sub = parse_select(p)   ← recursive SELECT
    expect(RParen)
    expect(AS)
    alias = parse_identifier
    → FromClause::Subquery { query: Box::new(sub), alias }
else:
    table_ref = parse_table_ref
    // Optional alias: AS identifier | identifier (if next is not JOIN/WHERE/etc.)
    alias = if eat(AS): Some(parse_identifier)
            else if is_alias_token(peek()): Some(parse_identifier)
            else None
    table_ref.alias = alias
    → FromClause::Table(table_ref)

fn is_alias_token(tok) → bool:
    matches Ident | QuotedIdent | DqIdent  (but NOT keywords like WHERE, JOIN, ON, etc.)
```

### parse_join_clauses

```
loop:
    join_type = match peek():
        INNER → advance, expect(JOIN), Inner
        LEFT  → advance, eat(OUTER), expect(JOIN), Left
        RIGHT → advance, eat(OUTER), expect(JOIN), Right
        FULL  → advance, eat(OUTER), expect(JOIN), Full
        CROSS → advance, expect(JOIN), Cross
        JOIN  → advance, Inner   ← bare JOIN = INNER JOIN
        _     → break

    from_item = parse_from_item(p)
    condition = if eat(ON): JoinCondition::On(parse_expr)
                else if eat(USING): JoinCondition::Using(parse_ident_list_paren)
                else: error

    joins.push(JoinClause { join_type, table: from_item, condition })
```

### parse_order_items

```
items = [parse_order_item]
while eat(Comma): items.push(parse_order_item)

parse_order_item:
    expr = parse_expr
    order = if eat(ASC): Asc else if eat(DESC): Desc else Asc
    nulls = if eat(NULLS):
        if eat(FIRST): Some(NullsOrder::First)
        else if eat(LAST): Some(NullsOrder::Last)
        else error
    else None
    OrderByItem { expr, order, nulls }
```

### parse_limit_offset

```
if eat(LIMIT):
    limit = Some(parse_expr)
    offset = if eat(OFFSET): Some(parse_expr) else None
    (limit, offset)
else:
    (None, None)
```

### parse_insert

```
expect(INTO)
table = parse_table_ref
columns = if eat(LParen): Some(parse_ident_list_then_rparen) else None

source = match peek():
    VALUES → advance
        rows = [[parse_expr...]] (one or more paren-lists)
        InsertSource::Values(rows)
    DEFAULT → advance, expect(VALUES)
        InsertSource::DefaultValues
    SELECT → parse_select → InsertSource::Select(Box::new(select))
    _ → error
```

### parse_update

```
table = parse_table_ref
expect(SET)
assignments = [parse_assignment]
while eat(Comma): assignments.push(parse_assignment)
where_clause = if eat(WHERE): Some(parse_expr) else None

parse_assignment:
    col = parse_identifier
    expect(Eq)
    val = parse_expr
    Assignment { column: col, value: val }
```

### parse_delete

```
expect(FROM)
table = parse_table_ref
where_clause = if eat(WHERE): Some(parse_expr) else None
```

---

## Implementation phases

### Phase 1 — Extend expr.rs
1. Add `parse_is_null` between `parse_not` and `parse_predicate`.
2. Rename old `parse_comparison` to `parse_predicate`; add BETWEEN/LIKE/IN dispatch.
3. Add `parse_addition` and `parse_multiplication`.
4. Extend `parse_atom` with:
   - `table.column` references
   - Function calls (including `f(*)` for COUNT(*))
5. Update the call chain: `parse_not` → `parse_is_null` → `parse_predicate` → `parse_addition` → `parse_multiplication` → `parse_unary` → `parse_atom`.

### Phase 2 — Implement dml.rs
1. `parse_dml`: dispatch on SELECT/INSERT/UPDATE/DELETE tokens.
2. `parse_select`: full implementation.
3. `parse_select_list` + `parse_select_item`.
4. `parse_from_item` + subquery support.
5. `parse_join_clauses` + `parse_join_condition`.
6. `parse_order_items`.
7. `parse_limit_offset`.
8. `parse_insert` + `parse_insert_values`.
9. `parse_update` + `parse_assignment`.
10. `parse_delete`.

### Phase 3 — Integration tests
File: `crates/axiomdb-sql/tests/integration_dml_parser.rs`

---

## Tests to write

### Expression extensions
```
test_arithmetic_add_sub_mul_div
test_arithmetic_precedence (2 + 3 * 4 = 2 + (3*4))
test_concat_operator
test_is_null
test_is_not_null
test_between
test_not_between
test_like
test_not_like
test_in_list
test_not_in_list
test_function_call
test_count_star
test_table_dot_column
test_nested_arithmetic_with_comparison
```

### SELECT
```
test_select_1 (without FROM)
test_select_wildcard
test_select_qualified_wildcard
test_select_column_alias
test_select_distinct
test_select_from_table
test_select_from_table_with_alias
test_select_from_table_implicit_alias
test_select_where
test_select_inner_join_on
test_select_left_join
test_select_cross_join
test_select_join_using
test_select_group_by
test_select_having
test_select_order_by_asc_desc
test_select_order_by_nulls_last
test_select_limit
test_select_limit_offset
test_select_from_subquery
test_select_full_query (all clauses)
```

### INSERT
```
test_insert_values_single_row
test_insert_values_multi_row
test_insert_with_column_list
test_insert_without_column_list
test_insert_default_values
test_insert_select
```

### UPDATE
```
test_update_single_col
test_update_multi_col
test_update_with_where
test_update_without_where
```

### DELETE
```
test_delete_with_where
test_delete_without_where
```

---

## Anti-patterns to avoid

- **DO NOT** parse `NOT BETWEEN/LIKE/IN` as `NOT (expr BETWEEN/LIKE/IN ...)` —
  the AST has `negated: bool` on these variants; use it instead of wrapping in UnaryOp.
- **DO NOT** lowercase function names in the parser — store as-is and lowercase
  at lookup time in the executor (preserves original spelling in error messages).
  Actually: DO lowercase for consistent executor lookup. Decision: lowercase at parse time.
- **DO NOT** require `AS` for table aliases in FROM — both `FROM t AS u` and
  `FROM t u` are common SQL and must be supported.
- **DO NOT** call `parse_select` recursively without guarding with `LParen` check —
  subqueries always appear inside `(...)`.

---

## Risks

| Risk | Mitigation |
|---|---|
| `NOT BETWEEN/LIKE/IN` ambiguous with `parse_not` | Consume `NOT` in `parse_predicate` only when followed by BETWEEN/LIKE/IN |
| `Star` in multiplication vs `SELECT *` | `Star` in expressions = multiply; `SelectItem::Wildcard` parsed before expression parser |
| Implicit alias consumes next keyword as alias | `is_alias_token` checks that next token is an identifier, not a keyword |
| LEFT OUTER JOIN consumes OUTER then expects JOIN | `eat(OUTER)` makes OUTER optional |
| Subquery depth: recursive parse_select | Each call creates a new parser state; no shared mutable state |
| COUNT(*) vs COUNT(a, *) | `*` only allowed as sole argument; error if mixed |
