use std::{
    fs::{File, OpenOptions},
    path::Path,
};

use fs2::FileExt;
use memmap2::MmapMut;
use nexusdb_core::error::DbError;
use tracing::{debug, info, warn};

use crate::{
    engine::StorageEngine,
    freelist::FreeList,
    page::{Page, PageType, HEADER_SIZE, PAGE_SIZE},
};

// ── Constantes ────────────────────────────────────────────────────────────────

const DB_FILE_MAGIC: u64 = 0x4E455855_53444201; // "NEXUSDB\1"
const DB_VERSION: u32 = 1;
/// Unidad de crecimiento: 64 páginas = 1 MB.
const GROW_PAGES: u64 = 64;

// Offsets fijos en el archivo para actualización directa sin re-parsear.
// PageHeader(64) + db_magic(8) + version(4) + _pad(4) = 80
const PAGE_COUNT_OFFSET: usize = HEADER_SIZE + 8 + 4 + 4;
// Offset del campo `checksum` dentro del PageHeader.
const CHECKSUM_OFFSET: usize = 12;

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
/// Layout del archivo:
/// - Página 0: Meta (`DbFileMeta` en body)
/// - Página 1: Bitmap de la free list (`FreeList` serializada)
/// - Páginas 2+: Data, Index, Overflow, etc.
///
/// El archivo `.db` se bloquea con `flock(LOCK_EX)` al abrirse y se desbloquea
/// al hacer drop. Esto previene corrupción por acceso simultáneo de dos procesos.
pub struct MmapStorage {
    mmap: MmapMut,
    /// El descriptor se mantiene abierto para `set_len` en `grow` y para
    /// sostener el file lock hasta que se haga drop de este struct.
    file: File,
    /// Free list en memoria. Se persiste a página 1 de forma lazy en `flush()`.
    freelist: FreeList,
    /// Indica que el freelist fue modificado y debe persistirse en el próximo flush.
    freelist_dirty: bool,
}

impl Drop for MmapStorage {
    fn drop(&mut self) {
        // Drop::drop() se ejecuta con todos los campos aún vivos; los campos se
        // dropean después en orden de declaración (mmap → file).
        // Liberamos el lock explícitamente aquí para mayor claridad; aunque el SO
        // también lo liberaría al cerrar el fd cuando `file` se dropee.
        if let Err(e) = self.file.unlock() {
            // Drop no puede retornar Result; solo se loggea.
            warn!(error = %e, "error al liberar el file lock al cerrar la BD");
        } else {
            debug!("file lock liberado");
        }
    }
}

impl MmapStorage {
    /// Crea un archivo nuevo en `path`. Falla si ya existe.
    pub fn create(path: &Path) -> Result<Self, DbError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;

        // Adquirir lock exclusivo antes de cualquier escritura. Si otro proceso
        // abrió el mismo archivo (raro en create_new, pero posible en race),
        // fallamos de inmediato en lugar de corromper.
        file.try_lock_exclusive()
            .map_err(|_| DbError::FileLocked { path: path.to_owned() })?;

        info!(path = %path.display(), pages = GROW_PAGES, "creando base de datos");

        let initial_size = GROW_PAGES * PAGE_SIZE as u64;
        file.set_len(initial_size)?;

        // SAFETY: archivo recién creado con tamaño correcto. Sin otros mapeos.
        let mut mmap = unsafe { MmapMut::map_mut(&file)? };

        // Escribir página 0 (Meta).
        Self::write_meta_to_mmap(&mut mmap, GROW_PAGES)?;

        // Inicializar FreeList: páginas 0 y 1 reservadas (meta + bitmap).
        let freelist = FreeList::new(GROW_PAGES, &[0, 1]);

        // Escribir página 1 (bitmap).
        Self::write_freelist_to_mmap(&mut mmap, &freelist)?;

        mmap.flush()?;

