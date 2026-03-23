# Spec: 3.13 — Catalog Change Notifier

## What to build (not how)

An internal notification mechanism that fires whenever `CatalogWriter` makes
a schema change (CREATE TABLE, DROP TABLE, CREATE INDEX, DROP INDEX), allowing
other engine components — plan cache (5.14), statistics cache (6.11) — to
react without polling.

---

## Firing semantics: on DDL execution, not on commit

Notifications fire at the moment the DDL heap mutation is written, **before**
`txn.commit()` is called. If the transaction is later rolled back, the
notification has already been delivered.

**Invariant subscribers must uphold:** every `SchemaChangeListener` must be
**idempotent** and **tolerant of spurious notifications**. A spurious
notification (DDL rolled back) causes unnecessary cache invalidation — a
performance cost, never a correctness error. The alternative (only notifying
on commit) would require TxnManager to buffer and replay schema events, which
is more complex than the correctness risk justifies at this stage.

This is documented in the `SchemaChangeListener` trait so future implementors
cannot miss it.

---

## SchemaChangeEvent

```rust
/// The kind of schema change that occurred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaChangeKind {
    /// A new user table was registered in nexus_tables.
    TableCreated { table_id: TableId },
    /// All rows for a table were marked deleted in all system tables.
    TableDropped { table_id: TableId },
    /// A new index was registered in nexus_indexes.
    IndexCreated { index_id: u32, table_id: TableId },
    /// An index row was marked deleted in nexus_indexes.
    IndexDropped { index_id: u32, table_id: TableId },
}

/// A schema change notification emitted by [`CatalogWriter`].
#[derive(Debug, Clone)]
pub struct SchemaChangeEvent {
    /// What changed.
    pub kind: SchemaChangeKind,
    /// TxnId of the in-progress transaction that made the change.
    /// May not yet be committed — see firing semantics above.
    pub txn_id: TxnId,
}
```

---

## SchemaChangeListener trait

```rust
/// Implemented by components that need to react to DDL changes.
///
/// ## Contract
///
/// Implementations MUST be:
/// - **Idempotent**: calling `on_schema_change` twice with the same event
///   must produce the same result as calling it once.
/// - **Spurious-tolerant**: the underlying transaction may roll back after
///   the notification is delivered. Implementations must not assume the
///   change is permanently committed.
/// - **Non-blocking**: the method is called synchronously inside `CatalogWriter`.
///   Implementations must return quickly. No I/O, no locks with long hold times.
pub trait SchemaChangeListener: Send + Sync {
    fn on_schema_change(&self, event: &SchemaChangeEvent);
}
```

---

## CatalogChangeNotifier

```rust
/// Registry of schema change listeners.
///
/// Holds a list of `Arc<dyn SchemaChangeListener>` and fans out each event
/// to all registered listeners in registration order.
///
/// Thread-safe: uses `std::sync::RwLock` internally — subscribe is rare
/// (at startup), notify is called during DDL (also rare, always `&self`).
pub struct CatalogChangeNotifier {
    // private
}

impl CatalogChangeNotifier {
    /// Creates an empty notifier with no subscribers.
    pub fn new() -> Self

    /// Registers a listener. Listeners are notified in registration order.
    /// Can be called from any thread at any time.
    pub fn subscribe(&self, listener: Arc<dyn SchemaChangeListener>)

    /// Delivers `event` to all registered listeners synchronously.
    /// Called by `CatalogWriter` after each DDL mutation.
    pub fn notify(&self, event: &SchemaChangeEvent)

    /// Returns the number of registered listeners. Useful for tests.
    pub fn listener_count(&self) -> usize
}

impl Default for CatalogChangeNotifier {
    fn default() -> Self { Self::new() }
}
```

---

## CatalogWriter integration

`CatalogWriter` gains an optional `Arc<CatalogChangeNotifier>` that is set
via a builder-style method. The notifier is optional: existing callers that
do not set one continue working without modification.

```rust
impl<'a> CatalogWriter<'a> {
    /// Attaches a notifier. All subsequent DDL operations will fire events
    /// on the notifier after writing to the heap.
    ///
    /// Can be called at most once. Calling twice replaces the previous notifier.
    pub fn with_notifier(mut self, notifier: Arc<CatalogChangeNotifier>) -> Self

    // Existing methods unchanged. Internally, each DDL operation now
    // optionally calls self.notifier.as_ref().map(|n| n.notify(&event)).
}
```

### Events fired by each operation

