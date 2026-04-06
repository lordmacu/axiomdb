# MySQL Compatibility Gaps — AxiomDB

Last updated: 2026-04-06 (ninth audit)

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

### `INSERT` column count mismatch silently pads with `NULL` — **bug**

`INSERT INTO t (a, b) VALUES (1)` — one value for two columns — does not return an
error. The executor at `insert.rs:1622-1633` silently pads the missing column with
`NULL`, even in strict mode. MySQL returns error 1136: "Column count doesn't match
value count at row 1".

```sql
CREATE TABLE t (a INT, b INT NOT NULL);
INSERT INTO t (a, b) VALUES (1);   -- AxiomDB: inserts (1, NULL), MySQL: ERROR 1136
```

Silent padding masks programmer errors and violates `NOT NULL` constraints silently.

Fix: in `executor/insert.rs`, after building the column map, check
`values.len() == col_map.len()` and return `DbError::ColumnCountMismatch` if not.

### Unknown COM_* commands close the connection

All unrecognised `COM_*` command bytes fall through to error 1047 "Unknown command"
and may close the connection (`handler.rs:1356-1367`). MySQL clients (connectors,
ORMs, tools) sometimes send commands that are valid in some MySQL versions but rare:
`COM_STATISTICS` (0x09), `COM_PROCESS_INFO` (0x0a), `COM_DEBUG` (0x0d),
`COM_FIELD_LIST` (0x04), `COM_REFRESH` (0x07), `COM_SHUTDOWN` (0x08),
`COM_BINLOG_DUMP` (0x12), `COM_STMT_FETCH` (0x1c).

MySQL always stays connected and returns a graceful error packet. AxiomDB dropping
the connection is a protocol violation that causes client reconnect loops.

Fix: add match arms for known-but-unimplemented COM bytes that return ERR 1047 +
keep the connection alive; only truly unknown bytes should close.

### FK references not updated when a table is renamed — **data integrity bug**

`ALTER TABLE t RENAME TO new_name` and `RENAME TABLE t TO new_name` do not update
foreign key definitions that reference the renamed table. If table `orders` has a FK
pointing to `users`, and `users` is renamed to `accounts`, the FK catalog entry still
stores the old reference. Subsequent FK enforcement (INSERT into `orders`, DELETE from
`accounts`) breaks silently or errors with table-not-found.

- `executor/ddl.rs:1697-1711` — `alter_rename_table()` updates only the table's own
  catalog entry; it does not scan `axiom_foreign_keys` for `parent_table_id` or child
  references pointing to the renamed table
- Fix: after updating the table catalog entry, query `axiom_foreign_keys` for all FK
  rows where `parent_table_name = old_name` (or `parent_table_id`) and update them
  to the new name; do the same for child-side FKs if stored by name

### `COUNT(DISTINCT col)` / `SUM(DISTINCT col)` / `AVG(DISTINCT col)` not parsed

The aggregate function parser only supports `DISTINCT` inside `GROUP_CONCAT`.
For all other aggregates, `DISTINCT` inside the function call is not recognised:

```sql
SELECT COUNT(DISTINCT user_id) FROM events;       -- parse error
SELECT SUM(DISTINCT amount) FROM transactions;    -- parse error
SELECT AVG(DISTINCT score) FROM grades;           -- parse error
```

Very common in analytics queries — used any time a distinct count is needed without
a subquery.

- `parser/expr.rs:422-512` — in `parse_ident_or_call`, after opening `(`, check for
  an optional `DISTINCT` keyword and store `distinct: bool` in the aggregate AST node
- `executor/aggregate.rs:174-187` — apply deduplication before accumulating when
  `distinct = true`

### `IS TRUE` / `IS FALSE` / `IS NOT TRUE` / `IS NOT FALSE` predicates

MySQL boolean predicates are not in the IS-clause parser:

