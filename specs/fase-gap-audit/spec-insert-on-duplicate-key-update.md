# Spec: INSERT ... ON DUPLICATE KEY UPDATE (MySQL upsert)

## What to build (not how)

MySQL's `INSERT ... ON DUPLICATE KEY UPDATE` (ODKU) — a per-row upsert
that, when the incoming row would violate a PRIMARY KEY or UNIQUE
constraint, updates the conflicting row with a user-supplied
assignment list instead of erroring. Coexists with `INSERT` and
`REPLACE` on the same statement surface. Supports the `VALUES(col)`
pseudo-function in the assignment list to reference the would-have-
been-inserted row.

Behavior must match MariaDB's observable semantics at statement level
(researched in `sql/sql_insert.cc::write_record` DUP_UPDATE branch),
using the same proactive-lookup model we applied to REPLACE INTO.

## Inputs / Outputs

### Inputs

```sql
[/* INSERT [IGNORE] [LOW_PRIORITY|HIGH_PRIORITY|DELAYED] INTO */] table_ref
    [(col1, col2, ...)]
    {VALUES (expr,...), (...) | DEFAULT VALUES | SET col=val,... | SELECT ...}
    ON DUPLICATE KEY UPDATE
        col1 = rhs1,
        col2 = rhs2,
        ...
```

`rhs` may be any expression. Two kinds of column references inside
`rhs` carry distinct meaning:

- `col` (bare, qualified, or via table alias) → the **existing row**'s
  value for that column (as it stood before the UPDATE).
