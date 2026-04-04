# Spec: 40.12 — Integration Tests & Benchmarks

## What to build (not how)

End-to-end validation that the entire concurrent writer system (40.1-40.11)
works correctly under real-world conditions. This is the final gate before
declaring Phase 40 complete. No shortcuts — every concurrency scenario must
be tested and every performance claim must be measured.

## Research findings

### InnoDB test methodology
- **sysbench**: industry-standard OLTP benchmark (oltp_read_write, oltp_insert, oltp_update)
- **MySQL Test Runner (MTR)**: thousands of .test files covering isolation, deadlock,
  concurrent DML, crash recovery under concurrency
- **Key scenarios tested**: 2-way deadlock, 3-way deadlock, phantom reads, dirty reads,
  non-repeatable reads, lost updates, write skew

### PostgreSQL test methodology
- **pg_regress**: regression tests including concurrent transaction scenarios
- **isolationtester**: specialized tool for testing transaction isolation
  (schedules interleaved operations across multiple sessions)
- **pgbench**: standard benchmark for concurrent transaction throughput
- **Key scenarios**: serialization anomalies, deadlock detection, lock timeout,
  concurrent DDL+DML, index corruption under concurrency

### What AxiomDB needs
Both databases test at THREE levels:
1. **Unit tests**: per-component (lock manager, WAL, freelist) — already in 40.1-40.11
2. **Integration tests**: multi-component (DML with locks + WAL + MVCC together)
3. **System tests**: wire protocol (real MySQL clients, concurrent connections, crash scenarios)

## Test scenarios (minimum 20 tests)

### Correctness tests (10 tests)

#### Test 1: Two clients INSERT into same table
```
Client A: INSERT INTO t VALUES (1, 'a')    → succeeds
Client B: INSERT INTO t VALUES (2, 'b')    → succeeds (concurrent, different rows)
Verify: both rows present in table
```

#### Test 2: Two clients UPDATE same row
```
Client A: BEGIN; UPDATE t SET val='x' WHERE id=1;  → acquires X lock
Client B: UPDATE t SET val='y' WHERE id=1;          → waits for A's lock
Client A: COMMIT;                                    → B's lock granted
Client B: (completes)                                → val='y'
Verify: final value is 'y', not 'x' (B's update applied last)
```

#### Test 3: Two clients UPDATE different rows (parallel)
```
Client A: UPDATE t SET val='x' WHERE id=1   → acquires X(row 1)
Client B: UPDATE t SET val='y' WHERE id=2   → acquires X(row 2), no conflict
Verify: both updates applied, A and B ran concurrently (timing check)
```

#### Test 4: Simple A↔B deadlock
```
Client A: BEGIN; UPDATE t SET val='a' WHERE id=1;   → X(row 1)
Client B: BEGIN; UPDATE t SET val='b' WHERE id=2;   → X(row 2)
Client A: UPDATE t SET val='a2' WHERE id=2;          → waits for B's X(row 2)
Client B: UPDATE t SET val='b2' WHERE id=1;          → DEADLOCK detected
Verify: one client gets Deadlock error, other succeeds
        victim can retry and succeed
```

#### Test 5: 3-way deadlock cycle
```
Client A: BEGIN; locks row 1
Client B: BEGIN; locks row 2
Client C: BEGIN; locks row 3
Client A: requests row 2 (waits for B)
Client B: requests row 3 (waits for C)
Client C: requests row 1 (DEADLOCK — A→B→C→A cycle)
Verify: exactly one victim, other two succeed after victim rollback
```

#### Test 6: MVCC isolation (reader sees consistent snapshot)
```
Client A: BEGIN;
Client A: SELECT * FROM t;                → sees 10 rows
Client B: INSERT INTO t VALUES (11, 'new');
Client B: COMMIT;
Client A: SELECT * FROM t;                → still sees 10 rows (REPEATABLE READ)
Client A: COMMIT;
Client A: SELECT * FROM t;                → now sees 11 rows (new snapshot)
```

#### Test 7: DELETE during SELECT
```
Client A: BEGIN; SELECT * FROM t WHERE id=5;  → sees row 5
Client B: DELETE FROM t WHERE id=5; COMMIT;
Client A: SELECT * FROM t WHERE id=5;         → still sees row 5 (MVCC)
Client A: COMMIT;
Client A: SELECT * FROM t WHERE id=5;         → row 5 gone (new snapshot)
```

#### Test 8: DDL blocks DML, DML doesn't block DDL unnecessarily
```
Client A: BEGIN; INSERT INTO t VALUES (1, 'a');  → holds IX(t)
Client B: DROP TABLE t;                           → waits (DDL needs X(t), conflicts with IX)
Client A: COMMIT;                                 → B's DDL proceeds
Verify: table dropped after A's commit
```

#### Test 9: Autocommit stress (10 clients × 100 INSERTs each)
```
10 clients simultaneously inserting 100 rows each via autocommit
Verify: exactly 1000 rows in table, no duplicates, no missing
```

