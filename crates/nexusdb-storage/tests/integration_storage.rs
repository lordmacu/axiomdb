//! Tests de integración para el storage engine.
//!
//! Diferencia con los unit tests en src/:
//! - Prueban comportamiento end-to-end, no implementaciones internas.
//! - Simulan crash recovery (drop + reabrir).
//! - Verifican equivalencia de comportamiento entre MmapStorage y MemoryStorage.
//! - Ejercitan el trait StorageEngine como interfaz unificada.

use nexusdb_storage::{MemoryStorage, MmapStorage, Page, PageType, StorageEngine};
use tempfile::TempDir;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn tmp_dir() -> TempDir {
    tempfile::tempdir().expect("crear directorio temporal")
}

fn write_pattern(engine: &mut dyn StorageEngine, page_id: u64, pattern: u8) {
    let mut page = Page::new(PageType::Data, page_id);
    page.body_mut().fill(pattern);
    page.update_checksum();
    engine.write_page(page_id, &page).expect("write_page");
}

fn assert_pattern(engine: &dyn StorageEngine, page_id: u64, pattern: u8) {
    let page = engine.read_page(page_id).expect("read_page");
    assert!(
        page.body().iter().all(|&b| b == pattern),
        "página {page_id}: patrón esperado {pattern:#x} no encontrado"
    );
}

// ── Crash recovery ────────────────────────────────────────────────────────────

#[test]
fn test_crash_recovery_data_survives() {
    let dir = tmp_dir();
    let db_path = dir.path().join("test.db");
    let page_id;

    // Escribir datos y hacer flush.
    {
        let mut engine = MmapStorage::create(&db_path).expect("create");
        page_id = engine.alloc_page(PageType::Data).expect("alloc");
        write_pattern(&mut engine, page_id, 0xAB);
        engine.flush().expect("flush");
        // `engine` se dropea aquí — simula cierre limpio.
    }

    // Reabrir y verificar que los datos sobrevivieron.
    {
        let engine = MmapStorage::open(&db_path).expect("reopen");
        assert_pattern(&engine, page_id, 0xAB);
    }
}

#[test]
fn test_crash_recovery_freelist_survives() {
    let dir = tmp_dir();
    let db_path = dir.path().join("test.db");
    let allocated_ids: Vec<u64>;

    {
        let mut engine = MmapStorage::create(&db_path).expect("create");
        allocated_ids = (0..5)
            .map(|_| engine.alloc_page(PageType::Data).expect("alloc"))
            .collect();
        // Liberar la primera página.
        engine.free_page(allocated_ids[0]).expect("free");
        engine.flush().expect("flush");
    }

    // Tras reabrir, el freelist recuerda qué páginas estaban en uso.
    {
        let mut engine = MmapStorage::open(&db_path).expect("reopen");
        // La primera ID libre debe ser `allocated_ids[0]` (fue liberada).
        let next = engine.alloc_page(PageType::Data).expect("alloc");
        assert_eq!(
            next, allocated_ids[0],
            "freelist no persistió: esperado {}, obtenido {}",
            allocated_ids[0], next
        );
        // Las páginas en uso siguen sin reasignarse.
        let next2 = engine.alloc_page(PageType::Data).expect("alloc");
        assert!(
            !allocated_ids[1..].contains(&next2),
            "página en uso reasignada tras recovery: {next2}"
        );
    }
}

#[test]
fn test_crash_recovery_multiple_grows() {
    let dir = tmp_dir();
    let db_path = dir.path().join("test.db");
    let count_after_grows;

    {
        let mut engine = MmapStorage::create(&db_path).expect("create");
        // Agotar la capacidad inicial para forzar dos grows.
        let initial = engine.page_count();
        for _ in 0..(initial - 2 + 64 + 1) {
            engine.alloc_page(PageType::Data).expect("alloc");
        }
        count_after_grows = engine.page_count();
        engine.flush().expect("flush");
    }

    {
        let engine = MmapStorage::open(&db_path).expect("reopen");
        assert_eq!(
            engine.page_count(),
            count_after_grows,
            "page_count no persistió tras grows"
        );
    }
}

// ── Equivalencia MmapStorage ↔ MemoryStorage ─────────────────────────────────