| Method | Event |
|---|---|
| `create_table` | `SchemaChangeKind::TableCreated { table_id }` |
| `delete_table` | `SchemaChangeKind::TableDropped { table_id }` |
| `create_index` | `SchemaChangeKind::IndexCreated { index_id, table_id }` |
| `delete_index` | `SchemaChangeKind::IndexDropped { index_id, table_id }` |
| `create_column` | *(no event — columns are part of CREATE TABLE DDL)* |

`create_column` does not fire a notification because it is always called within
a CREATE TABLE transaction. `TableCreated` is sufficient to signal that a new
table (with its columns) now exists.

`ColumnAdded` / `ColumnDropped` events are deferred to Phase 4.22 (ALTER TABLE).

---

## Inputs / Outputs

| Operation | Input | Output | Errors |
|---|---|---|---|
| `CatalogChangeNotifier::subscribe` | `Arc<dyn SchemaChangeListener>` | `()` | none |
| `CatalogChangeNotifier::notify` | `&SchemaChangeEvent` | `()` | none |
| `CatalogWriter::with_notifier` | `Arc<CatalogChangeNotifier>` | `Self` | none |
| DDL methods (unchanged signatures) | unchanged | unchanged | unchanged |

---

## Use cases

1. **No notifier (backward compat)**: `CatalogWriter::new(storage, txn)?` works
   identically to before — no notification infrastructure required.

2. **Single subscriber**: attach a `TestListener` (implements
   `SchemaChangeListener`), call `create_table`. Listener's `on_schema_change`
   is called exactly once with `TableCreated`.

3. **Multiple subscribers**: register two listeners. Both receive the event.
   Order matches registration order.

4. **Rollback scenario**: `create_table` fires `TableCreated`. Caller then
   calls `txn.rollback()`. The table is gone, but the notification was already
   delivered. Listener re-checks the catalog and finds no table — handles
   gracefully (idempotent).

5. **drop_table fires TableDropped + IndexDropped**: `delete_table` deletes
   rows from all three system tables. One `TableDropped` is fired. One
   `IndexDropped` is fired per index found and deleted.

6. **Listener count**: `notifier.listener_count()` returns correct count after
   subscribe calls.

7. **notify with no listeners**: no-op. No panic, no error.

---

## Acceptance criteria

- [ ] `SchemaChangeKind` has four variants: `TableCreated`, `TableDropped`,
      `IndexCreated`, `IndexDropped`
- [ ] `SchemaChangeEvent` carries `kind: SchemaChangeKind` and `txn_id: TxnId`
- [ ] `SchemaChangeListener` trait is `Send + Sync` with `on_schema_change`
- [ ] `CatalogChangeNotifier::new()` creates an empty notifier
- [ ] `subscribe` adds listeners; `listener_count` returns the correct count
- [ ] `notify` calls `on_schema_change` on all registered listeners
- [ ] `notify` with zero listeners is a no-op (no panic)
- [ ] Listeners are called in registration order
- [ ] `CatalogWriter::with_notifier` sets the notifier and returns `Self`
- [ ] `CatalogWriter::create_table` fires `TableCreated` with correct `table_id` and `txn_id`
- [ ] `CatalogWriter::delete_table` fires `TableDropped` with correct `table_id`
- [ ] `CatalogWriter::delete_table` fires `IndexDropped` for each index deleted
- [ ] `CatalogWriter::create_index` fires `IndexCreated` with correct `index_id` and `table_id`
- [ ] `CatalogWriter::delete_index` fires `IndexDropped` with correct `index_id`
- [ ] `CatalogWriter::create_column` fires NO event
- [ ] `CatalogWriter` without a notifier passes all existing tests unchanged
- [ ] Two subscribers both receive every event
- [ ] No `unwrap()` in `src/` (excluding tests and benches)

---

## ⚠️ DEFERRED

- `ColumnAdded` / `ColumnDropped` events → Phase 4.22 (ALTER TABLE)
- Post-commit notification (fire only on committed DDL) → Phase 5.14 when plan
  cache arrives and correctness tradeoffs can be evaluated in context
- Async notification (tokio::sync::broadcast) → Phase 5 (TCP server introduces
  async runtime)
- Persistent listener registry (listeners re-register on open) → not needed;
  listeners register in code at DB open time

---

## Out of scope

- Any actual listener implementation (plan cache, stats cache) — those are in
  Phases 5.14 and 6.11
- Schema version counter in meta page — simple enough to add in Phase 5 if needed
- Notification batching (one event per DDL statement vs. one per row deleted)

---

## Dependencies

- `nexusdb-catalog`: `CatalogWriter`, `SchemaChangeEvent` and related types live here
- `nexusdb-core`: `TxnId`, `TableId` (already in scope)
- `std::sync::RwLock`: for `CatalogChangeNotifier` internal list (no new deps)
