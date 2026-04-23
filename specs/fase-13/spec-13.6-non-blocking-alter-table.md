# Spec: 13.6 — Non-blocking ALTER TABLE

Phase: 13 — Advanced PostgreSQL
Task: 13.6 Non-blocking ALTER TABLE
Status: implemented

## Context

`ALTER TABLE` already works in AxiomDB for heap and clustered tables, but the
rewrite-heavy variants (`ADD COLUMN`, `DROP COLUMN`, `MODIFY COLUMN`, `ADD
PRIMARY KEY`) still execute as a blocking DDL path:

- the MySQL wire layer takes `catalog_lock.write()` for schema-changing DDL
- the executor rewrites rows inline on the live table root
- concurrent reads/writes against the same table wait behind the DDL

`docs/progreso.md` describes `13.6` as:

> shadow table + WAL delta + atomic swap

That wording implies a fuller online-DDL engine than the current runtime
coordination supports today.

## Problem

Long-running heap rewrites block normal traffic because schema cutover and row
copy happen inside the same exclusive DDL window.

For large tables, this is the wrong UX even if the final semantics are correct.

## Goal

Deliver a first real non-blocking `ALTER TABLE` slice that allows long-running
table rewrites to happen off the live table while preserving a short, bounded
cutover window.

## Delivered cut

Implemented as specified, with one explicit narrowing:

- heap tables only
- exactly one rewrite-heavy column op per statement:
  `ADD COLUMN`, `DROP COLUMN`, or `MODIFY COLUMN`
- reads continue during shadow copy
- concurrent writes to the target table fail fast with `LockTimeout`
- cutover publishes new root/schema/index metadata atomically at the end

Still deferred:

- concurrent-writer replay / WAL delta
- clustered tables
- multi-op non-blocking ALTER statements
- online `ADD PRIMARY KEY`

## Recommended cut

Implement **reader-friendly shadow rebuild + atomic cutover** for heap-table
column rewrites:

- build a shadow heap relation with the new column layout
- copy rows into the shadow relation outside the long exclusive DDL window
- take a brief exclusive cutover lock only for final validation + root/catalog swap
- allow concurrent readers to keep using the old table during the copy phase
- reject concurrent writes to the target table during the rewrite window

This is intentionally narrower than full online DDL with write replay. It gives
real operational value without requiring a new WAL-delta subsystem in the same
subphase.

## Non-goals

- Not implementing generic WAL-delta capture/replay for concurrent writes.
- Not delivering full PostgreSQL/MySQL-style “online DDL with concurrent
  writers” in this slice.
- Not covering clustered-table rewrites in the first cut.
- Not covering every `ALTER TABLE` variant; start with rewrite-heavy heap
  column ops only.
- Not exposing asynchronous background jobs or progress reporting.

## Public SQL surface

No new SQL syntax is required.

The visible contract is behavioral:

- supported heap `ALTER TABLE` rewrite operations no longer monopolize the
  table for the whole row-copy duration
- concurrent reads continue during the shadow-copy phase
- concurrent writes to the table being altered fail fast or wait only at the
  bounded cutover point, depending on the chosen guard semantics

## Scope

### In scope

- heap `ALTER TABLE ADD COLUMN`
- heap `ALTER TABLE DROP COLUMN`
- heap `ALTER TABLE MODIFY COLUMN`
- shadow-table materialization + copy
- atomic metadata/root cutover
- bounded read/write coordination during copy vs swap
- SQL + wire acceptance coverage

### Out of scope

- clustered-table online rewrite
- `ALTER TABLE ADD PRIMARY KEY` online migration
- online index creation beyond what existing `CREATE INDEX` already does
- background worker framework
- progress views / admin introspection for the rewrite job

## Semantics

- The source table remains readable while the shadow copy is being built.
- The shadow table is not user-visible before cutover.
- The cutover is atomic from the perspective of subsequent statements:
  readers see either the old table or the new table, never a mixed schema.
- If the shadow build fails, the live table remains unchanged.
- If cutover validation fails, the shadow table is discarded and the live table
  remains authoritative.
- Session/local schema caches and prepared-plan invalidation still occur exactly
  once at successful cutover.

## Design sketch

1. Analyze the `ALTER TABLE` operation into a rewrite plan.
2. Allocate a shadow relation with the destination schema.
3. Snapshot-scan the live heap table and copy transformed rows into shadow.
4. Hold a table-local “rewrite in progress” guard so concurrent writers to that
   table do not mutate the live table during the copy window.
5. Acquire the short exclusive DDL cutover window.
6. Revalidate no unsupported state drift occurred.
7. Swap catalog-visible root/schema metadata to the shadow relation.
8. Drop/retire the old heap root and invalidate caches once.

## Approach options

### Approach A — Shadow copy + writer quiescence + atomic swap

Pros:

- fits the current `catalog_lock` + executor model
- materially reduces reader downtime
- no WAL-delta capture/replay subsystem required
- keeps risk bounded to heap rewrite operations

Cons:

- concurrent writes to the target table are still blocked/rejected during the
  rewrite window
- not full “online DDL” in the MySQL/InnoDB sense
- roadmap wording needs to be explicit about the bounded first cut

### Approach B — Full shadow copy + WAL delta replay + atomic swap

Pros:

- closest to the literal roadmap wording
- allows concurrent reads and writes during the copy phase
- stronger foundation for later online schema changes

Cons:

- much more invasive
- needs write capture/replay semantics for every touched DML path
- interacts deeply with MVCC, savepoints, rollback, and statement staging
- likely too large for a single Phase 13 subphase

## Recommended approach

Approach A.

It closes a real operational gap now and keeps the implementation coherent with
the current server/executor architecture. Approach B should stay deferred until
there is dedicated infrastructure for write replay and online schema jobs.

## Edge cases

- [ ] concurrent `SELECT` during shadow copy sees stable old schema
- [ ] concurrent `INSERT/UPDATE/DELETE` on target table is rejected or blocked
      predictably during rewrite
- [ ] failure during copy leaves old table untouched
- [ ] failure during cutover leaves old table untouched
- [ ] cache/schema_version invalidation happens once at successful swap
- [ ] temp/unlogged/materialized relations are either excluded or handled explicitly
- [ ] FK/index metadata remains consistent after cutover

## Done criteria

- [x] bounded non-blocking heap `ALTER TABLE` rewrite path exists
- [x] supported reads continue during shadow copy
- [x] target-table writes are coordinated safely during rewrite
- [x] cutover is atomic and failure-safe
- [x] dedicated tests cover concurrent read behavior and failure rollback
- [x] wire-visible acceptance coverage exists
- [x] docs/memory explain the bounded contract honestly

## References

- `docs/progreso.md`
- `db.md`
- `crates/axiomdb-sql/src/executor/ddl_alter_column.rs`
- `crates/axiomdb-network/src/mysql/handler.rs`
- `crates/axiomdb-network/src/mysql/shared_db.rs`
