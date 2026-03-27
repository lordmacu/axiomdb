# Spec: Primary-key SELECT access path (Phase 6.16)

## Reviewed first

These AxiomDB files were reviewed before writing this spec:

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-sql/src/planner.rs`
- `crates/axiomdb-sql/src/executor/select.rs`
- `crates/axiomdb-embedded/src/lib.rs`
- `benches/comparison/local_bench.py`
- `benches/comparison/axiomdb_bench/src/main.rs`

These research files were reviewed for behavior/technique references:

- `research/sqlite/src/where.c`
- `research/sqlite/src/insert.c`
- `research/postgres/src/backend/executor/nodeIndexscan.c`
- `research/postgres/src/backend/access/heap/heapam.c`

## Research synthesis

### What AxiomDB already does today

AxiomDB already has the physical pieces for fast point lookup:

- PK B-Trees are populated on INSERT (`6.9`)
- `SELECT` already knows how to execute `IndexLookup` and `IndexRange`
- the embedded and wire benchmarks both route through the same planner and
  single-table SELECT executor

The remaining gap is planner-side:

- `find_index_on_col(...)` still rejects `idx.is_primary`
- therefore `SELECT * FROM t WHERE id = literal` falls through to `Scan`
- the benchmark note in `axiomdb_bench` that says "full scan" is still true for
  PK equality/range lookups

### Borrow / reject / adapt

#### SQLite — `research/sqlite/src/where.c`, `research/sqlite/src/insert.c`
- Borrow: primary-key equality should be treated as a first-class access path,
  not as a special case of a slower generic scan.
- Reject: rowid-table / WITHOUT ROWID storage model as a template for AxiomDB.
- Adapt: keep heap + PK B-Tree separate, but allow the planner to pick the PK
  index for equality/range predicates on its leading column.

#### PostgreSQL — `research/postgres/src/backend/executor/nodeIndexscan.c`,
#### `research/postgres/src/backend/access/heap/heapam.c`
- Borrow: simple index scan means "index first, heap fetch second", with full
  row qualification still happening in the executor.
- Reject: full executor node stack and PostgreSQL's richer scan-key machinery.
- Adapt: reuse AxiomDB's existing `AccessMethod::IndexLookup` /
  `AccessMethod::IndexRange` executor paths, but make the planner emit them for
  PRIMARY KEY indexes too.

### AxiomDB-first decision

This subphase is not a general planner rewrite. It specifically fixes the
planner blind spot that prevents PRIMARY KEY access for single-table SELECT.

The chosen approach is:

- allow PRIMARY KEY indexes in the relevant SELECT planner helpers
- treat PRIMARY KEY equality as a forced indexed path, bypassing the current
  small-table/statistics gate
- allow PRIMARY KEY range predicates to reuse the existing range machinery
  under the current cost rules unless a tighter forced rule is justified by the
  benchmark

This closes the real performance debt without widening scope into composite
planner design or full cost-model redesign.

## What to build

Add PRIMARY KEY-aware planning for single-table SELECT so AxiomDB no longer
falls back to full heap scans for `WHERE pk = literal`, and can also consider
PRIMARY KEY range predicates through the same planner path.

The new behavior must:

- allow PRIMARY KEY indexes to participate in single-column planner matching
- force `PRIMARY KEY = literal` to use an indexed path in both ctx and non-ctx
  SELECT execution
- preserve current partial-index and session-collation guards
- keep executor semantics unchanged: index lookup/range returns candidate RIDs,
  executor reads heap rows, and `WHERE` is still rechecked

## Inputs / Outputs

- Input:
  - `SelectStmt`
  - resolved `TableDef`, `ColumnDef`, `IndexDef`
  - optional session collation and stats in the ctx path
- Output:
  - unchanged `QueryResult::Rows`
  - faster PK equality and narrow PK range queries
- Errors:
  - unchanged planner/executor errors
  - no new SQL syntax or wire-visible errors

## Use cases

1. `SELECT * FROM bench_users WHERE id = 42`
   On a table with `PRIMARY KEY (id)`, the planner emits `IndexLookup` instead
   of `Scan`.

2. `SELECT * FROM bench_users WHERE id >= 1000 AND id < 2000`
   The planner may emit `IndexRange` on the PK instead of forcing a full scan.

3. `SELECT * FROM users WHERE email = 'a@b.com'`
   Existing secondary-index behavior remains unchanged.

4. `SELECT * FROM users WHERE name = 'jose'`
   Under non-binary session collation on `TEXT`, text indexes still obey the
   existing collation guard.

## Acceptance criteria

- [ ] PRIMARY KEY indexes are eligible for single-table SELECT planning.
- [ ] `SELECT * FROM t WHERE pk = literal` no longer falls back to `Scan`.
- [ ] The ctx and non-ctx SELECT paths both use the PRIMARY KEY indexed access
      path.
- [ ] Existing secondary-index planner behavior does not regress.
- [ ] Session collation guards remain correct for text indexes.
- [ ] `local_bench.py --scenario select_pk` improves because the planner now
      reaches the PK B-Tree path end to end.

## Out of scope

- Composite-index planner redesign
- Multi-column PRIMARY KEY prefix planning beyond the existing leading-column
  matcher
- New SQL syntax
- Index-only scan redesign
- SQL/wire materialization micro-optimizations after the planner picks the PK
  path

## Dependencies

- `6.3` basic query planner
- `6.9` PK B-Tree population on INSERT
- `6.13` index-only scans and current SELECT executor access-method handling

## ⚠️ DEFERRED

- Composite PRIMARY KEY/prefix planner improvements beyond the current
  single-column matcher → pending in a future planner subphase
- Further SQL/wire `select_pk` latency reductions after the PK path is active
  → pending in a follow-up performance subphase
