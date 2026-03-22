use std::collections::HashMap;

use nexusdb_core::error::DbError;

use crate::{
    freelist::FreeList,
    page::{Page, PageType},
};

/// Storage engine en RAM — sin I/O, ideal para tests unitarios.
///
/// Las páginas se almacenan como `Box<Page>`, lo que garantiza la alineación
/// correcta (align 64) en heap. La página 0 es siempre la Meta page.
/// Integra un `FreeList` para alloc/free con reutilización de páginas.
pub struct MemoryStorage {
    pages: HashMap<u64, Box<Page>>,
    freelist: FreeList,
}

impl MemoryStorage {
    /// Crea un storage vacío con la página 0 (Meta) inicializada.
    ///
    /// Inicia con capacidad para `initial_pages` páginas (default: 64).
    pub fn new() -> Self {
        const INITIAL_PAGES: u64 = 64;

        let mut pages = HashMap::new();
        pages.insert(0, Box::new(Page::new(PageType::Meta, 0)));

        // Páginas 0 y 1 son reservadas (meta y espacio para bitmap en mmap).
        let freelist = FreeList::new(INITIAL_PAGES, &[0, 1]);

        MemoryStorage { pages, freelist }
    }

    /// Retorna una referencia a la página `page_id` verificando su checksum.
    pub fn read_page(&self, page_id: u64) -> Result<&Page, DbError> {
        let page = self
            .pages
            .get(&page_id)
            .map(|b| b.as_ref())
            .ok_or(DbError::PageNotFound { page_id })?;
        page.verify_checksum()?;
        Ok(page)
    }

    /// Escribe `page` en `page_id`. Crea la entrada si no existía.
    pub fn write_page(&mut self, page_id: u64, page: &Page) -> Result<(), DbError> {
        let bytes = *page.as_bytes();
        let owned = Page::from_bytes(bytes)?;
        self.pages.insert(page_id, Box::new(owned));
        Ok(())
    }

    /// Reserva la siguiente página libre y retorna su `page_id`.
    ///
    /// Si el freelist está lleno, crece automáticamente.
    pub fn alloc_page(&mut self, page_type: PageType) -> u64 {
        if let Some(page_id) = self.freelist.alloc() {
            self.pages
                .insert(page_id, Box::new(Page::new(page_type, page_id)));
            return page_id;
        }

        // Crecer en 64 páginas y reintentar.
        let old_total = self.freelist.total_pages();
        self.freelist.grow(old_total + 64);
        self.freelist
            .alloc()
            .expect("freelist vacío después de grow — imposible")
            .also(|&page_id| {
                self.pages
                    .insert(page_id, Box::new(Page::new(page_type, page_id)));
            })
    }

    /// Devuelve `page_id` al pool de páginas libres.
    ///
    /// Retorna error en double-free o page_id fuera de rango.
    pub fn free_page(&mut self, page_id: u64) -> Result<(), DbError> {
        // No permitir liberar páginas reservadas del sistema.
        if page_id == 0 || page_id == 1 {
            return Err(DbError::Other(format!(
                "no se puede liberar la página reservada {page_id}"
            )));
        }
        self.freelist.free(page_id)?;
        // Mantener los bytes en el HashMap — se sobreescribirán en el próximo alloc.
        Ok(())
    }

    /// No-op: no hay I/O que sincronizar.
    pub fn flush(&self) -> Result<(), DbError> {
        Ok(())
    }

    /// Número total de páginas en el bitmap (capacidad actual).
    pub fn page_count(&self) -> u64 {
        self.freelist.total_pages()
    }

    /// Número de páginas libres disponibles.
    pub fn free_count(&self) -> u64 {
        self.freelist.free_count()
    }

    /// Compatibilidad hacia atrás con tests anteriores — delega a alloc_page.
    pub fn alloc_page_raw(&mut self, page_type: PageType) -> u64 {
        self.alloc_page(page_type)
    }
}

/// Helper trait para el patrón `value.also(|v| { ... })` en alloc_page.
trait Also: Sized {
    fn also<F: FnOnce(&Self)>(self, f: F) -> Self {
        f(&self);
        self
    }
}
impl Also for u64 {}