        debug!(path = %path.display(), "base de datos inicializada y lista");
        Ok(MmapStorage {
            mmap,
            file,
            freelist,
            freelist_dirty: false,
        })
    }

    /// Abre un archivo existente en `path`.
    pub fn open(path: &Path) -> Result<Self, DbError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;

        // Adquirir lock exclusivo de forma no-bloqueante.
        // Si otro proceso ya tiene el archivo abierto, retornamos error inmediato
        // en lugar de bloquear o corromper.
        file.try_lock_exclusive()
            .map_err(|_| DbError::FileLocked { path: path.to_owned() })?;

        info!(path = %path.display(), "abriendo base de datos");

        // SAFETY: archivo existente, lock exclusivo adquirido — sin otros mapeos mutables activos.
        let mmap = unsafe { MmapMut::map_mut(&file)? };

        // Validar página 0.
        let page_count = {
            let meta_page = Self::read_page_from_mmap(&mmap, 0)?;
            let file_meta = Self::parse_file_meta(meta_page);

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
            file_meta.page_count
        };

        // Cargar FreeList desde página 1.
        let freelist = {
            let bitmap_page = Self::read_page_from_mmap(&mmap, 1)?;
            FreeList::from_bytes(bitmap_page.body(), page_count)
        };

        info!(path = %path.display(), page_count, "base de datos abierta");
        debug!(free_pages = freelist.free_count(), "freelist cargada desde disco");

        Ok(MmapStorage {
            mmap,
            file,
            freelist,
            freelist_dirty: false,
        })
    }

    /// Extiende el archivo en `extra_pages` páginas, remapea y actualiza metadata.
    ///
    /// Retorna el `page_id` de la primera página nueva.
    pub fn grow(&mut self, extra_pages: u64) -> Result<u64, DbError> {
        let old_count = self.page_count();
        let new_count = old_count + extra_pages;
        debug!(old_count, new_count, extra_pages, "storage creciendo");
        let new_size = new_count * PAGE_SIZE as u64;

        self.file.set_len(new_size)?;

        // SAFETY: archivo extendido a `new_size` bytes. Sin referencias externas
        // al mmap anterior (tenemos `&mut self`).
        self.mmap = unsafe { MmapMut::map_mut(&self.file)? };

        // Actualizar page_count en meta y CRC32c.
        self.update_page_count_in_mmap(new_count);

        // Extender la freelist para cubrir las nuevas páginas.
        self.freelist.grow(new_count);
        Self::write_freelist_to_mmap(&mut self.mmap, &self.freelist)?;

        Ok(old_count)
    }

    // ── Privados ──────────────────────────────────────────────────────────────

    fn read_page_from_mmap(mmap: &MmapMut, page_id: u64) -> Result<&Page, DbError> {
        let offset = page_id as usize * PAGE_SIZE;
        if offset + PAGE_SIZE > mmap.len() {
            return Err(DbError::PageNotFound { page_id });
        }
        let ptr = mmap[offset..].as_ptr();
        // SAFETY: offset dentro del mmap (verificado). mmap alineado ≥4KB (múltiplo de 64).
        // PAGE_SIZE=16384 múltiplo de 64 → cada página cumple align_of::<Page>()==64.
        // Page es repr(C, align(64)). Sin alias mutables (función toma &MmapMut).
        let page = unsafe { &*(ptr as *const Page) };
        page.verify_checksum()?;
        Ok(page)
    }

    fn write_meta_to_mmap(mmap: &mut MmapMut, page_count: u64) -> Result<(), DbError> {
        let mut meta_page = Page::new(PageType::Meta, 0);
        let file_meta = DbFileMeta {
            db_magic: DB_FILE_MAGIC,
            version: DB_VERSION,
            _pad: 0,
            page_count,
            _reserved: [0u8; PAGE_SIZE - HEADER_SIZE - 24],
        };
        // SAFETY: body y DbFileMeta tienen el mismo tamaño (const assert).
        // Escritura a memoria exclusiva de meta_page.
        unsafe {
            std::ptr::copy_nonoverlapping(
                &file_meta as *const DbFileMeta as *const u8,
                meta_page.body_mut().as_mut_ptr(),
                PAGE_SIZE - HEADER_SIZE,
            );
        }
        meta_page.update_checksum();
        mmap[..PAGE_SIZE].copy_from_slice(meta_page.as_bytes());
        Ok(())
    }

    fn write_freelist_to_mmap(mmap: &mut MmapMut, freelist: &FreeList) -> Result<(), DbError> {
        let mut bitmap_page = Page::new(PageType::Free, 1);
        freelist.to_bytes(bitmap_page.body_mut());
        bitmap_page.update_checksum();
        let offset = PAGE_SIZE; // página 1
        mmap[offset..offset + PAGE_SIZE].copy_from_slice(bitmap_page.as_bytes());
        Ok(())
    }

    fn parse_file_meta(page: &Page) -> &DbFileMeta {
        // SAFETY: body tiene PAGE_SIZE-HEADER_SIZE bytes = size_of::<DbFileMeta>()
        // (const assert). Page está align(64), body[0] en offset 64 → align 64.
        // DbFileMeta es repr(C) sin padding (tamaño == suma de campos).
        unsafe { &*(page.body().as_ptr() as *const DbFileMeta) }
    }

    /// Lee un u64 little-endian en `offset` del mmap.
    ///
    /// El slice siempre tiene exactamente 8 bytes (offset verificado por el
    /// caller o constante estática), por lo que la conversión no puede fallar.
    #[inline]
    fn read_u64_at(mmap: &[u8], offset: usize) -> u64 {
        // SAFETY del try_into: el slice tiene exactamente 8 bytes porque
        // `offset + 8 <= mmap.len()` está garantizado por la invariante de que
        // el mmap tiene al menos PAGE_SIZE bytes y PAGE_COUNT_OFFSET + 8 < PAGE_SIZE.
        u64::from_le_bytes(
            mmap[offset..offset + 8]
                .try_into()
                .expect("slice de 8 bytes para u64 — garantizado por invariante del mmap"),
        )
    }

    /// Actualiza page_count y CRC32c de la meta page directamente en el mmap.
    fn update_page_count_in_mmap(&mut self, count: u64) {
        self.mmap[PAGE_COUNT_OFFSET..PAGE_COUNT_OFFSET + 8].copy_from_slice(&count.to_le_bytes());
        let checksum = crc32c::crc32c(&self.mmap[HEADER_SIZE..PAGE_SIZE]);
        self.mmap[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].copy_from_slice(&checksum.to_le_bytes());
    }
}

