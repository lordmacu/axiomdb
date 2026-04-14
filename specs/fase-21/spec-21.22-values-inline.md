# Spec: 21.22 — `VALUES` as inline table

## What to build

SQL-standard inline table constructor usable in `FROM` position:

```sql
SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS t(id, name);

SELECT u.id, v.label
  FROM users u
  JOIN (VALUES (1, 'admin'), (2, 'user')) AS v(id, label)
    ON v.id = u.role_id;
```

Both PostgreSQL and MySQL 8 support this form. Useful for:
- Inline lookup tables without CREATE TABLE.
- Testing queries with controlled data.
- ORM-generated queries for multi-row UPSERT sources.
- Emulating a small dimension table.

## Inputs / outputs

### Grammar

```
from_item := '(' 'VALUES' row (',' row)* ')'
             [ 'AS' ] alias
             [ '(' col_name (',' col_name)* ')' ]

row := '(' expr (',' expr)* ')'
```

All rows must have the same column count. Column names come from the
parenthesized alias list; if absent, default to `column1`, `column2`,
… (PG), which we normalize to `column1..N`.

### AST

New `FromClause::Values(Box<ValuesClause>)` variant:

```rust
pub struct ValuesClause {
    pub rows: Vec<Vec<Expr>>,            // all rows; each row same length
    pub alias: String,                    // required — PG/MySQL both
                                          // require an alias on inline VALUES
    pub column_names: Option<Vec<String>>,// None → default column1..N
}
```

### Semantics

- Each inner row is evaluated once against an empty scope at execution
  time (no outer correlation in this subphase — consistent with the
  JSON_TABLE non-correlated path).
- Column types inferred from the first row's expressions. Later rows
  are coerced if compatible; type-mismatch across rows → error.
- Row count = len(rows). No deduplication.

## Use cases

```sql
-- Lookup in JOIN.
SELECT p.*, c.tag
  FROM products p
  JOIN (VALUES (1, 'hot'), (2, 'cold')) AS c(id, tag)
    ON c.id = p.category;

-- Standalone.
SELECT * FROM (VALUES (1, 'x'), (2, 'y')) AS t(n, s);

-- Single-row form (both MySQL and PG allow it).
SELECT * FROM (VALUES (42)) AS t(x);
```

## Acceptance criteria

- [ ] `SELECT * FROM (VALUES (…)) AS t(cols)` parses.
- [ ] Multi-row form works — all rows produced.
- [ ] Alias with column list produces the declared names.
- [ ] Alias without column list → default `column1, column2, …`.
- [ ] JOIN with `VALUES` right side works.
- [ ] APPLY / LATERAL with `VALUES` is out of scope (no correlation
      support in this subphase; rejected with clear error if doc
      references outer columns — consistent with 11.25a).
- [ ] Row-count mismatch across rows → parse error.
- [ ] Empty `VALUES ()` → parse error.
- [ ] Integration tests in `tests/integration_values_inline.rs`.
- [ ] 1 wire smoke assertion.

## Out of scope

- Standalone `VALUES` as a top-level query (no parens, no FROM)
  — `VALUES (1, 2), (3, 4);`. PG supports; MySQL 8 supports (as
  `TABLE VALUES ROW(...)`). Deferred — adds statement-level
  dispatch to `parse_dml`. Can be added later on top of the same
  AST.
- `VALUES` as UPDATE / DELETE source. Rare in practice.

## Cross-engine

- **PostgreSQL** `gram.y:15000-ish` — `select_values` production.
  VALUES is a top-level query form that returns rows; subselecting
  it via `(VALUES …)` wraps it in a `RangeSubselect`.
- **MySQL 8** supports `SELECT * FROM (VALUES ROW(…)) AS t(…)` —
  the `ROW` keyword is optional in 8.0.19+.
- **SQLite** accepts both forms via `compound_select`.

## Dependencies

- No new evaluator primitives — rows are `Vec<Value>` after
  evaluating each `Expr` against an empty scope.
- Requires match-arm additions across every `FromClause` consumer:
  analyzer_bind, analyzer_stmt, analyzer_ddl, select_core,
  select_joins_ctx, dml_join, exec_explain, select_helpers,
  plan_deps, parser/dml UPDATE/DELETE rejection, parser/json_table
  FROM dispatch. Same pattern as 11.25a `FromClause::JsonbSrf`.
