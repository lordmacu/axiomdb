# Plan: 3.13 — Catalog Change Notifier

## Files to create / modify

| File | Action | Description |
|---|---|---|
| `crates/axiomdb-catalog/src/notifier.rs` | CREATE | `SchemaChangeKind`, `SchemaChangeEvent`, `SchemaChangeListener`, `CatalogChangeNotifier` |
| `crates/axiomdb-catalog/src/writer.rs` | MODIFY | Add `notifier: Option<Arc<CatalogChangeNotifier>>` field + `with_notifier` + fire events |
| `crates/axiomdb-catalog/src/lib.rs` | MODIFY | Add `pub mod notifier` + re-exports |
| `crates/axiomdb-catalog/tests/integration_catalog_notifier.rs` | CREATE | Integration tests |

No new crates, no new Cargo.toml dependencies (`std::sync::RwLock` is sufficient).

---

## Algorithm / Data structure

### CatalogChangeNotifier internal layout

```rust
pub struct CatalogChangeNotifier {
    listeners: std::sync::RwLock<Vec<Arc<dyn SchemaChangeListener>>>,
}
```

- `subscribe(&self, listener)`: acquires write lock, pushes to Vec.
- `notify(&self, event)`: acquires read lock, iterates Vec, calls each listener.
- Read lock in `notify` allows multiple concurrent DDL threads to notify in
  parallel (safe since listeners are `Send + Sync` and hold the event by ref).
- Write lock in `subscribe` is held only during registration (startup).

### CatalogWriter notifier field

```rust
pub struct CatalogWriter<'a> {
    storage: &'a mut dyn StorageEngine,
    txn: &'a mut TxnManager,
    page_ids: CatalogPageIds,
    notifier: Option<Arc<CatalogChangeNotifier>>,   // ← new
}
```

`with_notifier` is a builder method that returns `Self`:

```rust
pub fn with_notifier(mut self, notifier: Arc<CatalogChangeNotifier>) -> Self {
    self.notifier = Some(notifier);
    self
}
```

### Fire helper (private)

```rust
impl CatalogWriter<'_> {
    fn fire(&self, kind: SchemaChangeKind) {
        if let Some(n) = &self.notifier {
            let txn_id = self.txn.active_txn_id().unwrap_or(0);
            n.notify(&SchemaChangeEvent { kind, txn_id });
        }
    }
}
```

`unwrap_or(0)` is safe: `fire` is only called from within DDL methods that already
verified `active_txn_id()` (they return `NoActiveTransaction` before reaching `fire`).

### Events fired per operation

```
create_table → after HeapChain::insert + record_insert:
    fire(TableCreated { table_id })

delete_table → after all rows deleted:
    fire(TableDropped { table_id })
    for each index deleted: fire(IndexDropped { index_id, table_id })

create_index → after HeapChain::insert + record_insert:
    fire(IndexCreated { index_id, table_id })

delete_index → after HeapChain::delete + record_delete:
    fire(IndexDropped { index_id, table_id })

create_column → no event
```

---

## Implementation phases

### Phase 1 — notifier.rs

1. Define `SchemaChangeKind` with four variants.
2. Define `SchemaChangeEvent { kind, txn_id }`.
3. Define `SchemaChangeListener` trait with `on_schema_change`.
4. Implement `CatalogChangeNotifier`:
   - `new()` → empty `RwLock<Vec<...>>`
   - `subscribe()` → write lock + push
   - `notify()` → read lock + iter
   - `listener_count()` → read lock + len
5. Implement `Default` for `CatalogChangeNotifier`.
6. Unit tests inline in `notifier.rs` (see below).

### Phase 2 — writer.rs modifications

1. Add `notifier: Option<Arc<CatalogChangeNotifier>>` field to `CatalogWriter`.
2. Initialize to `None` in `CatalogWriter::new`.
3. Add `with_notifier(mut self, notifier: Arc<CatalogChangeNotifier>) -> Self`.
4. Add private `fire(&self, kind: SchemaChangeKind)` helper.
5. Call `self.fire(...)` at the end of each DDL method after successful mutation.

   **Exact placement for each method:**

   - `create_table`: after `record_insert` succeeds → `self.fire(TableCreated { table_id })`
   - `delete_table`: after all three scan+delete loops → for each index deleted,
     collect index_ids first, then call `self.fire(TableDropped { table_id })`, then
     `self.fire(IndexDropped { index_id, table_id })` for each index.
   - `create_index`: after `record_insert` → `self.fire(IndexCreated { index_id, table_id: row.table_id })`
   - `delete_index`: after `record_delete` → `self.fire(IndexDropped { index_id, table_id: def.table_id })`

6. Import `SchemaChangeKind` and `CatalogChangeNotifier` from `crate::notifier`.

### Phase 3 — lib.rs

1. Add `pub mod notifier;`
2. Re-export: `pub use notifier::{CatalogChangeNotifier, SchemaChangeEvent, SchemaChangeKind, SchemaChangeListener};`

### Phase 4 — Integration tests

File: `crates/axiomdb-catalog/tests/integration_catalog_notifier.rs`

