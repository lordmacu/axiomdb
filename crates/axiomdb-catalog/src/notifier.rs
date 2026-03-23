//! Catalog change notifier — internal pub-sub for DDL schema changes.
//!
//! ## Firing semantics
//!
//! Notifications fire at the moment the DDL heap mutation is written, **before**
//! `txn.commit()` is called. If the transaction is later rolled back, the
//! notification was already delivered. Subscribers must be idempotent and
//! spurious-tolerant: a spurious notification (DDL rolled back) causes
//! unnecessary cache invalidation — a performance cost, never a correctness error.
//!
//! ## Usage
//!
//! ```rust,ignore
//! let notifier = Arc::new(CatalogChangeNotifier::new());
//! notifier.subscribe(Arc::new(my_plan_cache));
//!
//! txn.begin()?;
//! let writer = CatalogWriter::new(&mut storage, &mut txn)?
//!     .with_notifier(Arc::clone(&notifier));
//! let table_id = writer.create_table("public", "users")?;
//! txn.commit()?;
//! // my_plan_cache.on_schema_change was already called with TableCreated.
//! ```

use std::sync::{Arc, RwLock};

use axiomdb_core::TxnId;

use crate::schema::TableId;

// ── SchemaChangeKind ──────────────────────────────────────────────────────────

/// The kind of schema change that occurred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaChangeKind {
    /// A new user table was registered in `nexus_tables`.
    TableCreated { table_id: TableId },
    /// All rows for a table were marked deleted across all system tables.
    TableDropped { table_id: TableId },
    /// A new index was registered in `nexus_indexes`.
    IndexCreated { index_id: u32, table_id: TableId },
    /// An index row was marked deleted in `nexus_indexes`.
    IndexDropped { index_id: u32, table_id: TableId },
}

// ── SchemaChangeEvent ─────────────────────────────────────────────────────────

/// A schema change notification emitted by [`CatalogWriter`].
///
/// Note: the `txn_id` may belong to a transaction that is later rolled back.
/// See the module-level firing semantics for details.
///
/// [`CatalogWriter`]: crate::writer::CatalogWriter
#[derive(Debug, Clone)]
pub struct SchemaChangeEvent {
    /// What changed.
    pub kind: SchemaChangeKind,
    /// TxnId of the in-progress transaction that made the change.
    /// May not yet be committed — see module-level firing semantics.
    pub txn_id: TxnId,
}

// ── SchemaChangeListener ──────────────────────────────────────────────────────

/// Implemented by components that react to DDL schema changes.
///
/// ## Contract
///
/// Implementations MUST be:
///
/// - **Idempotent**: calling `on_schema_change` twice with the same event must
///   produce the same observable result as calling it once.
/// - **Spurious-tolerant**: the underlying transaction may roll back after the
///   notification is delivered. Do not assume the change is permanently committed.
/// - **Non-blocking**: this method is called synchronously inside `CatalogWriter`.
///   Return quickly. No heavy I/O, no locks with long hold times.
pub trait SchemaChangeListener: Send + Sync {
    fn on_schema_change(&self, event: &SchemaChangeEvent);
}

// ── CatalogChangeNotifier ─────────────────────────────────────────────────────

/// Registry of [`SchemaChangeListener`]s that fans out DDL events to all subscribers.
///
/// Thread-safe: uses `std::sync::RwLock` internally.
/// `subscribe` acquires a write lock (called at startup, rare).
/// `notify` acquires a read lock (called during DDL, also rare).
pub struct CatalogChangeNotifier {
    listeners: RwLock<Vec<Arc<dyn SchemaChangeListener>>>,
}

impl CatalogChangeNotifier {
    /// Creates an empty notifier with no subscribers.
    pub fn new() -> Self {
        Self {
            listeners: RwLock::new(Vec::new()),
        }
    }

    /// Registers a listener. Listeners are notified in registration order.
    ///
    /// Can be called from any thread at any time.
    pub fn subscribe(&self, listener: Arc<dyn SchemaChangeListener>) {
        self.listeners
            .write()
            .expect("CatalogChangeNotifier RwLock poisoned")
            .push(listener);
    }

    /// Delivers `event` to all registered listeners synchronously.
    ///
    /// Called by [`CatalogWriter`] after each DDL heap mutation.
    /// If no listeners are registered, this is a no-op.
    ///
    /// [`CatalogWriter`]: crate::writer::CatalogWriter
    pub fn notify(&self, event: &SchemaChangeEvent) {
        let guard = self
            .listeners
            .read()
            .expect("CatalogChangeNotifier RwLock poisoned");
        for listener in guard.iter() {
            listener.on_schema_change(event);
        }
    }

    /// Returns the number of registered listeners.
    pub fn listener_count(&self) -> usize {
        self.listeners
            .read()
            .expect("CatalogChangeNotifier RwLock poisoned")
            .len()
    }
}

impl Default for CatalogChangeNotifier {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ── Test helpers ──────────────────────────────────────────────────────────

