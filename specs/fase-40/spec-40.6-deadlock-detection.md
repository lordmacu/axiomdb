# Spec: 40.6 — Deadlock Detection

## What to build (not how)

A deadlock detector that identifies circular wait dependencies between transactions
and resolves them by aborting the least-costly victim. Without this, two transactions
waiting for each other's locks will hang forever.

## Research findings

### InnoDB deadlock detection (primary reference)
- **Brent's cycle detection** (Floyd's tortoise & hare variant): O(n) space, O(2n) time
- **Synchronous**: runs on EVERY lock wait — immediate detection, no stale deadlocks
- **Protected by** `lock_sys.wait_mutex` — separate from partition locks
- **Wait-for graph**: implicit via `trx->lock.wait_trx` pointer chain
- **Victim**: requesting transaction (simplest — the one that triggered detection)
- **Max iterations**: bounded by `2 * max_trx_count` (~262K default)

### PostgreSQL deadlock detection (reference for advanced resolution)
- **DFS with soft/hard edges**: distinguishes held-lock blocks (hard) from queue-order blocks (soft)
- **Asynchronous**: runs on timeout (`deadlock_timeout` = 1 second default) — NOT on every wait
- **Soft deadlock resolution**: rearrange wait queues via topological sort to break cycle
  WITHOUT aborting any transaction (only when possible)
- **Hard deadlock**: if no rearrangement works → abort requesting transaction
- **Requires ALL partition locks**: expensive, but runs rarely (only on timeout)
- **Constraint propagation**: recursive attempt to find valid queue orderings

### Design choice for AxiomDB: Two-tier hybrid

