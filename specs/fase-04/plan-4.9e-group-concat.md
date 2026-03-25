# Plan: 4.9e — GROUP_CONCAT

## Files to create/modify

| File | Change |
|------|--------|
| `crates/axiomdb-sql/src/expr.rs` | Add `Expr::GroupConcat` variant |
| `crates/axiomdb-sql/src/lexer.rs` | Add `Separator` keyword token |
| `crates/axiomdb-sql/src/parser/expr.rs` | Special-case GROUP_CONCAT in `parse_ident_or_call` |
| `crates/axiomdb-sql/src/analyzer.rs` | Resolve column refs inside `Expr::GroupConcat` |
| `crates/axiomdb-sql/src/eval.rs` | Add `Expr::GroupConcat` arm (returns error: must be in aggregate ctx) |
| `crates/axiomdb-sql/src/executor.rs` | All aggregate infrastructure changes (see below) |
| `crates/axiomdb-sql/src/partial_index.rs` | Add `Expr::GroupConcat` arm in expression cloner |
| `crates/axiomdb-sql/tests/integration_group_concat.rs` | New test file |

---

## Algorithm / Data structures

### 1. New AST variant: `Expr::GroupConcat`

```rust
// In expr.rs — add to Expr enum:
GroupConcat {
    /// The expression to concatenate (coerced to TEXT per row).
    expr: Box<Expr>,
    /// DISTINCT — deduplicate values before concatenating.
    distinct: bool,
    /// ORDER BY inside the aggregate: list of (sort_expr, direction).
    order_by: Vec<(Expr, SortOrder)>,
    /// SEPARATOR string. Default = ",".
    separator: String,
},
```

`SortOrder` already in `ast.rs`. Import it in `expr.rs`.

### 2. Lexer: `Separator` keyword

```rust
// In lexer.rs, Token enum — add:
#[token("SEPARATOR", ignore(ascii_case))]
Separator,
```

Needed so the parser can distinguish `SEPARATOR` from a column name inside GROUP_CONCAT.

### 3. Parser: special-case `group_concat` / `string_agg`

In `parser/expr.rs`, `parse_ident_or_call`:

```
if name.eq_ignore_ascii_case("group_concat"):
    expect(LParen)
    distinct = eat(Token::Distinct)
    expr = parse_expr()
    order_by = []
    separator = ","

    if peek == Token::Order:
        eat(Order); expect(By)
        loop:
            ob_expr = parse_expr()
            dir = if eat(Asc) → Asc
                  elif eat(Desc) → Desc
                  else → Asc
            order_by.push((ob_expr, dir))
            if !eat(Comma): break
            // stop if next token is SEPARATOR or RParen (not an expr)
            if peek == Token::Separator || peek == Token::RParen: break

    if eat(Token::Separator):
        separator = expect_string_literal()  // read the separator text

    expect(RParen)
    return Expr::GroupConcat { expr, distinct, order_by, separator }

if name.eq_ignore_ascii_case("string_agg"):
    // PostgreSQL-compatible 2-arg form: string_agg(expr, sep)
    expect(LParen)
    expr = parse_expr()
    expect(Comma)
    sep_val = parse_expr()  // must be literal, evaluated at parse time
    // Extract literal string value; error if not a string literal
    separator = match sep_val {
        Expr::Literal(Value::Text(s)) => s,
        _ => return Err("string_agg separator must be a string literal")
    }
    expect(RParen)
    return Expr::GroupConcat { expr, distinct: false, order_by: [], separator }
```

**Key detail**: When parsing ORDER BY inside GROUP_CONCAT, stop at `SEPARATOR` or `)` to avoid confusing the separator keyword with a column name. The loop check: after consuming `,`, peek at next token — if it's `Separator` or `RParen`, stop.

### 4. Analyzer: recurse into `Expr::GroupConcat`

In `analyzer.rs`, `resolve_expr_full` — add arm:

