# Spec: 13.1 — Materialized views

## Status

Approved.

Phase `13.1` is the first materialized-view slice in AxiomDB. The repo does
not yet implement regular `CREATE VIEW` / `DROP VIEW` (`20.1`), so this task
cannot depend on a pre-existing logical-view layer.

The bounded contract for `13.1` is therefore:

- `CREATE MATERIALIZED VIEW name AS SELECT ...`
- `REFRESH MATERIALIZED VIEW name`
- `DROP MATERIALIZED VIEW name`

implemented as a **catalog-owned materialized relation** that stores rows like a
real table and remembers its defining `SELECT`.

## What to build

### SQL surface

Supported statements:

- `CREATE MATERIALIZED VIEW mv_name AS SELECT ...`
- `REFRESH MATERIALIZED VIEW mv_name`
- `DROP MATERIALIZED VIEW mv_name`
- `DROP MATERIALIZED VIEW IF EXISTS mv_name`

Initial creation executes the `SELECT`, materializes all rows, infers output
columns the same way CTAS does today, and persists both:

- the physical rows
- the defining SQL text needed for future refresh

`REFRESH MATERIALIZED VIEW` performs a **full rebuild**, not incremental
maintenance.

## Expected behavior

### Creation

- `CREATE MATERIALIZED VIEW mv AS SELECT ...` creates a new relation visible in
  catalog and metadata listings.
- The stored result rows reflect the `SELECT` result at creation time.
- Output columns are derived from the `SELECT` result metadata, using the same
  bounded inference rule as CTAS for the first implementation.
- Materialized views are heap-backed in this subphase; no primary key or
  secondary indexes are synthesized automatically.
- The defining query must be persisted in catalog metadata so refresh can
  rerun it later.

### Refresh

- `REFRESH MATERIALIZED VIEW mv` reruns the persisted defining query and
  replaces the materialized contents atomically from the SQL user's point of
  view.
- `REFRESH` is full rebuild only; no incremental maintenance, no
  `CONCURRENTLY`, and no partial refresh in this subphase.
- The materialized view's column shape must remain compatible with its stored
  definition. If the defining query can no longer be resolved, refresh errors.

### Drop

- `DROP MATERIALIZED VIEW mv` removes the materialized relation and its
  associated metadata.
- `DROP MATERIALIZED VIEW IF EXISTS mv` succeeds when the object is absent.
- Dropping a base table used by a materialized view is out of scope for
  dependency-tracked invalidation in this subphase; the immediate contract is
  only that a later `REFRESH` fails cleanly if the source query no longer
  resolves.

### Metadata / visibility

- `SHOW FULL TABLES` must report materialized views distinctly instead of
  always returning `BASE TABLE`.
- Information-schema/table metadata must expose materialized views as a
  relation type distinct from ordinary base tables.
- Ordinary `SELECT ... FROM mv` should work through the existing table path,
  because a materialized view is stored physically.

## Acceptance criteria

- [ ] Dedicated `13.1` spec and plan exist.
- [ ] Parser accepts `CREATE MATERIALIZED VIEW`, `REFRESH MATERIALIZED VIEW`,
      and `DROP MATERIALIZED VIEW`.
- [ ] Catalog metadata persists enough information to distinguish a
      materialized view from a base table and to store its defining query.
- [ ] `CREATE MATERIALIZED VIEW mv AS SELECT ...` materializes rows and allows
      ordinary `SELECT` from `mv`.
- [ ] `REFRESH MATERIALIZED VIEW mv` replaces stale rows with freshly computed
      rows from the stored query.
- [ ] `DROP MATERIALIZED VIEW` removes the relation and metadata.
- [ ] `SHOW FULL TABLES` or equivalent metadata paths expose materialized views
      distinctly.
- [ ] New targeted SQL tests cover create / refresh / drop / metadata.
- [ ] Wire smoke includes a bounded `13.1` block.
- [ ] `cargo fmt --check` passes.
- [ ] Targeted materialized-view tests pass.
- [ ] `python3 tools/wire-test.py` passes.
- [ ] `cargo test --workspace` passes.
- [ ] `cargo clippy --workspace -- -D warnings` passes.

## Out of scope

- Regular `CREATE VIEW` / `DROP VIEW`.
- Updatable views.
- Incremental refresh.
- `REFRESH MATERIALIZED VIEW CONCURRENTLY`.
- Automatic dependency invalidation / cascading source-table DDL tracking.
- Automatic indexes, unique enforcement, or optimizer rewrite-to-MV logic.
- Cross-database refresh scheduling / background jobs.

## Approach decision

### Approach A — materialized relation backed by physical table storage

Pros:
- Reuses CTAS-style query materialization and existing table read paths.
- Avoids blocking `13.1` on the not-yet-built regular view subsystem.
- Delivers real user-visible value with bounded executor/catalog change.

Cons:
- Needs new catalog metadata for relation type + defining query.
- Refresh is full rebuild only in the first cut.

### Approach B — wait for regular views, then add storage on top

Pros:
- Cleaner long-term relation model if views already existed.
- Less duplication between logical view metadata and materialized view metadata.

Cons:
- Wrong cut for the current roadmap order.
- Effectively blocks `13.1` on `20.1`.
- Delays a useful feature for architecture purity that the repo does not yet
  have.

Chosen: **Approach A**.
