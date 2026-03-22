use std::collections::HashMap;

use nexusdb_core::error::DbError;

use crate::page::{Page, PageType};

/// Storage engine en RAM — sin I/O, ideal para tests unitarios.
///
/// Las páginas se almacenan como `Box<Page>`, lo que garantiza la alineación
/// correcta (align 64) en heap. La página 0 es siempre la Meta page.
pub struct MemoryStorage {
    pages: HashMap<u64, Box<Page>>,
    next_page_id: u64,
}

impl MemoryStorage {
    /// Crea un storage vacío con la página 0 (Meta) inicializada.
    pub fn new() -> Self {
        let mut storage = MemoryStorage {
            pages: HashMap::new(),
            next_page_id: 1,
        };
        storage
            .pages
            .insert(0, Box::new(Page::new(PageType::Meta, 0)));
        storage
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
        // SAFETY: Page::from_bytes verifica magic y checksum antes de construir.
        // Usamos from_bytes para obtener un Page válido a partir de los bytes
        // del page entrante. Si el checksum es inválido, retornamos error.
        let owned = Page::from_bytes(bytes)?;
        self.pages.insert(page_id, Box::new(owned));
        if page_id >= self.next_page_id {
            self.next_page_id = page_id + 1;
        }
        Ok(())
    }

    /// Reserva una nueva página del tipo indicado y retorna su `page_id`.
    pub fn alloc_page_raw(&mut self, page_type: PageType) -> u64 {
        let page_id = self.next_page_id;
        self.next_page_id += 1;
        self.pages
            .insert(page_id, Box::new(Page::new(page_type, page_id)));
        page_id
    }

    /// No-op: no hay I/O que sincronizar.
    pub fn flush(&self) -> Result<(), DbError> {
        Ok(())
    }

    /// Número de páginas reservadas (page_id más alto + 1).
    pub fn page_count(&self) -> u64 {
        self.next_page_id
    }
}

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
        let mut page = Page::new(PageType::Data, 1);
        page.body_mut()[0] = 0xDE;
        page.body_mut()[1] = 0xAD;
        page.update_checksum();

        storage.write_page(1, &page).unwrap();
        let read = storage.read_page(1).unwrap();
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
    fn test_alloc_page_raw_consecutive() {
        let mut storage = MemoryStorage::new();
        assert_eq!(storage.alloc_page_raw(PageType::Data), 1);
        assert_eq!(storage.alloc_page_raw(PageType::Data), 2);
        assert_eq!(storage.alloc_page_raw(PageType::Index), 3);
    }

    #[test]
    fn test_alloc_page_is_readable() {
        let mut storage = MemoryStorage::new();
        let id = storage.alloc_page_raw(PageType::Data);
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
    fn test_page_count_grows() {
        let mut storage = MemoryStorage::new();
        assert_eq!(storage.page_count(), 1);
        storage.alloc_page_raw(PageType::Data);
        storage.alloc_page_raw(PageType::Data);
        assert_eq!(storage.page_count(), 3);
    }

    #[test]
    fn test_write_invalid_checksum_is_rejected() {
        let mut storage = MemoryStorage::new();
        let mut page = Page::new(PageType::Data, 1);
        // Corromper body sin actualizar checksum
        page.body_mut()[0] = 0xFF;
        assert!(storage.write_page(1, &page).is_err());
    }
}