// ── StorageEngine impl ────────────────────────────────────────────────────────

impl StorageEngine for MmapStorage {
    fn read_page(&self, page_id: u64) -> Result<&Page, DbError> {
        // Leer page_count directo del mmap sin verificar checksum — hot path.
        let count = Self::read_u64_at(&self.mmap, PAGE_COUNT_OFFSET);
        if page_id >= count {
            return Err(DbError::PageNotFound { page_id });
        }
        Self::read_page_from_mmap(&self.mmap, page_id)
    }

    fn write_page(&mut self, page_id: u64, page: &Page) -> Result<(), DbError> {
        let count = self.page_count();
        if page_id >= count {
            return Err(DbError::PageNotFound { page_id });
        }
        let offset = page_id as usize * PAGE_SIZE;
        self.mmap[offset..offset + PAGE_SIZE].copy_from_slice(page.as_bytes());
        Ok(())
    }

    fn alloc_page(&mut self, page_type: PageType) -> Result<u64, DbError> {
        // Intentar asignar desde la freelist actual.
        if let Some(page_id) = self.freelist.alloc() {
            let new_page = Page::new(page_type, page_id);
            let offset = page_id as usize * PAGE_SIZE;
            self.mmap[offset..offset + PAGE_SIZE].copy_from_slice(new_page.as_bytes());
            self.freelist_dirty = true;
            return Ok(page_id);
        }

        // Freelist agotada: crecer el storage.
        // grow() persiste freelist internamente porque cambia el page_count.
        let first_new = self.grow(GROW_PAGES)?;
        let page_id = self.freelist.alloc().ok_or(DbError::StorageFull)?;
        debug_assert_eq!(page_id, first_new);

        let new_page = Page::new(page_type, page_id);
        let offset = page_id as usize * PAGE_SIZE;
        self.mmap[offset..offset + PAGE_SIZE].copy_from_slice(new_page.as_bytes());
        self.freelist_dirty = true;
        Ok(page_id)
    }