```rust
Expr::GroupConcat { expr, distinct, order_by, separator } => {
    let expr = resolve_expr_full(*expr, ctx, outer_scopes, state)?;
    let order_by = order_by
        .into_iter()
        .map(|(e, dir)| Ok((resolve_expr_full(e, ctx, outer_scopes, state)?, dir)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Expr::GroupConcat {
        expr: Box::new(expr),
        distinct,
        order_by,
        separator,
    })
}
```

### 5. `eval.rs`: `Expr::GroupConcat` returns error

GROUP_CONCAT is only valid as an aggregate inside a grouped SELECT. If evaluated
outside that context (e.g., in WHERE), return an error:

```rust
Expr::GroupConcat { .. } => Err(DbError::InvalidValue {
    reason: "GROUP_CONCAT can only be used as an aggregate function".into(),
}),
```

### 6. `executor.rs` — aggregate infrastructure changes

#### 6a. `is_aggregate()` and `contains_aggregate()`

```rust
fn is_aggregate(name: &str) -> bool {
    matches!(name, "count" | "sum" | "min" | "max" | "avg")
    // NOTE: group_concat is NOT here — it's handled via Expr::GroupConcat directly
}

fn contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::GroupConcat { .. } => true,  // add this arm first
        Expr::Function { name, .. } if is_aggregate(name.as_str()) => true,
        // ... rest unchanged
    }
}
```

#### 6b. `AggExpr` — convert from struct to enum

```rust
enum AggExpr {
    /// Standard aggregate: COUNT, SUM, MIN, MAX, AVG.
    Simple {
        name: String,
        arg: Option<Expr>,
        agg_idx: usize,
    },
    /// GROUP_CONCAT / string_agg aggregate.
    GroupConcat {
        expr: Box<Expr>,
        distinct: bool,
        order_by: Vec<(Expr, SortOrder)>,
        separator: String,
        agg_idx: usize,
    },
}

impl AggExpr {
    fn agg_idx(&self) -> usize {
        match self { Self::Simple { agg_idx, .. } | Self::GroupConcat { agg_idx, .. } => *agg_idx }
    }

    /// True if this AggExpr was created from the given Expr.
    fn matches_simple(&self, name: &str, arg: &Option<Expr>) -> bool {
        matches!(self, Self::Simple { name: n, arg: a, .. } if n == name && a == arg)
    }

    fn matches_group_concat(&self, gc_expr: &Expr, distinct: bool, order_by: &[(Expr, SortOrder)], separator: &str) -> bool {
        matches!(self, Self::GroupConcat { expr, distinct: d, order_by: ob, separator: sep, .. }
            if expr.as_ref() == gc_expr && *d == distinct && ob == order_by && sep == separator)
    }
}
```

#### 6c. `AggAccumulator` — add `GroupConcat` variant

```rust
enum AggAccumulator {
    // ... existing variants unchanged ...
    GroupConcat {
        /// Accumulated rows: (value_as_text, order_key_values).
        /// order_key_values is empty when there is no ORDER BY.
        rows: Vec<(String, Vec<Value>)>,
        separator: String,
        distinct: bool,
        /// Sort direction per ORDER BY key (true = ASC).
        order_by_dirs: Vec<bool>,
    },
}
```

`AggAccumulator::new()` for GROUP_CONCAT:
```rust
"group_concat" => AggAccumulator::GroupConcat {
    rows: Vec::new(),
    separator: agg.separator().to_string(),  // read from AggExpr
    distinct: agg.is_distinct(),
    order_by_dirs: agg.order_by_dirs(),      // Vec<bool>
},
```

`AggAccumulator::update()` for GROUP_CONCAT:
```rust
Self::GroupConcat { rows, .. } => {
    // 1. Evaluate the GROUP_CONCAT expr against the row
    let val = match eval(gc_expr, row)? {
        Value::Null => return Ok(()),  // skip NULLs
        v => value_to_display_string(v),  // coerce to text
    };
    // 2. Evaluate ORDER BY key expressions
    let keys: Vec<Value> = gc_order_by.iter()
        .map(|(e, _)| eval(e, row))
        .collect::<Result<Vec<_>, _>>()?;
    rows.push((val, keys));
    Ok(())
}
```

