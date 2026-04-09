//! Deadlock detection via Brent's cycle-finding algorithm.
//!
//! Runs synchronously on every lock wait (InnoDB pattern, not PostgreSQL's
//! timeout-based approach). Follows the `wait_for` chain: if txn A waits for
//! txn B which waits for txn A, a cycle is detected.
//!
//! Algorithm: Brent's tortoise & hare, adapted from InnoDB `Deadlock::find_cycle`
//! in `lock0lock.cc` (lines 313-330). O(n) time, O(1) space for detection,
//! then O(k) to collect cycle members where k = cycle length.
//!
//! Victim selection: score-based — prefer fewer undo ops (less rollback work),
//! then newest txn (highest txn_id), then least wait time.

use std::collections::HashMap;
use std::time::Instant;

use axiomdb_core::TxnId;

use crate::txn_locks::LockRef;

/// An edge in the wait-for graph: "this txn is blocked by `blocking_txn`".
#[derive(Debug, Clone)]
pub(crate) struct WaitEdge {
    /// The transaction that holds the conflicting lock.
    pub blocking_txn: TxnId,
    /// Which lock we're waiting on (for abort_waiter dispatch).
    pub lock_ref: LockRef,
    /// When we started waiting (for victim scoring).
    pub wait_started: Instant,
    /// Number of undo ops in the waiting txn (for victim scoring).
    pub undo_count: usize,
}

/// Wait-for map: `txn_id → WaitEdge`.
///
/// Protected by its own `Mutex` in `LockManager` (InnoDB `wait_mutex` pattern).
/// Separate from shard locks to avoid lock ordering issues.
pub(crate) type WaitForMap = HashMap<TxnId, WaitEdge>;

/// Brent's cycle detection on the wait-for chain.
///
/// Starting from `start`, follows `wait_for[txn].blocking_txn` links.
/// Returns `Some(cycle_members)` if a cycle is found, `None` otherwise.
///
/// The cycle members are collected by retracing from the cycle entry point.
///
/// Based on InnoDB `Deadlock::find_cycle()` which uses Brent's algorithm
/// instead of Floyd's because it requires fewer comparisons per step.
pub(crate) fn detect_cycle(start: TxnId, wait_for: &WaitForMap) -> Option<Vec<TxnId>> {
    // Phase 1: Brent's algorithm to detect if a cycle exists.
    let mut tortoise = start;
    let mut hare = start;
    let mut power: usize = 1;
    let mut lambda: usize = 0;

    loop {
        // Advance hare one step.
        let edge = wait_for.get(&hare)?;
        hare = edge.blocking_txn;

        // Check if blocking txn is even waiting (if not in map, chain ends).
        if hare == tortoise {
            // Cycle found!
            break;
        }

        // If hare reached a txn not in the wait_for map, no cycle.
        if !wait_for.contains_key(&hare) && hare != start {
            // hare is not waiting → chain ends, no cycle involving start.
            // But check: if hare == start's blocker and start is in the map,
            // we already checked that. The cycle must include start.
            return None;
        }

        lambda += 1;
        if lambda == power {
            tortoise = hare;
            power <<= 1;
            lambda = 0;
        }

        // Safety bound: prevent infinite loops on corrupt state.
        if power > 1 << 20 {
            return None;
        }
    }

    // Phase 2: Collect cycle members by retracing from the meeting point.
    let mut cycle = Vec::new();
    let mut current = hare;
    loop {
        cycle.push(current);
        let edge = wait_for.get(&current)?;
        current = edge.blocking_txn;
        if current == hare {
            break;
        }
        // Safety bound.
        if cycle.len() > 10_000 {
            return None;
        }
    }

    // Ensure start is in the cycle (it should be, since Brent's found it).
    if !cycle.contains(&start) {
        // Start is on the tail leading to the cycle but not in it.
        // Still a deadlock for anyone in the cycle.
        // Return the cycle as-is — victim will be selected from members.
    }

    Some(cycle)
}