**Tier 1 (immediate, every wait): Brent's cycle detection (InnoDB-style)**
- Fast: O(n) per check, typically resolves in <10 iterations
- Simple: follow `wait_for_trx` chain, detect cycle
- Victim: score-based selection (better than InnoDB's "always requesting txn")

**Tier 2 (on cycle found): Soft resolution attempt (PostgreSQL-inspired)**
- Before aborting: try rearranging wait queue to break cycle
- If rearrangement works → no abort needed (soft resolution)
- If not → abort victim (hard resolution)

This gives us InnoDB's speed for the common case (no deadlock) and PostgreSQL's
intelligence for the rare case (actual deadlock).

## Wait-for graph

### Implicit representation (InnoDB-style)
Each transaction in a wait state has:
```rust
pub struct WaitState {
    pub waiting_for_lock: LockRef,      // which lock we're blocked on
    pub blocking_txn: TxnId,            // which txn holds the conflicting lock
    pub wait_started: Instant,          // for timeout tracking
}
```

The graph is traversed by following `blocking_txn` → find that txn's `WaitState` → follow
its `blocking_txn` → etc. Cycle = same txn appears twice.

### No explicit graph structure needed
The wait-for edges are implicit in each transaction's wait state. This avoids
maintaining a separate data structure and keeps it always consistent.

## Brent's cycle detection algorithm

```rust
fn detect_cycle(start_txn: TxnId, active_txns: &ActiveTxnRegistry) -> Option<TxnId> {
    let mut tortoise = start_txn;
    let mut hare = start_txn;
    let mut power: u32 = 1;
    let mut lambda: u32 = 1;

    loop {
        // Advance hare one step
        let hare_wait = active_txns.get_wait_state(hare)?;
        hare = hare_wait.blocking_txn;

        if hare == 0 {
            return None; // hare reached end of chain — no cycle
        }

        if tortoise == hare {
            return Some(hare); // Cycle found! hare is in the cycle
        }

        if lambda == power {
            // Advance tortoise to hare's position
            tortoise = hare;
            power <<= 1; // double the period
            lambda = 0;
        }
        lambda += 1;

        // Safety bound: prevent infinite loop on corrupted state
        if lambda > MAX_ACTIVE_TXNS * 2 {
            return None;
        }
    }
}
```

**Why Brent's over Floyd's**: Brent's finds the cycle length directly and uses fewer
comparisons in practice. InnoDB chose it for a reason.

## Soft deadlock resolution (PostgreSQL-inspired)

When a cycle is detected, before aborting:

### Step 1: Identify edges in the cycle
```
Follow cycle to collect all (waiter, blocker) pairs:
  A waits for B, B waits for C, C waits for A → cycle edges: [(A,B), (B,C), (C,A)]
```

### Step 2: Classify edges as hard or soft
```
Hard edge: A waits for lock HELD by B → cannot rearrange, B must release
Soft edge: A waits because B is AHEAD in wait queue on same lock
           → CAN rearrange: move A before B in queue
```

### Step 3: Try rearrangement
For each soft edge in the cycle:
- Reverse the queue ordering (move waiter before blocker)
- Verify: does the new ordering create a new cycle?
- If no new cycle → apply rearrangement → RESOLVED (no abort needed)

### Step 4: If no rearrangement works
All edges are hard, or every rearrangement creates new cycles → MUST abort victim.

## Victim selection

### Score-based (better than InnoDB's "always abort requester")

```rust
fn select_victim(cycle_txns: &[TxnId], registry: &ActiveTxnRegistry) -> TxnId {
    let mut best_victim = cycle_txns[0];
    let mut best_score = u64::MAX;

    for &txn_id in cycle_txns {
        let txn = registry.get(txn_id);
        let score = compute_victim_score(txn);
        if score < best_score {
            best_score = score;
            best_victim = txn_id;
        }
    }
    best_victim
}

fn compute_victim_score(txn: &ConnectionTxn) -> u64 {
    let mut score: u64 = 0;

    // Prefer aborting txns with LESS work done (cheaper rollback)
    score += txn.undo_ops.len() as u64 * 1000;

    // Prefer aborting NEWER txns (lower txn_id = older = more invested)
    score += (u64::MAX - txn.txn_id) / 1_000_000;

    // Prefer aborting txns that have waited LESS (less wasted time)
    score += txn.total_wait_time_ms() as u64;

    // Bonus: read-only txns are cheaper to abort
    if txn.undo_ops.is_empty() {
        score = score.saturating_sub(50_000);
    }

    score
}
```

**Criteria (in priority order):**
1. Fewer undo operations = cheaper rollback = preferred victim
2. Newer transaction = less invested work
3. Less wait time accumulated
4. Read-only transactions preferred over writers

## Triggering and timing

| Event | Action |
|---|---|
| Transaction enters lock wait | Run Brent's detection immediately (synchronous) |
| Brent finds no cycle | Return — transaction continues waiting normally |
| Brent finds cycle | Attempt soft resolution → if fails, score-based victim selection |
| Victim chosen | Set `was_deadlock_victim = true` on victim's ConnectionTxn |
| Victim wakes up | Sees deadlock flag → returns `DbError::Deadlock` → triggers ROLLBACK |
| Other cycle members | Continue waiting (their blocking txn will release on commit) |

## Error handling

```rust
DbError::Deadlock {
    victim_txn_id: TxnId,
    cycle: Vec<TxnId>,  // for diagnostics
    message: String,     // human-readable cycle description
}
```

The application can retry the aborted transaction. InnoDB recommends automatic retry
with exponential backoff.

## Use cases

1. **Simple A↔B deadlock:**
   A holds X(row 1), waits for X(row 2).
   B holds X(row 2), waits for X(row 1).
   → Brent detects in 2 steps. Victim = txn with less work. Other succeeds.

2. **3-way cycle (A→B→C→A):**
   A waits for B, B waits for C, C waits for A.
   → Brent detects in 3-6 steps. One victim aborted.

3. **Soft deadlock (queue ordering):**
   A waits for lock on row 1 (queue: [B_granted, C_waiting, A_waiting]).
   C waits for lock on row 2 held by A.
   → Cycle: A→C→A (soft edge: A behind C in queue on row 1).
   → Rearrange: move A before C in queue on row 1 → cycle broken without abort.

4. **No deadlock (false positive prevention):**
   A waits for B. B is running (not waiting). → No cycle. A continues waiting.

5. **Lock timeout (no deadlock, just slow):**
   A waits for B for 50 seconds. B is doing a long operation.
   → No deadlock detected (B not waiting for anyone).
   → Lock timeout fires → `DbError::LockTimeout`.

## Acceptance criteria

- [ ] `detect_cycle()` implements Brent's algorithm correctly
- [ ] Cycle detection runs synchronously on every lock wait
- [ ] Soft resolution: rearrange wait queue for soft edges before aborting
- [ ] Hard resolution: score-based victim selection when soft fails
- [ ] Victim's ConnectionTxn flagged with `was_deadlock_victim`
- [ ] Victim receives `DbError::Deadlock` with cycle info
- [ ] Non-victim cycle members continue waiting normally
- [ ] No false positives: non-cyclic waits never trigger deadlock
- [ ] No false negatives: all cycles detected within 1 step of formation
- [ ] Safety bound: detection bounded by `2 * max_active_txns` iterations
- [ ] Unit tests: 2-way cycle, 3-way cycle, no-cycle, soft resolution, victim selection
- [ ] Stress test: 8 txns with random lock ordering → deadlocks detected and resolved
- [ ] Deadlock rate metric exposed via StatusRegistry

## Out of scope

- Distributed deadlock detection (across nodes)
- Deadlock avoidance (lock ordering hints to prevent deadlocks)
- Lock wait timeout tuning (use InnoDB default: 50 seconds)
- Exponential backoff on retry (application-level concern)

## Dependencies

- 40.5 (Lock Manager) — provides the lock structures, wait queues, conflict matrix
- 40.2 (Per-connection txn) — provides ConnectionTxn with undo_ops for victim scoring