```
test_no_notifier_backward_compatible
test_subscribe_listener_count
test_notify_no_listeners_no_panic
test_create_table_fires_table_created
test_delete_table_fires_table_dropped
test_delete_table_with_index_fires_index_dropped
test_create_index_fires_index_created
test_delete_index_fires_index_dropped
test_create_column_fires_no_event
test_multiple_listeners_all_receive
test_listeners_called_in_registration_order
test_spurious_notification_on_rollback (create → rollback → listener received TableCreated but table not visible)
```

---

## Tests to write

### Unit (in notifier.rs)

```rust
#[test]
fn test_notifier_empty_notify_no_panic() {
    let n = CatalogChangeNotifier::new();
    n.notify(&SchemaChangeEvent { kind: SchemaChangeKind::TableCreated { table_id: 1 }, txn_id: 0 });
}

#[test]
fn test_subscribe_increments_count() {
    let n = CatalogChangeNotifier::new();
    assert_eq!(n.listener_count(), 0);
    n.subscribe(Arc::new(NoopListener));
    assert_eq!(n.listener_count(), 1);
}

#[test]
fn test_listener_receives_event() {
    // use Arc<Mutex<Vec<SchemaChangeEvent>>> to capture events
    let events: Arc<Mutex<Vec<SchemaChangeEvent>>> = Arc::new(Mutex::new(vec![]));
    let events2 = Arc::clone(&events);
    let listener = CapturingListener { events: events2 };
    let n = CatalogChangeNotifier::new();
    n.subscribe(Arc::new(listener));
    let ev = SchemaChangeEvent { kind: SchemaChangeKind::TableCreated { table_id: 7 }, txn_id: 3 };
    n.notify(&ev);
    assert_eq!(events.lock().unwrap().len(), 1);
    assert!(matches!(events.lock().unwrap()[0].kind, SchemaChangeKind::TableCreated { table_id: 7 }));
}

#[test]
fn test_multiple_listeners_all_receive()
#[test]
fn test_registration_order_preserved()
```

### Integration

```rust
// Reuses setup() from integration_catalog_rw.rs pattern.

#[test]
fn test_create_table_fires_table_created() {
    let (mut storage, mut txn) = setup();
    let notifier = Arc::new(CatalogChangeNotifier::new());
    let events = capturing_listener(&notifier);
    txn.begin().unwrap();
    let table_id = {
        let writer = CatalogWriter::new(&mut storage, &mut txn).unwrap()
            .with_notifier(Arc::clone(&notifier));
        writer.create_table("public", "users").unwrap()
    };
    txn.commit().unwrap();
    let captured = events.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert!(matches!(captured[0].kind, SchemaChangeKind::TableCreated { table_id: t } if t == table_id));
}

#[test]
fn test_spurious_notification_on_rollback() {
    let (mut storage, mut txn) = setup();
    let notifier = Arc::new(CatalogChangeNotifier::new());
    let events = capturing_listener(&notifier);
    txn.begin().unwrap();
    {
        let writer = CatalogWriter::new(&mut storage, &mut txn).unwrap()
            .with_notifier(Arc::clone(&notifier));
        writer.create_table("public", "ghost").unwrap();
    }
    txn.rollback(&mut storage).unwrap();
    // Notification was delivered before rollback.
    assert_eq!(events.lock().unwrap().len(), 1, "notification fired even on rollback");
    // But the table is not in the catalog.
    let snap = txn.snapshot();
    let reader = CatalogReader::new(&storage, snap).unwrap();
    assert!(reader.get_table("public", "ghost").unwrap().is_none(),
        "table must not be visible after rollback");
}
```

---

## Anti-patterns to avoid

- **DO NOT** fire notifications inside the `fire` helper before the heap write
  succeeds — fire only after `record_insert/delete` returns `Ok`.
- **DO NOT** hold the `RwLock` write guard during listener dispatch — only hold
  read guard during `notify` to allow concurrent subscriptions without deadlock.
- **DO NOT** make `SchemaChangeListener::on_schema_change` `&mut self` — listeners
  must use interior mutability (`Mutex`, `RwLock`) for their own state; requiring
  `&mut self` would prevent `Arc<dyn SchemaChangeListener>`.
- **DO NOT** add `unwrap()` in `src/` — the `fire` helper silently skips if no
  notifier; `active_txn_id().unwrap_or(0)` is the only case and is documented.

---

## Risks

| Risk | Mitigation |
|---|---|
| Listener panics inside `on_schema_change` | Propagates to DDL caller — acceptable. Listeners must not panic (documented in trait) |
| `delete_table` fires events in wrong order | Test explicitly checks order: `TableDropped` then `IndexDropped` per index |
| `with_notifier` called after DDL started | Builder pattern returns `Self` — must be called before first DDL method (no runtime check needed; Rust move semantics enforce this) |
| `fire` called on rolled-back txn_id | Documented behavior; `txn_id` is informational only |

---

## Dependency graph (unchanged)

```
axiomdb-core          (TxnId, TableId, DbError)
    ↑
axiomdb-storage       (StorageEngine, HeapChain, meta)
    ↑
axiomdb-wal           (TxnManager)
    ↑
axiomdb-catalog       (notifier, bootstrap, schema, writer, reader)
```

No new external crates required.