```sql
WHERE is_active IS TRUE          -- parse error or wrong result
WHERE deleted IS NOT FALSE       -- parse error
WHERE (score > 90) IS TRUE       -- parse error
```

- `parser/expr.rs:120-131` — after `IS [NOT]`, currently only handles `NULL`;
  extend to check for `TRUE` / `FALSE` tokens and emit the corresponding predicate
- Semantics: `IS TRUE` is equivalent to `= 1`; `IS NOT TRUE` is `<> 1 OR IS NULL`;
  mirrors MySQL 3-valued logic

### `CREATE TABLE … SELECT …` (CTAS)

`CREATE TABLE new_table SELECT * FROM existing_table` — the most common table-copy
idiom in MySQL — is not handled. The CREATE TABLE parser at `ddl.rs:36-66` expects
`(column_defs)` immediately after the table name; a `SELECT` keyword there causes a
parse error.

```sql
CREATE TABLE audit_backup SELECT * FROM audit_log WHERE year = 2023;
```

Fix: after parsing the table name, branch on `Token::Select` to execute the inner
SELECT and derive columns/data from the result; column types inferred from SELECT
output.

### `CREATE TABLE … LIKE other_table` (schema clone)

`CREATE TABLE new_table LIKE existing_table` copies the full schema (columns, indexes,
constraints) without copying data. Not in the parser — `LIKE` after the table name is
not handled (`ddl.rs:36-66`).

```sql
CREATE TABLE users_staging LIKE users;
```

Fix: after parsing the table name, if next token is `LIKE`, read the source table name
and copy its `TableDef` from the catalog into the new table.

### `INSERT IGNORE` not parsed

`INSERT IGNORE INTO t VALUES (...)` silences constraint violations (UNIQUE, FK,
NOT NULL) and continues; MySQL returns a warning count instead of an error. Used
heavily for idempotent bulk loads and deduplication patterns.

```sql
INSERT IGNORE INTO tags (post_id, tag) VALUES (1, 'rust');  -- parse error
```

- `parser/dml.rs:376-436` — add `IGNORE` token consumption after `INSERT`
- Store `ignore: bool` in `InsertStmt`; executor wraps each row in a try-catch and
  converts `DuplicateKey` / `NotNullViolation` to warnings when `ignore = true`

### Positional `ORDER BY` / `GROUP BY` (column ordinals)

`ORDER BY 1, 2` and `GROUP BY 1` — referring to SELECT list columns by position —
are not recognized by the parser or executor. MySQL and the SQL standard both support
this. Every ORM-generated pagination query and many hand-written analytics queries
rely on it.

```sql
SELECT name, age FROM users ORDER BY 2 DESC, 1 ASC;  -- AxiomDB: parse error
SELECT dept, COUNT(*) FROM employees GROUP BY 1;      -- AxiomDB: parse error
```

Fix: in `executor/select.rs`, after parsing `ORDER BY` / `GROUP BY`, resolve integer
literals to the corresponding position in the `SELECT` projection list before
evaluating.

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

### Session variables return `None` for common ORM capability queries

`session.rs:get_variable()` returns `None` for any variable not in its explicit
match list. MySQL clients and ORMs query these on connect to detect server
capabilities:

```sql
SELECT @@performance_schema;      -- None → ORM throws or skips feature
SELECT @@have_query_cache;        -- None
SELECT @@log_bin;                 -- None → replication-aware clients break
SELECT @@net_buffer_length;       -- None
SELECT @@transaction_read_only;   -- None → JDBC connection validation fails
SELECT @@sql_mode;                -- None → strictness-aware ORMs break
```

Fix: add safe defaults to `session.rs:get_variable()`:
`performance_schema→"0"`, `have_query_cache→"NO"`, `log_bin→"0"`,
`net_buffer_length→"16384"`, `transaction_read_only→"0"`, `sql_mode→""`.

### `SELECT ... FOR UPDATE` / `SELECT ... LOCK IN SHARE MODE` not parsed