`AggAccumulator::finalize()` for GROUP_CONCAT:
```rust
Self::GroupConcat { rows, separator, distinct, order_by_dirs } => {
    if rows.is_empty() {
        return Value::Null;
    }

    // 1. Sort if ORDER BY present
    if !order_by_dirs.is_empty() {
        rows.sort_by(|(_, keys_a), (_, keys_b)| {
            for (i, &asc) in order_by_dirs.iter().enumerate() {
                let a = keys_a.get(i).unwrap_or(&Value::Null);
                let b = keys_b.get(i).unwrap_or(&Value::Null);
                let cmp = compare_values_null_last(a, b);
                let cmp = if asc { cmp } else { cmp.reverse() };
                if cmp != Ordering::Equal { return cmp; }
            }
            Ordering::Equal
        });
    }

    // 2. Deduplicate if DISTINCT (preserve order after sort)
    let values: Vec<String> = if *distinct {
        let mut seen = HashSet::new();
        rows.iter()
            .filter(|(v, _)| seen.insert(v.clone()))
            .map(|(v, _)| v.clone())
            .collect()
    } else {
        rows.iter().map(|(v, _)| v.clone()).collect()
    };

    // 3. Truncate to 1 MB (group_concat_max_len)
    const MAX_LEN: usize = 1_048_576;
    let mut result = String::new();
    for (i, val) in values.into_iter().enumerate() {
        if i > 0 {
            result.push_str(separator);
        }
        result.push_str(&val);
        if result.len() >= MAX_LEN {
            result.truncate(MAX_LEN);
            break;
        }
    }

    Value::Text(result)
}
```

`value_to_display_string(v: Value) -> String` — new private helper:
```rust
fn value_to_display_string(v: Value) -> String {
    match v {
        Value::Text(s) => s,
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Real(f) => format!("{f}"),
        Value::Bool(b) => if b { "1".into() } else { "0".into() },
        Value::Null => String::new(),   // should not be reached (callers skip NULL)
        other => format!("{other:?}"),  // fallback
    }
}
```

`compare_values_null_last(a: &Value, b: &Value) -> Ordering` — reuse the existing
`compare_values` function if it already exists, or use value_to_key_bytes approach.

#### 6d. `collect_agg_exprs_from()` — handle `Expr::GroupConcat`

```rust
Expr::GroupConcat { expr, distinct, order_by, separator } => {
    // Build a GroupConcat AggExpr and deduplicate
    let already_registered = result.iter().any(|ae|
        ae.matches_group_concat(expr, *distinct, order_by, separator));

    if !already_registered {
        let idx = result.len();
        result.push(AggExpr::GroupConcat {
            expr: expr.clone(),
            distinct: *distinct,
            order_by: order_by.clone(),
            separator: separator.clone(),
            agg_idx: idx,
        });
    }
    // Recurse into ORDER BY sub-expressions (they may contain subqueries etc.)
    for (e, _) in order_by {
        collect_agg_exprs_from(e, result);
    }
}
```

#### 6e. `eval_with_aggs()` — lookup `Expr::GroupConcat`

```rust
Expr::GroupConcat { expr, distinct, order_by, separator } => {
    let idx = agg_exprs.iter()
        .find(|ae| ae.matches_group_concat(expr, *distinct, order_by, separator))
        .map(|ae| ae.agg_idx())
        .ok_or_else(|| DbError::InvalidValue {
            reason: "GROUP_CONCAT not found in aggregate list".into(),
        })?;
    Ok(agg_values[idx].clone())
}
```

#### 6f. `AggAccumulator::update()` signature problem

The current `update` method receives `(agg: &AggExpr, row: &[Value])`.
For GROUP_CONCAT, we need the `order_by` exprs from the `AggExpr::GroupConcat`.
This is already accessible: `agg` is the `AggExpr` itself, so we can pattern-match it.

The existing update loop in `execute_select_grouped` (around line 2990):
```rust
for (acc, agg) in state.accumulators.iter_mut().zip(agg_exprs.iter()) {
    acc.update(agg, row)?;
}
```

In `update`, add GROUP_CONCAT arm that borrows order_by from `agg`.

