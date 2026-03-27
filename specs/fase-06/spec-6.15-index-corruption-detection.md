# Spec: 6.15 — Index corruption detection

## What to build

Add startup-time index integrity verification so AxiomDB can detect when an
index no longer matches its heap table and automatically rebuild that index
before the database accepts traffic.

This subphase is **not** a user-facing SQL `REINDEX` feature. It is an
internal open-time recovery/integrity pass.

The design must distinguish three layers clearly:

1. **Physical page checksum verification**
   Already exists in `MmapStorage::open(...)` and validates every allocated
   page, including index pages, before traffic is accepted.

2. **B+Tree structural readability**
   `6.15` must fail open if an index tree cannot be traversed/read
   consistently enough to enumerate its entries.

3. **Logical index-vs-heap divergence**
   `6.15` must detect when the heap-visible rows imply one set of index
   entries and the actual B+Tree contains a different set. Divergent indexes
   must be rebuilt automatically during startup.

The heap table remains the source of truth. If the heap scans cleanly and the
index entries differ, AxiomDB rebuilds the index from heap-visible rows and
swaps the catalog root to the rebuilt tree before accepting connections.

## Research synthesis

### AxiomDB files reviewed first

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-storage/src/mmap.rs`
- `crates/axiomdb-storage/src/integrity.rs`
- `crates/axiomdb-network/src/mysql/database.rs`
- `crates/axiomdb-embedded/src/lib.rs`
- `crates/axiomdb-sql/src/executor/ddl.rs`
- `crates/axiomdb-sql/src/table.rs`
- `crates/axiomdb-catalog/src/reader.rs`
- `crates/axiomdb-catalog/src/writer.rs`
- `crates/axiomdb-index/src/tree.rs`
- `crates/axiomdb-sql/src/index_maintenance.rs`
- `crates/axiomdb-sql/src/partial_index.rs`

### Research sources and how they inform this subphase

- `research/postgres/contrib/amcheck/verify_nbtree.c`
  Borrow: separate structural index verification from heap cross-checking,
  and treat the heap as the reference when validating that every visible tuple
  is indexed.
  Reject: full `amcheck` breadth, Bloom-filter sampling strategy, and
  PostgreSQL-specific relation/lock machinery.
  Adapt: AxiomDB will do a direct full comparison of expected vs actual index
  entries during startup, because current deployments are small enough and the
  engine already has exact heap scans.

- `research/postgres/contrib/amcheck/verify_common.c`
  Borrow: verification should happen in a dedicated check phase, not be mixed
  into normal reads or DML paths.
  Reject: permissions/GUC/lock orchestration tied to PostgreSQL backends.
  Adapt: AxiomDB performs the check during `Database::open` / `Db::open`,
  before any client traffic exists.

- `research/postgres/doc/src/sgml/ref/reindex.sgml`
  Borrow: rebuilding an index should read the table as source of truth and
  replace the old index copy.
  Reject: exposing SQL `REINDEX INDEX/TABLE/SCHEMA/DATABASE` in this phase.
  Adapt: `6.15` provides internal startup rebuild only; SQL `REINDEX` stays
  deferred to later roadmap items.

- `research/sqlite/src/pragma.c`
  Borrow: integrity verification should report index/table mismatches
  explicitly and treat `integrity_check` as a dedicated validation workflow,
  not a side effect of normal queries.
  Reject: PRAGMA surface and VM opcode implementation.
  Adapt: AxiomDB will expose no SQL syntax yet, but it will adopt the same
  conceptual split between “detect mismatch” and “repair by rebuild”.

- `research/sqlite/test/pragma.test`
  Borrow: concrete mismatch classes worth detecting:
  “wrong number of entries in index” and “row N missing from index”.
  Reject: reproducing SQLite’s exact text output or PRAGMA result format.
  Adapt: AxiomDB reports divergence through structured Rust errors/logging and
  rebuilds affected indexes automatically at startup.

- `research/sqlite/src/build.c`
  Borrow: `REINDEX` should refill an index from table contents rather than try
  to patch arbitrary corruption in place.
  Reject: SQL parser integration and multi-object `REINDEX` dispatch in this
  phase.
  Adapt: AxiomDB will reuse its CREATE INDEX-style build path as an internal
  index refill primitive.

- `research/mariadb-server/storage/innobase/row/row0sel.cc`
  Borrow: row-vs-index verification should compare actual clustered row values
  against secondary index fields, not rely on metadata guesses.
  Reject: InnoDB’s clustered-index/secondary-index/page-cursor architecture.
  Adapt: AxiomDB will reconstruct expected encoded keys from heap-visible row
  values using the same key-encoding rules as normal index maintenance.

- `research/oceanbase/src/rootserver/ddl_task/ob_rebuild_index_task.cpp`
  Borrow: rebuild should be a dedicated repair workflow, not ad hoc mutation of
  the existing corrupted index.
  Reject: distributed DDL scheduler/task orchestration.
  Adapt: AxiomDB performs a local single-writer rebuild during startup using an
  internal transaction.

## Inputs / Outputs

- Input:
  - an already-open `StorageEngine`
  - a recovered `TxnManager`
  - catalog-visible `TableDef`, `ColumnDef`, and `IndexDef`
  - startup mode (`server` open or embedded open)
- Output:
  - clean open if all indexes verify or are rebuilt successfully
  - error if an index tree is unreadable/corrupted in a way that prevents safe rebuild
- Errors:
  - physical checksum/page-read failures from `MmapStorage::open(...)` remain unchanged
  - new integrity failure if startup verification finds an unrebuildable index problem
  - existing parse/row/index errors if catalog rows or heap rows are structurally invalid

## Use cases

1. **Clean database open**
   Heap-visible rows and all indexes match. Startup verification does no writes
   and the database opens normally.

2. **Missing index entries after a previous bug/crash**
   Heap rows are valid, but one index is missing entries or has stale extra
   entries. Startup detects the mismatch and rebuilds only the divergent index.

3. **Partial index divergence**
   A partial index contains rows that no longer satisfy the predicate, or is
   missing rows that do satisfy it. Startup recompiles the stored predicate,
   computes the expected entry set, and rebuilds the index if needed.

4. **Unreadable B+Tree**
   The heap is present, but traversing the index returns `BTreeCorrupted` or an
   equivalent structural read failure. Startup fails open rather than trying to
   patch an unreadable tree in place.

5. **PK / FK / non-unique index verification**
   The verifier uses the same key rules as runtime maintenance:
   primary/unique exact keys, non-unique `key||RecordId`, and FK auto-index
   composite keys.

## Acceptance criteria

- [ ] On database open, AxiomDB performs logical verification for every visible index in the catalog before accepting traffic.
- [ ] `MmapStorage::open(...)` remains the only layer that verifies physical page checksums; `6.15` does not duplicate that work.
- [ ] The verifier computes the expected entry set from heap-visible rows using the same semantics as runtime index maintenance, including partial indexes, PK indexes, FK auto-indexes, non-unique `key||RecordId`, and NULL-skipping rules.
- [ ] The verifier enumerates the actual entry set from the B+Tree and compares it against the expected heap-derived entry set.
- [ ] If expected and actual entry sets differ, AxiomDB rebuilds the divergent index automatically during startup and swaps the catalog root to the rebuilt tree.
- [ ] Rebuild uses the existing table contents as source of truth; it does not try to patch arbitrary existing index pages in place.
- [ ] Rebuild preserves catalog identity (`index_id`, name, predicate, fillfactor, uniqueness/PK/FK flags) and only replaces the root page ID.
- [ ] Old index pages are reclaimed transactionally after the rebuild commit is durable, not freed eagerly mid-rebuild.
- [ ] If an index tree cannot be traversed/read well enough to enumerate entries safely, open fails with an integrity error instead of silently rebuilding.
- [ ] Both server open and embedded open run the same integrity/rebuild pass.

## Out of scope

- SQL `REINDEX INDEX/TABLE/DATABASE`
- Concurrent rebuild
- Online rebuild while serving traffic
- Full `amcheck`-style page invariant auditing beyond what is needed to enumerate entries safely
- Table/heap repair when the heap itself is corrupt
- Statistics refresh, histogram rebuild, or planner work

## Dependencies

- `6.1-6.3` secondary indexes
- `6.4` bloom filter semantics (verification must respect current key formats, though bloom itself is not persisted)
- `6.5`, `6.7`, `6.8`, `6.9`, `6.13` because verification must understand FK indexes, partial indexes, fillfactor, PK/non-unique encodings, and INCLUDE-era catalog state
- `5.16` deferred free / root rotation pattern for safe post-commit page reclamation

## ⚠️ DEFERRED

- SQL `REINDEX` statement surface → pending in `19.15` / `37.44`
- Broader integrity APIs such as `CHECK TABLE` / `PRAGMA integrity_check` equivalents → pending in later diagnostics phases
- Salvage of unreadable/catastrophically corrupted B+Trees when old pages cannot be safely enumerated for reclamation → pending in a later storage-repair phase
