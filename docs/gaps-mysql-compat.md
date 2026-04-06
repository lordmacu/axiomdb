# MySQL Compatibility Gaps — AxiomDB

Last updated: 2026-04-05

This document tracks SQL features that are missing or incomplete relative to MySQL 8.
Items are ordered by implementation priority within each section.

---

## HIGH PRIORITY

These block common ORMs, migration tools, and client libraries.

### `UNION` / `UNION ALL`

Tokenized but not parsed. Core SQL feature used by almost every ORM for pagination,
reporting, and multi-source queries.

- Parser: add `parse_union` in `parser/mod.rs`
- Executor: collect two `SelectStmt` results and merge/deduplicate

### `INSERT ... ON DUPLICATE KEY UPDATE`

MySQL-specific upsert. Used heavily by ORMs (Sequelize, TypeORM, GORM) for
idempotent inserts.

- Parser: extend `InsertStmt` with `on_duplicate: Option<Vec<Assignment>>`
- Executor: attempt insert; on `DuplicateKey`, apply assignments and UPDATE

### `REPLACE INTO`

MySQL-specific upsert (DELETE + INSERT semantics). Common in migrations and bulk
loaders.

- Parser: new `ReplaceStmt` (same shape as `InsertStmt`)
- Executor: attempt insert; on `DuplicateKey`, delete old row + insert new one

### `SELECT` without `FROM` (wildcard path)

`SELECT *` without `FROM` returns `NotImplemented`. Harmless to fix — just return an
empty column list. Many health-check queries use `SELECT 1` (already works) but some
clients probe with `SELECT *`.

- `executor/select.rs:783` — return empty row for wildcard without FROM

### Subquery in `FROM` (derived tables)

`SELECT … FROM (SELECT …) alias` is rejected. Used by ORMs for pagination wrappers
and aggregate sub-selects.

- `executor/select.rs:667` and `:1141`
- Requires evaluating the inner SELECT first, then treating the result as a virtual table

### `DATE` column type

`DATE` is parsed but returns `NotImplemented` in the executor. Blocks any schema
with date-only columns.

- `executor/shared.rs:179`
- Map to `ColumnType::Timestamp` with truncation, or add a new `ColumnType::Date`

---

## MEDIUM PRIORITY

These affect specific use cases but are not blockers for basic ORM usage.

### `DROP PRIMARY KEY` on clustered table

`ALTER TABLE t DROP PRIMARY KEY` returns `NotImplemented` on clustered tables.
The PK is the clustered index root — dropping it requires a full table rebuild
back to heap layout (reverse of `ALTER TABLE REBUILD`).

- `executor/ddl.rs:1031`
- Only relevant for clustered tables; heap tables don't have an inline PK tree

### `ALTER TABLE DROP / MODIFY COLUMN` auto-index handling

`DROP COLUMN col` and `MODIFY COLUMN col ...` return `NotImplemented` if the
column is part of an index (`ddl.rs:1858`, `ddl.rs:1944`). Current error message:
"Cannot drop/change type: it is part of index. Drop the index first."

MySQL automatically drops or rebuilds the index as part of the column operation.
ORMs expect this to work without manual index removal.

- `executor/ddl.rs:1858` (DROP COLUMN guard)
- `executor/ddl.rs:1944` (MODIFY COLUMN type-change guard)
- Fix: detect affected indexes, drop them, rewrite column, rebuild the index

### `SHOW WARNINGS` / `SHOW ERRORS`

Not in the parser. MySQL connectors (JDBC, MySQL Connector/Python, mysqlclient)
issue `SHOW WARNINGS` after every DML statement to surface soft errors and
notes. Without it, JDBC-based ORMs may hang waiting for a response.

- Parser: add `ShowWarningsStmt` / `ShowErrorsStmt`
- Executor: return the per-session warning buffer; `SHOW WARNINGS LIMIT N`
  and `SHOW ERRORS` should read from a `Vec<Warning>` on `SessionContext`

### `SHOW CREATE TABLE`