Row-level locking hints are not in the parser. JPA/Hibernate, GORM, and hand-written
transaction code use these extensively:

```sql
SELECT * FROM accounts WHERE id = 1 FOR UPDATE;         -- parse error
SELECT * FROM products WHERE id = 5 LOCK IN SHARE MODE; -- parse error
```

- `parser/dml.rs:92-105` — after LIMIT/OFFSET, check for `FOR UPDATE` and
  `LOCK IN SHARE MODE`; store `lock_mode: Option<LockMode>` in `SelectStmt`
- Executor: execute as normal SELECT for now (row-level locking is Phase 13.7);
  silently accepting the syntax unblocks every ORM that generates these clauses

### `SELECT HIGH_PRIORITY` / `STRAIGHT_JOIN` modifiers not consumed

MySQL SELECT modifiers placed between `SELECT` and the column list are not
recognised by the parser and cause parse errors:

```sql
SELECT HIGH_PRIORITY id FROM t WHERE id = 1;   -- parse error
SELECT STRAIGHT_JOIN * FROM t JOIN s ON t.id = s.t_id;  -- parse error
```

These are hints/directives that have no effect on result correctness — they only
matter for optimizer behaviour. The correct fix is to consume and discard them.

- `parser/dml.rs:50-106` — after `SELECT`, consume optional `HIGH_PRIORITY`,
  `SQL_SMALL_RESULT`, `SQL_BIG_RESULT`, `SQL_BUFFER_RESULT`, `STRAIGHT_JOIN`
  before parsing the column list

### Multi-table `DELETE` / `UPDATE` with `JOIN` (MySQL-specific syntax)

MySQL allows deleting or updating rows via JOIN across multiple tables:

```sql
DELETE o FROM orders o JOIN customers c ON o.customer_id = c.id
WHERE c.deleted_at IS NOT NULL;

UPDATE orders o JOIN customers c ON o.customer_id = c.id
SET o.priority = c.tier WHERE c.country = 'US';
```

Neither `DeleteStmt` nor `UpdateStmt` support a join clause or multiple target
tables (`parser/dml.rs:440-476`). Widely used in data migration and cleanup scripts.

Fix: extend AST and parser to recognise the multi-table form; executor evaluates the
join, then applies DELETE/UPDATE only to the primary target table's rows.

### `CALL procedure_name()` / `DO expr` statements not parsed

`CALL` and `DO` are not in the lexer token set or the top-level parser dispatch
(`parser/mod.rs:227-293`). Any application using stored procedures or inline
expression execution will immediately get a parse error.

- `CALL p(args)` — execute a stored procedure; return `NotImplemented` until
  Phase 16.7, but must parse cleanly
- `DO expr` — evaluate an expression and discard the result (used for side effects
  like `DO SLEEP(1)` or `DO RELEASE_LOCK('name')`)

Fix: add `Call` and `Do` to the lexer; parse and return `NotImplemented` (or
silently succeed for DO with no visible output).

### Column type wire encoding missing integer subtypes and date subtypes

`result.rs:datatype_to_mysql_type()` does not map all MySQL column types to the
correct protocol type codes. Missing:

| MySQL type | Expected code | Current behaviour |
|---|---|---|
| TINYINT | `0x01` (TINY) | falls to default |
| SMALLINT | `0x02` (SHORT) | falls to default |
| MEDIUMINT | `0x09` (INT24) | falls to default |
| YEAR | `0x0d` (YEAR) | not in AxiomDB AST |
| TIME | `0x0b` (TIME) | not in AxiomDB ColumnType |
| DATETIME | `0x0c` (DATETIME) | mapped same as TIMESTAMP |

Tools like MySQL Workbench, DBeaver, and JDBC drivers use the column type byte to
decide how to format and display values. Wrong codes cause silent display errors
(timestamps shown as strings, year shown as integer, etc.).

