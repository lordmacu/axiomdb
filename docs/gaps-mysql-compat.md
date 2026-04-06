# MySQL Compatibility Gaps — AxiomDB

Last updated: 2026-04-05 (fifth audit)

This document tracks SQL features that are missing or incomplete relative to MySQL 8.
Items are ordered by implementation priority within each section.

---

## HIGH PRIORITY

These block common ORMs, migration tools, and client libraries.

### Column attributes: `UNSIGNED`, `COLLATE`, `CHARACTER SET`, `ON UPDATE CURRENT_TIMESTAMP`, `COMMENT`

Any of these column-level attributes cause a **parse error** because they are not
recognized after the column type in `parser/ddl.rs:77-135`.

```sql
id       INT UNSIGNED,                              -- parse error
name     VARCHAR(100) COLLATE utf8mb4_unicode_ci,   -- parse error
locale   VARCHAR(10)  CHARACTER SET utf8mb4,        -- parse error
updated  TIMESTAMP    ON UPDATE CURRENT_TIMESTAMP,  -- parse error
score    INT          COMMENT 'Player score',       -- parse error
```

- `UNSIGNED` — marks a non-negative integer; MySQL stores double the positive range
- `COLLATE` / `CHARACTER SET` — per-column encoding; present in virtually every
  modern MySQL schema dump
- `ON UPDATE CURRENT_TIMESTAMP` — auto-updates a TIMESTAMP column on row UPDATE;
  ubiquitous in audit/tracking tables (`created_at`, `updated_at` patterns)
- `COMMENT` — column metadata; used by all schema tools

Fix: extend `parse_column_def` to consume and discard (or store) these tokens
after the data type and before the column constraints loop.

### `CREATE TABLE` inline `INDEX` / `KEY` definitions

`CREATE TABLE t (..., INDEX idx_name (col), ...)` causes a **parse error** because
`INDEX` and `KEY` are not in `is_table_constraint_start` (`parser/ddl.rs:68-72`).
Used in virtually every MySQL schema — the most common way to define non-unique
indexes inline.

```sql
CREATE TABLE posts (
  id      INT PRIMARY KEY,
  user_id INT,
  status  VARCHAR(20),
  INDEX   idx_user (user_id),          -- parse error
  KEY     idx_status (status)          -- parse error
);
```

Fix: add `Token::Index` and `Token::Key` (or Ident "key") to `is_table_constraint_start`;
parse as `CREATE INDEX` — the named index is created on the same table.

### `CREATE TABLE` table options (ENGINE, CHARSET, COLLATE, COMMENT, AUTO_INCREMENT)

`CREATE TABLE t (...) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci`
causes a **parse error** because the parser returns immediately after `)` without
consuming table options. Every `mysqldump` output and most ORM migration scripts
append these options. This blocks importing any real MySQL schema.

- `parser/ddl.rs:58` — after `p.expect(&Token::RParen)?` add a loop that consumes
  table options: `ENGINE=`, `DEFAULT CHARSET=`, `CHARSET=`, `COLLATE=`, `COMMENT=`,
  `ROW_FORMAT=`, `AUTO_INCREMENT=N`, `KEY_BLOCK_SIZE=`, `COMPRESSION=`, `PACK_KEYS=`;
  all can be silently ignored (store or discard)

### `INFORMATION_SCHEMA` virtual database

`SELECT * FROM information_schema.tables` / `.columns` / `.statistics` returns
table-not-found. All major ORMs (Sequelize, TypeORM, Prisma, ActiveRecord, GORM,
Hibernate) query `information_schema` on connect to discover schema, validate
migrations, and generate SQL. Without it, ORM auto-discovery is impossible.

- Executor: intercept queries to `information_schema.*` and route to a virtual
  catalog reader; minimum tables: `TABLES`, `COLUMNS`, `KEY_COLUMN_USAGE`,
  `REFERENTIAL_CONSTRAINTS`, `TABLE_CONSTRAINTS`, `STATISTICS`

### `BEGIN WORK` / `START TRANSACTION READ ONLY` / `START TRANSACTION READ WRITE`

`BEGIN WORK` causes a parse error: `WORK` is not consumed. `START TRANSACTION READ ONLY`
and `START TRANSACTION READ WRITE` also fail because the `READ` token is left over
after `START TRANSACTION` is parsed.

- `parser/mod.rs:354-363`
- Fix: after consuming optional `TRANSACTION`, also eat optional `WORK` (for BEGIN)
  and optional `READ ONLY` / `READ WRITE` modifiers (for both)
