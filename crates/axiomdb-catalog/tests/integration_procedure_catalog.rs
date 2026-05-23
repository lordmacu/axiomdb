//! Catalog persistence tests for stored procedures (Phase 16.7).
//!
//! - upsert / get / delete / list in `axiom_procedures`
//! - replace semantics (same schema.name)
//! - persistence across MmapStorage reopen

use axiomdb_catalog::{
    CatalogBootstrap, CatalogReader, CatalogWriter, ColumnType, ProcLanguage, ProcParam,
    ProcParamMode, ProcedureDef,
};
use axiomdb_storage::{MemoryStorage, MmapStorage, StorageEngine};
use axiomdb_wal::TxnManager;

fn setup() -> (MemoryStorage, TxnManager) {
    let storage = MemoryStorage::new();
    CatalogBootstrap::init(&storage).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("test.wal");
    let txn = TxnManager::create(&wal_path).unwrap();
    std::mem::forget(dir);
    (storage, txn)
}

fn sample_proc(schema: &str, name: &str) -> ProcedureDef {
    ProcedureDef {
        schema_name: schema.into(),
        name: name.into(),
        params: vec![
            ProcParam {
                mode: ProcParamMode::In,
                name: "a".into(),
                data_type: ColumnType::Int,
            },
            ProcParam {
                mode: ProcParamMode::Out,
                name: "b".into(),
                data_type: ColumnType::Text,
            },
        ],
        language: ProcLanguage::PlPgSql,
        body_sql: "BEGIN b := 'x'; END".into(),
    }
}

#[test]
fn create_and_get_procedure() {
    let (storage, txn) = setup();
    let mut conn = txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&storage, &txn, &mut conn).unwrap();
        w.upsert_procedure(sample_proc("public", "p")).unwrap();
    }
    txn.commit(conn).unwrap();

    let mut reader = CatalogReader::new(&storage, txn.snapshot()).unwrap();
    let found = reader.get_procedure("public", "p").unwrap();
    assert_eq!(found, Some(sample_proc("public", "p")));
    // Different schema does not match.
    assert!(reader.get_procedure("other", "p").unwrap().is_none());
    // Missing name does not match.
    assert!(reader.get_procedure("public", "q").unwrap().is_none());
}

#[test]
fn upsert_replaces_existing() {
    let (storage, txn) = setup();
    let mut conn = txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&storage, &txn, &mut conn).unwrap();
        w.upsert_procedure(sample_proc("public", "p")).unwrap();
    }
    txn.commit(conn).unwrap();

    let mut replaced = sample_proc("public", "p");
    replaced.body_sql = "BEGIN b := 'y'; END".into();
    replaced.language = ProcLanguage::MySql;
    let mut conn = txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&storage, &txn, &mut conn).unwrap();
        w.upsert_procedure(replaced.clone()).unwrap();
    }
    txn.commit(conn).unwrap();

    let mut reader = CatalogReader::new(&storage, txn.snapshot()).unwrap();
    // Exactly one row, and it is the replacement.
    let all = reader.list_procedures(Some("public")).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(reader.get_procedure("public", "p").unwrap(), Some(replaced));
}

#[test]
fn delete_procedure_found_and_absent() {
    let (storage, txn) = setup();
    let mut conn = txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&storage, &txn, &mut conn).unwrap();
        w.upsert_procedure(sample_proc("public", "p")).unwrap();
    }
    txn.commit(conn).unwrap();

    let mut conn = txn.begin().unwrap();
    let (found, absent) = {
        let mut w = CatalogWriter::new(&storage, &txn, &mut conn).unwrap();
        let found = w.delete_procedure("public", "p").unwrap();
        let absent = w.delete_procedure("public", "nope").unwrap();
        (found, absent)
    };
    txn.commit(conn).unwrap();
    assert!(found, "existing procedure should delete");
    assert!(!absent, "missing procedure delete returns false");

    let mut reader = CatalogReader::new(&storage, txn.snapshot()).unwrap();
    assert!(reader.get_procedure("public", "p").unwrap().is_none());
}

#[test]
fn delete_returns_false_when_root_uninitialized() {
    let (storage, txn) = setup();
    let mut conn = txn.begin().unwrap();
    let absent = {
        let mut w = CatalogWriter::new(&storage, &txn, &mut conn).unwrap();
        w.delete_procedure("public", "p").unwrap()
    };
    txn.commit(conn).unwrap();
    assert!(!absent);
}

#[test]
fn list_procedures_filters_by_schema() {
    let (storage, txn) = setup();
    let mut conn = txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&storage, &txn, &mut conn).unwrap();
        w.upsert_procedure(sample_proc("public", "a")).unwrap();
        w.upsert_procedure(sample_proc("public", "b")).unwrap();
        w.upsert_procedure(sample_proc("sales", "c")).unwrap();
    }
    txn.commit(conn).unwrap();

    let mut reader = CatalogReader::new(&storage, txn.snapshot()).unwrap();
    let public = reader.list_procedures(Some("public")).unwrap();
    assert_eq!(
        public.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        vec!["a", "b"]
    );
    let all = reader.list_procedures(None).unwrap();
    assert_eq!(all.len(), 3);
    // sorted by (schema, name): public.a, public.b, sales.c
    assert_eq!(all[0].name, "a");
    assert_eq!(all[2].schema_name, "sales");
}

#[test]
fn procedure_persists_across_reopen() {
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("catalog_proc.db");
    let wal_path = db_dir.path().join("catalog_proc.wal");

    {
        let storage = MmapStorage::create(&db_path).unwrap();
        CatalogBootstrap::init(&storage).unwrap();
        let txn = TxnManager::create(&wal_path).unwrap();
        let mut conn = txn.begin().unwrap();
        {
            let mut w = CatalogWriter::new(&storage, &txn, &mut conn).unwrap();
            w.upsert_procedure(sample_proc("public", "p")).unwrap();
        }
        txn.commit(conn).unwrap();
        storage.flush().unwrap();
    }

    {
        let storage = MmapStorage::open(&db_path).unwrap();
        let txn = TxnManager::open(&wal_path).unwrap();
        let mut reader = CatalogReader::new(&storage, txn.snapshot()).unwrap();
        let found = reader
            .get_procedure("public", "p")
            .unwrap()
            .expect("procedure should persist across reopen");
        assert_eq!(found, sample_proc("public", "p"));
    }
}
