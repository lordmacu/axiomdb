//! Integration tests for CatalogChangeNotifier (subphase 3.13).
//!
//! Verifies that CatalogWriter fires the correct SchemaChangeEvents after each
//! DDL mutation, that multiple listeners all receive events, and that the
//! spurious-notification contract holds on rollback.

use std::sync::{Arc, Mutex};

use axiomdb_catalog::{
    bootstrap::CatalogBootstrap,
    notifier::{CatalogChangeNotifier, SchemaChangeEvent, SchemaChangeKind},
    reader::CatalogReader,
    schema::{ColumnDef, ColumnType, IndexDef},
    writer::CatalogWriter,
};
use axiomdb_storage::MemoryStorage;
use axiomdb_wal::TxnManager;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn setup() -> (MemoryStorage, TxnManager) {
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("test.wal");
    let txn = TxnManager::create(&wal_path).unwrap();
    std::mem::forget(dir);
    (storage, txn)
}

/// Registers a capturing listener on `notifier` and returns a handle to the
/// collected events.
fn capturing_listener(notifier: &CatalogChangeNotifier) -> Arc<Mutex<Vec<SchemaChangeEvent>>> {
    use axiomdb_catalog::notifier::SchemaChangeListener;

    struct CapturingListener {
        events: Arc<Mutex<Vec<SchemaChangeEvent>>>,
    }
    impl SchemaChangeListener for CapturingListener {
        fn on_schema_change(&self, event: &SchemaChangeEvent) {
            self.events.lock().unwrap().push(event.clone());
        }
    }

    let events: Arc<Mutex<Vec<SchemaChangeEvent>>> = Arc::new(Mutex::new(vec![]));
    notifier.subscribe(Arc::new(CapturingListener {
        events: Arc::clone(&events),
    }));
    events
}

// ── Backward compatibility ────────────────────────────────────────────────────

#[test]
fn test_no_notifier_backward_compatible() {
    let (mut storage, mut txn) = setup();
    // CatalogWriter without a notifier must work identically to before.
    txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        w.create_table("public", "users").unwrap();
    }
    txn.commit().unwrap();

    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    assert!(reader.get_table("public", "users").unwrap().is_some());
}

// ── create_table ──────────────────────────────────────────────────────────────

#[test]
fn test_create_table_fires_table_created() {
    let (mut storage, mut txn) = setup();
    let notifier = Arc::new(CatalogChangeNotifier::new());
    let events = capturing_listener(&notifier);

    txn.begin().unwrap();
    let table_id = {
        let mut w = CatalogWriter::new(&mut storage, &mut txn)
            .unwrap()
            .with_notifier(Arc::clone(&notifier));
        w.create_table("public", "orders").unwrap()
    };
    txn.commit().unwrap();

    let guard = events.lock().unwrap();
    assert_eq!(guard.len(), 1, "expected exactly one event");
    assert!(
        matches!(guard[0].kind, SchemaChangeKind::TableCreated { table_id: t } if t == table_id),
        "expected TableCreated with table_id={table_id}, got {:?}",
        guard[0].kind
    );
    assert_ne!(guard[0].txn_id, 0, "txn_id must be non-zero");
}

#[test]
fn test_create_table_fires_before_commit() {
    // Notification must arrive before commit() is called.
    let (mut storage, mut txn) = setup();
    let notifier = Arc::new(CatalogChangeNotifier::new());
    let events = capturing_listener(&notifier);

    txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&mut storage, &mut txn)
            .unwrap()
            .with_notifier(Arc::clone(&notifier));
        w.create_table("public", "t").unwrap();
        // Before commit: event already delivered.
        assert_eq!(
            events.lock().unwrap().len(),
            1,
            "event must fire before commit"
        );
    }
    txn.commit().unwrap();
}

// ── create_column ─────────────────────────────────────────────────────────────

#[test]
fn test_create_column_fires_no_event() {
    let (mut storage, mut txn) = setup();
    let notifier = Arc::new(CatalogChangeNotifier::new());
    let events = capturing_listener(&notifier);

    txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&mut storage, &mut txn)
            .unwrap()
            .with_notifier(Arc::clone(&notifier));
        let tid = w.create_table("public", "t").unwrap();
        // create_column must NOT fire an additional event.
        let count_after_table = events.lock().unwrap().len();
        w.create_column(ColumnDef {
            table_id: tid,
            col_idx: 0,
            name: "id".into(),
            col_type: ColumnType::BigInt,
            nullable: false,
            auto_increment: false,
        })
        .unwrap();
        let count_after_column = events.lock().unwrap().len();
        assert_eq!(
            count_after_table, count_after_column,
            "create_column must not fire any event"
        );
    }
    txn.commit().unwrap();
}

