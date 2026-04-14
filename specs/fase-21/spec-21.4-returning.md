# Spec: 21.4 — `RETURNING` clause

## What to build

Add `RETURNING <select_item> [, ...]` (and `RETURNING *`) to all three
DML statements. When present, the statement returns the projected rows
instead of the usual affected-row count:

```sql
INSERT INTO users (email) VALUES ('a@x'), ('b@x')
 RETURNING id, email;

UPDATE orders SET status = 'paid'
 WHERE id = 42
 RETURNING id, status, updated_at;

DELETE FROM sessions
 WHERE expires_at < NOW()
 RETURNING id;
```

PG-standard. MariaDB supports it. MySQL 8 does **not** (it's the single
most frequently requested MySQL parity gap). Critical for ORMs: Prisma,
Sequelize, SQLAlchemy all rely on `RETURNING` to fetch auto-generated
IDs and modified-row state without a second query.

## Inputs / outputs

### Grammar

```
insert_stmt := INSERT ... VALUES(...)|SELECT|... [ RETURNING return_list ]
update_stmt := UPDATE ... SET ... [ WHERE ... ] [ ORDER BY ... ]
               [ LIMIT ... ] [ RETURNING return_list ]
delete_stmt := DELETE ... [ WHERE ... ] [ ORDER BY ... ] [ LIMIT ... ]
               [ RETURNING return_list ]

return_list := '*' | return_item (',' return_item)*
return_item := expr [ AS alias ]
             | qualified '.' '*'
             | qualified '.' alias
```

`RETURNING *` projects every column of the target table. Aliases and
expressions (`RETURNING id * 2 AS doubled_id`) work as in SELECT list.

### AST

Each DML struct gains:

```rust
pub returning: Vec<SelectItem>,   // empty = no RETURNING
```

Reusing the existing `SelectItem` enum avoids any new projection
primitive.

### Executor semantics

| Stmt | Row source for `RETURNING` projection |
|---|---|
| `INSERT` | The **new** rows as written to storage (post-auto-increment, post-DEFAULT, post-generated) |
| `UPDATE` | The **new** (post-update) row values |
| `DELETE` | The **deleted** rows, captured **before** storage deletes them |

For `INSERT ... ON DUPLICATE KEY UPDATE` (21.5e) / `ON CONFLICT DO
UPDATE` (21.5) the projected row is the post-conflict-resolution row
(PG parity). Out of scope for this subphase since the UPSERT family
lands in 21.5 — interaction tested there.

When `returning` is empty → `QueryResult::Affected { count, ... }` as
today.
When `returning` is non-empty → `QueryResult::Rows { columns, rows }`
with one row per mutated row. Columns come from `SelectItem`
resolution against the target table.

### Ordering

Result row order matches storage iteration order (PG: undefined;
this subphase mirrors current iteration). Deterministic under
`ORDER BY ... LIMIT` clauses since UPDATE/DELETE already honor them.

## Use cases

```sql
-- ORM: fetch auto-generated ID.
INSERT INTO users (email) VALUES ('a@x') RETURNING id;

-- Delete-and-archive in one shot.
WITH d AS (
  DELETE FROM events WHERE expires_at < NOW() RETURNING *
) INSERT INTO event_archive SELECT * FROM d;

-- Update-only-if-changed fingerprint.
UPDATE docs SET content = $1 WHERE id = $2 AND content != $1
 RETURNING id;
-- Empty result set signals "no change".

-- Bulk insert with generated IDs.
INSERT INTO items (name) VALUES ('a'), ('b'), ('c')
 RETURNING id, name;
```

## Acceptance criteria

- [ ] `INSERT ... RETURNING col1, col2` returns one row per new row.
- [ ] `INSERT ... RETURNING *` returns all columns.
- [ ] Auto-increment PK values are visible in `RETURNING id`.
- [ ] `UPDATE ... RETURNING *` returns post-update values.
- [ ] `DELETE ... RETURNING *` returns pre-delete values.
- [ ] Expressions and aliases work (`RETURNING id * 2 AS doubled`).
- [ ] Qualified projection: `RETURNING t.*` (where `t` is the target).
- [ ] Empty mutation (zero rows matched) returns zero-row result set,
      not `Affected{count:0}`.
- [ ] Without `RETURNING`, behavior unchanged — `QueryResult::Affected`.
- [ ] Integration tests in `tests/integration_returning.rs` (10+
      tests).
- [ ] 2 wire smoke assertions (INSERT RETURNING, DELETE RETURNING).

## Out of scope

- `RETURNING OLD / NEW` qualifiers (PG 18 addition). Deferred —
  no ORM uses it broadly yet.
- `RETURNING` with set-returning functions in the projection
  (`RETURNING unnest(array_col)`). Deferred with SRF-in-projection.
- `RETURNING` on MERGE (21.5) — covered as part of 21.5.

## Cross-engine

- **PostgreSQL** `gram.y:13149` — grammar has been in place since
  PG 8.2. Production `returning_clause := RETURNING target_list`.
- **MariaDB** — supports for INSERT / UPDATE / DELETE (10.0+).
- **MySQL 8** — does NOT support. Parser-level addition gives
  AxiomDB a MySQL-beyond-8 superset.
- **SQLite** — supports via syntactic extension in 3.35.

## Dependencies

- `SelectItem` enum in `ast.rs` (existing).
- DML executors in `insert_helpers.rs`, `update_entry.rs`,
  `executor/mod.rs` (delete) — need row-capture hooks.
- `project_row` / `build_derived_output_columns` in
  `executor/select_core.rs` — reuse for RETURNING projection.
