# Plan: 40.12 — Integration Tests & Benchmarks

## Files to create/modify

| File | Change |
|---|---|
| `benches/comparison/concurrent_bench.py` | **NEW**: Multi-client concurrent benchmark |
| `crates/axiomdb-network/tests/integration_concurrent_writers.rs` | **NEW**: 20 test scenarios |
| `crates/axiomdb-network/tests/integration_deadlock.rs` | **NEW**: Deadlock detection tests |
| `crates/axiomdb-network/tests/integration_mvcc_concurrent.rs` | **NEW**: MVCC isolation under concurrency |
| `tools/wire-test-concurrent.py` | **NEW**: Wire protocol concurrent smoke test |
| `docs/fase-40.md` | **NEW**: Phase 40 documentation with benchmark results |
| `docs/progreso.md` | Update Phase 40 status |

## Implementation phases

### Phase 1: Test infrastructure
- Helper functions: `spawn_connection(shared_db)`, `run_concurrent(n_clients, scenario)`
- Shared database factory: `create_test_shared_db() -> Arc<SharedDatabase>`
- Assertion helpers: `assert_row_count()`, `assert_row_value()`, `assert_tree_valid()`
- Timeout wrapper: all concurrent tests have max 30s timeout

### Phase 2: Correctness tests (Tests 1-10)
Implement each test as a standalone `#[tokio::test]`:
- Each test creates its own SharedDatabase (isolation between tests)
- Uses `tokio::spawn` for concurrent "clients"
- Synchronization via `tokio::sync::Barrier` to ensure concurrent execution
- Assertions verify final state after all clients complete

### Phase 3: Stress tests (Tests 11-15)
- Use Rayon or tokio multi-thread runtime
- 8 threads with configurable operation mix
- Data integrity check after all threads complete:
  - Row count matches expected (inserts - deletes)
  - B-tree invariants hold (all keys reachable, sorted)
  - WAL integrity (scan_forward produces valid entries)
  - No orphan pages (freelist consistent)

### Phase 4: concurrent_bench.py
```python
#!/usr/bin/env python3
"""
AxiomDB concurrent writer benchmark.

Usage:
  python3 concurrent_bench.py --clients 1,2,4,8 --scenario insert --rows 10000
  python3 concurrent_bench.py --clients 8 --scenario mixed --rows 50000

Requires: pymysql, running AxiomDB server on port 3309
"""

import argparse, json, threading, time, statistics
import pymysql

SCENARIOS = {
    "insert": run_concurrent_insert,
    "update_random": run_concurrent_update_random,
    "update_hotspot": run_concurrent_update_hotspot,
    "mixed": run_concurrent_mixed,
    "deadlock_stress": run_deadlock_stress,
}

def run_concurrent_insert(n_clients, total_rows, conn_params):
    rows_per_client = total_rows // n_clients
    threads = []
    results = []
    barrier = threading.Barrier(n_clients)

    for i in range(n_clients):
        t = threading.Thread(target=insert_worker,
                             args=(i, rows_per_client, barrier, conn_params, results))
        threads.append(t)

    t0 = time.perf_counter()
    for t in threads: t.start()
    for t in threads: t.join()
    elapsed = time.perf_counter() - t0

    total_ops = sum(r["ops"] for r in results)
    throughput = total_ops / elapsed
    return {"clients": n_clients, "throughput": throughput, "elapsed_ms": elapsed * 1000}
```

### Phase 5: Crash recovery test (Test 15)
- Start AxiomDB server
- 4 pymysql connections insert concurrently
- After N inserts: `os.kill(server_pid, signal.SIGKILL)`
- Restart server (triggers crash recovery)
- Verify: committed rows present, data integrity check passes

### Phase 6: Benchmark execution and documentation
- Run all benchmarks on the same machine (standardized environment)
- Record results in `docs/fase-40.md` with comparison table:

```markdown
## Concurrent Writer Performance

| Clients | INSERT ops/s | UPDATE ops/s | Mixed ops/s | Deadlock rate |
|---------|-------------|-------------|-------------|---------------|
| 1       | 14,200      | 12,800      | 11,500      | 0.0%          |
| 2       | 25,100      | 23,400      | 20,800      | 0.0%          |
| 4       | 44,800      | 40,200      | 36,100      | 0.1%          |
| 8       | 68,500      | 58,900      | 52,300      | 0.5%          |
| Scaling | 4.8×        | 4.6×        | 4.5×        | —             |
```

### Phase 7: Documentation and closure
- Write `docs/fase-40.md` with architecture description + benchmark results
- Update `docs/progreso.md` — mark all Phase 40 subfases complete
- Update `memory/architecture.md` — document SharedDatabase architecture
- Update `memory/project_state.md` — Phase 40 complete

## Tests organization

```
crates/axiomdb-network/tests/
  ├── integration_concurrent_writers.rs   # Tests 1-3, 9-10
  ├── integration_deadlock.rs             # Tests 4-5, 12
  ├── integration_mvcc_concurrent.rs      # Tests 6-8
  ├── integration_stress.rs              # Tests 11, 13-14
  └── integration_crash_concurrent.rs     # Test 15

benches/comparison/
  └── concurrent_bench.py                # Tests 16-20
```

## Anti-patterns to avoid

- DO NOT use `sleep()` for synchronization — use Barrier, Notify, or channels
- DO NOT use single-threaded tokio runtime — must be multi-thread for real concurrency
- DO NOT assume test ordering — each test must set up and tear down independently
- DO NOT skip crash recovery test — it's the ultimate validation
- DO NOT hardcode timings — use relative comparisons (X× speedup, not absolute ms)

## Risks

- **Flaky tests**: concurrent tests can be timing-sensitive. Mitigation: use Barrier
  for synchronization, generous timeouts, deterministic operation ordering where possible.
- **Crash recovery test complexity**: requires process management (start/kill/restart).
  Mitigation: use tempdir for database, controlled server process.
- **Benchmark variance**: concurrent benchmarks have higher variance than single-threaded.
  Mitigation: run 5 iterations, report median. Discard first (warmup).