- `SessionContext.next_txn_read_only: bool` for `READ ONLY` → restrict DML

### `DELETE` / `UPDATE` with `ORDER BY` + `LIMIT`

`DeleteStmt` and `UpdateStmt` have no `order_by` or `limit` fields (`ast.rs:264-277`).
The parser never reads them. Used extensively for safe batch processing:

```sql
DELETE FROM audit_log ORDER BY created_at ASC LIMIT 1000;
UPDATE jobs SET status='retrying' ORDER BY priority DESC LIMIT 50;
```

- `ast.rs` — add `order_by: Vec<SortExpr>` and `limit: Option<Expr>` to both structs
- `parser/dml.rs` — parse optional `ORDER BY` + `LIMIT` after `WHERE` in DELETE and UPDATE
- Executor: apply sort + limit before mutation

### `LOCK TABLES` / `UNLOCK TABLES` / `FLUSH TABLES` / `KILL`

Not in the lexer — `LOCK`, `UNLOCK`, `FLUSH`, `KILL` are not token keywords.
`mysqldump` uses all four:

```sql
LOCK TABLES t READ;
LOCK TABLES t WRITE;
UNLOCK TABLES;
FLUSH TABLES;
FLUSH TABLES WITH READ LOCK;
KILL QUERY 42;
KILL CONNECTION 42;
```

- Short-term fix: parse and silently ignore `LOCK`/`UNLOCK`/`FLUSH` (for import compatibility)
- `KILL QUERY` should send an interrupt signal to the target session
- `parser/mod.rs` — add token arms for these keywords

### `CONCAT(NULL, 'a')` returns `'a'` instead of `NULL` — **bug**

`eval/functions/string.rs:200` has a comment "SQL CONCAT skips NULLs (MySQL behavior)"
but this is wrong. MySQL's `CONCAT()` returns `NULL` if **any** argument is `NULL`.
It is `CONCAT_WS()` that skips NULLs. The current code implements `CONCAT_WS`
semantics for `CONCAT`.

```sql
SELECT CONCAT(NULL, 'hello');  -- AxiomDB: 'hello', MySQL: NULL ← wrong
SELECT CONCAT_WS(',', NULL, 'a');  -- AxiomDB: 'a',     MySQL: 'a'  ← correct
```

Fix: change the `Value::Null => {}` arm in `concat` to `Value::Null => return Ok(Value::Null)`.

### `SUBSTR(str, -3)` returns `''` instead of `'llo'` — **bug**

`eval/functions/string.rs:154`: `Value::Int(n) => n as usize`. When `n` is negative
(e.g. `-3i64`), casting to `usize` in Rust wraps to `18446744073709551613`, which
then clamps to `chars.len()` producing an empty slice.

MySQL treats a negative start position as counting from the end of the string:
```sql
SUBSTR('hello', -3)     -- MySQL: 'llo',  AxiomDB: ''  ← bug
SUBSTR('hello', -3, 2)  -- MySQL: 'll',   AxiomDB: ''  ← bug
```

Fix: before the `n as usize` cast, if `n < 0` compute `chars.len().saturating_sub(n.unsigned_abs())`.

### Expression operators: `REGEXP`, `RLIKE`, `XOR`, `DIV`, `<=>`, bitwise operators

These operators are not in the lexer and cause **parse errors**:

- `REGEXP` / `RLIKE` — regex pattern matching: `WHERE email REGEXP '^[a-z]+'`. Used in data validation and search queries. Should map to Rust `regex` crate.
- `NOT REGEXP` / `NOT RLIKE` — negated regex.
- `XOR` — boolean exclusive OR: `WHERE a XOR b`. Less common but valid SQL.
- `DIV` — integer division keyword: `7 DIV 2 = 3`. Many MySQL codebases use `DIV` for integer math to avoid float conversion.
- `<=>` — null-safe equality: `NULL <=> NULL = 1` (vs `NULL = NULL = NULL`). Used in queries that compare nullable columns without explicit `IS NULL` checks.
- `&`, `|`, `^`, `~` — bitwise AND/OR/XOR/NOT: `WHERE flags & 0x01 = 1`. Ubiquitous in permission/bitmask columns.
- `<<`, `>>` — bitwise shift: `SELECT 1 << 4`.

Fix: add tokens to lexer; add parse arms in `parser/expr.rs` at the correct precedence levels.