impl Default for MemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PageType;

    #[test]
    fn test_new_has_meta_page() {
        let storage = MemoryStorage::new();
        let page = storage.read_page(0).unwrap();
        assert_eq!(page.header().page_type, PageType::Meta as u8);
    }

    #[test]
    fn test_read_write_roundtrip() {
        let mut storage = MemoryStorage::new();
        let id = storage.alloc_page(PageType::Data);
        let mut page = Page::new(PageType::Data, id);
        page.body_mut()[0] = 0xDE;
        page.body_mut()[1] = 0xAD;
        page.update_checksum();

        storage.write_page(id, &page).unwrap();
        let read = storage.read_page(id).unwrap();
        assert_eq!(read.body()[0], 0xDE);
        assert_eq!(read.body()[1], 0xAD);
    }

    #[test]
    fn test_read_missing_page() {
        let storage = MemoryStorage::new();
        assert!(matches!(
            storage.read_page(99),
            Err(DbError::PageNotFound { page_id: 99 })
        ));
    }

    #[test]
    fn test_alloc_starts_from_2() {
        let mut storage = MemoryStorage::new();
        // Página 0 = meta (reservada), página 1 = bitmap (reservada).
        assert_eq!(storage.alloc_page(PageType::Data), 2);
        assert_eq!(storage.alloc_page(PageType::Data), 3);
        assert_eq!(storage.alloc_page(PageType::Index), 4);
    }

    #[test]
    fn test_free_and_realloc() {
        let mut storage = MemoryStorage::new();
        let id1 = storage.alloc_page(PageType::Data); // 2
        let _id2 = storage.alloc_page(PageType::Data); // 3
        storage.free_page(id1).unwrap();
        // El siguiente alloc debe reutilizar id1.
        assert_eq!(storage.alloc_page(PageType::Data), id1);
    }

    #[test]
    fn test_double_free_is_error() {
        let mut storage = MemoryStorage::new();
        let id = storage.alloc_page(PageType::Data);
        storage.free_page(id).unwrap();
        assert!(storage.free_page(id).is_err());
    }

    #[test]
    fn test_free_reserved_is_error() {
        let mut storage = MemoryStorage::new();
        assert!(storage.free_page(0).is_err());
        assert!(storage.free_page(1).is_err());
    }

    #[test]
    fn test_alloc_grows_automatically() {
        let mut storage = MemoryStorage::new();
        // Agotar las 62 páginas iniciales (64 - 2 reservadas).
        let ids: Vec<u64> = (0..62)
            .map(|_| storage.alloc_page(PageType::Data))
            .collect();
        assert_eq!(ids.len(), 62);
        // El siguiente alloc debe crecer y retornar página válida.
        let id = storage.alloc_page(PageType::Data);
        assert!(id >= 64);
    }

    #[test]
    fn test_alloc_page_is_readable() {
        let mut storage = MemoryStorage::new();
        let id = storage.alloc_page(PageType::Data);
        let page = storage.read_page(id).unwrap();
        assert_eq!(page.header().page_id, id);
        assert_eq!(page.header().page_type, PageType::Data as u8);
    }

    #[test]
    fn test_flush_is_noop() {
        let storage = MemoryStorage::new();
        assert!(storage.flush().is_ok());
    }

    #[test]
    fn test_free_count_updates() {
        let mut storage = MemoryStorage::new();
        let initial_free = storage.free_count();
        let id = storage.alloc_page(PageType::Data);
        assert_eq!(storage.free_count(), initial_free - 1);
        storage.free_page(id).unwrap();
        assert_eq!(storage.free_count(), initial_free);
    }

    #[test]
    fn test_write_invalid_checksum_is_rejected() {
        let mut storage = MemoryStorage::new();
        let mut page = Page::new(PageType::Data, 2);
        page.body_mut()[0] = 0xFF;
        // Sin update_checksum → checksum incorrecto.
        assert!(storage.write_page(2, &page).is_err());
    }
}
