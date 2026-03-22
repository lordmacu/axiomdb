use std::{
    fs::{File, OpenOptions},
    path::Path,
};

use memmap2::MmapMut;
use nexusdb_core::error::DbError;

use crate::page::{Page, PageType, HEADER_SIZE, PAGE_SIZE};

// ── Constantes ────────────────────────────────────────────────────────────────

const DB_FILE_MAGIC: u64 = 0x4E455855_53444201; // "NEXUSDB\1"
const DB_VERSION: u32 = 1;
/// Unidad de crecimiento: 64 páginas = 1 MB.
const GROW_PAGES: u64 = 64;

// ── DbFileMeta ────────────────────────────────────────────────────────────────

/// Metadatos del archivo almacenados en el body de la página 0.
/// Ocupa exactamente `PAGE_SIZE - HEADER_SIZE` bytes.
#[repr(C)]
struct DbFileMeta {
    db_magic: u64,
    version: u32,
    _pad: u32,
    page_count: u64,
    _reserved: [u8; PAGE_SIZE - HEADER_SIZE - 24],
}

const _: () = assert!(
    std::mem::size_of::<DbFileMeta>() == PAGE_SIZE - HEADER_SIZE,
    "DbFileMeta debe llenar exactamente el body de una página"
);

// ── MmapStorage ───────────────────────────────────────────────────────────────

/// Motor de storage basado en mmap.
///
/// El archivo está dividido en páginas de `PAGE_SIZE` bytes.
/// La página 0 es siempre la página Meta con `DbFileMeta` en su body.
pub struct MmapStorage {
    mmap: MmapMut,
    /// El descriptor debe mantenerse abierto mientras `mmap` esté activo.
    /// En Unix, cerrar el fd no invalida el mmap, pero lo conservamos para
    /// poder hacer `set_len` al crecer el archivo en fases futuras.
    #[allow(dead_code)]
    file: File,
}

impl MmapStorage {
    /// Crea un archivo nuevo en `path`.
    ///
    /// Falla si el archivo ya existe.
    pub fn create(path: &Path) -> Result<Self, DbError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;

        let initial_size = GROW_PAGES * PAGE_SIZE as u64;
        file.set_len(initial_size)?;

        // SAFETY: el archivo acaba de crearse con el tamaño correcto y no hay
        // otros mapeos activos. El descriptor vive mientras `MmapStorage` vive.
        let mut mmap = unsafe { MmapMut::map_mut(&file)? };

        // Inicializar página 0 (Meta).
        let mut meta_page = Page::new(PageType::Meta, 0);
        let file_meta = DbFileMeta {
            db_magic: DB_FILE_MAGIC,
            version: DB_VERSION,
            _pad: 0,
            page_count: GROW_PAGES,
            _reserved: [0u8; PAGE_SIZE - HEADER_SIZE - 24],
        };

        // SAFETY: body() tiene exactamente PAGE_SIZE - HEADER_SIZE bytes y
        // DbFileMeta ocupa el mismo tamaño (verificado por const assert).
        // La escritura es a memoria que pertenece exclusivamente a meta_page.
        let body = meta_page.body_mut();
        unsafe {
            std::ptr::copy_nonoverlapping(
                &file_meta as *const DbFileMeta as *const u8,
                body.as_mut_ptr(),
                PAGE_SIZE - HEADER_SIZE,
            );
        }
        meta_page.update_checksum();

        mmap[..PAGE_SIZE].copy_from_slice(meta_page.as_bytes());
        mmap.flush()?;

