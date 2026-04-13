# Spec: 21.19 — `FETCH FIRST` / `OFFSET n ROWS` (SQL:2008)

## What to build

Standard SQL row-limiting grammar as parser-level aliases for the
existing `LIMIT` / `OFFSET` machinery:

```
[ OFFSET n { ROW | ROWS } ]
[ FETCH { FIRST | NEXT } [ count ] { ROW | ROWS } ONLY ]
```

SQL:2008 introduced this as the portable form. PostgreSQL and MySQL
8.0.19+ both accept it. Applications targeting "standard SQL"
(Hibernate, jOOQ, Debezium) emit this form.

## Inputs / outputs

### Grammar

```
fetch_clause := 'FETCH' ('FIRST' | 'NEXT')
                [ expr ]
                ('ROW' | 'ROWS')
                'ONLY'

offset_clause := 'OFFSET' expr ('ROW' | 'ROWS')
```

Absent count in `FETCH FIRST ROW ONLY` / `FETCH FIRST ROWS ONLY`
implies count = 1 (PG parity).

Both clauses optional. Any order accepted (some engines require
OFFSET before FETCH; AxiomDB will accept both orders for lenience).

### Semantics

Direct desugar at parse time:
- `FETCH FIRST n ROWS ONLY` → `LIMIT n`
- `FETCH FIRST ROW ONLY`    → `LIMIT 1`
- `FETCH NEXT n ROWS ONLY`  → `LIMIT n` (FIRST/NEXT interchangeable)
- `OFFSET n ROWS`           → `OFFSET n` (ROW/ROWS noise words)
- `OFFSET n ROW`            → `OFFSET n`

No AST change: both clauses produce the same `stmt.limit` /
`stmt.offset` expressions the existing LIMIT path produces.

## Use cases

```sql
-- Portable row window.
SELECT * FROM orders
 ORDER BY id
 OFFSET 20 ROWS
 FETCH FIRST 10 ROWS ONLY;

-- Single row.
SELECT * FROM events
 ORDER BY ts DESC
 FETCH FIRST ROW ONLY;

-- Pagination (jOOQ / Hibernate default output).
SELECT * FROM t
 ORDER BY id
 OFFSET 0 ROWS
 FETCH NEXT 50 ROWS ONLY;
```

## Acceptance criteria

- [ ] `FETCH FIRST n ROWS ONLY` parses and limits to n rows.
- [ ] `FETCH FIRST ROW ONLY` implies count = 1.
- [ ] `FETCH FIRST ROWS ONLY` (no count, plural) implies count = 1.
- [ ] `FETCH NEXT n ROW[S] ONLY` equivalent to `FETCH FIRST`.
- [ ] `OFFSET n ROWS` and `OFFSET n ROW` both accepted; `ROW`/`ROWS`
      are noise words.
- [ ] `OFFSET n ROWS FETCH FIRST m ROWS ONLY` combinado funciona.
- [ ] Existing `LIMIT n` / `LIMIT n OFFSET m` / MySQL `LIMIT o, c`
      still parse.
- [ ] Cannot combine `LIMIT` + `FETCH FIRST` on the same query
      (parse error — mixing forms is nonstandard and masks bugs).
- [ ] Integration tests in `tests/integration_fetch_first.rs`.
- [ ] 1 wire smoke assertion.

## Out of scope

- `WITH TIES` modifier (PG / Oracle / SQL Server). Requires
  evaluating the ORDER BY key of the last accepted row and keeping
  all subsequent rows whose key ties — not a simple LIMIT. Deferred
  until window functions / peer-group infrastructure lands.
- `FETCH FIRST n PERCENT ROWS ONLY` (SQL Server / Oracle). Requires
  row-count estimate or two-pass. Deferred.

## Cross-engine

- **PostgreSQL** `gram.y:7660` — `FetchStmt` (cursor fetch) distinct
  from SELECT-level row limit, which lives in `select_fetch_first_value`
  inside `opt_select_limit`. AxiomDB follows the SELECT-level path.
- **MySQL 8.0.19+** — accepts `FETCH FIRST` / `FETCH NEXT` as a
  spelling alias for `LIMIT`.
- **SQL Server**: `OFFSET n ROWS FETCH NEXT m ROWS ONLY` is the only
  supported row window (no `LIMIT` keyword).

## Dependencies

- Existing `parse_limit_offset` in `parser/dml.rs`.
- Existing `stmt.limit` / `stmt.offset` fields in `SelectStmt`.