Fix: add `ColumnType::TinyInt`, `SmallInt`, `MediumInt`, `Time`, `DateTime` to the
type system; map all in `datatype_to_mysql_type()`.

### Standalone `RENAME TABLE t1 TO t2` statement not in parser

MySQL's `RENAME TABLE` is a standalone DDL statement, distinct from `ALTER TABLE t
RENAME TO t2`. It also supports renaming multiple tables atomically:

```sql
RENAME TABLE users TO accounts;
RENAME TABLE a TO b, c TO d;   -- atomic multi-rename
```

There is no `RenameTable` variant in the `Stmt` enum (`ast.rs:472-513`). Only
`ALTER TABLE t RENAME TO ...` is handled. ORMs (ActiveRecord, Flyway, Liquibase)
generate the standalone form.

Fix: add `Stmt::RenameTable(Vec<(String, String)>)` to the AST; parse in
`parser/mod.rs`; executor calls `alter_rename_table()` for each pair atomically.

### `@@global.var` / `@@session.var` prefix not stripped in variable lookup

`SELECT @@global.max_allowed_packet` passes `"@@global.max_allowed_packet"` to
`session.rs:get_variable()`. The function strips `@@session.` and `@@` but not
`@@global.`, so the lookup fails and returns `None`.

```sql
SELECT @@global.max_allowed_packet;    -- None → client error
SELECT @@session.transaction_isolation; -- works
SET @@global.autocommit = 1;           -- variable name not stripped correctly
```

Fix: in `get_variable()` and `apply_set()`, strip `@@global.` prefix with the same
treatment as `@@session.`.

### `SET CHARACTER SET charset` not handled

`SET CHARACTER SET utf8mb4` is a MySQL statement that sets only the client character
set (not the connection collation, unlike `SET NAMES`). It is not handled in
`session.rs:apply_set()` — only `SET NAMES` and generic variable assignments are
there. JDBC drivers use this form.

Fix: add a branch in `apply_set()` recognising `CHARACTER SET` (two tokens) and
setting `client_charset` only.

### Missing session variables: `character_set_system`, `init_connect`, `collation_server`

These MySQL standard read-only variables return `None` from `get_variable()` and
cause errors in ORMs and monitoring tools:

- `character_set_system` — always `utf8mb4` in MySQL 8
- `init_connect` — SQL to run on new connections; default is `""` (empty)
- `collation_server` — server-level collation; default `utf8mb4_0900_ai_ci`
- `character_set_server` — already partially handled?

Fix: add explicit `Some(...)` returns in `get_variable()` before the fallthrough.

### `ALTER TABLE t CONVERT TO CHARACTER SET charset`

Rewrites all text columns to a new character set. Not in the `AlterTableOp` enum
(`ast.rs:345-389`). Common in migrations upgrading `latin1` → `utf8mb4`.

```sql
ALTER TABLE users CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
```

Fix: add `AlterTableOp::ConvertCharset { charset: String, collate: Option<String> }`;
executor rewrites TEXT/VARCHAR column encodings and updates table/column metadata in
catalog.

### `UPDATE t SET col = DEFAULT` not parsed

MySQL's `DEFAULT` keyword is valid in the SET clause of UPDATE to reset a column
to its default value:

```sql
UPDATE products SET stock = DEFAULT WHERE discontinued = 1;
UPDATE users SET last_seen = DEFAULT;   -- resets to column DEFAULT (e.g. NOW())
```

The parser at `parser/dml.rs:462-467` treats `DEFAULT` as an identifier, not a
keyword, in this position — it either errors or resolves `DEFAULT` as a column name.

Fix: in `parse_assignment`, after `col =`, check for `Token::Default` and emit
`Expr::Default`; executor resolves it to the column's stored default (same path
as `INSERT ... VALUES (1, DEFAULT)`).

### `COM_STATISTICS` (0x09) stub needed

`COM_STATISTICS` is sent by some MySQL monitoring agents and legacy clients. It
expects a plain-text response (not a packet frame) with counters like
`Uptime: 3600  Threads: 1  ...`. The current handler returns error 1047 and may
close the connection.

Fix: add `0x09 =>` case returning a minimal statistics string (uptime, thread count)
without closing the connection.

### `LIKE … ESCAPE '\'` clause not parsed