// ── create_index ──────────────────────────────────────────────────────────────

#[test]
fn test_create_index_fires_index_created() {
    let (mut storage, mut txn) = setup();
    let notifier = Arc::new(CatalogChangeNotifier::new());
    let events = capturing_listener(&notifier);

    txn.begin().unwrap();
    let (table_id, index_id) = {
        let mut w = CatalogWriter::new(&mut storage, &mut txn)
            .unwrap()
            .with_notifier(Arc::clone(&notifier));
        let tid = w.create_table("public", "t").unwrap();
        let iid = w
            .create_index(IndexDef {
                index_id: 0,
                table_id: tid,
                name: "t_pkey".into(),
                root_page_id: 99,
                is_unique: true,
                is_primary: true,
                columns: vec![],
                predicate: None,
                fillfactor: 90,
                is_fk_index: false,
                include_columns: vec![],
                index_type: 0,
                pages_per_range: 128,
            })
            .unwrap();
        (tid, iid)
    };
    txn.commit().unwrap();

    let guard = events.lock().unwrap();
    // Two events: TableCreated + IndexCreated.
    assert_eq!(guard.len(), 2);
    assert!(matches!(
        guard[0].kind,
        SchemaChangeKind::TableCreated { .. }
    ));
    assert!(
        matches!(guard[1].kind, SchemaChangeKind::IndexCreated { index_id: i, table_id: t } if i == index_id && t == table_id),
        "expected IndexCreated {{index_id={index_id}, table_id={table_id}}}, got {:?}",
        guard[1].kind
    );
}

// ── delete_table ──────────────────────────────────────────────────────────────

#[test]
fn test_delete_table_fires_table_dropped() {
    let (mut storage, mut txn) = setup();

    // Create table (separate txn).
    txn.begin().unwrap();
    let table_id = {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        w.create_table("public", "t").unwrap()
    };
    txn.commit().unwrap();

    // Drop table with notifier.
    let notifier = Arc::new(CatalogChangeNotifier::new());
    let events = capturing_listener(&notifier);

    txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&mut storage, &mut txn)
            .unwrap()
            .with_notifier(Arc::clone(&notifier));
        w.delete_table(table_id).unwrap();
    }
    txn.commit().unwrap();

    let guard = events.lock().unwrap();
    assert_eq!(guard.len(), 1, "expected TableDropped only (no indexes)");
    assert!(
        matches!(guard[0].kind, SchemaChangeKind::TableDropped { table_id: t } if t == table_id),
        "expected TableDropped with table_id={table_id}, got {:?}",
        guard[0].kind
    );
}

#[test]
fn test_delete_table_with_indexes_fires_index_dropped_per_index() {
    let (mut storage, mut txn) = setup();

    // Create table + 2 indexes.
    txn.begin().unwrap();
    let (table_id, iid1, iid2) = {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        let tid = w.create_table("public", "t").unwrap();
        let i1 = w
            .create_index(IndexDef {
                index_id: 0,
                table_id: tid,
                name: "idx1".into(),
                root_page_id: 10,
                is_unique: true,
                is_primary: true,
                columns: vec![],
                predicate: None,
                fillfactor: 90,
                is_fk_index: false,
                include_columns: vec![],
                index_type: 0,
                pages_per_range: 128,
            })
            .unwrap();
        let i2 = w
            .create_index(IndexDef {
                index_id: 0,
                table_id: tid,
                name: "idx2".into(),
                root_page_id: 11,
                is_unique: false,
                is_primary: false,
                columns: vec![],
                predicate: None,
                fillfactor: 90,
                is_fk_index: false,
                include_columns: vec![],
                index_type: 0,
                pages_per_range: 128,
            })
            .unwrap();
        (tid, i1, i2)
    };
    txn.commit().unwrap();

    // Drop table — expect TableDropped + 2x IndexDropped.
    let notifier = Arc::new(CatalogChangeNotifier::new());
    let events = capturing_listener(&notifier);

    txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&mut storage, &mut txn)
            .unwrap()
            .with_notifier(Arc::clone(&notifier));
        w.delete_table(table_id).unwrap();
    }
    txn.commit().unwrap();

    let guard = events.lock().unwrap();
    // 1 TableDropped + 2 IndexDropped = 3 events.
    assert_eq!(guard.len(), 3, "expected 3 events (1 table + 2 indexes)");

    assert!(matches!(
        guard[0].kind,
        SchemaChangeKind::TableDropped { .. }
    ));

    let dropped_ids: Vec<u32> = guard[1..]
        .iter()
        .filter_map(|e| {
            if let SchemaChangeKind::IndexDropped { index_id, .. } = e.kind {
                Some(index_id)
            } else {
                None
            }
        })
        .collect();
    assert!(
        dropped_ids.contains(&iid1),
        "iid1 missing from IndexDropped events"
    );
    assert!(
        dropped_ids.contains(&iid2),
        "iid2 missing from IndexDropped events"
    );
}