Used by MySQL Workbench, Sequel Pro, and `mysqldump` to reconstruct schemas.

- No AST node yet; add `ShowCreateTableStmt` + parse `SHOW CREATE TABLE t`
- Executor: reconstruct `CREATE TABLE` SQL from catalog (columns + indexes + constraints)

### `DECIMAL` / `NUMERIC` column type

`DECIMAL(p,s)` is parsed but returns `NotImplemented` in the executor.

- `executor/shared.rs:176`
- Simplest path: map to `ColumnType::Float` with a precision note (lossy but unblocking)
- Correct path: add `ColumnType::Decimal(u8, u8)` with fixed-point arithmetic

### `SHOW VARIABLES` / `SHOW STATUS`

MySQL clients (JDBC, MySQL Connector, many ORMs) issue these on connect to detect
server capabilities. Currently not parsed.

- Add to parser SHOW dispatch
- Executor: return a static table of known variables (e.g. `character_set_server`, `max_allowed_packet`)

### Multi-column foreign keys

`REFERENCES t (col1, col2)` returns `NotImplemented` at 3 sites in `ddl.rs`.

- `executor/ddl.rs:182` (ADD CONSTRAINT), `executor/ddl.rs:2087` (DROP CONSTRAINT)
- Requires encoding compound FK keys in `axiom_foreign_keys`

### `ON UPDATE CASCADE` / `ON UPDATE SET NULL`

FK update actions beyond `RESTRICT`/`NO ACTION` return `NotImplemented`.

- `fk_enforcement.rs:776`
- Requires walking child rows on UPDATE and propagating the change

### `ON DELETE SET DEFAULT` / `ON UPDATE SET DEFAULT`

- `fk_enforcement.rs:672` and `:776`
- Requires stored DEFAULT expressions in catalog (currently not persisted)

### `DROP INDEX` without `ON table`

MySQL syntax: `DROP INDEX idx ON table`. AxiomDB requires the `ON table` part.
Some clients issue `DROP INDEX idx` without it.

- `executor/ddl.rs:1000`
- Look up the index name across all tables in the schema

### `ADD CONSTRAINT PRIMARY KEY`

`ALTER TABLE t ADD PRIMARY KEY (col)` returns `NotImplemented`.

- `executor/ddl.rs:2263`
- Requires full table rewrite to clustered format (same as `ALTER TABLE REBUILD`)

### `EXPLAIN SELECT …`

Returns `NotImplemented` outside of `execute_with_ctx`. Used by developers and
query analyzers.

- `executor/mod.rs:1188`
- Needs to be wired into the no-ctx dispatch path

---

## LOW PRIORITY

Advanced features, rarely needed for basic MySQL client compatibility.

### `SAVEPOINT` / `ROLLBACK TO SAVEPOINT` / `RELEASE SAVEPOINT`

Return `NotImplemented` outside `execute_with_ctx`. Already planned for Phase 40.

- `executor/mod.rs:1191`

### CTEs (`WITH … AS (SELECT …)`)

Not in AST or parser. Required for recursive queries.

### Window functions (`ROW_NUMBER()`, `RANK()`, `LAG()`, `LEAD()`, etc.)

Not in AST or parser. Required for analytics queries.

### `UNION ALL` with ORDER BY / LIMIT on outer query

Even after `UNION` is implemented, `ORDER BY` and `LIMIT` on the combined result
need a separate pass.

### `CREATE VIEW` / `DROP VIEW`

Not in AST or parser.

### `SHOW PROCESSLIST`

Not parsed. Used by monitoring tools.

### `RAND()` function

Not implemented in `eval/functions/`. Easy to add.

- `eval/functions/numeric.rs` — `Value::Real(rand::random())`

### `GREATEST()` / `LEAST()` functions

Not implemented. Common in MySQL queries for clamping values.

### `HEX()` / `UNHEX()` functions

Not implemented.

### `DATE_ADD()` / `DATE_SUB()`

Not implemented. Needed for date arithmetic.