`WHERE path LIKE 'reports\_%' ESCAPE '\'` — the `ESCAPE` clause that defines the
escape character for `LIKE` patterns — is not in the AST or parser. Any schema that
searches for literal `%` or `_` characters requires this.

```sql
SELECT * FROM files WHERE name LIKE '100\%' ESCAPE '\';   -- parse error
```

Fix: add `escape: Option<Expr>` to `BinaryOp::Like` or as a separate `Expr::Like`
variant; `eval/core.rs` `like_match` already has an escape param, just needs wiring.

### Column `DEFAULT` expressions not persisted in catalog

`ColumnDef` in the schema (catalog's `ColumnDef` struct, `schema.rs:503`) has no
`default_value` field. Default expressions parsed by the DDL parser are silently
discarded after parse. Consequences:

- `ON DELETE SET DEFAULT` / `ON UPDATE SET DEFAULT` FK actions cannot work
- `INSERT DEFAULT VALUES` cannot fall back to column-defined defaults (it only
  produces NULL or the AUTO_INCREMENT value)
- `ALTER TABLE t ALTER COLUMN price SET DEFAULT 0` has nowhere to store the result

Fix: add `default_expr: Option<Expr>` (or its SQL string form) to the persisted
`ColumnDef`; serialize/deserialize in `catalog/schema.rs`; executor resolves it on
INSERT and FK referential actions.

### `AUTO_INCREMENT` with explicit value `0` should auto-assign

MySQL treats `0` and `NULL` identically for `AUTO_INCREMENT` columns: both result
in the next sequence value being assigned. AxiomDB stores the literal `0`.

```sql
INSERT INTO t (id, name) VALUES (0, 'Alice');  -- MySQL: id assigned 1, AxiomDB: id = 0
```

Fix: in `executor/insert.rs`, when a column has `auto_increment=true` and the
supplied value is `Value::Int(0)`, treat it the same as `NULL` and call the
sequence generator.

### `VARCHAR(N)` length not validated or enforced

`VARCHAR(10)` accepts and stores strings longer than 10 characters without truncation
or error. The column type's `length` field is stored in the catalog but never checked
during INSERT or UPDATE.

```sql
CREATE TABLE t (name VARCHAR(5));
INSERT INTO t VALUES ('toolongvalue');   -- MySQL: error 1406 / truncation warning
                                          -- AxiomDB: stored as-is
```

Fix: in `executor/insert.rs` and `executor/update.rs`, after type coercion, check
`Value::Text(s).len() > col.varchar_len` and either truncate with a warning
(permissive mode) or return `DataTooLong` error (strict mode).

### `CHAR(n)` not distinguished from `VARCHAR(n)` — missing right-padding

`CHAR(5)` stores fixed-length strings right-padded with spaces; `VARCHAR(5)` stores
variable-length strings. AxiomDB treats both identically, making
`SELECT length(char_col)` return wrong results and breaking CHAR-based comparison
semantics (`'abc' = 'abc  '` is TRUE for CHAR columns in MySQL).

Fix: add `ColumnType::Char(u32)` distinct from `ColumnType::Varchar(u32)` (or a
flag on the existing VARCHAR type); pad to length on INSERT; strip trailing spaces
before comparison and output.

### Default string comparison is case-sensitive (MySQL default is case-insensitive)

`SessionCollation::Binary` is the AxiomDB default, making all string comparisons
case-sensitive. MySQL's default collation is `utf8mb4_general_ci` (case-insensitive).
This means:

```sql
SELECT * FROM users WHERE name = 'alice';  -- MySQL: finds 'Alice'; AxiomDB: does not
```

Any application written against MySQL and then tested against AxiomDB will find
silent behavior differences for text equality, LIKE, ORDER BY, and GROUP BY.

- `session.rs` — change `SessionCollation::default()` from `Binary` to `CaseInsensitive`
  when `CompatMode` is `MySQL` (already have `CompatMode::Mysql`)
- Alternatively, emit the correct `@@collation_connection = utf8mb4_general_ci` in
  the server greeting so clients see the expected default

### Date string implicit coercion in comparisons

`WHERE created_at = '2024-01-01'` does not implicitly parse `'2024-01-01'` as a
`DATE`/`TIMESTAMP` when comparing against a TIMESTAMP column. MySQL automatically
coerces the string using `STR_TO_DATE` semantics.

```sql
SELECT * FROM logs WHERE created_at = '2024-01-01';       -- AxiomDB: type error
SELECT * FROM logs WHERE created_at >= '2024-01-01 00:00:00';  -- works (explicit)
```

Fix: in `executor/type_coercion.rs` (or `eval/core.rs`), when comparing a `Text`
value against a `Timestamp`/`Date` column, attempt `STR_TO_DATE(text, '%Y-%m-%d')`
and `STR_TO_DATE(text, '%Y-%m-%d %H:%i:%s')` as coercion fallbacks before
returning a type mismatch error.

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

### Math functions: `PI()`, `LOG()`, `EXP()`, trig functions, `RADIANS()` / `DEGREES()`

Not in `eval/functions/mod.rs` dispatch table. Fall to `NotImplemented`:

- `PI()` — constant π (3.14159…)
- `LOG(x)` / `LOG(base, x)` — natural log / log base N
- `LOG2(x)` / `LOG10(x)` — log base 2 and 10
- `EXP(x)` — eˣ
- `SIN(x)` / `COS(x)` / `TAN(x)` — trigonometric
- `ATAN(x)` / `ATAN2(y, x)` — inverse tangent
- `RADIANS(d)` / `DEGREES(r)` — angle unit conversion

Fix: add all to `eval/functions/numeric.rs` — all are one-liners using Rust's
`f64::{ln, log2, log10, exp, sin, cos, tan, atan, atan2}`.

### `expr + INTERVAL 1 DAY` temporal arithmetic

`NOW() + INTERVAL 1 DAY`, `created_at + INTERVAL 3 MONTH` and similar interval
expressions are not in the parser. `INTERVAL` is not a lexer token, so these cause
parse errors. Ubiquitous in date-range queries and expiry calculations.

```sql
SELECT * FROM sessions WHERE expires_at < NOW() + INTERVAL 30 MINUTE;
SELECT created_at + INTERVAL 1 YEAR FROM contracts;
```

Fix: add `Token::Interval` to the lexer; in the binary expression parser, when
`+` or `-` is followed by `INTERVAL n unit`, emit a special `Expr::IntervalAdd`
node; evaluator adds/subtracts the chrono `Duration` equivalent.

### `GROUP BY … WITH ROLLUP`

`SELECT dept, SUM(salary) FROM employees GROUP BY dept WITH ROLLUP` generates
subtotals at each grouping level (extra rows with `NULL` in place of each group key).
Neither the parser nor executor handles `WITH ROLLUP`; it causes a parse error.

Fix: after parsing the GROUP BY column list in `parser/dml.rs`, consume optional
`WITH ROLLUP` and set `with_rollup: bool` in `SelectStmt`; executor adds synthetic
NULL rows after each group boundary.

### `COLLATE collation_name` in expression context

MySQL allows overriding collation per-expression in WHERE / ORDER BY:

```sql
WHERE name COLLATE utf8mb4_unicode_ci = 'Alice'
ORDER BY name COLLATE utf8mb4_bin
```

The `COLLATE` keyword is only recognised in DDL column definitions, not in DML
expression context (`parser/expr.rs:406-556`).

Fix: after parsing any leaf expression (identifier, string literal), check for
`Token::Collate ident` and wrap in `Expr::Collate { expr, collation }`.

### `CREATE INDEX … USING HASH` silently creates a B-Tree

`CREATE INDEX idx ON t(col) USING HASH` is valid MySQL syntax and should either
create a hash index or return a clear "not supported" error. Currently it maps
silently to a B-Tree (`executor/ddl.rs:931-934`) with no warning, so clients that
check index metadata via `SHOW INDEX` will see `BTREE` where they expected `HASH`.

Fix: either add `IndexType::Hash` with explicit `NotImplemented` execution (honest
error) or emit a warning that HASH is silently promoted to BTREE.

### `CREATE INDEX … COMMENT 'text'` — comment field not persisted

`CREATE INDEX idx ON t(col) COMMENT 'my comment'` — MySQL allows attaching a comment
to an index — is not parsed. The `CreateIndexStmt` struct has no `comment` field
(`ast.rs:304-322`). The clause either causes a parse error or is silently discarded.

Fix: add `comment: Option<String>` to `CreateIndexStmt` and `IndexDef`; parser
consumes `COMMENT 'text'` after the WITH clause; catalog stores it.

### `RENAME TABLE a TO b, c TO d` multi-table atomic rename

The current rename implementation processes one pair via `break` after the first
`RenameTable` op (`ddl.rs:1711`), preventing multi-pair renames. MySQL guarantees
all renames in a `RENAME TABLE a TO b, c TO d` are atomic.

Fix: remove the `break` from the ALTER TABLE rename branch and collect all rename
pairs before executing; or handle multi-pair rename in the standalone `RENAME TABLE`
statement (once that is added).

### `COM_FIELD_LIST` (0x04)

Old MySQL clients (MySQL 5.x CLI, some ODBC drivers) use `COM_FIELD_LIST` to fetch
column metadata without executing a SELECT. Current handler: unknown command error.

Fix: add `0x04 =>` case, parse the table name from the payload, look up the schema,
and return `ColumnDefinition` packets (same structs used in result set headers).

### `SHOW TABLES` dynamic column name (`Tables_in_<db>`)

MySQL returns `SHOW TABLES` with a column named `Tables_in_<database>` where
`<database>` is the current database name. AxiomDB returns a static column name.
MySQL CLI and some ORMs use this column name for formatting.

Fix: in the SHOW TABLES executor path, build the column metadata dynamically as
`format!("Tables_in_{}", current_database)`.

### `IS DISTINCT FROM` / `IS NOT DISTINCT FROM` standard syntax

SQL standard `a IS NOT DISTINCT FROM b` is a verbose null-safe equality (same as
`<=>` in MySQL). Not in the IS-clause parser (`parser/expr.rs:120-131`). Rarely
used in MySQL code but appears in SQL-standard ORM output.

Fix: after `IS [NOT]`, check for `DISTINCT FROM` keyword sequence and emit
`BinaryOp::NullSafeEq` (same as `<=>`).

### `MATCH(cols) AGAINST (expr)` fulltext search syntax

`WHERE MATCH(title, body) AGAINST ('search terms' IN BOOLEAN MODE)` is not in the
parser. Will cause a parse error. Fulltext index execution can return
`NotImplemented` for now — just parsing cleanly unblocks schema imports.

Fix: add `MATCH` as a special token; parse `MATCH(col_list) AGAINST (expr [mode])`
as a special expression node; executor returns `NotImplemented` until Phase 22.

### `COM_RESET_CONNECTION` incomplete reset

`COM_RESET_CONNECTION` (0x1f) at `handler.rs:889-906` calls `SessionContext::new()`
but may not reset character set, collation, prepared statement cache, and user
variables accumulated during the session. MySQL specifies a full session-state wipe.

Fix: ensure `COM_RESET_CONNECTION` also clears prepared statements, collation
settings, and user-defined `@variables`; verify `SessionContext::new()` covers all.

### `NATURAL JOIN` not implemented

`SELECT … FROM t1 NATURAL JOIN t2` — automatic join on columns with matching names
— is not in the Token enum or `parse_join_clauses()` (`parser/dml.rs:213-300`).

Fix: add `NATURAL` token; in the parser, detect `NATURAL [LEFT] JOIN` and build the
`USING` list from the intersection of column names from both tables.

### Date functions: `SYSDATE()`, `UTC_*`, `FROM_UNIXTIME()`, `DATEDIFF()`, `CONVERT_TZ()`

Missing from `eval/functions/datetime.rs`:

- `SYSDATE()` — returns execution time (MySQL: `NOW()` returns statement start time, `SYSDATE()` returns actual clock; currently both are aliased to NOW)
- `UTC_DATE()` / `UTC_TIME()` / `UTC_TIMESTAMP()` — UTC equivalents of current date/time
- `FROM_UNIXTIME(n)` — converts Unix epoch seconds to DATETIME
- `UNIX_TIMESTAMP()` / `UNIX_TIMESTAMP(dt)` — current time or given DATETIME as Unix epoch
- `DATEDIFF(d1, d2)` — days between two dates
- `CONVERT_TZ(dt, from_tz, to_tz)` — timezone conversion

### String functions: `NVL2()`, `INSERT(str, pos, len, new)`, `ORD()`, `SOUNDEX()`, `CHAR()` variadic

Not in the function dispatcher:

- `NVL2(expr, val_if_not_null, val_if_null)` — 3-arg null conditional (`nulls.rs`)
- `INSERT(str, pos, len, newstr)` — replaces `len` chars at `pos` with `newstr` (the string manipulation function, NOT SQL INSERT)
- `ORD(str)` — returns Unicode code point of first character
- `SOUNDEX(str)` — phonetic encoding
- `CHAR(n1, n2, …)` — current impl takes single arg; MySQL accepts variadic and concatenates chars

### `CRC32()`, `BENCHMARK()`, `INET_ATON()` / `INET_NTOA()` / `IS_IPV4()` / `IS_IPV6()`

Not in `eval/functions/mod.rs` dispatch:

- `CRC32(str)` → 32-bit CRC checksum as unsigned int; used in data integrity checks
- `BENCHMARK(n, expr)` → evaluates `expr` N times, returns 0; used in performance testing
- `INET_ATON('10.0.0.1')` → 167772161; `INET_NTOA(167772161)` → `'10.0.0.1'`
- `INET6_ATON(str)` / `INET6_NTOA(bytes)` — IPv6 equivalents
- `IS_IPV4(str)` / `IS_IPV6(str)` / `IS_IPV4_MAPPED(str)` — IP address validation

### `CONVERT(expr, type)` MySQL two-argument syntax

MySQL's `CONVERT(expr, CHAR)` / `CONVERT(expr, UNSIGNED)` two-argument form is not
in the function dispatcher — only `CONVERT(expr USING charset)` is partially
handled. Both forms are common in MySQL queries:

```sql
SELECT CONVERT(price, CHAR);             -- stringify a numeric
SELECT CONVERT('42', UNSIGNED INTEGER);  -- parse string as unsigned int
```

Fix: in `executor/select.rs` or `eval/functions/`, detect when `CONVERT` has two
positional args (instead of `USING`) and route to the type-cast path (same as
`CAST(expr AS type)`).

### `DISTINCT` with non-selected `ORDER BY` column — missing validation

`SELECT DISTINCT a FROM t ORDER BY b` should be rejected when `b` is not in the
SELECT list (MySQL error 3065, SQL standard violation). AxiomDB currently executes
it silently, producing undefined ordering.

Fix: in the semantic analyzer (`analyzer.rs` or `select.rs`), after resolving ORDER
BY expressions, verify that any ORDER BY column is either in the DISTINCT SELECT
list or is an expression of a column that is.

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