        Ok(MmapStorage { mmap, file })
    }

    /// Abre un archivo existente en `path`.
    pub fn open(path: &Path) -> Result<Self, DbError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;

        // SAFETY: el archivo existe y tiene al menos PAGE_SIZE bytes (validado
        // al leer la meta page). No hay otros mapeos mutables activos.
        let mmap = unsafe { MmapMut::map_mut(&file)? };

        let storage = MmapStorage { mmap, file };

        // Validar página 0.
        let meta_page = storage.read_page_raw(0)?;
        let file_meta = storage.file_meta(meta_page);

        if file_meta.db_magic != DB_FILE_MAGIC {
            return Err(DbError::Other(format!(
                "archivo inválido: db_magic esperado {:#018x}, obtenido {:#018x}",
                DB_FILE_MAGIC, file_meta.db_magic
            )));
        }
        if file_meta.version != DB_VERSION {
            return Err(DbError::Other(format!(
                "versión de archivo no soportada: {}",
                file_meta.version
            )));
        }

        Ok(storage)
    }

    /// Retorna una referencia zero-copy a la página `page_id`.
    ///
    /// Verifica el checksum antes de retornar.
    pub fn read_page(&self, page_id: u64) -> Result<&Page, DbError> {
        self.read_page_raw(page_id)
    }

    /// Escribe `page` en la posición `page_id` del mmap.
    ///
    /// El `page_id` debe estar dentro de `page_count` actual.
    pub fn write_page(&mut self, page_id: u64, page: &Page) -> Result<(), DbError> {
        let count = self.page_count();
        if page_id >= count {
            return Err(DbError::PageNotFound { page_id });
        }
        let offset = page_id as usize * PAGE_SIZE;
        self.mmap[offset..offset + PAGE_SIZE].copy_from_slice(page.as_bytes());
        Ok(())
    }

    /// Sincroniza el mmap con el disco (msync).
    pub fn flush(&mut self) -> Result<(), DbError> {
        self.mmap.flush()?;
        Ok(())
    }

    /// Número total de páginas en el archivo.
    pub fn page_count(&self) -> u64 {
        // SAFETY: la página 0 fue validada en open/create. El mmap tiene al
        // menos PAGE_SIZE bytes. Mismas garantías que read_page_raw.
        let meta_page = match self.read_page_raw(0) {
            Ok(p) => p,
            // Si la meta page falla algo está muy mal; retornar 0 es seguro.
            Err(_) => return 0,
        };
        self.file_meta(meta_page).page_count
    }

    // ── Privados ──────────────────────────────────────────────────────────────

    /// Referencia zero-copy a una página del mmap sin validar page_count.
    /// Usado internamente para leer la meta page antes de que page_count esté disponible.
    fn read_page_raw(&self, page_id: u64) -> Result<&Page, DbError> {
        let file_size = self.mmap.len();
        let offset = page_id as usize * PAGE_SIZE;

        if offset + PAGE_SIZE > file_size {
            return Err(DbError::PageNotFound { page_id });
        }

        let ptr = self.mmap[offset..].as_ptr();

        // SAFETY: `offset` está dentro del mmap (verificado arriba).
        // El mmap está alineado a la página del SO (≥4KB, múltiplo de 64).
        // `PAGE_SIZE` (16384) es múltiplo de 64, así que cada `offset` es
        // múltiplo de 64 y cumple `align_of::<Page>() == 64`.
        // `Page` es `repr(C, align(64))`. No hay alias mutable simultáneo:
        // `write_page` toma `&mut self`, excluyendo esta referencia compartida.
        let page = unsafe { &*(ptr as *const Page) };
        page.verify_checksum()?;
        Ok(page)
    }

    /// Interpreta el body de la página 0 como `DbFileMeta`.
    fn file_meta<'a>(&self, meta_page: &'a Page) -> &'a DbFileMeta {
        let body = meta_page.body();
        // SAFETY: body tiene PAGE_SIZE - HEADER_SIZE bytes. DbFileMeta ocupa
        // exactamente ese tamaño (const assert). body está alineado porque
        // Page está align(64) y HEADER_SIZE=64, por lo que body[0] está en
        // un offset múltiplo de 64 dentro de una región align(64).
        unsafe { &*(body.as_ptr() as *const DbFileMeta) }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PageType;

    fn tmp_path() -> std::path::PathBuf {
        tempfile::NamedTempFile::new()
            .unwrap()
            .into_temp_path()
            .to_path_buf()
    }

    #[test]
    fn test_create_and_open() {
        let path = tmp_path();
        {
            let storage = MmapStorage::create(&path).unwrap();
            assert_eq!(storage.page_count(), GROW_PAGES);
        }
        let storage = MmapStorage::open(&path).unwrap();
        assert_eq!(storage.page_count(), GROW_PAGES);
    }

    #[test]
    fn test_read_write_roundtrip() {
        let path = tmp_path();
        let mut storage = MmapStorage::create(&path).unwrap();

        let mut page = Page::new(PageType::Data, 5);
        page.body_mut()[0] = 0xAB;
        page.body_mut()[1] = 0xCD;
        page.update_checksum();

        storage.write_page(5, &page).unwrap();

        let read = storage.read_page(5).unwrap();
        assert_eq!(read.body()[0], 0xAB);
        assert_eq!(read.body()[1], 0xCD);
        assert_eq!(read.header().page_id, 5);
    }

    #[test]
    fn test_out_of_bounds_read() {
        let path = tmp_path();
        let storage = MmapStorage::create(&path).unwrap();
        let result = storage.read_page(GROW_PAGES + 1);
        assert!(matches!(result, Err(DbError::PageNotFound { .. })));
    }

    #[test]
    fn test_flush_and_reopen() {
        let path = tmp_path();
        {
            let mut storage = MmapStorage::create(&path).unwrap();
            let mut page = Page::new(PageType::Data, 3);
            page.body_mut()[0] = 0x42;
            page.update_checksum();
            storage.write_page(3, &page).unwrap();
            storage.flush().unwrap();
        }

        let storage = MmapStorage::open(&path).unwrap();
        let page = storage.read_page(3).unwrap();
        assert_eq!(page.body()[0], 0x42);
    }

    #[test]
    fn test_out_of_bounds_write() {
        let path = tmp_path();
        let mut storage = MmapStorage::create(&path).unwrap();
        let page = Page::new(PageType::Data, 999);
        let result = storage.write_page(GROW_PAGES + 1, &page);
        assert!(matches!(result, Err(DbError::PageNotFound { .. })));
    }
}
