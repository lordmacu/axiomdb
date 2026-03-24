# Spec: 4.15 — Interactive CLI (REPL)

## What to build (not how)

A standalone interactive command-line tool (`axiomdb-cli`) that connects directly
to a local AxiomDB database file — no server, no network. The user types SQL
statements and sees formatted results immediately, similar to `sqlite3` or `psql`.

The CLI lives in a new crate `crates/axiomdb-cli` as an independent binary.

---

## User-facing behavior

### Startup

```
$ axiomdb-cli ./mydb
AxiomDB 0.1.0 — interactive shell
Type SQL ending with ; to execute. Type .help for commands.

axiomdb>
```

If no path is given, defaults to `./axiomdb.db`.

### SQL execution

```
axiomdb> SELECT * FROM users WHERE age > 25;
+----+-------+-----+
| id | name  | age |
+----+-------+-----+
|  1 | Alice |  30 |
|  3 | Carol |  28 |
+----+-------+-----+
2 rows (1ms)

axiomdb> INSERT INTO users VALUES (4, 'Dave', 22);
1 row affected (0ms)

axiomdb> CREATE TABLE orders (id INT, user_id INT);
OK (2ms)
```

### Multi-line queries

Input accumulates until a `;` is seen at the end of a line (after trimming).
The secondary prompt `   ->` shows continuation:

```
axiomdb> SELECT id,
   ->   name
   ->   FROM users;
+----+-------+
...
```

### Dot commands

```
axiomdb> .help
  .help               Show this message
  .tables             List all user tables
  .schema [table]     Show column definitions for table (all tables if omitted)
  .open <path>        Open a different database file
  .quit               Exit (also: .exit, \q, Ctrl-D)

axiomdb> .tables
orders
users

axiomdb> .schema users
Table: users
  id    INT       NOT NULL
  name  TEXT      nullable
  age   INT       nullable
  Indexes: (none)

axiomdb> .schema
Table: orders
  id       INT  NOT NULL
  user_id  INT  nullable

Table: users
  id    INT       NOT NULL
  name  TEXT      nullable
  age   INT       nullable
```

### Error display

```
axiomdb> SELECT * FROM nonexistent;
Error [42P01]: table 'nonexistent' not found
```

### Exit

```
axiomdb> .quit
Bye.
```

Also exits on Ctrl-D (EOF) silently.

---

## Inputs / Outputs

### CLI arguments

```
axiomdb-cli [path]
```

- `path`: path to `.db` file (default: `./axiomdb.db`). The WAL file is inferred
  as `<path-without-extension>.wal` or `<path>.wal`.

### stdin/stdout

- Reads from `stdin` line by line using `std::io::stdin().lock()`.
- Writes prompt to `stdout` when stdin is a TTY (`isatty` check via `std::io::IsTerminal`).
- In non-interactive mode (pipe/file), no prompt is printed — only results.

### Exit codes

- `0` — clean exit (`.quit`, EOF)
- `1` — fatal error (failed to open database)

---

## Table formatting

Results are printed as ASCII tables:

```
+--------+----------+-----+
| col1   | col2     | col3 |
+--------+----------+-----+
| value1 | value2   |   42 |
+--------+----------+-----+
N rows (Xms)
```

Rules:
- Column widths are the maximum of the header length and the longest value in that column.
- Numeric types (`INT`, `BIGINT`, `REAL`) are right-aligned. All others are left-aligned.
- `NULL` is displayed as the literal string `NULL` (no quotes).
- `BOOL` is displayed as `true` / `false`.
- `UUID` is displayed as the canonical hyphenated form.
- `BYTES` / `BLOB` is displayed as `<N bytes>` (not raw binary).
- Strings longer than 64 characters are truncated with `…` to keep the table readable.

---

## Use cases

### 1. Interactive query
```
axiomdb ./data/prod.db
axiomdb> SELECT COUNT(*) FROM events;
+----------+
| count(*) |
+----------+
|    15420 |
+----------+
1 row (3ms)
```

### 2. Non-interactive (pipe)
```bash
echo "SELECT COUNT(*) FROM users;" | axiomdb-cli ./data/dev.db
+----------+
| count(*) |
+----------+
|       42 |
+----------+
1 row (1ms)
```

### 3. Script file
```bash
axiomdb-cli ./data/dev.db < migration.sql
OK (2ms)
OK (1ms)
2 rows affected (0ms)
```

### 4. .schema inspection
```
axiomdb> .schema users
Table: users
  id    BIGINT  NOT NULL  AUTO_INCREMENT
  email TEXT    NOT NULL
  name  TEXT    nullable
  Indexes:
    uq_users_email (email) UNIQUE
```

---

## Acceptance criteria

- [ ] `axiomdb-cli <path>` opens the database and shows the interactive prompt
- [ ] SQL statements ending with `;` are executed and results displayed as ASCII table
- [ ] Multi-line statements accumulate until `;` is found
- [ ] DML results show `N rows affected`; DDL shows `OK`; errors show the message
- [ ] `.quit`, `.exit`, `\q` exit cleanly; Ctrl-D (EOF) also exits
- [ ] `.tables` lists all user tables alphabetically
- [ ] `.schema [table]` shows columns (name, type, nullable, auto_increment) and indexes
- [ ] `.help` shows available commands
- [ ] Non-interactive mode (piped input) works without printing prompts
- [ ] Timing in milliseconds shown after each result
- [ ] `cargo build --release -p axiomdb-cli` produces a standalone binary

---

## Out of scope

- Readline history / arrow keys / tab completion — Phase 4.15b
- `\e` editor integration (external $EDITOR) — future
- `.import` / `.export` CSV — future
- Color output — future
- `EXPLAIN` display — future

---

## Dependencies

- `axiomdb-network` (for `Database::open`, `execute_query`) ✅
- `axiomdb-sql` (for `QueryResult`, `Value`, `ColumnMeta`) ✅
- `std::io::IsTerminal` (stable since Rust 1.70) — no external crate needed
- No async runtime needed — all I/O is synchronous in the CLI

✅ Spec written. You can now switch to `/effort medium` for the Plan phase.
