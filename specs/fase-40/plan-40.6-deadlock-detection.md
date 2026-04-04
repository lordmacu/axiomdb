# Plan: 40.6 — Deadlock Detection

## Files to create/modify

| File | Change |
|---|---|
| `crates/axiomdb-lock/src/deadlock.rs` | **NEW**: Brent's detection + soft resolution + victim selection |
| `crates/axiomdb-lock/src/manager.rs` | Integrate deadlock check into lock wait path |
| `crates/axiomdb-lock/src/lib.rs` | Export deadlock module |

## Implementation phases

### Phase 1: Wait-for tracking
- Add `wait_state: Option<WaitState>` to per-transaction lock state
- `WaitState { blocking_txn: TxnId, waiting_for_lock: LockRef, started: Instant }`
- Set on lock wait, cleared on grant/abort
- Accessible from LockManager for graph traversal

### Phase 2: Brent's cycle detection
- `detect_cycle(start_txn, registry) -> Option<Vec<TxnId>>`
- Follow `blocking_txn` chain using Brent's tortoise/hare
- On cycle found: walk cycle to collect all participating txn_ids
- Safety bound: `2 * MAX_ACTIVE_TXNS` iterations max

### Phase 3: Edge classification (soft vs hard)
- For each edge in detected cycle:
  - **Hard edge**: waiter blocked by HELD lock (lock in granted list)
  - **Soft edge**: waiter blocked by QUEUE ORDERING (another waiter ahead in queue)
- `classify_edges(cycle, lock_manager) -> Vec<(TxnId, TxnId, EdgeType)>`

### Phase 4: Soft resolution attempt
- For each soft edge in cycle:
  - Rearrange: move waiter before blocker in wait queue
  - Re-check: does new ordering create a new cycle?
  - If no new cycle → apply rearrangement → return RESOLVED
- If no soft edge works → fall through to hard resolution

### Phase 5: Victim selection
- `select_victim(cycle_txns, registry) -> TxnId`
- Score = (undo_ops_count × 1000) + (age_bonus) + (wait_time)
- Lower score = preferred victim
- Read-only bonus: subtract 50,000 from score

### Phase 6: Victim notification
- Set `was_deadlock_victim = true` on victim's ConnectionTxn
- Notify victim's wait condition (wake up from lock wait)
- Victim checks flag → returns `DbError::Deadlock`
- Victim's transaction is rolled back by caller

### Phase 7: Integration into lock wait path
- In `LockManager::acquire_record_lock()`, after enqueueing waiter:
  ```
  set_wait_state(txn_id, blocking_txn);
  match detect_cycle(txn_id, registry) {
      None => { /* no deadlock, continue waiting */ }
      Some(cycle) => {
          match try_soft_resolution(cycle, self) {
              Resolved => { /* queue rearranged, re-check if granted */ }
              Failed => {
                  let victim = select_victim(cycle, registry);
                  notify_victim(victim);
                  if victim == txn_id {
                      return Err(DbError::Deadlock { ... });
                  }
              }
          }
      }
  }
  ```

## Tests to write

1. **Simple A↔B deadlock**: A holds X(r1), waits X(r2). B holds X(r2), waits X(r1).
   → Cycle detected. One aborted.
2. **3-way cycle**: A→B→C→A. → Cycle detected. One aborted.
3. **No deadlock**: A waits for B, B is running (not waiting). → No cycle.
4. **Soft deadlock resolution**: A and B in wait queue on same lock, C holds lock.
   Queue ordering creates cycle → rearrange queue → resolved without abort.
5. **Victim selection**: A has 100 undo ops, B has 5. → B selected as victim (less work).
6. **Safety bound**: artificial chain of 1000 txns (no cycle) → terminates, no infinite loop.
7. **Concurrent deadlock checks**: 4 threads simultaneously detect deadlocks → no corruption.
8. **Deadlock + timeout interaction**: deadlock detected before timeout → deadlock error, not timeout.

## Anti-patterns to avoid

- DO NOT hold lock manager shard locks during cycle detection — acquire a separate
  deadlock detection mutex (like InnoDB's `wait_mutex`)
- DO NOT modify wait queues during detection — only modify during resolution phase
- DO NOT check for deadlock on every lock operation — only on WAIT (when conflict found)
- DO NOT abort all cycle participants — only ONE victim
- DO NOT retry inside the deadlock detector — return error, let application retry

## Risks

- **Detection cost on every wait**: Brent's is O(n) per check. With 1000 active txns,
  worst case ~2000 iterations × ~10ns/iter = ~20µs. Acceptable — lock waits are milliseconds.
- **Soft resolution correctness**: Queue rearrangement must not violate lock semantics.
  Mitigation: after rearrangement, verify no new cycle via recursive re-check (PostgreSQL pattern).
- **Concurrent detection**: Two transactions in the same cycle both detect it simultaneously.
  Mitigation: use a deadlock detection mutex. Only one detection runs at a time.
  InnoDB uses `lock_sys.wait_mutex` for exactly this.