// ── delete_index ──────────────────────────────────────────────────────────────

#[test]
fn test_delete_index_fires_index_dropped() {
    let (mut storage, mut txn) = setup();

    txn.begin().unwrap();
    let (table_id, index_id) = {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        let tid = w.create_table("public", "t").unwrap();
        let iid = w
            .create_index(IndexDef {
                index_id: 0,
                table_id: tid,
                name: "idx".into(),
                root_page_id: 5,
                is_unique: false,
                is_primary: false,
                columns: vec![],
                predicate: None,
                fillfactor: 90,
                is_fk_index: false,
                include_columns: vec![],
                index_type: 0,
                pages_per_range: 128,
            })
            .unwrap();
        (tid, iid)
    };
    txn.commit().unwrap();

    let notifier = Arc::new(CatalogChangeNotifier::new());
    let events = capturing_listener(&notifier);

    txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&mut storage, &mut txn)
            .unwrap()
            .with_notifier(Arc::clone(&notifier));
        w.delete_index(index_id).unwrap();
    }
    txn.commit().unwrap();

    let guard = events.lock().unwrap();
    assert_eq!(guard.len(), 1);
    assert!(
        matches!(guard[0].kind, SchemaChangeKind::IndexDropped { index_id: i, table_id: t } if i == index_id && t == table_id),
        "expected IndexDropped {{index_id={index_id}, table_id={table_id}}}, got {:?}",
        guard[0].kind
    );
}

// ── Multiple listeners ────────────────────────────────────────────────────────

#[test]
fn test_multiple_listeners_all_receive_events() {
    let (mut storage, mut txn) = setup();
    let notifier = Arc::new(CatalogChangeNotifier::new());
    let events_a = capturing_listener(&notifier);
    let events_b = capturing_listener(&notifier);
    let events_c = capturing_listener(&notifier);

    txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&mut storage, &mut txn)
            .unwrap()
            .with_notifier(Arc::clone(&notifier));
        w.create_table("public", "t").unwrap();
    }
    txn.commit().unwrap();

    assert_eq!(
        events_a.lock().unwrap().len(),
        1,
        "listener A must receive event"
    );
    assert_eq!(
        events_b.lock().unwrap().len(),
        1,
        "listener B must receive event"
    );
    assert_eq!(
        events_c.lock().unwrap().len(),
        1,
        "listener C must receive event"
    );
}

// ── Spurious notification on rollback ─────────────────────────────────────────

#[test]
fn test_spurious_notification_on_rollback() {
    // Notification fires before commit. If txn rolls back, the table is gone
    // but the notification was already delivered. This is the documented
    // spurious-notification contract.
    let (mut storage, mut txn) = setup();
    let notifier = Arc::new(CatalogChangeNotifier::new());
    let events = capturing_listener(&notifier);

    txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&mut storage, &mut txn)
            .unwrap()
            .with_notifier(Arc::clone(&notifier));
        w.create_table("public", "ghost").unwrap();
    }
    // Rollback — the table write is undone.
    txn.rollback(&mut storage).unwrap();

    // Notification was delivered before the rollback.
    assert_eq!(
        events.lock().unwrap().len(),
        1,
        "notification must have fired even though txn rolled back"
    );

    // The table is not in the catalog after rollback.
    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    assert!(
        reader.get_table("public", "ghost").unwrap().is_none(),
        "table must not be visible after rollback"
    );
}
