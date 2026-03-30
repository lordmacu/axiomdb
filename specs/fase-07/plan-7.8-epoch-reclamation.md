# Plan: 7.8 — Epoch-Based Reclamation

## Files to create/modify

| File | Action | Purpose |
|------|--------|---------|
| `crates/axiomdb-network/src/mysql/snapshot_registry.rs` | **Create** | `SnapshotRegistry` struct |
| `crates/axiomdb-network/src/mysql/mod.rs` | Modify | Add `pub mod snapshot_registry` |
| `crates/axiomdb-network/src/mysql/database.rs` | Modify | Store registry in `Database`, use in flush |
| `crates/axiomdb-network/src/mysql/handler.rs` | Modify | Register/unregister around query execution |
| `docs-site/src/internals/mvcc.md` | Modify | Epoch reclamation section |
| `docs/progreso.md` | Modify | Mark 7.8 |

## Implementation Phases

### Phase 1: SnapshotRegistry

1. Create `snapshot_registry.rs` with `SnapshotRegistry` struct
2. `Vec<AtomicU64>` slots, `new(max_connections)`, `register`, `unregister`
3. `oldest_active()` — scan slots, return min or `u64::MAX`
4. `active_count()` — count non-zero slots
5. Unit tests

### Phase 2: Integration

6. Add `pub snapshot_registry: Arc<SnapshotRegistry>` to `Database`
7. Initialize in `Database::open()` with max_connections = 1024
8. In `handler.rs`: register snapshot before query, unregister after
9. In `database.rs` `flush()` path: pass `registry.oldest_active()` to storage

### Phase 3: Documentation + close

10. Update docs + progreso

## Tests

```
test_registry_empty_returns_max
test_registry_single_reader
test_registry_multiple_readers_returns_min
test_registry_unregister_clears_slot
test_registry_oldest_after_unregister
```
