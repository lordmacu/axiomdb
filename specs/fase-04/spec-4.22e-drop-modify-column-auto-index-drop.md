# Spec: 4.22e — ALTER TABLE DROP/MODIFY COLUMN auto-index-drop

## What to build (not how)
Implement index-aware `ALTER TABLE ... DROP COLUMN` and `ALTER TABLE ... MODIFY COLUMN` so these operations work when the table has secondary indexes.

Required behavior:

- `DROP COLUMN` automatically removes every affected secondary index instead of returning `NotImplemented`.
- `MODIFY COLUMN` automatically rebuilds every affected secondary index so lookups, uniqueness checks, partial-index membership, and clustered secondary bookmarks remain correct after the type/nullability change.
- The behavior must work for both heap tables and clustered tables.
- The operation must preserve surviving index metadata: name, uniqueness, predicate, fillfactor, INCLUDE columns, index type, and BRIN `pages_per_range`.
- If the target column participates in the PRIMARY KEY, a foreign key constraint, or a CHECK constraint, the operation must fail clearly instead of leaving broken metadata behind.

Affected index means any secondary index whose definition depends on the column through:

- key columns
- INCLUDE columns
- partial-index predicate

Heap-specific rule:

- Heap rewrites may change physical `RecordId`s, so any surviving secondary index must remain valid after the statement completes.

Clustered-specific rule:

- Clustered rewrites keep the primary key stable; only secondary indexes whose definition depends on the changed/dropped column need rebuild/drop.

## Inputs / Outputs
- Input:
  - `ALTER TABLE ... DROP COLUMN [IF EXISTS]`
  - `ALTER TABLE ... MODIFY COLUMN ...`
  - current `TableDef`
  - current catalog `ColumnDef`, `IndexDef`, `ConstraintDef`, `FkDef`
  - active `StorageEngine`, `TxnManager`, `ConnectionTxn`
- Output:
  - successful ALTER result (`QueryResult::Empty` for plain drop/modify)
  - updated catalog + valid secondary-index roots
- Errors:
  - `ColumnNotFound` / no-op for `IF EXISTS`
  - clear error if the column is part of the PRIMARY KEY
  - clear error if the column is referenced by FK or CHECK metadata
  - existing type coercion / duplicate / nullability errors from row rewrite or index rebuild

## Use cases
1. A heap table has `UNIQUE(email)` and `ALTER TABLE t DROP COLUMN email`; the statement succeeds and the secondary index disappears automatically.
2. A heap table has `INDEX(score)` and `ALTER TABLE t MODIFY COLUMN score BIGINT`; the statement succeeds and `WHERE score = ...` still uses a valid index afterward.
3. A clustered table has `UNIQUE(email)` and `ALTER TABLE t MODIFY COLUMN email TEXT`; the clustered secondary index is rebuilt and remains queryable.
4. A partial index `CREATE INDEX active_idx ON t(status) WHERE deleted_at IS NULL` is dropped automatically when `deleted_at` is dropped.
5. `ALTER TABLE child DROP COLUMN parent_id` fails if `parent_id` is referenced by a foreign key.
6. `ALTER TABLE t MODIFY COLUMN id BIGINT` fails if `id` belongs to the PRIMARY KEY.

## Acceptance criteria
- [ ] `ALTER TABLE t DROP COLUMN c` auto-drops affected secondary indexes on heap tables
- [ ] `ALTER TABLE t DROP COLUMN c` auto-drops affected secondary indexes on clustered tables
- [ ] surviving heap secondary indexes remain valid after `DROP COLUMN` rewrites
- [ ] `ALTER TABLE t MODIFY COLUMN c ...` rebuilds affected secondary indexes on heap tables
- [ ] `ALTER TABLE t MODIFY COLUMN c ...` rebuilds affected secondary indexes on clustered tables
- [ ] secondary indexes with `predicate`, `include_columns`, `fillfactor`, `index_type`, and `pages_per_range` preserve their metadata after rebuild
- [ ] unique secondary indexes still enforce duplicates after `MODIFY COLUMN`
- [ ] dropping/modifying a PRIMARY KEY column is rejected
- [ ] dropping/modifying a column referenced by FK metadata is rejected
- [ ] dropping/modifying a column referenced by CHECK metadata is rejected
- [ ] targeted SQL integration tests cover heap + clustered behavior
- [ ] MySQL wire smoke covers at least one successful auto-drop and one successful auto-rebuild path

## Out of scope
- Automatically rewriting or dropping FOREIGN KEY constraints
- Automatically rewriting or dropping CHECK constraints
- `ALTER TABLE ... DROP PRIMARY KEY`
- Changing PRIMARY KEY column types/layouts as part of this subphase
- Non-ALTER DDL operations

## Dependencies
- Phase 4.22 basic ALTER TABLE row-rewrite support
- Phase 4.22b constraint catalog (`axiom_constraints`)
- Phase 4.22c ADD PRIMARY KEY / clustered promotion
- Phase 39.19 clustered rebuild + clustered storage primitives
- Phase 40.3b clustered add/drop/modify row rewrite path
- Partial-index predicate compiler / dependency collector
- Catalog index-root update and deferred-free page support

## ⚠️ DEFERRED
- `ALTER TABLE ... ADD COLUMN` on heap tables with existing secondary indexes shares the same RID-rewrite pressure. This spec does not promise that path; if the implementation exposes a reusable helper, the remaining gap must stay visible in `docs/progreso.md`.