    fn free_page(&mut self, page_id: u64) -> Result<(), DbError> {
        if page_id == 0 || page_id == 1 {
            return Err(DbError::Other(format!(
                "no se puede liberar la página reservada {page_id}"
            )));
        }
        self.freelist.free(page_id)?;
        self.freelist_dirty = true;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DbError> {
        // Persistir freelist si fue modificada desde el último flush.
        if self.freelist_dirty {
            Self::write_freelist_to_mmap(&mut self.mmap, &self.freelist)?;
            self.freelist_dirty = false;
        }
        self.mmap.flush()?;
        Ok(())
    }

    fn page_count(&self) -> u64 {
        Self::read_u64_at(&self.mmap, PAGE_COUNT_OFFSET)
    }
}

impl MmapStorage {
    /// Número de páginas libres actualmente (para benchmarks y monitoreo).
    pub fn free_count(&self) -> u64 {
        self.freelist.free_count()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tests::run_storage_engine_suite;

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
    fn test_file_lock_prevents_double_open() {
        let path = tmp_path();
        let _storage1 = MmapStorage::create(&path).unwrap();

        // Segundo intento de abrir mientras el primero está vivo → FileLocked.
        let result = MmapStorage::open(&path);
        assert!(
            matches!(result, Err(DbError::FileLocked { .. })),
            "esperaba FileLocked, obtuvo un resultado distinto"
        );
    }

    #[test]
    fn test_lock_released_after_drop() {
        let path = tmp_path();
        {
            let _storage = MmapStorage::create(&path).unwrap();
            // _storage tiene el lock
        }
        // Drop liberó el lock; reabrir debe funcionar.
        let storage = MmapStorage::open(&path).unwrap();
        assert_eq!(storage.page_count(), GROW_PAGES);
    }

    #[test]
    fn test_storage_engine_suite() {
        let path = tmp_path();
        let mut storage = MmapStorage::create(&path).unwrap();
        run_storage_engine_suite(&mut storage);
    }

    #[test]
    fn test_alloc_never_returns_reserved() {
        let path = tmp_path();
        let mut storage = MmapStorage::create(&path).unwrap();
        let ids: Vec<u64> = (0..10)
            .map(|_| storage.alloc_page(PageType::Data).unwrap())
            .collect();
        assert!(!ids.contains(&0));
        assert!(!ids.contains(&1));
    }

    #[test]
    fn test_alloc_free_reuse() {
        let path = tmp_path();
        let mut storage = MmapStorage::create(&path).unwrap();
        let id = storage.alloc_page(PageType::Data).unwrap();
        storage.free_page(id).unwrap();
        let id2 = storage.alloc_page(PageType::Data).unwrap();
        assert_eq!(id, id2);
    }

    #[test]
    fn test_freelist_persists_across_reopen() {
        let path = tmp_path();
        let allocated;
        {
            let mut storage = MmapStorage::create(&path).unwrap();
            allocated = storage.alloc_page(PageType::Data).unwrap();
            storage.flush().unwrap();
        }
        // Reabrir — el freelist debe recordar que `allocated` está en uso.
        let mut storage = MmapStorage::open(&path).unwrap();
        let next = storage.alloc_page(PageType::Data).unwrap();
        assert_ne!(
            next, allocated,
            "freelist no persistió: reutilizó página en uso"
        );
    }

    #[test]
    fn test_grow_triggers_on_exhaustion() {
        let path = tmp_path();
        let mut storage = MmapStorage::create(&path).unwrap();
        let initial_count = storage.page_count();
        // Agotar todas las páginas libres (GROW_PAGES - 2 reservadas).
        for _ in 0..(GROW_PAGES - 2) {
            storage.alloc_page(PageType::Data).unwrap();
        }
        // El siguiente alloc debe crecer automáticamente.
        storage.alloc_page(PageType::Data).unwrap();
        assert!(storage.page_count() > initial_count);
    }

    #[test]
    fn test_read_write_roundtrip() {
        let path = tmp_path();
        let mut storage = MmapStorage::create(&path).unwrap();
        let id = storage.alloc_page(PageType::Data).unwrap();

        let mut page = Page::new(PageType::Data, id);
        page.body_mut()[0] = 0xBE;
        page.body_mut()[1] = 0xEF;
        page.update_checksum();

        storage.write_page(id, &page).unwrap();
        let read = storage.read_page(id).unwrap();
        assert_eq!(read.body()[0], 0xBE);
        assert_eq!(read.body()[1], 0xEF);
    }

    #[test]
    fn test_flush_and_reopen_data() {
        let path = tmp_path();
        let id;
        {
            let mut storage = MmapStorage::create(&path).unwrap();
            id = storage.alloc_page(PageType::Data).unwrap();
            let mut page = Page::new(PageType::Data, id);
            page.body_mut()[0] = 0x42;
            page.update_checksum();
            storage.write_page(id, &page).unwrap();
            storage.flush().unwrap();
        }
        let storage = MmapStorage::open(&path).unwrap();
        assert_eq!(storage.read_page(id).unwrap().body()[0], 0x42);
    }
}