#### Test 10: Explicit transaction rollback releases locks
```
Client A: BEGIN; UPDATE t SET val='a' WHERE id=1;  → X(row 1)
Client B: UPDATE t SET val='b' WHERE id=1;          → waits
Client A: ROLLBACK;                                  → releases X(row 1)
Client B: (granted lock)                             → update succeeds
Verify: val='b' (A's change rolled back, B's applied)
```

### Stress tests (5 tests)

#### Test 11: 8 threads mixed workload
```
8 threads: 50% INSERT, 25% UPDATE, 15% SELECT, 10% DELETE
Each thread: 1000 operations
Verify: no data corruption, all operations returned success or expected error
        row count = inserts - deletes
```

#### Test 12: Lock contention stress
```
8 threads all updating the SAME 10 rows in random order
Each thread: 500 updates
Verify: no deadlocks hang (all detected and resolved)
        final values consistent (each row updated exactly total_per_row times)
```

#### Test 13: Page split under concurrency
```
4 threads inserting into same table with sequential keys
Fill table to 100K rows → multiple page splits
Verify: B-tree structure valid (all keys reachable, sorted order)
        no orphan pages, no missing keys
```

#### Test 14: WAL group commit under concurrency
```
8 threads doing autocommit INSERT (1000 each)
Measure: fsyncs < 8000 (group commit batches multiple commits per fsync)
Verify: all 8000 rows durable (crash and recover)
```

#### Test 15: Crash recovery after concurrent writes
```
4 threads inserting concurrently
After 500 inserts per thread: kill -9 the server
Restart: crash recovery runs
Verify: committed rows present, uncommitted rows absent
        no corruption in heap, index, WAL
```

### Performance benchmarks (5 tests)

#### Test 16: Concurrent INSERT throughput
```
python3 benches/comparison/concurrent_bench.py \
  --clients 1,2,4,8 --scenario insert --rows 10000

Expected:
  1 client:  ~14K ops/s (baseline)
  2 clients: ~25K ops/s (1.8×)
  4 clients: ~45K ops/s (3.2×)
  8 clients: ~70K ops/s (5.0×)
```

#### Test 17: Concurrent UPDATE throughput (different rows)
```
--clients 1,2,4,8 --scenario update_random --rows 10000

Expected: near-linear scaling (different rows = minimal contention)
```

#### Test 18: Concurrent UPDATE throughput (same rows — contention)
```
--clients 1,2,4,8 --scenario update_hotspot --rows 100 --updates 10000

Expected: sub-linear scaling (lock contention limits parallelism)
  Measure: lock wait time, deadlock rate, throughput
```

#### Test 19: Mixed read-write workload
```
--clients 8 --read-pct 80 --write-pct 20 --rows 50000

Expected: readers don't block writers (MVCC), throughput ~4× vs single writer
```

#### Test 20: insert_autocommit benchmark improvement
```
python3 benches/comparison/local_bench.py \
  --scenario insert_autocommit --rows 1000

Expected: improved from 🔴 0.62x via group commit under real concurrency
  (single-client benchmark may not improve — benefit is multi-client)
```

## Benchmark tool: concurrent_bench.py

**New benchmark script** (extends local_bench.py for concurrent clients):

```python
# benches/comparison/concurrent_bench.py
# Usage: python3 concurrent_bench.py --clients 1,2,4,8 --scenario insert --rows 10000

Scenarios:
  insert           — N clients each insert rows/N rows via autocommit
  update_random    — N clients update random rows (low contention)
  update_hotspot   — N clients update same 100 rows (high contention)
  mixed            — N clients with configurable read/write ratio
  deadlock_stress  — N clients with intentional lock ordering conflicts

Output:
  clients  throughput  latency_p50  latency_p99  deadlock_rate  fsync_count
  1        14,200 r/s  0.07ms       0.15ms       0.0%           1000
  2        25,100 r/s  0.08ms       0.20ms       0.0%           600
  4        44,800 r/s  0.09ms       0.35ms       0.1%           350
  8        68,500 r/s  0.12ms       0.80ms       0.5%           200
```

## Acceptance criteria

- [ ] All 10 correctness tests pass (Tests 1-10)
- [ ] All 5 stress tests pass without corruption (Tests 11-15)
- [ ] All 5 benchmarks produce measurable results (Tests 16-20)
- [ ] Crash recovery test (Test 15) passes with real I/O
- [ ] Deadlock test (Tests 4, 5) correctly detects and resolves
- [ ] MVCC isolation test (Tests 6, 7) passes (no dirty reads, no phantom reads)
- [ ] DDL test (Test 8) correctly serializes with DML
- [ ] 8 concurrent writers: **≥4× throughput** vs single writer
- [ ] Deadlock rate < 5% under random workload (Test 12)
- [ ] Group commit: fsyncs < clients × rows (Test 14)
- [ ] Wire protocol: real pymysql connections, not mock
- [ ] `concurrent_bench.py` script committed and documented
- [ ] Results documented in `docs/fase-40.md`
- [ ] `docs/progreso.md` updated with Phase 40 status

## Out of scope

- Comparison against MySQL/PostgreSQL concurrent throughput (future)
- Distributed transaction tests (Phase 34)
- Replication under concurrency (Phase 18)
- Online DDL tests (ALTER TABLE concurrent with DML)

## Dependencies

- ALL of 40.1-40.11 must be complete before integration testing