#### 6g. Other places requiring `Expr::GroupConcat` arms

| Function | Change |
|----------|--------|
| `collect_column_refs` | Recurse into `expr` and `order_by` exprs |
| `subst_expr` | Clone GroupConcat with substituted `expr` and `order_by` |
| `grouped_col_name` | Return `"GROUP_CONCAT(...)"` |
| `grouped_expr_type` | Return `(DataType::Text, true)` (nullable) |
| `partial_index.rs` cloner | Clone GroupConcat (pass through unchanged, not reachable) |

---

## Implementation phases

1. **Add `Separator` token to lexer** — build check
2. **Add `Expr::GroupConcat` to `expr.rs`** — add `SortOrder` import
3. **Add `Expr::GroupConcat` stubs to all Expr match exhaustiveness points:**
   - `eval.rs` (2 match arms) — return error
   - `analyzer.rs` — recurse
   - `executor.rs` (collect_column_refs, subst_expr, contains_aggregate,
     collect_agg_exprs_from, eval_with_aggs, grouped_col_name, grouped_expr_type)
   - `partial_index.rs` — pass-through clone
4. **Parser: special-case `group_concat` and `string_agg`** — build + parse test
5. **`AggExpr` → enum** — update all access sites
6. **Add `GroupConcat` to `AggAccumulator`** — new(), update(), finalize()
7. **Add `value_to_display_string` helper**
8. **Add `compare_values_null_last` helper** (or reuse existing)
9. **Wire up `update()` to pass `order_by` from `AggExpr::GroupConcat`**
10. **Write integration tests**
11. **Update wire-test.py**

---

## Tests to write

### Integration (`integration_group_concat.rs`)

```
basic GROUP_CONCAT comma-separated
custom SEPARATOR
NULL values skipped
all-NULL group returns NULL
empty group returns NULL
ORDER BY ASC
ORDER BY DESC
ORDER BY multi-column
DISTINCT deduplication
DISTINCT + ORDER BY
string_agg(col, sep) alias
GROUP_CONCAT in GROUP BY query
GROUP_CONCAT in ungrouped query (single group)
HAVING GROUP_CONCAT(...) LIKE '%...'
```

### Wire (`tools/wire-test.py`)
```python
cur.execute("SELECT GROUP_CONCAT(name ORDER BY name SEPARATOR ',') FROM ...")
cur.execute("SELECT GROUP_CONCAT(DISTINCT tag) FROM ...")
```

---

## Anti-patterns to avoid

- **DO NOT** add GROUP_CONCAT to `is_aggregate()` (it's detected via `Expr::GroupConcat` directly, not by function name)
- **DO NOT** use `unwrap()` anywhere in finalize or update
- **DO NOT** sort in-place before knowing if DISTINCT is needed — deduplicate after sort (DISTINCT preserves sorted order)
- **DO NOT** forget to handle `GroupConcat` in `subst_expr` (used for correlated subquery evaluation) — pass-through if exprs contain no Column refs from outer scope
- **DO NOT** let `collect_agg_exprs_from` recurse infinitely — don't recurse into `expr` of GroupConcat as a top-level aggregate (just register it); only recurse into `order_by` sub-exprs that might contain nested aggregates (they shouldn't, but stay safe)

## Risks

- **`AggExpr` is a struct today** → converting to enum touches many sites but is mechanical. Search: `ae.arg`, `ae.name`, `ae.agg_idx` — update all access patterns
- **Separator detection in ORDER BY list**: when parsing `col ORDER BY col2 SEPARATOR ','`, the parser must distinguish `col2, col3` (two ORDER BY columns) from `col2 SEPARATOR ','` (end of ORDER BY). Solution: after eating a comma in the ORDER BY loop, peek — if next token is `Token::Separator` or `Token::RParen`, stop the ORDER BY loop
- **`string_agg` separator is a literal** — the separator must be parseable at parse time as a string literal. If it's an expression, reject with a clear error. (DuckDB also requires this.)
- **`compare_values_null_last`** — check if this already exists in executor.rs. If not, use `value_to_key_bytes` for comparison (already used in ORDER BY sort).