    struct NoopListener;
    impl SchemaChangeListener for NoopListener {
        fn on_schema_change(&self, _event: &SchemaChangeEvent) {}
    }

    /// A listener that captures all received events for inspection.
    struct CapturingListener {
        events: Arc<Mutex<Vec<SchemaChangeEvent>>>,
    }

    impl SchemaChangeListener for CapturingListener {
        fn on_schema_change(&self, event: &SchemaChangeEvent) {
            self.events.lock().unwrap().push(event.clone());
        }
    }

    fn capturing_listener(notifier: &CatalogChangeNotifier) -> Arc<Mutex<Vec<SchemaChangeEvent>>> {
        let events: Arc<Mutex<Vec<SchemaChangeEvent>>> = Arc::new(Mutex::new(vec![]));
        notifier.subscribe(Arc::new(CapturingListener {
            events: Arc::clone(&events),
        }));
        events
    }

    fn table_created(table_id: TableId) -> SchemaChangeEvent {
        SchemaChangeEvent {
            kind: SchemaChangeKind::TableCreated { table_id },
            txn_id: 1,
        }
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_new_has_zero_listeners() {
        let n = CatalogChangeNotifier::new();
        assert_eq!(n.listener_count(), 0);
    }

    #[test]
    fn test_default_equals_new() {
        let n: CatalogChangeNotifier = Default::default();
        assert_eq!(n.listener_count(), 0);
    }

    #[test]
    fn test_notify_no_listeners_is_noop() {
        let n = CatalogChangeNotifier::new();
        // Must not panic.
        n.notify(&table_created(1));
    }

    #[test]
    fn test_subscribe_increments_count() {
        let n = CatalogChangeNotifier::new();
        n.subscribe(Arc::new(NoopListener));
        assert_eq!(n.listener_count(), 1);
        n.subscribe(Arc::new(NoopListener));
        assert_eq!(n.listener_count(), 2);
    }

    #[test]
    fn test_listener_receives_event() {
        let n = CatalogChangeNotifier::new();
        let events = capturing_listener(&n);

        n.notify(&table_created(42));

        let guard = events.lock().unwrap();
        assert_eq!(guard.len(), 1);
        assert!(matches!(
            guard[0].kind,
            SchemaChangeKind::TableCreated { table_id: 42 }
        ));
        assert_eq!(guard[0].txn_id, 1);
    }

    #[test]
    fn test_multiple_listeners_all_receive() {
        let n = CatalogChangeNotifier::new();
        let events_a = capturing_listener(&n);
        let events_b = capturing_listener(&n);

        n.notify(&table_created(7));

        assert_eq!(events_a.lock().unwrap().len(), 1);
        assert_eq!(events_b.lock().unwrap().len(), 1);
    }

    #[test]
    fn test_listeners_called_in_registration_order() {
        // Track call order via a shared sequence counter.
        let order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(vec![]));

        struct OrderedListener {
            name: &'static str,
            order: Arc<Mutex<Vec<&'static str>>>,
        }
        impl SchemaChangeListener for OrderedListener {
            fn on_schema_change(&self, _event: &SchemaChangeEvent) {
                self.order.lock().unwrap().push(self.name);
            }
        }

        let n = CatalogChangeNotifier::new();
        n.subscribe(Arc::new(OrderedListener {
            name: "first",
            order: Arc::clone(&order),
        }));
        n.subscribe(Arc::new(OrderedListener {
            name: "second",
            order: Arc::clone(&order),
        }));
        n.subscribe(Arc::new(OrderedListener {
            name: "third",
            order: Arc::clone(&order),
        }));

        n.notify(&table_created(1));

        let guard = order.lock().unwrap();
        assert_eq!(*guard, vec!["first", "second", "third"]);
    }

    #[test]
    fn test_idempotent_notify_multiple_times() {
        let n = CatalogChangeNotifier::new();
        let events = capturing_listener(&n);

        let ev = table_created(5);
        n.notify(&ev);
        n.notify(&ev);
        n.notify(&ev);

        // Three calls → three events received (listener decides idempotency,
        // not the notifier).
        assert_eq!(events.lock().unwrap().len(), 3);
    }

    #[test]
    fn test_all_schema_change_kinds_cloneable() {
        let kinds = vec![
            SchemaChangeKind::TableCreated { table_id: 1 },
            SchemaChangeKind::TableDropped { table_id: 2 },
            SchemaChangeKind::IndexCreated {
                index_id: 1,
                table_id: 3,
            },
            SchemaChangeKind::IndexDropped {
                index_id: 2,
                table_id: 4,
            },
        ];
        for kind in kinds {
            let ev = SchemaChangeEvent {
                kind: kind.clone(),
                txn_id: 10,
            };
            let cloned = ev.clone();
            assert_eq!(cloned.txn_id, 10);
            assert_eq!(cloned.kind, kind);
        }
    }
}