### `TIMESTAMPDIFF()`

Not implemented. Common in age/duration calculations.

### `DATE()` scalar function

`DATE(ts)` extracts the date portion from a TIMESTAMP. Not in `eval/functions/datetime.rs`.
Also missing: `TIME(ts)` (extract time), `ADDDATE()` / `SUBDATE()` (aliases for
`DATE_ADD` / `DATE_SUB`).

- `eval/functions/datetime.rs`

### `EXTRACT(unit FROM expr)`

SQL standard date/time extraction not in parser or evaluator.
`EXTRACT(YEAR FROM ts)` returns the year as an integer. Differs from `YEAR(ts)` in that
the unit is a keyword, not a function argument.

- Parser: special-case `EXTRACT(unit FROM expr)` syntax
- Evaluator: map to existing year/month/day/hour/minute/second logic

### Additional date component functions

Not implemented: `WEEK()`, `WEEKDAY()`, `WEEKOFYEAR()`, `QUARTER()`, `DAYNAME()`,
`MONTHNAME()`, `DAYOFWEEK()`, `DAYOFMONTH()`, `DAYOFYEAR()`, `YEARWEEK()`,
`LAST_DAY()`, `MAKEDATE(year, day)`, `MAKETIME(h, m, s)`, `TIME_TO_SEC()`, `SEC_TO_TIME()`.
All belong in `eval/functions/datetime.rs`.

### `SHA1()` / `SHA2()` / `MD5()` hash functions

Not implemented. Used for content hashing, password migration scripts, and checksums.

- `eval/functions/` — straightforward: call Rust `sha1` / `sha2` / `md5` crate

### `SLEEP(seconds)` function

Not implemented. Widely used in integration tests and simulations.
MySQL: `SLEEP(N)` pauses N seconds and returns 0 (or 1 if interrupted).

- `eval/functions/system.rs` — `std::thread::sleep(Duration::from_secs_f64(n))`

### `TRUNCATE(n, d)` numeric function

Not implemented. `TRUNCATE(3.14159, 2)` → `3.14`. Different from `TRUNCATE TABLE`.
Commonly used in financial calculations.

- `eval/functions/numeric.rs`

### `BIN()` / `OCT()` / `CONV()` base conversion

Not implemented. `BIN(255)` → `'11111111'`; `OCT(8)` → `'10'`;
`CONV('ff', 16, 10)` → `'255'`. Used for bit manipulation and protocol parsing.

- `eval/functions/numeric.rs` or `string.rs`

### `ELT()` / `FIELD()` string lookup functions

Not implemented. `ELT(2, 'a', 'b', 'c')` → `'b'` (Nth element);
`FIELD('b', 'a', 'b', 'c')` → `2` (1-based position of first match, 0 if not found).
Common in enum-style queries without a lookup table.

- `eval/functions/string.rs`

### `CONVERT(expr USING charset)`

Not implemented. Used by some MySQL ORMs.

### `JSON` column type

No JSON type in the catalog or executor. Blocks document-style schemas.

### `BLOB` / `MEDIUMBLOB` / `LONGBLOB`

No blob type. Binary data stored as `BYTES` but without size variants.

### `ENUM` / `SET` column types

Not implemented.

---

## ALREADY IMPLEMENTED (recently closed)

| Feature | Phase |
|---------|-------|
| `ALTER TABLE ADD COLUMN` (clustered) | 40.3b |
| `ALTER TABLE DROP COLUMN` (clustered) | 40.3b |
| `ALTER TABLE MODIFY COLUMN` (clustered) | 40.3b |
| `INSERT DEFAULT VALUES` | 40.3b |
| `SHOW INDEX FROM table` / `SHOW INDEXES` / `SHOW KEYS` | 40.3b |
| `CREATE INDEX ON` clustered table | 40.1b |
| `TRUNCATE TABLE` (clustered) | 39.x |
| `ANALYZE TABLE` (clustered) | 39.x |
| FK enforcement on clustered tables | 39.x |