fn run_equivalence_test(engine: &mut dyn StorageEngine) {
    // alloc retorna IDs desde 2 (0=meta, 1=bitmap).
    let id1 = engine.alloc_page(PageType::Data).expect("alloc 1");
    let id2 = engine.alloc_page(PageType::Index).expect("alloc 2");
    assert!(id1 >= 2);
    assert!(id2 > id1);

    // write + read roundtrip.
    write_pattern(engine, id1, 0xCC);
    assert_pattern(engine, id1, 0xCC);
    write_pattern(engine, id2, 0xDD);
    assert_pattern(engine, id2, 0xDD);

    // free + realloc reutiliza.
    engine.free_page(id1).expect("free");
    let id_reused = engine.alloc_page(PageType::Data).expect("realloc");
    assert_eq!(id_reused, id1);

    // double-free: liberar id2 (que sigue en uso) y luego liberarlo otra vez.
    engine.free_page(id2).expect("primera liberación de id2");
    assert!(
        engine.free_page(id2).is_err(),
        "double-free de id2 debería fallar"
    );

    // páginas reservadas no liberables.
    assert!(engine.free_page(0).is_err());
    assert!(engine.free_page(1).is_err());

    // read de página inexistente falla.
    assert!(engine.read_page(999_999).is_err());

    // flush no falla.
    engine.flush().expect("flush");
}

#[test]
fn test_mmap_storage_equivalence() {
    let dir = tmp_dir();
    let db_path = dir.path().join("equiv.db");
    let mut engine = MmapStorage::create(&db_path).expect("create");
    run_equivalence_test(&mut engine);
}

#[test]
fn test_memory_storage_equivalence() {
    let mut engine = MemoryStorage::new();
    run_equivalence_test(&mut engine);
}

// ── StorageEngine como trait object ──────────────────────────────────────────

#[test]
fn test_box_dyn_storage_engine_mmap() {
    let dir = tmp_dir();
    let db_path = dir.path().join("dyn.db");
    let mut engine: Box<dyn StorageEngine> =
        Box::new(MmapStorage::create(&db_path).expect("create"));

    let id = engine.alloc_page(PageType::Data).expect("alloc");
    write_pattern(engine.as_mut(), id, 0xFF);
    assert_pattern(engine.as_ref(), id, 0xFF);
    engine.flush().expect("flush");
}

#[test]
fn test_box_dyn_storage_engine_memory() {
    let mut engine: Box<dyn StorageEngine> = Box::new(MemoryStorage::new());
    let id = engine.alloc_page(PageType::Data).expect("alloc");
    write_pattern(engine.as_mut(), id, 0x42);
    assert_pattern(engine.as_ref(), id, 0x42);
}

// ── Crecimiento automático ────────────────────────────────────────────────────

#[test]
fn test_mmap_auto_grow_on_exhaustion() {
    let dir = tmp_dir();
    let db_path = dir.path().join("grow.db");
    let mut engine = MmapStorage::create(&db_path).expect("create");
    let initial_count = engine.page_count();

    // Agotar páginas iniciales.
    for _ in 0..(initial_count - 2) {
        engine.alloc_page(PageType::Data).expect("alloc");
    }
    // Este alloc debe crecer automáticamente.
    let id = engine.alloc_page(PageType::Data).expect("alloc tras grow");
    assert!(
        id >= initial_count,
        "alloc tras grow debe retornar ID en el rango nuevo"
    );
    assert!(
        engine.page_count() > initial_count,
        "page_count debe haber crecido"
    );
}

#[test]
fn test_memory_auto_grow_on_exhaustion() {
    let mut engine = MemoryStorage::new();
    let initial = engine.page_count();
    for _ in 0..(initial - 2) {
        engine.alloc_page(PageType::Data).expect("alloc");
    }
    let id = engine.alloc_page(PageType::Data).expect("alloc tras grow");
    assert!(id >= initial);
    assert!(engine.page_count() > initial);
}

// ── Integridad del checksum en disco ─────────────────────────────────────────

#[test]
fn test_corrupted_page_detected_on_read() {
    use std::io::{Seek, SeekFrom, Write};

    let dir = tmp_dir();
    let db_path = dir.path().join("corrupt.db");
    let page_id;

    {
        let mut engine = MmapStorage::create(&db_path).expect("create");
        page_id = engine.alloc_page(PageType::Data).expect("alloc");
        write_pattern(&mut engine, page_id, 0x55);
        engine.flush().expect("flush");
    }

    // Corromper 1 byte del body de la página en disco.
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&db_path)
            .expect("abrir archivo");
        let offset = page_id as u64 * nexusdb_storage::PAGE_SIZE as u64
            + nexusdb_storage::HEADER_SIZE as u64
            + 100;
        file.seek(SeekFrom::Start(offset)).expect("seek");
        file.write_all(&[0xFFu8]).expect("write corrupción");
    }

    // Reabrir y verificar que la lectura detecta la corrupción.
    {
        let engine = MmapStorage::open(&db_path).expect("reopen");
        let result = engine.read_page(page_id);
        assert!(
            result.is_err(),
            "checksum inválido debería retornar error, no datos corruptos"
        );
    }
}