### `SET SESSION` / `SET GLOBAL` / multi-variable `SET`

These forms cause parse errors or silently fail:

```sql
SET SESSION transaction_isolation = 'READ-COMMITTED';  -- SESSION keyword not consumed
SET GLOBAL max_allowed_packet = 16777216;              -- GLOBAL keyword not consumed
SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED; -- rejected
SET var1 = val1, var2 = val2;                          -- comma-separated not parsed
SET @user_var = 42;                                    -- @var (single @) not supported
```

- `SET SESSION var = val` — JDBC drivers use this form exclusively
- `SET GLOBAL var = val` — admin tools
- Multi-variable `SET` with comma — standard MySQL
- `SET @user_var = expr` — user-defined variables; widely used for temp values in migrations

Fix: in `parser/mod.rs:380`, handle SESSION/GLOBAL keywords before variable name;
parse comma-separated list; support `@ident` as a user variable reference.

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

### `LIMIT offset, count` (MySQL comma syntax — reversed)

MySQL's `LIMIT offset, count` form (first number is offset, second is limit) is not
parsed. `parser/dml.rs` only handles `LIMIT count [OFFSET offset]`.

```sql
SELECT * FROM t LIMIT 5, 10;   -- MySQL: skip 5, return 10
                                -- AxiomDB: misparses as LIMIT 5 (correct) but ignores 10
```

The comma form is extremely common in legacy MySQL code and ORMs targeting MySQL 5.x.

Fix: after parsing the first `LIMIT` number, if next token is `,` consume it and
treat first number as offset, second as limit.

### `HAVING` clause with `SELECT` alias

```sql
SELECT dept, COUNT(*) AS cnt FROM employees GROUP BY dept HAVING cnt > 5;
```

The `HAVING` clause cannot resolve `cnt` — it was defined as an alias in `SELECT`.
AxiomDB requires repeating the expression: `HAVING COUNT(*) > 5`.

MySQL (and PostgreSQL) allow aliases defined in `SELECT` to be used in `HAVING`.
This is a very common pattern.

Fix: in `executor/aggregate.rs`, before evaluating the HAVING predicate, build an alias
map from the SELECT projections and substitute aliases in the HAVING expression.

### `INSERT INTO t SET col1=1, col2=2` (MySQL-specific SET syntax)

Not in the parser. MySQL's alternative INSERT syntax using assignment list:

```sql
INSERT INTO users SET name='Alice', email='alice@example.com', active=1;
```

Used by MySQL ORMs and legacy applications. `InsertSource::Set` variant missing
from `ast.rs`.

Fix: in `parser/dml.rs`, after parsing the table ref, branch on `Token::Set` →
parse assignment list → treat as single-row `VALUES`.

### `INSERT INTO t (a,b) VALUES (1, DEFAULT)` — DEFAULT in VALUES list

`DEFAULT` as a value inside a `VALUES` list is not an `Expr` variant (`expr.rs`).
Common when inserting with explicit column list and wanting defaults for some:

```sql
INSERT INTO products (name, price, stock) VALUES ('Widget', 9.99, DEFAULT);
```

Fix: add `Expr::Default` variant; in executor, resolve it to the column's default value
(same logic as `INSERT DEFAULT VALUES`).

### `CREATE DATABASE IF NOT EXISTS` / `CHARACTER SET` / `COLLATE`

`parse_create_database()` (`parser/ddl.rs:19`) parses only the database name.
Extra tokens cause parse errors:

```sql
CREATE DATABASE IF NOT EXISTS mydb;                                 -- parse error
CREATE DATABASE mydb CHARACTER SET utf8mb4 COLLATE utf8mb4_bin;    -- parse error
ALTER DATABASE mydb CHARACTER SET utf8mb4;                         -- parse error
```

Fix: consume optional `IF NOT EXISTS`, then loop consuming `CHARACTER SET name`,
`COLLATE name`, `DEFAULT CHARSET name` — all can be stored or ignored.

### `ALTER TABLE ADD INDEX` / `DROP INDEX` / `CHANGE COLUMN` / `AUTO_INCREMENT=N`

Not in `AlterTableOp` enum. Common in migration scripts:

```sql
ALTER TABLE t ADD INDEX idx_email (email);          -- not parsed
ALTER TABLE t ADD UNIQUE INDEX uk_slug (slug);      -- not parsed
ALTER TABLE t DROP INDEX idx_email;                 -- not parsed (only DROP CONSTRAINT)
ALTER TABLE t CHANGE COLUMN old_name new_name INT;  -- not parsed (rename+retype in one op)
ALTER TABLE t AUTO_INCREMENT = 1000;                -- not parsed
ALTER TABLE t ALTER COLUMN price SET DEFAULT 0;     -- not parsed
ALTER TABLE t ALTER COLUMN price DROP DEFAULT;      -- not parsed
```

- `ADD INDEX` / `ADD UNIQUE INDEX` — most migration tools add indexes via ALTER TABLE, not via separate `CREATE INDEX`
- `CHANGE COLUMN` — MySQL-specific rename+retype in one operation; many ORMs generate this
- `AUTO_INCREMENT = N` — reset the auto-increment sequence counter; used after bulk deletes and migrations
- `ALTER COLUMN SET DEFAULT` / `DROP DEFAULT` — change column defaults without rewriting rows

### `ALTER TABLE RENAME INDEX`

`ALTER TABLE t RENAME INDEX old_name TO new_name` is not in the `AlterTableOp` enum.
Common in migration tools that rename indexes without dropping/recreating them.

- `ast.rs:368` — add `AlterTableOp::RenameIndex { old_name: String, new_name: String }`
- `parser/ddl.rs` — parse `RENAME INDEX old TO new` in the ALTER TABLE dispatch
- Executor: update index name in `axiom_indexes` catalog; no data movement needed

### `SQL_CALC_FOUND_ROWS` / `FOUND_ROWS()`

`SELECT SQL_CALC_FOUND_ROWS * FROM t LIMIT 10` sets a session counter to the total
pre-LIMIT row count; `SELECT FOUND_ROWS()` returns it. Legacy MySQL pagination idiom
(deprecated in MySQL 8.0.17 but still in wide use).

- Parser: consume and discard `SQL_CALC_FOUND_ROWS` modifier in SELECT
- Executor: before applying LIMIT, stash full row count in session
- `eval/functions/system.rs` — add `found_rows` function reading session counter

### `LAST_INSERT_ID(expr)` with argument

`LAST_INSERT_ID(expr)` with a non-zero argument evaluates `expr`, stores it as the
new last-insert-id, and returns it. Currently `system.rs:19` ignores all arguments
and reads the thread-local. Applications use this idiom for manual sequence tracking.

- `eval/functions/system.rs:19` — check `args.len() > 0`; eval `args[0]`; write to
  `THREAD_LAST_INSERT_ID` and return the value

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

### `SET FOREIGN_KEY_CHECKS = 0` / `SET UNIQUE_CHECKS = 0`

`mysqldump` wraps every import with these statements. If AxiomDB doesn't recognize
them as valid SET variables, the import fails immediately. They should be accepted
and either silently ignored or honoured.

- `FOREIGN_KEY_CHECKS = 0` — disables FK enforcement during bulk import
- `UNIQUE_CHECKS = 0` — disables unique constraint checking during bulk import
- `sql_notes = 0` — suppresses note-level warnings

Fix: add these to the handled SET variables in `session.rs`; `FOREIGN_KEY_CHECKS=0`
ideally propagates to the executor to skip FK validation.

### `SHOW FULL TABLES` / `SHOW FULL COLUMNS` / `SHOW TABLE STATUS`

ORM schema discovery uses these variants:

- `SHOW FULL TABLES [FROM db] [LIKE pattern]` — adds `Table_type` column (`BASE TABLE` / `VIEW`); Sequelize and ActiveRecord use this
- `SHOW FULL COLUMNS FROM t` — adds `Privileges` and `Comment` columns; Prisma and TypeORM use this
- `SHOW TABLE STATUS [FROM db] [LIKE pattern]` — returns rows/engine/charset per table; schema tools use this for metadata

Parser currently does not handle the `FULL` modifier or `TABLE STATUS`.

### `SHOW ENGINES` / `SHOW CHARSET` / `SHOW COLLATION`

Not parsed. MySQL Workbench, DBeaver, TablePlus probe these on connect:

- `SHOW ENGINES` — list storage engines; return a single row: `AxiomDB | DEFAULT | ...`
- `SHOW CHARSET` / `SHOW CHARACTER SET` — list supported charsets; return utf8mb4, utf8, latin1
- `SHOW COLLATION [LIKE pattern]` — list collations; return utf8mb4_unicode_ci, utf8mb4_bin, etc.

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

