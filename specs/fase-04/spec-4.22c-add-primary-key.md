# Spec: 4.22c — ALTER TABLE ADD PRIMARY KEY

## What to build (not how)
Implement `ALTER TABLE t ADD [CONSTRAINT name] PRIMARY KEY (col [, ...])` for existing heap tables.

The statement must:

- validate that the target table does not already have a primary key
- validate that every referenced column exists
- reject the operation if any existing visible row has `NULL` in any primary-key column
- reject the operation if existing visible rows contain duplicate primary-key tuples
- install primary-key metadata for the table
- make all primary-key columns `NOT NULL` in catalog metadata
- rewrite the table from heap layout to clustered layout using the new primary key order
- rebuild existing secondary indexes so they use clustered primary-key bookmarks
- preserve committed rows and existing secondary-index behavior after the rewrite

The feature is MySQL-compatibility oriented: ORMs and migrations that create a heap table first and add the primary key later must succeed when the data already satisfies primary-key rules.

## Inputs / Outputs
- Input: analyzed `AlterTableStmt` with one `AlterTableOp::AddConstraint(TableConstraint::PrimaryKey { name, columns })`
- Input: heap table resolved from catalog, plus its current column/index metadata
- Output: successful ALTER result for a heap table with a new primary key installed
- Output: table storage layout switched from heap to clustered
- Output: `QueryResult::Affected { count, last_insert_id: None }` where `count` is the number of rows rewritten into clustered storage
- Errors:
  - `TableNotFound` if the target table does not exist
  - `ColumnNotFound` if any primary-key column does not exist
  - `InvalidValue` if any existing row contains `NULL` in a primary-key column
  - `UniqueViolation` if existing rows contain duplicate primary-key tuples
  - error if the table already has a primary key
  - propagated storage/catalog/WAL errors if the rewrite cannot complete

## Use cases
1. A migration creates `users(id INT, email TEXT)` and later runs `ALTER TABLE users ADD PRIMARY KEY (id)`; the table becomes clustered and existing rows remain queryable.
2. `ALTER TABLE users ADD PRIMARY KEY (id)` on an empty heap table succeeds and produces an empty clustered table with a primary index.
3. `ALTER TABLE users ADD PRIMARY KEY (email)` fails when at least one existing row has `email = NULL`.
4. `ALTER TABLE users ADD PRIMARY KEY (email)` fails when two existing rows have the same `email`.
5. `ALTER TABLE users ADD PRIMARY KEY (id)` preserves pre-existing secondary indexes on other columns after the rewrite.
6. `ALTER TABLE users ADD PRIMARY KEY (region, code)` succeeds for a composite key and orders the clustered storage by the encoded composite tuple.

## Acceptance criteria
- [ ] `ALTER TABLE t ADD PRIMARY KEY (id)` succeeds on a populated heap table whose `id` values are unique and non-NULL
- [ ] successful `ADD PRIMARY KEY` changes the table storage layout to clustered
- [ ] successful `ADD PRIMARY KEY` makes the primary-key column(s) non-nullable in catalog metadata
- [ ] existing committed rows are still returned correctly after the rewrite
- [ ] existing secondary indexes still return correct results after the rewrite
- [ ] `ALTER TABLE t ADD PRIMARY KEY (id)` succeeds on an empty heap table
- [ ] `ALTER TABLE t ADD PRIMARY KEY (id)` fails if any existing row has `id = NULL`
- [ ] `ALTER TABLE t ADD PRIMARY KEY (id)` fails if existing rows contain duplicate `id` values
- [ ] `ALTER TABLE t ADD PRIMARY KEY (a, b)` supports composite primary keys
- [ ] `ALTER TABLE t ADD PRIMARY KEY (id)` fails if the table already has a primary key
- [ ] failure during the rewrite does not leave stray primary-key metadata or reachable orphan pages
- [ ] MySQL wire smoke covers `ALTER TABLE ... ADD PRIMARY KEY (...)`

## Out of scope
- Reusing an existing UNIQUE index as the physical primary-key structure without rebuilding
- Online / non-blocking `ADD PRIMARY KEY`
- `DROP PRIMARY KEY` or replacing one primary key with another in the same statement
- Heapless/clustered-to-clustered primary-key replacement
- InnoDB-style in-place `ALGORITHM=INPLACE` / `ALGORITHM=COPY` knobs

## Dependencies
- Phase 39.19 clustered rebuild path (`ALTER TABLE ... REBUILD`)
- ALTER TABLE constraint dispatch from Phase 4.22b
- Heap index build helpers for precomputing a unique B-tree root
- CatalogWriter support for creating indexes and replacing column metadata
- B-tree page cleanup helper for rollback-on-error of provisional index roots
