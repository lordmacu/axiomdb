# Spec: 7.8 — Epoch-Based Reclamation

## Context

When a writer performs Copy-on-Write on a B-Tree node, old pages go into the
`deferred_frees` queue tagged with a snapshot epoch. Currently `flush()` passes
`u64::MAX` to `release_deferred_frees()`, releasing ALL pages immediately. This
is correct under the single-writer `Arc<RwLock<Database>>` model (no readers
active during flush), but:

1. **Not forward-compatible** with relaxed lock models (concurrent reader + writer)
2. **No diagnostic visibility** into snapshot lifetimes or leaked snapshots
3. **Depends on implicit RwLock assumption** that may be violated by future changes

Phase 7.8 replaces the `u64::MAX` placeholder with a `SnapshotRegistry` that
tracks the oldest active snapshot across all connections, enabling safe epoch-
based page reclamation.

**Reference:** DuckDB tracks `lowest_active_start` via an active transaction list.
InnoDB uses `clone_oldest_view()` to merge all active ReadViews. SQLite uses
`nFetchOut` + `aReadMark[]`. AxiomDB follows the DuckDB model: a fixed-size slot
array with atomic operations, no additional locking.

---

## What to build

### A. SnapshotRegistry

A thread-safe registry of active snapshot IDs across all connections. Each
connection gets a fixed slot (indexed by connection ID). Atomic read/write
operations — no lock required beyond what RwLock already provides.

```rust
pub struct SnapshotRegistry {
    slots: Vec<AtomicU64>,   // slot[conn_id] = snapshot_id or 0
    max_connections: usize,
}
```

### B. Connection integration

When a connection starts executing a query:
- Register: `registry.register(conn_id, snapshot_id)`
When the query completes:
- Unregister: `registry.unregister(conn_id)`

### C. Replace u64::MAX in flush()

`flush()` calls `registry.oldest_active()` to determine which deferred pages
are safe to release:
- If no readers active: returns `u64::MAX` (release all — same as current)
- If readers active: returns min of active snapshot IDs

### D. Diagnostic API

`registry.active_count()` and `registry.oldest_active()` expose snapshot state
for monitoring and leak detection.

---

## Inputs / Outputs

### `SnapshotRegistry::new(max_connections: usize)`
- Creates a registry with `max_connections` slots, all initialized to 0

### `SnapshotRegistry::register(conn_id: u32, snapshot_id: u64)`
- Sets `slots[conn_id] = snapshot_id`

### `SnapshotRegistry::unregister(conn_id: u32)`
- Sets `slots[conn_id] = 0`

### `SnapshotRegistry::oldest_active() -> u64`
- Returns min of all non-zero slots, or `u64::MAX` if no readers active

### `SnapshotRegistry::active_count() -> usize`
- Returns count of non-zero slots

---

## Acceptance Criteria

- [ ] `SnapshotRegistry` created with configurable max_connections
- [ ] `register(conn_id, snapshot_id)` sets slot atomically
- [ ] `unregister(conn_id)` clears slot atomically
- [ ] `oldest_active()` returns min of active slots or u64::MAX
- [ ] `active_count()` returns number of active readers
- [ ] `flush()` uses `registry.oldest_active()` instead of `u64::MAX`
- [ ] Under RwLock: behavior identical to current (all slots 0 during flush)
- [ ] Handler registers/unregisters snapshot around query execution
- [ ] Registry stored in `Database` struct (shared via Arc)
- [ ] Unit tests for registry operations
- [ ] `cargo test --workspace` passes clean

---

## Out of Scope

- **Per-table snapshot tracking:** all connections share one global registry
- **Snapshot leak auto-recovery:** logging only, no automatic cleanup
- **Lock-free concurrent reader+writer:** the RwLock model remains; registry
  is forward-compatible infrastructure

## Dependencies

- `MmapStorage::release_deferred_frees(oldest_active_snapshot)` — already exists
- `MmapStorage::set_current_snapshot()` — already exists
- `handler.rs` connection lifecycle — already has `conn_id`