- `VALUES(col)` → the **incoming row**'s value for that column
  (would-have-been-inserted). `VALUES(col)` is valid **only** inside
  the ODKU assignment list; anywhere else it stays a parse/resolve
  error (matches MariaDB's `IN_UPDATE_ON_DUP_KEY` parsing guard).

`INSERT IGNORE ... ON DUPLICATE KEY UPDATE` is accepted syntactically —
`IGNORE` only ever suppresses non-conflict errors (NOT NULL, CHECK,
FK child-insert, etc.); the ODKU clause handles unique conflicts.

### Outputs

`QueryResult::Affected { count, last_insert_id }` where `count` is
summed across all rows in the statement using MySQL's per-row rule:

| Per-row outcome                        | Contribution to `count` |
|----------------------------------------|-------------------------|
| Inserted (no conflict)                 | 1                       |
| Updated and values changed             | 2                       |
| Matched but UPDATE left the row equal  | 0                       |

`last_insert_id`:
- Insert branch + AUTO_INCREMENT generated → first generated AI value.
- UPDATE branch → not modified (the proposed row's AI is discarded).

## Use cases

1. **Counter increments** — `INSERT INTO hits (page, n) VALUES ('/', 1)
   ON DUPLICATE KEY UPDATE n = n + 1;` atomically bumps a hit counter.
2. **Upsert with proposed value** — `INSERT INTO kv (k,v) VALUES
   ('foo', 'new') ON DUPLICATE KEY UPDATE v = VALUES(v);` overwrites
   the existing value using the row the caller provided.
3. **Idempotent catalog ingest** — bulk `INSERT ... ON DUPLICATE KEY
   UPDATE` from an external feed, where the ODKU clause merges updated
   attributes without deleting / re-inserting (unlike REPLACE, this
   preserves the existing RID, FK children, and downstream materialized
   state).
4. **ORM upsert** — Doctrine, SQLAlchemy-MySQL, Rails, etc. all emit
   ODKU for their "save or update" paths.
5. **Touch-update-at** — `INSERT INTO audit (id, note, updated_at)
   VALUES (1, 'x', NOW()) ON DUPLICATE KEY UPDATE updated_at = VALUES
   (updated_at);` keeps a row's timestamp current.

## Acceptance criteria

- [ ] **Parsing**: the clause `ON DUPLICATE KEY UPDATE a = expr, b =
      expr ...` is accepted after every existing INSERT source form
      (`VALUES`, `DEFAULT VALUES`, `SET`, `SELECT`), with or without
      `IGNORE` / priority prefixes.
- [ ] **`VALUES(col)` pseudo-function**: parses inside the ODKU
      assignment list; the RHS evaluates to the proposed row's value
      for `col`. Referencing an unknown column raises
      `DbError::ColumnNotFound`.
- [ ] **`VALUES(col)` outside ODKU**: the existing scalar function
      `VALUES(...)` (none in MySQL but reserved) stays a normal
      function-call or parse error — no new expression form is exposed
      outside ODKU.
- [ ] **No conflict → plain INSERT** behavior: `count += 1`, FK child
      validation, all indexes maintained, `last_insert_id` set from
      AI column.
- [ ] **Conflict on PRIMARY KEY**: UPDATE branch runs; the existing
      row is updated in place (RID preserved); secondary-index entries
      are maintained through the standard index-maintenance path; FK
      child-update enforcement runs on any changed FK columns;
      `count += 2` if values changed, `0` if unchanged.
- [ ] **Conflict on non-PK UNIQUE index**: same as PK (first
      conflicting unique index wins — MariaDB behavior).
- [ ] **Conflict on composite UNIQUE index**: full tuple conflict is
      resolved by updating the matching row.
- [ ] **NULL in unique-key column**: no conflict (MATCH SIMPLE
      behavior), row is inserted as normal, no UPDATE branch.
- [ ] **Multi-index conflict**: when the new row could displace
      different rows via different unique indexes, the FIRST
      conflicting index (in catalog order) wins — the matched row is
      updated; the other unique index is not touched. If the UPDATE
      clause itself creates a *new* unique conflict (row-after-update
      collides with a third row on some unique index), the statement
      errors out (post-update re-check via index maintenance).
- [ ] **FK enforcement on UPDATE branch**:
  - child-insert validation on the updated row's FK columns
    (reference still valid),
  - parent-update enforcement on any referenced parent key columns
    that moved (CASCADE / SET NULL / SET DEFAULT / RESTRICT fire
    exactly as for a plain UPDATE).
- [ ] **`last_insert_id`**: only set on INSERT-branch rows that
      generated an AUTO_INCREMENT. UPDATE-branch rows do not alter
      `last_insert_id` (MariaDB `insert_id_for_cur_row = 0`).
- [ ] **AUTO_INCREMENT reclamation**: AI values generated for a row
      that goes down the UPDATE branch are discarded — the next insert
      starts from the correct counter value (matches MariaDB's
      `prev_insert_id_for_cur_row` restore).
- [ ] **Clustered tables**: return `DbError::NotImplemented` cleanly.
      Deferred to follow-up, mirrors the REPLACE MVP scope.
- [ ] **Batch `VALUES (...), (...), (...)`**: per-row independent
      conflict handling; `count` is the sum across rows.
- [ ] **`INSERT ... SELECT ... ON DUPLICATE KEY UPDATE`**: supported;
      the SELECT is materialized before the per-row UPDATE/INSERT
      loop (matches REPLACE's self-reference guarantee).
- [ ] **`INSERT IGNORE ... ON DUPLICATE KEY UPDATE`**: accepted; the
      two flags coexist — IGNORE continues to suppress NON-conflict
      errors (NOT NULL, CHECK, FK child-insert), ODKU still handles
      unique conflicts. The parser does NOT forbid the combination.
- [ ] **Integration tests** in `tests/integration_insert_on_dup.rs`
      cover every bullet above.
- [ ] `cargo test --workspace` clean.
- [ ] `cargo clippy --workspace -- -D warnings` clean.
- [ ] `cargo fmt --check` clean.

## Out of scope

- **PostgreSQL `ON CONFLICT ... DO UPDATE SET ... WHERE <pred>`**: the
  WHERE filter is a PG extension and not part of MySQL ODKU. Not
  implemented here. If added later, reserve the AST room.
- **PG `EXCLUDED.col` syntax**: PG's spelling of `VALUES(col)`. Not
  supported; MySQL's `VALUES(col)` is the canonical MySQL-compat form.
- **PG `NULLS NOT DISTINCT`** unique-index option: treats all NULLs
  as equal. Not supported; we keep MySQL's "NULL never conflicts"
  semantics.
- **`CLIENT_FOUND_ROWS` flag behavior**: MariaDB alters `affected_rows`
  to `copied + touched` instead of `copied + updated` under this
  client capability flag. Ignored — we always report `copied +
  updated` (changed only). Matches PG and MySQL default.
- **Triggers**: AxiomDB has no trigger system yet (Phase 16.3). When
  triggers land the INSERT path must fire BEFORE/AFTER INSERT on the
  insert branch and BEFORE/AFTER UPDATE on the update branch — the
  ODKU helper will inherit this for free since it uses the existing
  low-level insert + update entry points.
- **Clustered-table ODKU**: deferred to a follow-up spec. MVP returns
  `NotImplemented`, identical scope to REPLACE INTO.
- **Binlog / replication**: AxiomDB has no replication layer.

## Dependencies

- Existing INSERT path (`executor/insert_heap_ctx.rs` — heap, which
  already supports single-row and batch modes).
- Existing UPDATE path (`executor/update_ctx.rs`) — the ODKU helper
  will re-use the low-level update primitives (`TableEngine::
  update_row` via the eval path, or the more granular `HeapChain` +
  `index_maintenance` combo).
- Unique-index lookup (`BTree::lookup_in`, `clustered_tree::lookup`)
  + bloom filter (`bloom.might_exist`).
- Partial-index predicate compiler (`partial_index::
  compile_index_predicates`).
- FK enforcement — child update (`check_fk_child_update`) and parent
  update (`enforce_fk_on_parent_update`).
- The `replace_helpers.rs` unique-index probe logic is a sibling —
  the ODKU helper will share the "find conflicting row" subroutine
  by extracting it into a common helper.
- `Expr::InsertValue { col_name: String }` new AST variant and eval
  support.

## Effort for next step

- **Plan: medium** — parser extension is mechanical, the executor
  helper is the substantive work (evaluating assignments against a
  dual-row context, per-row count bookkeeping, AI reclamation on
  UPDATE branch, post-update unique re-check).