### `FORMAT(n, d)` number formatting function

`FORMAT(1234567.89, 2)` → `'1,234,567.89'` — formats a number with thousands
separators and d decimal places. Used in reporting queries and display layers.

- `eval/functions/string.rs` — straightforward string formatting

### `EXPLAIN FORMAT=JSON` / `EXPLAIN ANALYZE`

`EXPLAIN FORMAT=JSON SELECT ...` returns a structured JSON plan; `EXPLAIN ANALYZE`
includes actual row counts and execution times. Used by query-analysis tools.

- Parser: extend EXPLAIN stmt with `format: Option<ExplainFormat>` and `analyze: bool`
- Executor: serialize existing plan struct as JSON for FORMAT=JSON

### `COM_CHANGE_USER` (0x11) not handled

JDBC connection pools (c3p0, HikariCP with older MySQL drivers) send
`COM_CHANGE_USER` to recycle a connection with a new user context. The handler
has no case for command byte `0x11`, so the connection will be closed or hang.

Fix: in `handler.rs` command loop, add a case for `0x11` that resets session state
and re-authenticates (or simply resets like `COM_RESET_CONNECTION`).

### `INSERT LOW_PRIORITY` / `HIGH_PRIORITY` / `DELAYED` modifiers

Not in the lexer. These modifiers precede `INTO` in MySQL INSERT:

```sql
INSERT LOW_PRIORITY INTO t VALUES (1);
INSERT HIGH_PRIORITY INTO t VALUES (1);
INSERT DELAYED INTO t VALUES (1);
```

Fix: in `parser/dml.rs`, after `Token::Insert`, consume and discard optional
`LOW_PRIORITY`, `HIGH_PRIORITY`, or `DELAYED` before `INTO`.

### `SELECT ... INTO @var` / `SELECT ... INTO OUTFILE`

Not parsed. MySQL allows storing SELECT results:

```sql
SELECT name INTO @user_name FROM users WHERE id = 1 LIMIT 1;
SELECT * INTO OUTFILE '/tmp/data.csv' FIELDS TERMINATED BY ',' FROM t;
```

The `INTO` keyword in SELECT context (between SELECT list and FROM) is not
in the parser or AST.

### `CREATE INDEX col(N)` prefix index

`IndexColumn` struct (`ast.rs:169`) has no `prefix_length` field. MySQL allows
indexing only the first N characters of a TEXT/VARCHAR column:

```sql
CREATE INDEX idx ON t (description(100));
```

Fix: add `prefix_length: Option<u32>` to `IndexColumn`; encode prefix in B-Tree key.

### `FULLTEXT INDEX` / `SPATIAL INDEX` in CREATE INDEX / CREATE TABLE

`IndexType` enum only has `BTree` and `Brin` (`ast.rs:290`). `FULLTEXT` and `SPATIAL`
keywords are not in the parser. These appear in mysqldump output:

```sql
CREATE FULLTEXT INDEX ft_idx ON t (body);
ALTER TABLE t ADD FULLTEXT INDEX ft_idx (body);
```

Fix: add `Fulltext` and `Spatial` variants to `IndexType`; executor returns
`NotImplemented` with a clear message (or silently creates a regular B-Tree index).

### Version-conditional MySQL comments (`/*!...*/`)

`mysqldump` wraps version-specific SQL in `/*!50600 SET ... */` comments.
The parser currently strips all `/* ... */` comments, losing the code inside.

```sql
/*!40101 SET character_set_client = utf8 */;       -- silently dropped
/*!50600 SET GLOBAL innodb_file_per_table = 1 */;  -- silently dropped
```

Fix: in the lexer, detect `/*!` prefix; if the embedded version number is ≤ AxiomDB's
MySQL compatibility version (80000), execute the content; otherwise skip.

### `GENERATED ALWAYS AS (expr) STORED/VIRTUAL` columns

Computed columns not in parser or executor:

```sql
CREATE TABLE orders (
  price    DECIMAL(10,2),
  tax_rate DECIMAL(5,4),
  total    DECIMAL(10,2) GENERATED ALWAYS AS (price * (1 + tax_rate)) STORED
);
```

- `STORED` — value computed on INSERT/UPDATE and persisted on disk
- `VIRTUAL` — value computed at read time, not stored

Fix: add to `ColumnDef.generated: Option<(Expr, Generated)>`;
executor materializes on INSERT/UPDATE (STORED) or SELECT (VIRTUAL).

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
