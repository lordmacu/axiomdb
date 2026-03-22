use nexusdb_core::error::DbError;

use crate::page::{Page, PageType};

/// Interfaz unificada del motor de almacenamiento.
///
/// Implementaciones: [`MmapStorage`] (disco, mmap) y [`MemoryStorage`] (RAM, tests).
///
/// ## Lifetimes y borrow checker
/// `read_page` retorna `&Page` ligada a `&self` — mientras esa referencia exista,
/// no es posible llamar métodos `&mut self`. Este invariante es correcto: previene
/// modificaciones mientras se lee, sin necesitar locks.
///
/// ## Uso con trait objects
/// ```rust,ignore
/// fn do_something(engine: &mut dyn StorageEngine) { ... }
/// let engine: Box<dyn StorageEngine> = Box::new(MemoryStorage::new());
/// ```
///
/// [`MmapStorage`]: crate::MmapStorage
/// [`MemoryStorage`]: crate::MemoryStorage
pub trait StorageEngine: Send {
    /// Retorna una referencia zero-copy a la página `page_id`.
    /// Verifica el checksum antes de retornar.
    fn read_page(&self, page_id: u64) -> Result<&Page, DbError>;

    /// Escribe `page` en `page_id`. La página debe tener checksum válido.
    fn write_page(&mut self, page_id: u64, page: &Page) -> Result<(), DbError>;

    /// Reserva una nueva página del tipo indicado. Crece el storage si es necesario.
    fn alloc_page(&mut self, page_type: PageType) -> Result<u64, DbError>;

    /// Devuelve `page_id` al pool de páginas libres.
    /// Retorna error en double-free o page_id inválido.
    fn free_page(&mut self, page_id: u64) -> Result<(), DbError>;

    /// Sincroniza con almacenamiento durable. No-op en MemoryStorage.
    fn flush(&mut self) -> Result<(), DbError>;

    /// Capacidad actual (páginas totales en el storage).
    fn page_count(&self) -> u64;
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::page::PageType;

    /// Suite de tests genérica para cualquier implementación de StorageEngine.
    /// Llamar desde los tests específicos de cada implementación.
    pub fn run_storage_engine_suite(engine: &mut dyn StorageEngine) {
        // alloc retorna page_ids únicos.
        let id1 = engine.alloc_page(PageType::Data).unwrap();
        let id2 = engine.alloc_page(PageType::Data).unwrap();
        let id3 = engine.alloc_page(PageType::Index).unwrap();
        assert_ne!(id1, id2);
        assert_ne!(id2, id3);

        // write + read roundtrip.
        let mut page = Page::new(PageType::Data, id1);
        page.body_mut()[0] = 0xAB;
        page.body_mut()[1] = 0xCD;
        page.update_checksum();
        engine.write_page(id1, &page).unwrap();
        let read = engine.read_page(id1).unwrap();
        assert_eq!(read.body()[0], 0xAB);
        assert_eq!(read.body()[1], 0xCD);

        // free + realloc reutiliza el page_id.
        engine.free_page(id1).unwrap();
        let id_reused = engine.alloc_page(PageType::Data).unwrap();
        assert_eq!(id_reused, id1);

        // double-free: liberar id1 sin haberlo asignado de nuevo → error.
        // id1 está USADO (lo acabamos de re-asignar), así que liberarlo una vez
        // es correcto; liberarlo dos veces seguidas sí es double-free.
        engine.free_page(id1).unwrap(); // primera liberación — válida
        assert!(engine.free_page(id1).is_err()); // segunda — double-free

        // read en página inexistente es error.
        assert!(engine.read_page(999_999).is_err());

        // flush no falla.
        engine.flush().unwrap();
    }
}