/// Select the victim transaction from a deadlock cycle.
///
/// Scoring (lower = more likely to be victim):
/// - Primary: fewer undo ops (less rollback work to discard)
/// - Secondary: newest txn (highest txn_id — joined later)
/// - Tertiary: least wait time (waited less, lost less investment)
///
/// This matches the spec's scoring function and is more sophisticated than
/// InnoDB's "always abort the requester" approach.
pub(crate) fn select_victim(cycle: &[TxnId], wait_for: &WaitForMap) -> TxnId {
    let now = Instant::now();
    let mut best_victim = cycle[0];
    let mut best_score = u64::MAX;

    for &txn_id in cycle {
        let score = if let Some(edge) = wait_for.get(&txn_id) {
            let undo_weight = (edge.undo_count as u64).saturating_mul(1000);
            let wait_ms = now.duration_since(edge.wait_started).as_millis() as u64;
            undo_weight.saturating_add(wait_ms)
        } else {
            u64::MAX // shouldn't happen — prefer others
        };

        if score < best_score || (score == best_score && txn_id > best_victim) {
            best_score = score;
            best_victim = txn_id;
        }
    }

    best_victim
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(blocking: TxnId, undos: usize) -> WaitEdge {
        WaitEdge {
            blocking_txn: blocking,
            lock_ref: LockRef::Record {
                page_id: 0,
                slot_id: 0,
            },
            wait_started: Instant::now(),
            undo_count: undos,
        }
    }

    #[test]
    fn no_cycle_linear_chain() {
        let mut wf = WaitForMap::new();
        wf.insert(1, edge(2, 0));
        wf.insert(2, edge(3, 0));
        // 3 is not waiting → chain ends.
        assert!(detect_cycle(1, &wf).is_none());
    }

    #[test]
    fn simple_a_b_cycle() {
        let mut wf = WaitForMap::new();
        wf.insert(1, edge(2, 0));
        wf.insert(2, edge(1, 0));
        let cycle = detect_cycle(1, &wf).unwrap();
        assert!(cycle.contains(&1));
        assert!(cycle.contains(&2));
        assert_eq!(cycle.len(), 2);
    }

    #[test]
    fn three_way_cycle() {
        let mut wf = WaitForMap::new();
        wf.insert(10, edge(20, 0));
        wf.insert(20, edge(30, 0));
        wf.insert(30, edge(10, 0));
        let cycle = detect_cycle(10, &wf).unwrap();
        assert_eq!(cycle.len(), 3);
        assert!(cycle.contains(&10));
        assert!(cycle.contains(&20));
        assert!(cycle.contains(&30));
    }

    #[test]
    fn victim_prefers_fewer_undos() {
        let mut wf = WaitForMap::new();
        // Txn 1: 10 undo ops (expensive to rollback)
        wf.insert(1, edge(2, 10));
        // Txn 2: 0 undo ops (cheap to rollback) → should be victim
        wf.insert(2, edge(1, 0));

        let victim = select_victim(&[1, 2], &wf);
        assert_eq!(victim, 2, "should pick txn with fewer undo ops");
    }

    #[test]
    fn victim_tiebreak_newest_txn() {
        let mut wf = WaitForMap::new();
        wf.insert(1, edge(2, 0));
        wf.insert(2, edge(1, 0));
        // Both have 0 undos, started at same time.
        // Tiebreak: highest txn_id (newest) → txn 2.
        let victim = select_victim(&[1, 2], &wf);
        assert_eq!(victim, 2, "should pick newest txn on tie");
    }

    #[test]
    fn empty_wait_for_no_cycle() {
        let wf = WaitForMap::new();
        assert!(detect_cycle(1, &wf).is_none());
    }

    #[test]
    fn self_cycle() {
        let mut wf = WaitForMap::new();
        wf.insert(1, edge(1, 0));
        let cycle = detect_cycle(1, &wf).unwrap();
        assert_eq!(cycle.len(), 1);
        assert!(cycle.contains(&1));
    }
}
