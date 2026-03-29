# Spec: MVCC Visibility Rules (Phase 7.1)

## Reviewed first

These AxiomDB files were reviewed before writing this spec:

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-storage/src/heap.rs` — `RowHeader`, `is_visible()`, 24-byte layout
- `crates/axiomdb-storage/src/heap_chain.rs` — `scan_visible`, `scan_visible_ro`, `is_slot_visible`, `PAGE_FLAG_ALL_VISIBLE`
- `crates/axiomdb-core/src/traits.rs` — `TransactionSnapshot`, `committed()`, `active()`
- `crates/axiomdb-wal/src/txn.rs` — `TxnManager`, `ActiveTxn`, `begin()`, `commit()`, `rollback()`, `snapshot()`, `active_snapshot()`
- `crates/axiomdb-sql/src/session.rs` — `SessionContext`
- `crates/axiomdb-network/src/mysql/database.rs` — `Arc<Mutex<Database>>`, `execute_query`
- `crates/axiomdb-network/src/mysql/session.rs` — `ConnectionState`, `get_variable`

## Research synthesis

### PostgreSQL (`research/postgres/`)

- `HeapTupleSatisfiesMVCC()` in `heapam_visibility.c` (lines 917–1096):
  checks xmin committed/in-progress/aborted, then xmax committed/in-progress.
  Uses `XidInMVCCSnapshot()` which does binary search in `xip[]` array
  of active transaction IDs stored in `SnapshotData`.
- Snapshot lifetime in `snapmgr.c` (lines 271–377):
  `GetTransactionSnapshot()` calls `GetSnapshotData()` per statement for
  READ COMMITTED; freezes first snapshot for REPEATABLE READ.
- Invariant: `xmin <= xmax`, all `xip[]` entries in `[xmin, xmax)`,
  my own xid never in `xip[]`.

### MariaDB/InnoDB (`research/mariadb-server/`)

- `ReadView::changes_visible()` in `read0types.h` (lines 129–137):
  three-way decision — `id >= m_low_limit_id` → not visible,
  `id < m_up_limit_id` → visible, else binary search in `m_ids`.
- `m_creator_trx_id` check for read-your-own-writes (line 252).
- READ COMMITTED: new ReadView per statement.
  REPEATABLE READ: same ReadView for entire transaction.

### DuckDB (`research/duckdb/`)

- `DuckTransaction` uses timestamps (`start_time`, `transaction_id`, `commit_id`).
- `UpdateInfo::AppliesToTransaction()` in `update_info.hpp` (lines 60–87):
  `version_number > start_time && version_number != transaction_id`.
- **No active transaction ID array** — visibility is O(1) integer comparison.
- DuckDB does **not** support READ COMMITTED — always uses RR semantics.

### AxiomDB-first decision

AxiomDB already uses the DuckDB/timestamp model:

- `RowHeader::is_visible(snap)` uses O(1) comparisons, no ID arrays.
- `TransactionSnapshot` has only `snapshot_id` + `current_txn_id`.
- Single-writer constraint means no concurrent writes to disambiguate.

The visibility **algorithm** is already correct. What this subphase adds:

1. Explicit isolation level selection (RC vs RR vs SERIALIZABLE).
2. Per-statement snapshot refresh for READ COMMITTED inside explicit txns.
3. SQL surface: `SET TRANSACTION ISOLATION LEVEL`, `@@transaction_isolation`.
4. Formal correctness tests proving no dirty reads, no non-repeatable reads
   (in RR), no phantom reads (in RR).

PostgreSQL and InnoDB both implement RC vs RR as purely a **snapshot lifetime
policy** — the visibility check itself is identical. AxiomDB should do the same.

SERIALIZABLE is accepted as a keyword but aliased to REPEATABLE READ because
the single-writer constraint already prevents write skew and serialization
anomalies. This matches MySQL 8 behavior (InnoDB SERIALIZABLE adds gap locks
but the visibility rules are identical to RR).

## What to build (not how)

Make the MVCC isolation level an explicit, configurable property of each
transaction so that:

- Each session has a **default isolation level** (`REPEATABLE READ` by default,
  matching MySQL).
- `SET TRANSACTION ISOLATION LEVEL` changes the level for the **next**
  transaction only.
- `SET SESSION transaction_isolation` changes the session default.
- `SELECT @@transaction_isolation` returns the effective level.
- Inside an explicit transaction with READ COMMITTED, each statement sees a
  **fresh snapshot** reflecting all data committed at statement start, plus
  the transaction's own writes.
- Inside an explicit transaction with REPEATABLE READ, all statements see the
  **same snapshot** taken at `BEGIN`, plus the transaction's own writes.
- Autocommit statements always use READ COMMITTED semantics (fresh snapshot
  per statement) regardless of the session default.
- `SERIALIZABLE` is accepted and treated as `REPEATABLE READ`.

The visibility check (`RowHeader::is_visible`) does **not** change. Only the
snapshot creation policy changes.

## Inputs / Outputs

- Input:
  - `SET [SESSION] transaction_isolation = 'READ-COMMITTED'` /
    `'REPEATABLE-READ'` / `'SERIALIZABLE'`
  - `SET TRANSACTION ISOLATION LEVEL READ COMMITTED` /
    `REPEATABLE READ` / `SERIALIZABLE`
  - `SELECT @@transaction_isolation` / `@@session.transaction_isolation`
  - `BEGIN` (uses effective isolation level)
  - DML statements inside an explicit transaction
- Output:
  - Correct snapshot selection per statement based on isolation level
  - `@@transaction_isolation` returns `'REPEATABLE-READ'` (default) or
    `'READ-COMMITTED'`
- Errors:
  - `SET TRANSACTION ISOLATION LEVEL` inside an active transaction →
    error (cannot change mid-txn, MySQL behavior)
  - Unknown isolation level string → `InvalidValue`
  - `READ UNCOMMITTED` → accepted but silently upgraded to `READ COMMITTED`
    (MySQL behavior)

## Use cases

1. Default behavior unchanged:
   - `BEGIN; SELECT ...; SELECT ...; COMMIT;`
   - both SELECTs see the same snapshot (REPEATABLE READ)

2. READ COMMITTED inside explicit txn:
   - `SET SESSION transaction_isolation = 'READ-COMMITTED';`
   - `BEGIN; SELECT COUNT(*) FROM t;` — sees 100 rows
   - (another connection inserts 10 rows and commits)
   - `SELECT COUNT(*) FROM t;` — sees 110 rows (new snapshot)
   - `COMMIT;`

3. REPEATABLE READ inside explicit txn:
   - `SET SESSION transaction_isolation = 'REPEATABLE-READ';`
   - `BEGIN; SELECT COUNT(*) FROM t;` — sees 100 rows
   - (another connection inserts 10 rows and commits)
   - `SELECT COUNT(*) FROM t;` — still sees 100 rows (frozen snapshot)
   - `COMMIT;`

4. Per-transaction override:
   - `SET TRANSACTION ISOLATION LEVEL READ COMMITTED;`
   - `BEGIN; ...` — this txn uses RC
   - Next `BEGIN` reverts to session default

5. Autocommit is always RC:
   - `SET SESSION transaction_isolation = 'REPEATABLE-READ';`
   - `SELECT COUNT(*) FROM t;` — autocommit, uses fresh snapshot
   - This is consistent with MySQL behavior

6. SERIALIZABLE accepted:
   - `SET SESSION transaction_isolation = 'SERIALIZABLE';`
   - Internally treated as REPEATABLE READ
   - `SELECT @@transaction_isolation` returns `'SERIALIZABLE'`

## Acceptance criteria

- [ ] `IsolationLevel` enum with `ReadCommitted`, `RepeatableRead`, `Serializable` variants.
- [ ] `SessionContext` has `transaction_isolation: IsolationLevel` (default: `RepeatableRead`).
- [ ] `SessionContext` has `next_txn_isolation: Option<IsolationLevel>` for per-txn override.
- [ ] `TxnManager::begin_with_isolation(level)` stores isolation level in `ActiveTxn`.
- [ ] `TxnManager::active_snapshot()` returns fresh snapshot for RC, frozen for RR.
- [ ] `SET SESSION transaction_isolation = '...'` changes session default.
- [ ] `SET TRANSACTION ISOLATION LEVEL ...` sets `next_txn_isolation`.
- [ ] `SET TRANSACTION ISOLATION LEVEL` inside active txn returns error.
- [ ] `SELECT @@transaction_isolation` returns the effective level.
- [ ] `READ UNCOMMITTED` silently upgrades to `READ COMMITTED`.
- [ ] `SERIALIZABLE` accepted and stored; internally uses RR snapshot policy.
- [ ] Autocommit statements always use per-statement snapshot regardless of session isolation.
- [ ] Formal test: no dirty reads in RC.
- [ ] Formal test: no non-repeatable reads in RR.
- [ ] Formal test: no phantom reads in RR (structural, single-writer guarantees this).
- [ ] Existing tests pass without regression (visibility algorithm unchanged).

## Out of scope

- Serializable Snapshot Isolation (SSI) with write-skew detection
- Gap locks or predicate locks
- Multi-writer concurrency (Phase 7.4–7.5)
- `READ UNCOMMITTED` as a distinct level (silently upgraded to RC)
- Per-statement `SET TRANSACTION` inside a running transaction
- `pg_stat_activity`-style visibility of isolation levels

## Dependencies

- Phase 3.4: `RowHeader`, `TransactionSnapshot` (already implemented)
- Phase 3.5: `TxnManager`, `BEGIN`/`COMMIT`/`ROLLBACK` (already implemented)
- Phase 5.9: `ConnectionState`, `@@variable` system (already implemented)

## ⚠️ DEFERRED

- True SERIALIZABLE isolation (SSI with write-skew detection) → Phase 7+ when multi-writer exists
- `SET TRANSACTION READ ONLY` / `READ WRITE` → later transaction attribute subphase
- Isolation level visible in `SHOW PROCESSLIST` / `pg_stat_activity` → Phase 19
